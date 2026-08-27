//! What each subcommand does.
//!
//! Every handler returns a [`Report`] or a [`CliError`]; none of them writes to
//! a stream. That separation is what makes the output contract testable — the
//! shape of `--json` is a value these functions return, not a side effect of
//! having run them.
//!
//! # Reading this file while the backends are unfinished
//!
//! Three commands do their whole job today: `settings`, `hotkey` and the
//! `--dry-run` half of `capture`/`record`. The rest resolve their arguments
//! completely, then stop at [`crate::platform`] with a
//! [`CliError::NotImplemented`] naming the crate that owes the work.
//!
//! That order is deliberate. Validation, target resolution and destination
//! selection happen *before* the missing piece, so a mistake in the command line
//! is reported as a mistake in the command line even now, and the resolution
//! logic is exercised by tests that never touch a screen.

use std::path::Path;

use scrozz_core::{CaptureRequest, CaptureTarget, CursorMode, Error as CoreError};
use scrozz_export::{Clipboard, Encoder, FrameEncoder};
use scrozz_ocr::Ocr as _;

use crate::{
    cli::{
        CaptureArgs, Command, Compositor, DisplaySelector, HistoryCommand, HotkeyCommand,
        InteractiveMode, ListWhat, OcrSubject, RecordArgs, SettingsCommand, Sink, TargetSpec,
    },
    fault::{CliError, CliResult},
    hotkey_config, ipc,
    json::Json,
    platform,
    report::Report,
    settings,
};

/// Runs a command locally.
///
/// # Errors
///
/// Whatever the command produces. Cancellation arrives here as
/// [`scrozz_core::Error::Cancelled`] and is rendered as an outcome, not a fault.
pub fn dispatch(command: &Command) -> CliResult<Report> {
    dispatch_inner(command, None)
}

/// Runs a command while allowing an owner to cancel interactive capture.
///
/// Used by the GUI's forwarded-command worker so a portal picker cannot keep
/// application shutdown waiting. The ordinary CLI remains synchronous through
/// [`dispatch`].
pub(crate) fn dispatch_with_cancellation(
    command: &Command,
    cancellation: &scrozz_capture::CaptureCancellation,
) -> CliResult<Report> {
    if cancellation.is_cancelled() {
        return Err(CliError::Core(CoreError::Cancelled));
    }
    dispatch_inner(command, Some(cancellation))
}

fn dispatch_inner(
    command: &Command,
    cancellation: Option<&scrozz_capture::CaptureCancellation>,
) -> CliResult<Report> {
    match command {
        Command::Capture(args) => capture(args, cancellation),
        Command::Record(args) => record(args),
        Command::List(args) => list(args.what),
        Command::History(args) => history(&args.command),
        Command::Ocr(args) => ocr(args),
        Command::Settings(args) => settings_command(&args.command),
        Command::Hotkey(args) => hotkey(&args.command),
        Command::Gui => gui(),
    }
}

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

