//! The single-instance listener.
//!
//! # The problem this solves
//!
//! Once the menu-bar app is running it owns things a second process cannot see:
//! the capture stack on screen, the recording in progress, the hotkey
//! registrations. A `scrozz capture` typed into a terminal at that moment must
//! therefore happen *inside* the running app, so the result joins the stack the
//! user is already looking at rather than appearing nowhere.
//!
//! [`crate::ipc`] already has the client half — [`crate::ipc::forward`] is what
//! the terminal process calls, and `main` already routes through it. This module
//! is the half that answers, and it lives here rather than in `ipc.rs` because
//! it only exists while the GUI does.
//!
//! # Fidelity
//!
//! A forwarded command must produce byte-identical output to a local one; the
//! whole point is that a script cannot tell the difference. So the answer is
//! built exactly the way [`crate::report::Reporter::emit`] builds it — raw bytes
//! when there are raw bytes, the JSON envelope when `--json` was passed, the
//! trimmed human line otherwise — and the exit code is adopted verbatim.
//!
//! # Non-blocking
//!
//! [`Server::poll`] never waits. It is called from the main thread's tick,
//! between servicing the tray and the hotkey queue, and a blocking `accept()`
//! there would freeze the menu bar until someone happened to run a command.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, channel},
    },
    thread::JoinHandle,
    time::Duration,
};

use crate::{
    cli::{Cli, Command},
    commands,
    fault::{CliError, CliResult},
    gui::card::SurfaceWaker,
    ipc::{self, Response, StreamKind},
    report::{error_envelope, success_envelope},
};

#[cfg(unix)]
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;
#[cfg(unix)]
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(unix)]
const REQUEST_READ_POLL: Duration = Duration::from_millis(100);

/// A request from another process, waiting for its answer.
pub struct Request {
    /// The argument vector as typed, `argv[0]` included.
    pub argv: Vec<String>,
    /// The caller's working directory, so relative `--output` paths resolve
    /// against *their* directory rather than the daemon's.
    pub cwd: Option<PathBuf>,
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("argv", &self.argv)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

impl Request {
    /// Runs the command and answers the caller.
    ///
    /// Consumes the request, because the socket must be closed either way: a
    /// branch that forgot to reply would leave the terminal hanging on a read
    /// that never returns.
    ///
    /// Returns a successfully completed command, so the app can decide whether
    /// it also has local work to do. Failed commands have already been answered
    /// and must not mutate the live UI.
    pub fn serve(self) -> Option<Command> {
        self.serve_with(&mut |_, _| {}, |_| Ok(()))
    }

    /// Runs a command, then completes required in-process work before replying.
    ///
    /// The hook exists for operations such as terminal unpinning whose durable
    /// worker write must be ordered after older queued writes. A hook failure
    /// replaces the otherwise successful command response.
    ///
    /// `observed` receives the pixels of any capture the command took, so a
    /// `scrozz capture` typed at a terminal joins the stack the user is looking
    /// at rather than only landing in a file. It is separate from
    /// `after_success` because both want `&mut` access to the same application:
    /// the observer collects, and the caller acts once this returns.
    pub fn serve_with(
        self,
        observed: crate::commands::CaptureSink<'_>,
        after_success: impl FnOnce(&Command) -> CliResult<()>,
    ) -> Option<Command> {
        let (command, mut response) = run(&self.argv, self.cwd.as_deref(), observed);
        if response.code == 0
            && let Some(command) = command.as_ref()
            && let Err(error) = after_success(command)
        {
            response = Self::command_error_response(&self.argv, command, &error);
        }
        let succeeded = response.code == 0;
        self.reply(&response);
        command.filter(|_| succeeded)
    }

    fn command_error_response(argv: &[String], command: &Command, error: &CliError) -> Response {
        use clap::Parser as _;

        let mut with_argv0 = Vec::with_capacity(argv.len() + 1);
        with_argv0.push("scrozz".to_owned());
        with_argv0.extend_from_slice(argv);
        let json_requested = Cli::try_parse_from(with_argv0).is_ok_and(|cli| cli.global.json);
        if json_requested {
            let slug = command.slug();
            json(
                error.exit().code(),
                error_envelope(&slug, error).to_compact_string(),
            )
        } else {
            text(error.exit().code(), error.to_string())
        }
    }

