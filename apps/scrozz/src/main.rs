//! Scrozz — screenshots and screen recording for macOS, Windows and Linux.
//!
//! # One binary, two front ends
//!
//! Per decision D11 the CLI is not a convenience wrapper bolted on later; it is
//! the architecture. Every capture the GUI can take, the CLI can take headlessly.
//!
//! That matters for three separate reasons:
//!
//! 1. **On wlroots compositors it is the only way hotkeys can work at all.**
//!    There is no global-shortcut portal there, so the user binds a compositor
//!    keybinding to `scrozz capture`. Without a CLI, Scrozz simply has no
//!    hotkeys on sway or Hyprland.
//! 2. **It makes the app scriptable**, which no competitor in this space does
//!    well.
//! 3. **It makes the app testable by agents**, who cannot click.
//!
//! # How an invocation flows
//!
//! ```text
//! argv ──▶ clap ──▶ single-instance check ──▶ command ──▶ Report ──▶ streams
//!           │              │                     │                     │
//!           │              └── forward to the running app when it owns
//!           │                  the state this command touches
//!           │
//!           └── exit 2 on a parse error, before anything else happens
//! ```
//!
//! Four rules hold at every step, and each one has a module enforcing it:
//!
//! - **stdout is the result; stderr is everything else** ([`report`]). Logs never
//!   touch stdout, so `scrozz capture --stdout > shot.png` cannot be corrupted by
//!   a stray `--verbose`.
//! - **Every failure has a defined exit code** ([`exit`]). A script can tell
//!   "the user pressed Escape" from "the app is broken" without parsing text.
//! - **A known limitation is never a crash** ([`fault`]). Permission denials
//!   (D15) print the settings pane to open; platform gaps (D8) print why and
//!   what to do instead.
//! - **An unfinished backend is never a panic** ([`platform`]). Much of this
//!   workspace is `todo!()` today; none of it reaches the user as exit 101.

mod cli;
mod cloud;
mod commands;
mod exit;
mod fault;
mod gui;
mod hotkey_config;
mod ipc;
mod json;
mod output;
mod platform;
mod report;
mod settings;
#[cfg(test)]
mod test_env;

use std::{io::Write, process::ExitCode};

use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt};

use crate::{
    cli::{Cli, Command},
    exit::Exit,
    fault::{CliError, CliResult},
    report::{Report, Reporter},
};

fn main() -> ExitCode {
    // `try_parse` rather than `parse`, because `parse` exits the process itself
    // and would bypass the exit-code contract. `--help` and `--version` are
    // "errors" in clap's model, which is why the display path below is separate
    // from the failure path.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return render_clap_error(&err),
    };

    init_tracing(cli.global.verbose, cli.global.quiet);

    // A bare `scrozz` is the GUI.
    let command = cli.command.clone().unwrap_or(Command::Gui);
    run(&command, &cli)
}
fn run(command: &Command, cli: &Cli) -> ExitCode {
    let reporter = Reporter::from_global(&cli.global);
    let slug = command.slug();

    match execute(command, cli) {
        Ok(Outcome::Local(report)) => {
            if let Err(e) = reporter.emit(&slug, &report) {
                return report_stream_failure(&e);
            }
            Exit::Success.into()
        }
        Ok(Outcome::Relayed(code)) => ExitCode::from(code),
        Err(err) => {
            let status = err.exit();
            // Logged as well as printed: the human text is deliberately terse
            // guidance, and `-v` is where the detail belongs.
            tracing::debug!(exit = status.code(), kind = status.slug(), "{err}");
            if let Err(e) = reporter.emit_error(&slug, &err) {
                return report_stream_failure(&e);
            }
            status.into()
        }
    }
}

/// Where a command ran.
enum Outcome {
    /// Handled in this process.
    Local(Report),
    /// Handled by the running instance, which already produced the output.
    Relayed(u8),
}

fn execute(command: &Command, cli: &Cli) -> CliResult<Outcome> {
    if let Some(injected) = fault::simulated_error() {
        return Err(injected);
    }

    cli.validate()?;

    match commands::should_forward(command, cli.global.no_ipc) {
        ipc::Forwarding::Never => {}
        ipc::Forwarding::Prefer => {
            if let Some(code) = try_forward(command)? {
                return Ok(Outcome::Relayed(code));
            }
        }
        ipc::Forwarding::Require => {
            // No fallback: the state this command acts on lives in the other
            // process. Failing loudly beats silently doing nothing.
            let code = try_forward(command)?.ok_or_else(|| {
                CliError::ipc(
                    "no Scrozz is running. A recording belongs to the process that \
                     started it, so there is nothing here to stop.",
                )
            })?;
            return Ok(Outcome::Relayed(code));
        }
    }

    // The GUI is not a command that runs and returns; it is the process
    // becoming an application. It is handled here rather than in `commands` so
    // the command layer stays a pure request-to-report function — which is what
    // makes it usable from the IPC listener the GUI itself runs.
    if matches!(command, Command::Gui) {
        return gui::run(cli).map(Outcome::Local);
    }

    commands::dispatch(command).map(Outcome::Local)
}