fn capture(
    args: &CaptureArgs,
    cancellation: Option<&scrozz_capture::CaptureCancellation>,
) -> CliResult<Report> {
    args.validate()?;
    let target = args.target.resolve()?;
    let sinks = args.sinks();

    let plan = Json::obj([
        ("target", target_json(&target)),
        ("interactive", Json::Bool(args.target.is_interactive())),
        ("cursor", Json::Bool(args.cursor)),
        ("window_shadow", Json::Bool(!args.no_window_shadow)),
        ("format", Json::str(args.format().slug())),
        ("quality", Json::opt(args.quality, |q| Json::Int(q.into()))),
        ("delay_secs", Json::opt(args.delay, Json::Float)),
        ("sinks", Json::arr(sinks.iter().map(sink_json))),
    ]);

    if args.dry_run {
        return Ok(Report::new(
            Json::obj([("dry_run", Json::Bool(true)), ("plan", plan)]),
            describe_plan("Would capture", &target, &sinks),
        ));
    }

    // The delay is deliberately *not* honoured before the backend check. Making
    // a user wait five seconds to be told the feature is unimplemented is a
    // small cruelty that costs nothing to avoid.
    let backend = platform::capture_backend()?;
    let request = CaptureRequest {
        target: capture_target(&target)?,
        cursor: if args.cursor {
            CursorMode::Visible
        } else {
            CursorMode::Hidden
        },
        include_window_shadow: !args.no_window_shadow,
    };

    wait_for_capture_delay(args.delay, cancellation)?;

    let capture = match cancellation {
        Some(cancellation) => platform::capture_with_cancellation(&request, cancellation)?,
        None => backend.capture(&request)?,
    };
    let frame = &capture.frame;

    let bytes = FrameEncoder::new()
        .encode(frame, args.format().to_export())
        .map_err(CliError::Core)?;

    let mut written = Vec::new();
    let mut raw = None;
    for sink in &sinks {
        match sink {
            Sink::File(path) => {
                std::fs::write(path, &bytes).map_err(|e| CliError::Core(CoreError::Io(e)))?;
                written.push(path.display().to_string());
            }
            Sink::Clipboard => {
                scrozz_export::SystemClipboard::new()
                    .write_image(frame)
                    .map_err(CliError::Core)?;
                written.push("clipboard".to_string());
            }
            Sink::Stdout => raw = Some(bytes.clone()),
            // D18: any folder the user picks, which is what lets a Dropbox or
            // iCloud directory provide sync for free with no service on our side.
            Sink::DefaultFolder => {
                let dir = dirs::picture_dir()
                    .or_else(dirs::home_dir)
                    .unwrap_or_else(std::env::temp_dir);
                let name = format!(
                    "Scrozz {}.{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    args.format().slug()
                );
                let path = dir.join(name);
                std::fs::write(&path, &bytes).map_err(|e| CliError::Core(CoreError::Io(e)))?;
                written.push(path.display().to_string());
            }
        }
    }

    let data = Json::obj([
        ("plan", plan),
        ("width", Json::Int(i64::from(frame.width()))),
        ("height", Json::Int(i64::from(frame.height()))),
        ("scale", Json::Float(frame.scale.get())),
        ("bytes", Json::Int(bytes.len() as i64)),
        ("provenance", Json::str(format!("{:?}", capture.provenance))),
        (
            "written",
            Json::arr(written.iter().map(|w| Json::str(w.as_str()))),
        ),
    ]);

    let human = format!(
        "Captured {}×{} at {}× ({} KB){}",
        frame.width(),
        frame.height(),
        frame.scale.get(),
        bytes.len() / 1024,
        if written.is_empty() {
            String::new()
        } else {
            format!(" → {}", written.join(", "))
        }
    );

    let mut report = Report::new(data, human);
    report.raw = raw;
    Ok(report)
}

fn wait_for_capture_delay(
    seconds: Option<f64>,
    cancellation: Option<&scrozz_capture::CaptureCancellation>,
) -> CliResult<()> {
    let Some(seconds) = seconds else {
        return Ok(());
    };
    let mut remaining = std::time::Duration::try_from_secs_f64(seconds)
        .map_err(|_| CliError::usage(format!("--delay is too large: {seconds} seconds")))?;
    if cancellation.is_none() {
        std::thread::sleep(remaining);
        return Ok(());
    }
    const POLL: std::time::Duration = std::time::Duration::from_millis(25);

    while !remaining.is_zero() {
        if cancellation.is_some_and(scrozz_capture::CaptureCancellation::is_cancelled) {
            return Err(CliError::Core(CoreError::Cancelled));
        }
        let step = remaining.min(POLL);
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }

    if cancellation.is_some_and(scrozz_capture::CaptureCancellation::is_cancelled) {
        Err(CliError::Core(CoreError::Cancelled))
    } else {
        Ok(())
    }
}

/// Turns a resolved [`TargetSpec`] into the core request type.
///
/// The interactive modes have no representation here on purpose: choosing a
/// target on screen is the overlay's job, and it hands back a concrete target.
/// Modelling "the user has not chosen yet" as a [`CaptureTarget`] would push
/// that uncertainty into every backend.
fn capture_target(spec: &TargetSpec) -> CliResult<CaptureTarget> {
    match spec {
        TargetSpec::Region(rect) => Ok(CaptureTarget::Region(*rect)),
        TargetSpec::AllDisplays => Ok(CaptureTarget::AllDisplays),
        // Resolving a name needs enumeration, so it goes through the same
        // backend the capture will use — an id resolved by a different object
        // is an id that can disagree.
        TargetSpec::Display(sel) => {
            let displays = platform::target_enumerator()?.displays()?;
            let found = match sel {
                DisplaySelector::Primary => displays.iter().find(|d| d.is_primary),
                // The pointer's display, which is where an overlay should appear.
                DisplaySelector::Active => platform::target_enumerator()
                    .ok()
                    .and_then(|e| e.active_display().ok())
                    .and_then(|a| displays.iter().find(|d| d.id == a.id))
                    .or_else(|| displays.iter().find(|d| d.is_primary)),
                DisplaySelector::Id(name) => displays
                    .iter()
                    .find(|d| d.id.0 == *name || d.name.eq_ignore_ascii_case(name)),
            };
            found
                .map(|d| CaptureTarget::Display(d.id.clone()))
                .ok_or_else(|| {
                    CliError::Core(CoreError::InvalidRequest(format!(
                        "no display matches {sel:?}; `scrozz list displays` shows what is available"
                    )))
                })
        }
        TargetSpec::Window(name) => {
            let windows = platform::target_enumerator()?.windows()?;
            windows
                .iter()
                .find(|w| {
                    w.id.0 == *name
                        || w.title
                            .as_deref()
                            .is_some_and(|t| t.to_lowercase().contains(&name.to_lowercase()))
                })
                .map(|w| CaptureTarget::Window(w.id.clone()))
                .ok_or_else(|| {
                    CliError::Core(CoreError::InvalidRequest(format!(
                        "no window matches {name:?}; `scrozz list windows` shows what is available"
                    )))
                })
        }
        TargetSpec::Interactive(_) => Err(CliError::not_implemented(
            "choosing a target on screen",
            "scrozz-ui (the selection overlay)",
        )),
    }
}