    #[cfg(unix)]
    fn reply(self, response: &Response) {
        use std::{io::Write, net::Shutdown};

        let mut stream = self.stream;
        let bytes = ipc::encode_response(response);
        if let Err(err) = stream.write_all(&bytes) {
            tracing::warn!("could not answer a forwarded command: {err}");
        }
        let _ = stream.flush();
        // The client reads to EOF, so it only sees the answer once we close.
        let _ = stream.shutdown(Shutdown::Both);
    }

    #[cfg(not(unix))]
    fn reply(self, _response: &Response) {}
}

/// Executes a forwarded argument vector, exactly as a local run would.
///
/// Separated from [`Request`] so the fidelity rules can be tested without a
/// socket, which is the part most likely to drift from
/// [`crate::report::Reporter::emit`].
fn run(
    argv: &[String],
    cwd: Option<&Path>,
    observed: crate::commands::CaptureSink<'_>,
) -> (Option<Command>, Response) {
    use clap::Parser as _;

    if argv.is_empty() {
        // A forwarded invocation with nothing in it cannot be honoured, and
        // clap would answer with the help text rather than saying so.
        return (
            None,
            text(2, "the forwarded command had no arguments".to_owned()),
        );
    }

    // The wire carries the arguments *after* the program name — `ipc::forward`
    // is handed `env::args().skip(1)`. clap always treats its first element as
    // the program name and discards it, so without a placeholder the real
    // subcommand is eaten and every forwarded command parses as a bare
    // `scrozz`. That failure is invisible: the response comes back well-formed,
    // for the wrong command.
    let mut with_argv0 = Vec::with_capacity(argv.len() + 1);
    with_argv0.push("scrozz".to_owned());
    with_argv0.extend_from_slice(argv);

    let cli = match Cli::try_parse_from(&with_argv0) {
        Ok(cli) => cli,
        // clap's own rejection. There is no slug to report it under, because we
        // never got as far as knowing which subcommand was meant.
        Err(err) => return (None, text(2, err.to_string())),
    };

    let command = cli.command.clone().unwrap_or(Command::Gui);
    let slug = command.slug();

    // Relative paths belong to the caller's directory, not the daemon's. The
    // restore matters as much as the switch: this is the GUI's own process, and
    // every later capture would otherwise inherit whatever directory the last
    // forwarded command happened to run in.
    let restore = enter(cwd);
    let result = cli
        .validate()
        .and_then(|()| commands::dispatch_observed(&command, observed));
    restore();

    let response = match result {
        Ok(report) => {
            if let Some(bytes) = report.raw {
                Response {
                    code: 0,
                    stream: StreamKind::Binary,
                    payload: bytes,
                }
            } else if cli.global.json {
                json(0, success_envelope(&slug, report.data).to_compact_string())
            } else if cli.global.quiet {
                text(0, String::new())
            } else {
                text(0, report.human.trim_end().to_owned())
            }
        }
        Err(err) => {
            let code = err.exit().code();
            if cli.global.json {
                json(code, error_envelope(&slug, &err).to_compact_string())
            } else {
                text(code, err.to_string())
            }
        }
    };

    (Some(command), response)
}

/// Switches to `target`, returning how to switch back.
fn enter(target: Option<&Path>) -> impl FnOnce() {
    let previous = target.and_then(|dir| {
        let here = std::env::current_dir().ok()?;
        std::env::set_current_dir(dir).ok()?;
        Some(here)
    });

    move || {
        if let Some(here) = previous {
            let _ = std::env::set_current_dir(here);
        }
    }
}

fn text(code: u8, body: String) -> Response {
    Response {
        code,
        stream: StreamKind::Text,
        payload: body.into_bytes(),
    }
}

fn json(code: u8, body: String) -> Response {
    Response {
        code,
        stream: StreamKind::Json,
        payload: body.into_bytes(),
    }
}

/// The listener a running GUI holds.
pub struct Server {
    path: PathBuf,
    #[cfg(unix)]
    requests: Receiver<Request>,
    #[cfg(unix)]
    stop: Arc<AtomicBool>,
    #[cfg(unix)]
    worker: Option<JoinHandle<()>>,
}

impl Server {
    /// Binds the endpoint, taking over a stale socket if one is left behind.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Ipc`] if another instance already holds the endpoint
    /// — not a fault but a fact the caller must act on, by forwarding rather
    /// than starting a second menu-bar app.
    pub fn bind() -> CliResult<Self> {
        Self::bind_with_waker(None)
    }