/// Hands the invocation to a running instance, if there is one.
///
/// Returns `Ok(None)` when nothing is listening, which is the ordinary case and
/// not a failure.
fn try_forward(command: &Command) -> CliResult<Option<u8>> {
    let _ = command;
    if !matches!(ipc::probe(), ipc::Status::Running) {
        return Ok(None);
    }

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let response = ipc::forward(&argv)?;

    // Relayed byte for byte. The whole point of the single-instance design is
    // that `scrozz capture --json` produces the same document whether or not the
    // menu-bar app happens to be running; re-encoding here would break that.
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(&response.payload)
        .and_then(|()| stdout.flush())
    {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        other => other.map_err(|e| CliError::ipc(format!("could not relay the response: {e}")))?,
    }
    tracing::debug!(
        stream = response.stream.token(),
        bytes = response.payload.len(),
        "relayed from the running instance"
    );
    Ok(Some(response.code))
}

/// Renders `--help`, `--version` and parse failures.
///
/// clap writes these itself; the job here is only to get the stream and the exit
/// code right, and the three cases differ:
///
/// - **Help or version that was asked for** is the result. stdout, exit 0, so
///   `scrozz --help | less` works.
/// - **An incomplete command** — `scrozz list` with no `displays` or `windows` —
///   gets the same help text, but it is a usage failure, so stderr and exit 2.
///   clap would put it on stdout; a script piping `scrozz list` into `jq` should
///   not receive a help page and a success status.
/// - **Anything else** is a parse error: clap's own stream choice, exit 2.
fn render_clap_error(err: &clap::Error) -> ExitCode {
    use clap::error::ErrorKind;

    match err.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            let _ = err.print();
            Exit::Success.into()
        }
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            let rendered = err.render();
            let mut stderr = std::io::stderr();
            let _ = if std::io::IsTerminal::is_terminal(&stderr) {
                writeln!(stderr, "{}", rendered.ansi())
            } else {
                writeln!(stderr, "{rendered}")
            };
            Exit::Usage.into()
        }
        _ => {
            let _ = err.print();
            Exit::Usage.into()
        }
    }
}

/// The last resort when even writing the result failed.
fn report_stream_failure(err: &std::io::Error) -> ExitCode {
    // Not via the reporter: the reporter is what just failed.
    let _ = writeln!(std::io::stderr(), "scrozz: could not write output: {err}");
    Exit::Io.into()
}

/// Initialises logging.
///
/// # Two rules
///
/// **Logs go to stderr, always.** stdout carries the result — a JSON document or
/// a PNG — and a log line in the middle of either is a corrupt result.
///
/// **`RUST_LOG` wins.** `-v` raises the floor for people who do not want to think
/// about filter syntax; anyone who set `RUST_LOG` deliberately gets exactly what
/// they asked for.
fn init_tracing(verbose: u8, quiet: bool) {
    let default = match (quiet, verbose) {
        (true, _) => "error",
        (false, 0) => "warn",
        (false, 1) => "info",
        (false, 2) => "debug",
        (false, _) => "trace",
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("scrozz={default},warn")));

    // `try_init` rather than `init`: this is also reachable from the test
    // binary, where a second call must not abort the process.
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        // The CLI is often invoked from a compositor keybinding, where output
        // lands in a session log read long afterwards and out of order.
        .with_target(true)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_surface_is_internally_consistent() {
        // clap's own audit: duplicate argument ids, conflicting short flags,
        // groups naming arguments that do not exist. Cheap, and it catches
        // mistakes that would otherwise appear only at runtime.
        Cli::command().debug_assert();
    }

    #[test]
    fn help_succeeds_and_a_bad_argument_does_not() {
        let help = Cli::try_parse_from(["scrozz", "--help"]).unwrap_err();
        assert_eq!(render_clap_error(&help), ExitCode::from(0));

        let bad = Cli::try_parse_from(["scrozz", "--nonsense"]).unwrap_err();
        assert_eq!(render_clap_error(&bad), ExitCode::from(2));
    }

    #[test]
    fn version_succeeds() {
        let version = Cli::try_parse_from(["scrozz", "--version"]).unwrap_err();
        assert_eq!(render_clap_error(&version), ExitCode::from(0));
    }

    #[test]
    fn an_unknown_subcommand_is_a_usage_error() {
        let err = Cli::try_parse_from(["scrozz", "screenshot"]).unwrap_err();
        assert_eq!(render_clap_error(&err), ExitCode::from(2));
    }

    #[test]
    fn an_incomplete_command_is_a_usage_error_not_a_help_request() {
        // `scrozz list` names no thing to list. It gets the help text, but a
        // script must not read it as success.
        let err = Cli::try_parse_from(["scrozz", "list"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(render_clap_error(&err), ExitCode::from(2));
    }

    #[test]
    fn a_bare_invocation_is_the_gui() {
        let cli = Cli::try_parse_from(["scrozz"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn initialising_tracing_twice_is_harmless() {
        init_tracing(0, false);
        init_tracing(3, false);
    }
}