// ---------------------------------------------------------------------------
// record
// ---------------------------------------------------------------------------

fn record(args: &RecordArgs) -> CliResult<Report> {
    if args.stop {
        // Reaching here means no instance was running, because a running one
        // would have handled it. There is no session in this process to stop.
        return Err(CliError::Core(CoreError::InvalidRequest(
            "no recording is in progress; `record --stop` talks to the running \
             Scrozz, and nothing is running"
                .to_string(),
        )));
    }

    let target = args.target.resolve()?;
    let plan = Json::obj([
        ("target", target_json(&target)),
        ("fps", Json::Int(args.fps.into())),
        ("microphone", Json::Bool(args.microphone)),
        ("system_audio", Json::Bool(args.system_audio)),
        ("cursor", Json::Bool(args.cursor)),
        ("output", Json::opt(args.output.as_deref(), path_json)),
    ]);

    if args.dry_run {
        return Ok(Report::new(
            Json::obj([("dry_run", Json::Bool(true)), ("plan", plan)]),
            format!(
                "Would record {} at {} fps.",
                describe_target(&target),
                args.fps
            ),
        ));
    }

    let request = scrozz_record::RecordingRequest {
        target: capture_target(&target)?,
        microphone: args.microphone,
        system_audio: args.system_audio,
        fps: args.fps,
        show_cursor: args.cursor,
    };
    let _session = platform::start_recording(&request)?;

    Err(CliError::not_implemented(
        "recording the screen",
        "scrozz-record",
    ))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(what: ListWhat) -> CliResult<Report> {
    let enumerator = platform::target_enumerator();

    match what {
        ListWhat::Displays => {
            let displays = enumerator?.displays()?;
            let data = Json::arr(displays.iter().map(|d| {
                Json::obj([
                    ("id", Json::str(d.id.0.as_str())),
                    ("name", Json::str(d.name.as_str())),
                    ("width", Json::Float(d.bounds.size.width)),
                    ("height", Json::Float(d.bounds.size.height)),
                    ("scale", Json::Float(d.scale.get())),
                    ("primary", Json::Bool(d.is_primary)),
                ])
            }));
            let human = displays
                .iter()
                .map(|d| {
                    format!(
                        "{}  {}×{} @{}×{}",
                        d.id.0,
                        d.bounds.size.width,
                        d.bounds.size.height,
                        d.scale.get(),
                        if d.is_primary { "  (primary)" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Report::new(data, human))
        }
        ListWhat::Windows => {
            // D8: on Wayland this is not a missing feature, it is a missing
            // protocol. Saying so precisely — and naming the alternative that
            // does work — is the difference between a documented boundary and an
            // app that looks broken.
            if is_wayland() {
                return Err(CliError::Core(CoreError::Unsupported {
                    what: "listing windows".to_string(),
                    why: "Wayland has no window enumeration protocol: a client \
                          cannot see other clients' windows, by design. Use \
                          `scrozz capture --interactive window`, which asks the \
                          compositor's own picker to choose one."
                        .to_string(),
                }));
            }
            let windows = enumerator?.windows()?;
            let data = Json::arr(windows.iter().map(|w| {
                Json::obj([
                    ("id", Json::str(w.id.0.as_str())),
                    ("title", Json::opt(w.title.as_deref(), Json::str)),
                    (
                        "application",
                        Json::opt(w.application.as_deref(), Json::str),
                    ),
                    ("width", Json::Float(w.bounds.size.width)),
                    ("height", Json::Float(w.bounds.size.height)),
                ])
            }));
            let human = windows
                .iter()
                .map(|w| {
                    format!(
                        "{}  {}  {}",
                        w.id.0,
                        w.application.as_deref().unwrap_or("—"),
                        w.title.as_deref().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Report::new(data, human))
        }
    }
}

/// Whether this is a Wayland session.
fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

fn history(command: &HistoryCommand) -> CliResult<Report> {
    match command {
        HistoryCommand::List { .. } => {
            let _store = platform::store()?;
            Err(CliError::not_implemented(
                "listing the capture history",
                "scrozz-store",
            ))
        }
        HistoryCommand::Get { .. } => Err(CliError::not_implemented(
            "reading a stored capture",
            "scrozz-store (the Store trait has list, set_pinned and \
             enforce_retention, but no way to read a capture back)",
        )),
        HistoryCommand::Delete { .. } => Err(CliError::not_implemented(
            "deleting a stored capture",
            "scrozz-store (the Store trait exposes no delete)",
        )),
        HistoryCommand::Pin { .. } => {
            let _store = platform::store()?;
            Err(CliError::not_implemented(
                "pinning a capture",
                "scrozz-store",
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// ocr
// ---------------------------------------------------------------------------

fn ocr(args: &crate::cli::OcrArgs) -> CliResult<Report> {
    args.validate()?;
    let subject = args.resolve()?;

    // Check the platform before the subject: on Linux there is no engine at all,
    // and reading a file first only to say so afterwards wastes the user's time
    // and makes the failure look conditional when it is not.
    if !platform::ocr_available() {
        return Err(CliError::Core(CoreError::Unsupported {
            what: "recognising text".to_string(),
            why: "this build has no OCR engine. macOS uses Vision and Windows \
                  uses Windows.Media.Ocr, both supplied by the system; Linux has \
                  no system recogniser, and bundling one would add tens of \
                  megabytes of language model to every install."
                .to_string(),
        }));
    }

    match subject {
        OcrSubject::File(path) => {
            if !path.exists() {
                return Err(CliError::Core(CoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} does not exist", path.display()),
                ))));
            }
            let frame = platform::decode_image_file(&path)?;
            let blocks = platform::ocr_engine().recognize(&frame)?;
            Ok(ocr_report(&blocks, &path.display().to_string()))
        }
        OcrSubject::Capture(_) => {
            let _store = platform::store()?;
            Err(CliError::not_implemented(
                "recognising text in a stored capture",
                "scrozz-store",
            ))
        }
    }
}

/// Renders recognised text for both `--json` and human output.
///
/// The human rendering is **the text and nothing else**, one block per line, so
/// `scrozz ocr shot.png | pbcopy` does the obvious thing. Bounds and confidence
/// belong in `--json`, where a consumer asked for structure; printing them in
/// the human path would corrupt the far more common case of piping the text
/// somewhere.
fn ocr_report(blocks: &[scrozz_ocr::TextBlock], source: &str) -> Report {
    let text = blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let data = Json::obj([
        ("source", Json::str(source)),
        ("block_count", Json::Int(blocks.len() as i64)),
        ("text", Json::str(text.as_str())),
        (
            "blocks",
            Json::arr(blocks.iter().map(|b| {
                Json::obj([
                    ("text", Json::str(b.text.as_str())),
                    ("confidence", Json::Float(f64::from(b.confidence))),
                    ("x", Json::Float(b.bounds.origin.x)),
                    ("y", Json::Float(b.bounds.origin.y)),
                    ("width", Json::Float(b.bounds.size.width)),
                    ("height", Json::Float(b.bounds.size.height)),
                ])
            })),
        ),
    ]);

    Report::new(data, text)
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

fn settings_command(command: &SettingsCommand) -> CliResult<Report> {
    match command {
        SettingsCommand::Get { key: None } => Ok(Report::new(
            Json::obj([("settings", settings::all_json())]),
            settings::all_human(),
        )),

        SettingsCommand::Get { key: Some(key) } => {
            let setting = settings::lookup(key)?;
            Ok(Report::new(setting.to_json(), setting.default.to_string()))
        }

        SettingsCommand::Set { key, value } => {
            // Validate first and completely. A rejected value must be rejected
            // for the right reason: "that is not a format" is useful, "settings
            // are not implemented" is not, and the user needs to hear the first
            // one even while the second is true.
            let setting = settings::lookup(key)?;
            setting.validate(value)?;
            Err(CliError::not_implemented(
                format!("saving {key}"),
                "scrozz-store (settings persistence)",
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// hotkey
// ---------------------------------------------------------------------------

fn hotkey(command: &HotkeyCommand) -> CliResult<Report> {
    let HotkeyCommand::GenerateConfig {
        compositor,
        action,
        accelerator,
        exec,
    } = command;

    let compositor = resolve_compositor(*compositor)?;
    let config = hotkey_config::generate(compositor, exec, *action, accelerator.as_deref())?;

    Ok(Report::new(config.to_json(), config.to_text()))
}

/// Picks the compositor to target.
///
/// # Errors
///
/// On a system with a working global-shortcut API this is
/// [`CoreError::Unsupported`] rather than a usage error: the command is not
/// misused, it is inapplicable, and saying so explains why the user never needed
/// it.
fn resolve_compositor(explicit: Option<Compositor>) -> CliResult<Compositor> {
    if let Some(compositor) = explicit {
        return Ok(compositor);
    }
    if let Some(detected) = hotkey_config::detect_compositor() {
        return Ok(detected);
    }
    if cfg!(target_os = "linux") {
        return Err(CliError::usage(
            "no sway or Hyprland session was detected; \
             pass --compositor sway or --compositor hyprland",
        ));
    }
    Err(CliError::Core(CoreError::Unsupported {
        what: "generating compositor keybindings".to_string(),
        why: "this command exists for wlroots compositors, which have no \
              global-shortcut portal. This system registers hotkeys directly, so \
              Scrozz sets them up itself and there is nothing to paste. Pass \
              --compositor to generate a fragment anyway."
            .to_string(),
    }))
}

// ---------------------------------------------------------------------------
// gui
// ---------------------------------------------------------------------------

fn gui() -> CliResult<Report> {
    // D27: the GUI has no window at rest, so the first thing it must do is drop
    // out of the Dock. Called here rather than in the launcher so that the
    // ordering is visible: policy first, then anything that could put pixels on
    // screen.
    platform::become_accessory_app()?;

    Err(CliError::not_implemented(
        "launching the menu-bar app",
        "scrozz-ui (the shell has no event loop yet)",
    ))
}

// ---------------------------------------------------------------------------
// shared rendering
// ---------------------------------------------------------------------------

/// The JSON form of a resolved target.
///
/// A tagged object rather than a bare string: `{"kind":"region","x":0,...}` can
/// grow a field without breaking a consumer, and a consumer can switch on `kind`
/// without parsing prose.
#[must_use]
pub fn target_json(target: &TargetSpec) -> Json {
    match target {
        TargetSpec::Region(rect) => Json::obj([
            ("kind", Json::str("region")),
            ("x", Json::Float(rect.origin.x)),
            ("y", Json::Float(rect.origin.y)),
            ("width", Json::Float(rect.size.width)),
            ("height", Json::Float(rect.size.height)),
        ]),
        TargetSpec::Window(selector) => Json::obj([
            ("kind", Json::str("window")),
            ("selector", Json::str(selector.as_str())),
        ]),
        TargetSpec::Display(selector) => Json::obj([
            ("kind", Json::str("display")),
            (
                "selector",
                Json::str(match selector {
                    DisplaySelector::Primary => "primary",
                    DisplaySelector::Active => "active",
                    DisplaySelector::Id(id) => id.as_str(),
                }),
            ),
        ]),
        TargetSpec::AllDisplays => Json::obj([("kind", Json::str("all-displays"))]),
        TargetSpec::Interactive(mode) => Json::obj([
            ("kind", Json::str("interactive")),
            ("mode", Json::str(interactive_slug(*mode))),
        ]),
    }
}

/// The stable slug for an interactive mode.
#[must_use]
pub const fn interactive_slug(mode: InteractiveMode) -> &'static str {
    match mode {
        InteractiveMode::Region => "region",
        InteractiveMode::Window => "window",
        InteractiveMode::Display => "display",
    }
}

fn sink_json(sink: &Sink) -> Json {
    match sink {
        Sink::File(path) => Json::obj([
            ("kind", Json::str("file")),
            ("path", path_json(path.as_path())),
        ]),
        other => Json::obj([("kind", Json::str(other.slug()))]),
    }
}

fn path_json(path: &Path) -> Json {
    Json::str(path.to_string_lossy().into_owned())
}

fn describe_target(target: &TargetSpec) -> String {
    match target {
        TargetSpec::Region(rect) => format!(
            "the region {}\u{d7}{} at ({}, {})",
            rect.size.width, rect.size.height, rect.origin.x, rect.origin.y
        ),
        TargetSpec::Window(selector) => format!("the window {selector:?}"),
        TargetSpec::Display(DisplaySelector::Primary) => "the primary display".to_string(),
        TargetSpec::Display(DisplaySelector::Active) => "the active display".to_string(),
        TargetSpec::Display(DisplaySelector::Id(id)) => format!("display {id}"),
        TargetSpec::AllDisplays => "every display".to_string(),
        TargetSpec::Interactive(mode) => {
            format!("an interactively chosen {}", interactive_slug(*mode))
        }
    }
}

fn describe_plan(verb: &str, target: &TargetSpec, sinks: &[Sink]) -> String {
    let destinations: Vec<String> = sinks
        .iter()
        .map(|sink| match sink {
            Sink::File(path) => path.display().to_string(),
            Sink::DefaultFolder => "the capture folder".to_string(),
            Sink::Clipboard => "the clipboard".to_string(),
            Sink::Stdout => "stdout".to_string(),
        })
        .collect();
    format!(
        "{verb} {} to {}.",
        describe_target(target),
        destinations.join(" and ")
    )
}

/// Whether the running instance should handle this command instead.
///
/// Kept next to the handlers so the two are read together: a command that grows
/// shared state must also grow a forwarding policy.
#[must_use]
pub fn should_forward(command: &Command, no_ipc: bool) -> ipc::Forwarding {
    if no_ipc {
        return ipc::Forwarding::Never;
    }
    ipc::policy(command)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::{cli::Cli, exit::Exit};

    fn run(argv: &[&str]) -> CliResult<Report> {
        let cli = Cli::try_parse_from(argv).expect("should parse");
        cli.validate()?;
        dispatch(&cli.command.clone().expect("should have a command"))
    }

    fn json_of(argv: &[&str]) -> String {
        run(argv).expect("should succeed").data.to_compact_string()
    }

    // -- dry-run capture ---------------------------------------------------

    #[test]
    fn a_dry_run_capture_reports_its_plan_without_capturing() {
        let rendered = json_of(&[
            "scrozz",
            "capture",
            "--region",
            "10,20,300,400",
            "--dry-run",
        ]);
        assert!(rendered.contains(r#""dry_run":true"#), "{rendered}");
        assert!(rendered.contains(r#""kind":"region""#), "{rendered}");
        assert!(rendered.contains(r#""x":10.0"#), "{rendered}");
        assert!(rendered.contains(r#""width":300.0"#), "{rendered}");
    }

    #[test]
    fn a_bare_dry_run_capture_defaults_to_an_interactive_region() {
        let rendered = json_of(&["scrozz", "capture", "--dry-run"]);
        assert!(rendered.contains(r#""kind":"interactive""#), "{rendered}");
        assert!(rendered.contains(r#""mode":"region""#), "{rendered}");
        assert!(rendered.contains(r#""interactive":true"#), "{rendered}");
    }

    #[test]
    fn destinations_are_additive_and_ordered() {
        let rendered = json_of(&[
            "scrozz",
            "capture",
            "--all-displays",
            "-o",
            "/x/shot.png",
            "--clipboard",
            "--dry-run",
        ]);
        assert!(
            rendered.contains(r#""kind":"file","path":"/x/shot.png""#),
            "{rendered}"
        );
        assert!(rendered.contains(r#"{"kind":"clipboard"}"#), "{rendered}");
        assert!(!rendered.contains("default-folder"), "{rendered}");
    }

    #[test]
    fn with_no_destination_the_capture_folder_is_used() {
        let rendered = json_of(&["scrozz", "capture", "--dry-run"]);
        assert!(
            rendered.contains(r#"{"kind":"default-folder"}"#),
            "{rendered}"
        );
    }

    #[test]
    fn the_human_plan_reads_as_a_sentence() {
        let report = run(&["scrozz", "capture", "--all-displays", "--dry-run"]).unwrap();
        assert_eq!(
            report.human,
            "Would capture every display to the capture folder."
        );
    }

    #[test]
    fn format_and_quality_reach_the_plan() {
        let rendered = json_of(&[
            "scrozz",
            "capture",
            "--dry-run",
            "--format",
            "webp",
            "--quality",
            "72",
        ]);
        assert!(rendered.contains(r#""format":"webp""#), "{rendered}");
        assert!(rendered.contains(r#""quality":72"#), "{rendered}");
    }

    #[test]
    fn an_unset_quality_is_null_rather_than_missing() {
        let rendered = json_of(&["scrozz", "capture", "--dry-run"]);
        assert!(rendered.contains(r#""quality":null"#), "{rendered}");
        assert!(rendered.contains(r#""delay_secs":null"#), "{rendered}");
    }

    #[test]
    fn window_shadow_is_reported_positively() {
        // The flag is `--no-window-shadow`; the plan says `window_shadow`. A
        // consumer should not have to reason about a double negative.
        let rendered = json_of(&["scrozz", "capture", "--dry-run"]);
        assert!(rendered.contains(r#""window_shadow":true"#), "{rendered}");

        let rendered = json_of(&["scrozz", "capture", "--dry-run", "--no-window-shadow"]);
        assert!(rendered.contains(r#""window_shadow":false"#), "{rendered}");
    }

    #[test]
    fn a_dry_run_never_reaches_a_backend() {
        // The real protection against a stray capture during a test run.
        for argv in [
            vec!["scrozz", "capture", "--dry-run"],
            vec!["scrozz", "capture", "--display", "primary", "--dry-run"],
            vec!["scrozz", "capture", "--window", "Safari", "--dry-run"],
            vec!["scrozz", "record", "--dry-run"],
        ] {
            assert!(run(&argv).is_ok(), "{argv:?}");
        }
    }

    #[test]
    fn a_bad_region_is_rejected_before_anything_else_happens() {
        let cli = Cli::try_parse_from(["scrozz", "capture", "--region", "0,0,0,100", "--dry-run"]);
        assert!(cli.is_err(), "a zero-width region should not parse");
    }

    #[test]
    fn a_negative_delay_is_a_usage_error() {
        let err = run(&["scrozz", "capture", "--delay", "-1", "--dry-run"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Usage);
    }

    // -- dry-run record ----------------------------------------------------

    #[test]
    fn a_dry_run_record_reports_its_plan() {
        let rendered = json_of(&[
            "scrozz",
            "record",
            "--dry-run",
            "--fps",
            "60",
            "--microphone",
        ]);
        assert!(rendered.contains(r#""fps":60"#), "{rendered}");
        assert!(rendered.contains(r#""microphone":true"#), "{rendered}");
        assert!(rendered.contains(r#""system_audio":false"#), "{rendered}");
    }

    #[test]
    fn stopping_with_nothing_running_explains_itself() {
        let err = run(&["scrozz", "record", "--stop"]).unwrap_err();
        assert_eq!(err.exit(), Exit::InvalidRequest);
        assert!(
            err.to_string().contains("no recording is in progress"),
            "{err}"
        );
    }

    // -- unimplemented paths -----------------------------------------------

    #[test]
    fn every_unimplemented_command_reports_rather_than_panics() {
        let cases = [
            vec!["scrozz", "capture", "--region", "0,0,10,10"],
            vec!["scrozz", "record"],
            vec!["scrozz", "history", "list"],
            vec!["scrozz", "history", "get", "abc"],
            vec!["scrozz", "history", "delete", "abc"],
            vec!["scrozz", "history", "pin", "abc"],
            vec!["scrozz", "gui"],
        ];
        for argv in cases {
            let err = run(&argv).unwrap_err();
            assert!(
                !err.is_cancellation(),
                "{argv:?} should not look like a cancellation"
            );
            assert_ne!(err.exit(), Exit::Success, "{argv:?}");
        }
    }

    #[test]
    fn the_missing_store_reads_are_named_precisely() {
        // These are the two the Store trait genuinely cannot express today, as
        // opposed to merely lacking an implementation.
        let err = run(&["scrozz", "history", "get", "abc"]).unwrap_err();
        assert!(
            err.to_string().contains("no way to read a capture back"),
            "{err}"
        );

        let err = run(&["scrozz", "history", "delete", "abc"]).unwrap_err();
        assert!(err.to_string().contains("no delete"), "{err}");
    }

    #[test]
    fn launching_the_gui_from_a_test_does_nothing_visible() {
        // Load-bearing: the test suite must never put a window on screen.
        let err = run(&["scrozz", "gui"]).unwrap_err();
        assert_eq!(err.exit(), Exit::NotImplemented);
    }

    // -- settings ----------------------------------------------------------

    #[test]
    fn listing_settings_works_today() {
        let rendered = json_of(&["scrozz", "settings", "get"]);
        assert!(rendered.contains(r#""key":"capture.format""#), "{rendered}");
        assert!(
            rendered.contains(r#""key":"hotkey.record-stop""#),
            "{rendered}"
        );
    }

    #[test]
    fn reading_one_setting_returns_just_that_one() {
        let report = run(&["scrozz", "settings", "get", "capture.quality"]).unwrap();
        assert_eq!(report.human, "90");
        assert!(
            report
                .data
                .to_compact_string()
                .contains(r#""key":"capture.quality""#)
        );
    }

    #[test]
    fn an_unknown_setting_is_a_usage_error_with_a_suggestion() {
        let err = run(&["scrozz", "settings", "get", "capture.forrmat"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Usage);
        assert!(err.to_string().contains("capture.format"), "{err}");
    }

    #[test]
    fn a_bad_value_is_rejected_for_being_bad_not_for_being_unimplemented() {
        // The distinction that matters: the user must learn about their mistake
        // even though persistence is missing.
        let err = run(&["scrozz", "settings", "set", "capture.format", "gif"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Usage);
        assert!(err.to_string().contains("png"), "{err}");
    }

    #[test]
    fn a_good_value_reports_the_missing_persistence() {
        let err = run(&["scrozz", "settings", "set", "capture.format", "webp"]).unwrap_err();
        assert_eq!(err.exit(), Exit::NotImplemented);
        assert!(err.to_string().contains("capture.format"), "{err}");
    }

    // -- hotkey ------------------------------------------------------------

    #[test]
    fn generating_a_sway_config_works_today() {
        let report = run(&[
            "scrozz",
            "hotkey",
            "generate-config",
            "--compositor",
            "sway",
        ])
        .unwrap();
        assert!(
            report
                .human
                .contains("bindsym Mod4+Shift+4 exec scrozz capture")
        );
        assert!(
            report
                .data
                .to_compact_string()
                .contains(r#""compositor":"sway""#)
        );
    }

    #[test]
    fn generating_a_hyprland_config_works_today() {
        let report = run(&[
            "scrozz",
            "hotkey",
            "generate-config",
            "--compositor",
            "hyprland",
        ])
        .unwrap();
        assert!(
            report
                .human
                .contains("bind = SUPER SHIFT, 4, exec, scrozz capture")
        );
    }

    #[test]
    fn a_single_action_can_be_generated() {
        let report = run(&[
            "scrozz",
            "hotkey",
            "generate-config",
            "--compositor",
            "sway",
            "--action",
            "record-stop",
        ])
        .unwrap();
        assert_eq!(
            report
                .human
                .lines()
                .filter(|l| l.starts_with("bindsym"))
                .count(),
            1
        );
        assert!(report.human.contains("record --stop"));
    }

    #[test]
    fn a_custom_exec_path_is_honoured() {
        let report = run(&[
            "scrozz",
            "hotkey",
            "generate-config",
            "--compositor",
            "sway",
            "--exec",
            "/usr/local/bin/scrozz",
        ])
        .unwrap();
        assert!(report.human.contains("exec /usr/local/bin/scrozz capture"));
    }

    #[test]
    fn no_compositor_and_no_flag_explains_rather_than_fails_obscurely() {
        let _env = crate::test_env::lock();
        let err = run(&["scrozz", "hotkey", "generate-config"]);
        match err {
            // On a wlroots session the command legitimately succeeds.
            Ok(_) => assert!(hotkey_config::detect_compositor().is_some()),
            Err(e) => {
                let exit = e.exit();
                assert!(
                    matches!(exit, Exit::Usage | Exit::Unsupported),
                    "unexpected exit {exit:?}"
                );
                assert!(e.to_string().contains("--compositor"), "{e}");
            }
        }
    }

    // -- list --------------------------------------------------------------

    #[test]
    fn listing_windows_under_wayland_explains_the_protocol_gap() {
        let _env = crate::test_env::lock();
        crate::test_env::set("WAYLAND_DISPLAY", "wayland-0");
        let err = run(&["scrozz", "list", "windows"]).unwrap_err();

        assert_eq!(err.exit(), Exit::Unsupported);
        let text = err.to_human();
        assert!(text.contains("no window enumeration protocol"), "{text}");
        // D8 requires the alternative, not just the refusal.
        assert!(text.contains("--interactive window"), "{text}");
    }

    #[test]
    fn listing_displays_is_never_an_unsupported_platform_error() {
        // Every platform can enumerate displays; only windows are contentious.
        let err = list(ListWhat::Displays).unwrap_err();
        assert_eq!(err.exit(), Exit::NotImplemented);
    }

    // -- ocr ---------------------------------------------------------------

    #[test]
    fn ocr_on_a_platform_without_an_engine_says_why() {
        if platform::ocr_available() {
            return;
        }
        let err = run(&["scrozz", "ocr", "--capture", "abc"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Unsupported);
        assert!(err.to_string().contains("no system recogniser"), "{err}");
    }

    #[test]
    fn ocr_on_a_missing_file_reports_the_file_not_the_backend() {
        if !platform::ocr_available() {
            return;
        }
        let err = run(&["scrozz", "ocr", "--file", "./definitely-not-here.png"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Io);
    }

    // -- shared rendering --------------------------------------------------

    #[test]
    fn every_target_kind_renders_with_a_tag() {
        let cases = [
            (
                vec!["scrozz", "capture", "--region", "1,2,3,4", "--dry-run"],
                "region",
            ),
            (
                vec!["scrozz", "capture", "--window", "Safari", "--dry-run"],
                "window",
            ),
            (
                vec!["scrozz", "capture", "--display", "primary", "--dry-run"],
                "display",
            ),
            (
                vec!["scrozz", "capture", "--all-displays", "--dry-run"],
                "all-displays",
            ),
            (
                vec!["scrozz", "capture", "--interactive", "--dry-run"],
                "interactive",
            ),
        ];
        for (argv, kind) in cases {
            let rendered = json_of(&argv);
            assert!(
                rendered.contains(&format!(r#""kind":"{kind}""#)),
                "{argv:?} produced {rendered}"
            );
        }
    }

    #[test]
    fn interactive_slugs_match_the_value_enum_spelling() {
        assert_eq!(interactive_slug(InteractiveMode::Region), "region");
        assert_eq!(interactive_slug(InteractiveMode::Window), "window");
        assert_eq!(interactive_slug(InteractiveMode::Display), "display");
    }

    #[test]
    fn no_ipc_overrides_every_forwarding_policy() {
        let cli = Cli::try_parse_from(["scrozz", "record", "--stop"]).unwrap();
        let command = cli.command.unwrap();
        assert_eq!(should_forward(&command, false), ipc::Forwarding::Require);
        assert_eq!(should_forward(&command, true), ipc::Forwarding::Never);
    }
}