    /// Binds the default endpoint and wakes the window only when a request arrives.
    pub fn bind_with_waker(waker: Option<SurfaceWaker>) -> CliResult<Self> {
        Self::bind_at_with_waker(ipc::endpoint(), waker)
    }

    /// Binds a specific path.
    ///
    /// Exposed because tests must not fight over the real endpoint with a GUI
    /// the developer happens to be running.
    ///
    /// # Errors
    ///
    /// As [`Server::bind`].
    #[cfg(unix)]
    pub fn bind_at(path: PathBuf) -> CliResult<Self> {
        Self::bind_at_with_waker(path, None)
    }

    #[cfg(unix)]
    fn bind_at_with_waker(path: PathBuf, waker: Option<SurfaceWaker>) -> CliResult<Self> {
        use std::os::unix::net::UnixListener;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CliError::ipc(format!(
                    "could not make {} for the instance socket: {e}",
                    parent.display()
                ))
            })?;
        }

        clear_stale(&path)?;

        let listener = UnixListener::bind(&path)
            .map_err(|e| CliError::ipc(format!("could not listen at {}: {e}", path.display())))?;
        listener.set_nonblocking(true).map_err(|e| {
            CliError::ipc(format!("could not make the instance socket pollable: {e}"))
        })?;

        let (request_tx, requests) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("scrozz-ipc-listener".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Some(request) = read_request(stream) {
                                if request_tx.send(request).is_err() {
                                    break;
                                }
                                if let Some(waker) = &waker {
                                    waker();
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(8));
                        }
                        Err(error) => {
                            tracing::warn!("could not accept a forwarded command: {error}");
                            std::thread::sleep(Duration::from_millis(25));
                        }
                    }
                }
            })
            .map_err(|error| {
                CliError::ipc(format!(
                    "could not start the instance listener for {}: {error}",
                    path.display()
                ))
            })?;

        tracing::debug!(path = %path.display(), "listening for forwarded commands");
        Ok(Self {
            path,
            requests,
            stop,
            worker: Some(worker),
        })
    }

    /// Named-pipe support is not built yet, so there is nothing to listen on.
    ///
    /// # Errors
    ///
    /// Never. The GUI runs without single-instance forwarding rather than
    /// refusing to start over it.
    #[cfg(not(unix))]
    pub fn bind_at(path: PathBuf) -> CliResult<Self> {
        Self::bind_at_with_waker(path, None)
    }

    #[cfg(not(unix))]
    fn bind_at_with_waker(path: PathBuf, _waker: Option<SurfaceWaker>) -> CliResult<Self> {
        tracing::warn!(
            "this build has no named-pipe listener, so `scrozz capture` from a \
             terminal will run in its own process"
        );
        Ok(Self { path })
    }

    /// Takes one pending request, if there is one. Never blocks.
    #[cfg(unix)]
    pub fn poll(&self) -> Option<Request> {
        self.requests.try_recv().ok()
    }

    /// Nothing arrives without a listener.
    #[cfg(not(unix))]
    #[must_use]
    pub const fn poll(&self) -> Option<Request> {
        None
    }

    /// Where this server is listening.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
        // Otherwise the next launch finds a socket file with nothing behind it
        // and has to decide whether it is stale — solvable, but this is free.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn read_request(mut stream: std::os::unix::net::UnixStream) -> Option<Request> {
    use std::io::{ErrorKind, Read};

    if let Err(error) = stream.set_nonblocking(false) {
        tracing::warn!("could not make an accepted command socket blocking: {error}");
        return None;
    }
    if let Err(error) = stream.set_read_timeout(Some(REQUEST_READ_POLL)) {
        tracing::warn!("could not bound a forwarded-command read: {error}");
        return None;
    }
    let started = std::time::Instant::now();
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        if started.elapsed() >= REQUEST_READ_TIMEOUT {
            tracing::warn!("forwarded command exceeded the one-second read deadline");
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                if raw.len() as u64 + read as u64 > MAX_REQUEST_BYTES {
                    tracing::warn!(
                        bytes = raw.len() + read,
                        "forwarded command exceeded the one-megabyte request limit"
                    );
                    return None;
                }
                raw.extend_from_slice(&chunk[..read]);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                continue;
            }
            Err(error) => {
                tracing::warn!("could not read a forwarded command: {error}");
                return None;
            }
        }
    }

    let line = String::from_utf8_lossy(&raw);
    let argv = string_array(&line, "argv")?;
    let cwd = string_field(&line, "cwd").map(PathBuf::from);
    Some(Request { argv, cwd, stream })
}

/// Removes a socket file left behind by a crash.
///
/// Done by connecting rather than by a lock file: a lock file records what a
/// process *intended*, and a killed process leaves one behind saying it is still
/// running. A connection refused is proof.
#[cfg(unix)]
fn clear_stale(path: &Path) -> CliResult<()> {
    use std::os::unix::net::UnixStream;

    if !path.exists() {
        return Ok(());
    }
    if UnixStream::connect(path).is_ok() {
        return Err(CliError::ipc(format!(
            "another Scrozz is already running and listening at {}",
            path.display()
        )));
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}

/// Pulls a JSON string array out of the request line.
///
/// A full parser would be nicer, but this crate has no JSON reader and the input
/// is not arbitrary: it is produced by [`crate::ipc::encode_request`] in the same
/// protocol version, and the header check in
/// [`crate::ipc::parse_response`] guards the version. What matters is that a
/// malformed line yields `None` rather than a panic, because it arrives from
/// outside this process.
fn string_array(line: &str, key: &str) -> Option<Vec<String>> {
    let bytes = line.as_bytes();
    let mut at = line.find(&format!("\"{key}\":["))? + key.len() + 4;
    let mut values = Vec::new();

    loop {
        match bytes.get(at)? {
            b']' => return Some(values),
            b'"' => {
                let (value, end) = read_string(line, at + 1)?;
                values.push(value);
                at = end + 1;
            }
            // Whitespace and the separating commas.
            _ => at += 1,
        }
    }
}

/// Pulls a nullable JSON string field out of the request line.
fn string_field(line: &str, key: &str) -> Option<String> {
    let at = line.find(&format!("\"{key}\":\""))? + key.len() + 4;
    read_string(line, at).map(|(value, _)| value)
}

/// Reads one JSON string body starting at `from`, just after the opening quote.
///
/// Returns the decoded value and the byte index of its closing quote.
fn read_string(line: &str, from: usize) -> Option<(String, usize)> {
    let bytes = line.as_bytes();
    let mut out = String::new();
    let mut at = from;

    while at < bytes.len() {
        match bytes[at] {
            b'"' => return Some((out, at)),
            b'\\' => {
                at += 1;
                let escaped = *bytes.get(at)?;
                match escaped {
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => {
                        // `Json::str` only escapes control characters this way,
                        // so four hex digits and never a surrogate pair.
                        let hex = line.get(at + 1..at + 5)?;
                        out.push(char::from_u32(u32::from_str_radix(hex, 16).ok()?)?);
                        at += 4;
                    }
                    other => out.push(char::from(other)),
                }
                at += 1;
            }
            _ => {
                // Multi-byte characters pass through whole; indexing on a char
                // boundary is what makes the slice below safe.
                let rest = line.get(at..)?;
                let ch = rest.chars().next()?;
                out.push(ch);
                at += ch.len_utf8();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn an_argv_array_round_trips_through_the_wire_format() {
        let sent = argv(&["scrozz", "capture", "--json"]);
        let line = ipc::encode_request(&sent, None);
        assert_eq!(string_array(&line, "argv"), Some(sent));
    }

    #[test]
    fn a_cwd_round_trips_and_is_absent_when_not_sent() {
        let sent = argv(&["scrozz"]);
        let with = ipc::encode_request(&sent, Some(Path::new("/Users/someone/work")));
        assert_eq!(
            string_field(&with, "cwd").as_deref(),
            Some("/Users/someone/work")
        );

        let without = ipc::encode_request(&sent, None);
        assert_eq!(string_field(&without, "cwd"), None);
    }

    #[test]
    fn an_argument_containing_a_quote_survives() {
        // The case a naive split on `","` gets wrong.
        let sent = argv(&["scrozz", "capture", "-o", r#"/a "quoted" name.png"#]);
        let line = ipc::encode_request(&sent, None);
        assert_eq!(string_array(&line, "argv"), Some(sent));
    }

    #[test]
    fn an_argument_containing_a_backslash_survives() {
        let sent = argv(&["scrozz", r"C:\shots\a.png"]);
        let line = ipc::encode_request(&sent, None);
        assert_eq!(string_array(&line, "argv"), Some(sent));
    }

    #[test]
    fn a_non_ascii_argument_survives() {
        let sent = argv(&["scrozz", "capture", "-o", "/captures/écran ✅.png"]);
        let line = ipc::encode_request(&sent, None);
        assert_eq!(string_array(&line, "argv"), Some(sent));
    }

    #[test]
    fn an_empty_argv_is_an_empty_vector_not_a_failure() {
        let line = ipc::encode_request(&[], None);
        assert_eq!(string_array(&line, "argv"), Some(Vec::new()));
    }

    #[test]
    fn a_truncated_line_is_rejected_rather_than_panicking() {
        // This arrives from outside the process, so it must never abort us.
        assert_eq!(string_array(r#"{"argv":["scrozz"#, "argv"), None);
        assert_eq!(string_array(r#"{"argv":["#, "argv"), None);
        assert_eq!(string_array("{}", "argv"), None);
        assert_eq!(string_field(r#"{"cwd":"#, "cwd"), None);
        assert_eq!(string_field(r#"{"cwd":"unterminated"#, "cwd"), None);
    }

    #[test]
    fn a_missing_key_is_none() {
        let line = ipc::encode_request(&argv(&["scrozz"]), None);
        assert_eq!(string_array(&line, "nope"), None);
        assert_eq!(string_field(&line, "nope"), None);
    }

    #[test]
    fn an_empty_argument_vector_is_answered_not_ignored() {
        let (command, response) = run(&[], None, &mut |_, _| {});
        assert!(command.is_none());
        assert_eq!(response.code, 2);
        assert!(!response.payload.is_empty());
    }

    #[test]
    fn an_unparseable_command_answers_with_claps_own_message() {
        let (command, response) = run(&argv(&["nonsuch"]), None, &mut |_, _| {});
        assert!(command.is_none(), "there is no command to name");
        assert_eq!(response.code, 2);
        assert_eq!(response.stream, StreamKind::Text);
        let message = String::from_utf8_lossy(&response.payload);
        assert!(message.contains("nonsuch"), "{message}");
    }

    #[test]
    fn a_forwarded_failure_carries_the_same_exit_code_as_a_local_one() {
        // `list displays` needs a backend, which is guarded off by default, so
        // this is a stable failure that does not touch the screen.
        let (command, response) = run(&argv(&["list", "displays"]), None, &mut |_, _| {});
        assert!(matches!(command, Some(Command::List(_))));
        assert_ne!(
            response.code, 0,
            "a guarded backend must not report success"
        );
    }

    #[test]
    fn a_forwarded_json_failure_is_an_envelope_not_a_sentence() {
        let (_, response) = run(&argv(&["--json", "list", "displays"]), None, &mut |_, _| {});
        assert_eq!(response.stream, StreamKind::Json);
        let body = String::from_utf8_lossy(&response.payload);
        assert!(body.starts_with('{'), "{body}");
        assert!(body.contains("\"ok\":false"), "{body}");
        assert!(body.contains("\"command\":\"list.displays\""), "{body}");
    }

    #[test]
    fn a_forwarded_success_is_reported_verbatim() {
        // `capture --dry-run` reaches no backend, so it succeeds anywhere.
        let (_, response) = run(&argv(&["capture", "--dry-run"]), None, &mut |_, _| {});
        assert_eq!(response.code, 0);
        assert_eq!(response.stream, StreamKind::Text);
        let body = String::from_utf8_lossy(&response.payload);
        assert!(body.contains("Would capture"), "{body}");
        assert!(
            !body.ends_with('\n'),
            "the trailing newline is the wire's job"
        );
    }

    #[test]
    fn a_quiet_forwarded_command_says_nothing() {
        let (_, response) = run(
            &argv(&["--quiet", "capture", "--dry-run"]),
            None,
            &mut |_, _| {},
        );
        assert_eq!(response.code, 0);
        assert!(response.payload.is_empty());
    }

    #[test]
    fn a_json_forwarded_success_is_an_envelope() {
        let (_, response) = run(
            &argv(&["--json", "capture", "--dry-run"]),
            None,
            &mut |_, _| {},
        );
        assert_eq!(response.stream, StreamKind::Json);
        let body = String::from_utf8_lossy(&response.payload);
        assert!(body.contains("\"ok\":true"), "{body}");
        assert!(body.contains("\"command\":\"capture\""), "{body}");
    }

    #[test]
    fn a_response_survives_the_round_trip_the_client_will_make() {
        // The real check: whatever we produce, `ipc::parse_response` must read
        // back unchanged, or the terminal shows something different from a
        // local run.
        let (_, response) = run(
            &argv(&["--json", "capture", "--dry-run"]),
            None,
            &mut |_, _| {},
        );
        let wire = ipc::encode_response(&response);
        let parsed = ipc::parse_response(&wire).expect("our own wire format must parse");
        assert_eq!(parsed.code, response.code);
        assert_eq!(parsed.stream, response.stream);
        assert_eq!(parsed.payload, response.payload);
    }

    #[test]
    fn the_working_directory_is_restored_afterwards() {
        let before = std::env::current_dir().expect("a working directory");
        let elsewhere = std::env::temp_dir();
        let restore = enter(Some(&elsewhere));
        restore();
        assert_eq!(
            std::env::current_dir().expect("a working directory"),
            before
        );
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scrozz-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    #[cfg(unix)]
    #[test]
    fn a_server_removes_its_socket_when_dropped() {
        let dir = scratch("drop");
        let path = dir.join("drop.sock");
        {
            let server = Server::bind_at(path.clone()).expect("binding a fresh path");
            assert!(
                server.path().exists(),
                "the socket should exist while bound"
            );
        }
        assert!(!path.exists(), "the socket should be gone after the drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_socket_file_is_taken_over() {
        // The residue of a crash: a socket file with nothing behind it. Refusing
        // to start would mean one crash disables the app until someone finds the
        // file and deletes it.
        let dir = scratch("stale");
        let path = dir.join("stale.sock");
        std::fs::write(&path, b"not a socket").expect("writing the residue");

        let server = Server::bind_at(path.clone()).expect("a stale file must not block startup");
        assert_eq!(server.path(), path);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn polling_an_idle_server_yields_nothing() {
        let dir = scratch("idle");
        let server = Server::bind_at(dir.join("idle.sock")).expect("binding");
        assert!(server.poll().is_none());
        assert!(server.poll().is_none());
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_stalled_local_client_cannot_block_server_shutdown_forever() {
        let dir = scratch("stalled-client");
        let path = dir.join("stalled.sock");
        let server = Server::bind_at(path.clone()).expect("binding");
        let _stalled =
            std::os::unix::net::UnixStream::connect(&path).expect("connect without sending");
        std::thread::sleep(Duration::from_millis(30));

        let started = std::time::Instant::now();
        drop(server);
        assert!(
            started.elapsed() < REQUEST_READ_TIMEOUT + Duration::from_secs(1),
            "listener shutdown exceeded its bounded read timeout"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn oversized_forwarded_requests_are_rejected_before_unbounded_allocation() {
        use std::io::Write;
        use std::net::Shutdown;

        let (mut writer, reader) =
            std::os::unix::net::UnixStream::pair().expect("local socket pair");
        let sending = std::thread::spawn(move || {
            let chunk = vec![b'x'; 64 * 1024];
            let mut remaining = MAX_REQUEST_BYTES + 1;
            while remaining > 0 {
                let count = usize::try_from(remaining.min(chunk.len() as u64)).expect("bounded");
                writer.write_all(&chunk[..count]).expect("write request");
                remaining -= count as u64;
            }
            writer.shutdown(Shutdown::Write).expect("finish request");
        });

        assert!(read_request(reader).is_none());
        sending.join().expect("writer");
    }

    #[cfg(unix)]
    #[test]
    fn a_peer_that_never_finishes_is_bounded_by_the_read_timeout() {
        let (_writer, reader) = std::os::unix::net::UnixStream::pair().expect("local socket pair");
        let started = std::time::Instant::now();
        assert!(read_request(reader).is_none());
        assert!(
            started.elapsed() < REQUEST_READ_TIMEOUT + Duration::from_secs(1),
            "socket read exceeded its configured timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_drip_cannot_restart_the_wall_clock_deadline() {
        use std::io::Write;

        let (mut writer, reader) =
            std::os::unix::net::UnixStream::pair().expect("local socket pair");
        let sending = std::thread::spawn(move || {
            for _ in 0..20 {
                if writer.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        let started = std::time::Instant::now();
        assert!(read_request(reader).is_none());
        assert!(
            started.elapsed() < REQUEST_READ_TIMEOUT + Duration::from_secs(1),
            "partial reads restarted the wall-clock deadline"
        );
        sending.join().expect("slow writer");
    }

    #[cfg(unix)]
    #[test]
    fn a_second_server_on_a_live_endpoint_is_refused() {
        // This is what stops `scrozz gui` twice producing two menu-bar items.
        let dir = scratch("dup");
        let path = dir.join("dup.sock");

        let first = Server::bind_at(path.clone()).expect("the first should bind");
        let second = Server::bind_at(path.clone());
        assert!(second.is_err(), "the second must be refused");

        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_forwarded_command_reaches_the_server_and_is_answered() {
        // The whole point, end to end: the client half in `ipc` talking to the
        // server half here, over a real socket.
        let dir = scratch("round-trip");
        let path = dir.join("live.sock");
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&wakes);
        let waker: SurfaceWaker = Arc::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let server = Server::bind_at_with_waker(path.clone(), Some(waker)).expect("binding");

        // No program name: `try_forward` sends `env::args().skip(1)`, and the
        // server is the side that has to know that.
        let sent = argv(&["capture", "--dry-run"]);
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                // SAFETY-adjacent: the env var is process-global, but this test
                // is the only one using this endpoint name.
                unsafe { std::env::set_var(ipc::ENDPOINT_ENV, &path) };
                ipc::forward(&sent)
            }
        });

        let request = loop {
            if let Some(request) = server.poll() {
                break request;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(request.argv.first().map(String::as_str), Some("capture"));
        assert!(wakes.load(std::sync::atomic::Ordering::Relaxed) > 0);
        let command = request.serve_with(&mut |_, _| {}, |_| {
            assert!(
                !client.is_finished(),
                "the caller must remain blocked until the success hook finishes"
            );
            Ok(())
        });
        assert!(matches!(command, Some(Command::Capture(_))));

        let response = client
            .join()
            .expect("the client thread")
            .expect("a well-formed answer");
        assert_eq!(response.code, 0);
        assert!(
            String::from_utf8_lossy(&response.payload).contains("Would capture"),
            "the forwarded output must match a local run"
        );

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_client_may_pause_after_accept_before_sending_its_request() {
        use std::io::Write;
        use std::net::Shutdown;

        let dir = scratch("delayed-write");
        let path = dir.join("delayed.sock");
        let server = Server::bind_at(path.clone()).expect("binding");
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = std::os::unix::net::UnixStream::connect(path).expect("connect");
                std::thread::sleep(Duration::from_millis(50));
                stream
                    .write_all(br#"{"argv":["capture","--dry-run"],"cwd":null}"#)
                    .expect("delayed write");
                stream.shutdown(Shutdown::Write).expect("finish request");
            }
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let request = loop {
            if let Some(request) = server.poll() {
                break request;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "delayed request never reached the listener"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(request.argv, argv(&["capture", "--dry-run"]));
        drop(request);
        client.join().expect("client");
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
