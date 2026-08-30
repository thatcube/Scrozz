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

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use scrozz_core::Capture;

use crate::{
    cli::{Cli, Command},
    commands,
    fault::{CliError, CliResult},
    gui::{action::CaptureKind, card::SurfaceWaker},
    ipc::{self, Response, StreamKind},
    report::{error_envelope, success_envelope},
};

const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_READ_POLL: Duration = Duration::from_millis(100);
const REQUEST_QUEUE_DEPTH: usize = 1;
const WORKER_POLL: Duration = Duration::from_millis(8);

/// The one full-resolution capture a forwarded command may produce.
#[derive(Debug)]
pub(crate) struct ForwardedCapture {
    pub(crate) kind: CaptureKind,
    pub(crate) capture: Capture,
}

/// A request from another process, waiting for its answer.
pub struct Request {
    /// The argument vector as typed, `argv[0]` included.
    pub argv: Vec<String>,
    /// The caller's working directory, so relative `--output` paths resolve
    /// against *their* directory rather than the daemon's.
    pub cwd: Option<PathBuf>,
    reply: SyncSender<Response>,
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
    /// Runs a command, then completes required in-process work before replying.
    ///
    /// The hook exists for operations such as terminal unpinning whose durable
    /// worker write must be ordered after older queued writes. A hook failure
    /// replaces the otherwise successful command response.
    ///
    /// `after_success` receives ownership of the command's optional capture.
    /// There can be at most one, which bounds retention per request and lets the
    /// app move the frame directly into its capture pipeline before success is
    /// visible to the client.
    pub fn serve_with(
        self,
        after_success: impl FnOnce(&Command, Option<ForwardedCapture>) -> CliResult<()>,
    ) -> Option<Command> {
        let (command, response, captured) = run(&self.argv, self.cwd.as_deref());
        self.finish(command, response, captured, after_success)
    }

    fn finish(
        self,
        command: Option<Command>,
        mut response: Response,
        captured: Option<ForwardedCapture>,
        after_success: impl FnOnce(&Command, Option<ForwardedCapture>) -> CliResult<()>,
    ) -> Option<Command> {
        if response.code == 0
            && let Some(command) = command.as_ref()
            && let Err(error) = after_success(command, captured)
        {
            response = Self::command_error_response(&self.argv, command, &error);
        }
        let succeeded = response.code == 0;
        if self.reply.send(response).is_err() {
            tracing::debug!("forwarded command client disconnected before the reply");
        }
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
}

/// Executes a forwarded argument vector, exactly as a local run would.
///
/// Separated from [`Request`] so the fidelity rules can be tested without a
/// socket, which is the part most likely to drift from
/// [`crate::report::Reporter::emit`].
fn run(
    argv: &[String],
    cwd: Option<&Path>,
) -> (Option<Command>, Response, Option<ForwardedCapture>) {
    use clap::Parser as _;

    if argv.is_empty() {
        // A forwarded invocation with nothing in it cannot be honoured, and
        // clap would answer with the help text rather than saying so.
        return (
            None,
            text(2, "the forwarded command had no arguments".to_owned()),
            None,
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
        Err(err) => return (None, text(2, err.to_string()), None),
    };

    let command = cli.command.clone().unwrap_or(Command::Gui);
    let slug = command.slug();

    // Relative paths belong to the caller's directory, not the daemon's. The
    // restore matters as much as the switch: this is the GUI's own process, and
    // every later capture would otherwise inherit whatever directory the last
    // forwarded command happened to run in.
    let mut captured = None;
    let result = WorkingDirectory::enter(cwd).and_then(|_directory| {
        cli.validate().and_then(|()| {
            commands::dispatch_observed(&command, &mut |kind, capture| {
                retain_capture(&mut captured, kind, capture)
            })
        })
    });

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

    if response.code != 0 {
        captured = None;
    }
    (Some(command), response, captured)
}

fn retain_capture(
    slot: &mut Option<ForwardedCapture>,
    kind: CaptureKind,
    capture: Capture,
) -> CliResult<()> {
    if slot.is_some() {
        return Err(CliError::ipc(
            "a forwarded command produced more than one full-resolution capture",
        ));
    }
    *slot = Some(ForwardedCapture { kind, capture });
    Ok(())
}

struct WorkingDirectory(Option<PathBuf>);

impl WorkingDirectory {
    fn enter(target: Option<&Path>) -> CliResult<Self> {
        let Some(target) = target else {
            return Ok(Self(None));
        };
        let previous = std::env::current_dir().map_err(|error| {
            CliError::ipc(format!(
                "could not preserve the running instance working directory: {error}"
            ))
        })?;
        std::env::set_current_dir(target).map_err(|error| {
            CliError::ipc(format!(
                "could not enter the caller working directory {}: {error}",
                target.display()
            ))
        })?;
        Ok(Self(Some(previous)))
    }
}

impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take()
            && let Err(error) = std::env::set_current_dir(&previous)
        {
            tracing::error!(
                path = %previous.display(),
                "could not restore the GUI working directory: {error}"
            );
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
    requests: Receiver<Request>,
    stop: Arc<AtomicBool>,
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

        spawn(path, waker, move |requests, stop, waker| {
            unix_worker(listener, &requests, &stop, waker.as_ref());
        })
    }

    #[cfg(windows)]
    pub fn bind_at(path: PathBuf) -> CliResult<Self> {
        Self::bind_at_with_waker(path, None)
    }

    #[cfg(windows)]
    fn bind_at_with_waker(path: PathBuf, waker: Option<SurfaceWaker>) -> CliResult<Self> {
        let listener = ipc::windows_pipe::PipeListener::bind(&path)?;
        spawn(path, waker, move |requests, stop, waker| {
            windows_worker(listener, &requests, &stop, waker.as_ref());
        })
    }

    #[cfg(not(any(unix, windows)))]
    pub fn bind_at(path: PathBuf) -> CliResult<Self> {
        Self::bind_at_with_waker(path, None)
    }

    #[cfg(not(any(unix, windows)))]
    fn bind_at_with_waker(path: PathBuf, _waker: Option<SurfaceWaker>) -> CliResult<Self> {
        let (_sender, requests) = sync_channel(REQUEST_QUEUE_DEPTH);
        Ok(Self {
            path,
            requests,
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        })
    }

    /// Takes one pending request, if there is one. Never blocks.
    pub fn poll(&self) -> Option<Request> {
        match self.requests.try_recv() {
            Ok(request) => Some(request),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                tracing::warn!("the single-instance transport worker stopped");
                None
            }
        }
    }

    /// Where this server is listening.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        #[cfg(unix)]
        {
            // Otherwise the next launch finds a socket file with nothing behind
            // it and has to decide whether it is stale.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn spawn(
    path: PathBuf,
    waker: Option<SurfaceWaker>,
    run_worker: impl FnOnce(SyncSender<Request>, Arc<AtomicBool>, Option<SurfaceWaker>) + Send + 'static,
) -> CliResult<Server> {
    let (request_tx, requests) = sync_channel(REQUEST_QUEUE_DEPTH);
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = std::thread::Builder::new()
        .name("scrozz-ipc-listener".into())
        .spawn(move || run_worker(request_tx, worker_stop, waker))
        .map_err(|error| {
            CliError::ipc(format!(
                "could not start the instance listener for {}: {error}",
                path.display()
            ))
        })?;

    tracing::debug!(path = %path.display(), "listening for forwarded commands");
    Ok(Server {
        path,
        requests,
        stop,
        worker: Some(worker),
    })
}

#[cfg(unix)]
fn unix_worker(
    listener: std::os::unix::net::UnixListener,
    requests: &SyncSender<Request>,
    stop: &Arc<AtomicBool>,
    waker: Option<&SurfaceWaker>,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Err(error) = stream.set_read_timeout(Some(REQUEST_READ_POLL)) {
                    tracing::warn!("could not bound a forwarded-command read: {error}");
                    continue;
                }
                if let Err(error) = stream.set_write_timeout(Some(REQUEST_READ_POLL)) {
                    tracing::warn!("could not bound a forwarded-command reply: {error}");
                    continue;
                }
                serve_connection(&mut stream, requests, stop, waker);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(WORKER_POLL);
            }
            Err(error) => {
                tracing::warn!("could not accept a forwarded command: {error}");
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

#[cfg(windows)]
fn windows_worker(
    mut listener: ipc::windows_pipe::PipeListener,
    requests: &SyncSender<Request>,
    stop: &Arc<AtomicBool>,
    waker: Option<&SurfaceWaker>,
) {
    while !stop.load(Ordering::Acquire) {
        if let Err(error) = listener.accept(stop) {
            if !stop.load(Ordering::Acquire) {
                tracing::warn!("{error}");
            }
            std::thread::sleep(WORKER_POLL);
            continue;
        }
        serve_connection(&mut listener, requests, stop, waker);
        listener.disconnect();
    }
}

fn serve_connection(
    stream: &mut (impl std::io::Read + std::io::Write),
    requests: &SyncSender<Request>,
    stop: &Arc<AtomicBool>,
    waker: Option<&SurfaceWaker>,
) {
    let Some((argv, cwd)) = read_request(stream, stop) else {
        return;
    };
    let (reply, response) = sync_channel(1);
    let request = Request { argv, cwd, reply };
    if let Err(error) = requests.try_send(request) {
        let message = match error {
            TrySendError::Full(_) => "the running instance is busy",
            TrySendError::Disconnected(_) => "the running instance is shutting down",
        };
        answer(stream, &protocol_error(message), stop);
        return;
    }
    if let Some(waker) = waker {
        waker();
    }

    let deadline = Instant::now() + ipc::COMMAND_TIMEOUT;
    let response = loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break protocol_error("the forwarded command timed out");
        }
        match response.recv_timeout(remaining.min(REQUEST_READ_POLL)) {
            Ok(response) => break response,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                break protocol_error("the forwarded command did not produce a response");
            }
        }
    };
    answer(stream, &response, stop);
}

fn protocol_error(message: &str) -> Response {
    let error = CliError::ipc(message);
    text(error.exit().code(), error.to_string())
}

fn answer(
    stream: &mut (impl std::io::Read + std::io::Write),
    response: &Response,
    stop: &AtomicBool,
) {
    if let Err(error) = ipc::send_response_frame(
        stream,
        response,
        Instant::now() + ipc::TRANSFER_TIMEOUT,
        stop,
    ) {
        tracing::debug!("could not answer a forwarded command: {error}");
        return;
    }
    if let Err(error) = ipc::receive_ack(stream, Instant::now() + ipc::TRANSFER_TIMEOUT, stop) {
        tracing::debug!("forwarded command response was not acknowledged: {error}");
    }
}

fn read_request(
    stream: &mut impl std::io::Read,
    stop: &AtomicBool,
) -> Option<(Vec<String>, Option<PathBuf>)> {
    let raw = match ipc::receive_request_frame(stream, Instant::now() + REQUEST_READ_TIMEOUT, stop)
    {
        Ok(raw) => raw,
        Err(error) => {
            tracing::debug!("discarding incomplete forwarded command: {error}");
            return None;
        }
    };
    let line = match std::str::from_utf8(&raw) {
        Ok(line) if line.ends_with('\n') => &line[..line.len() - 1],
        Ok(_) => {
            tracing::warn!("forwarded command was not newline terminated");
            return None;
        }
        Err(error) => {
            tracing::warn!("forwarded command was not valid UTF-8: {error}");
            return None;
        }
    };
    if integer_field(line, "schema") != Some(ipc::REQUEST_SCHEMA) {
        tracing::warn!("forwarded command used an unsupported request schema");
        return None;
    }
    let argv = string_array(line, "argv")?;
    if argv.len() > 4_096 || argv.iter().any(|argument| argument.contains('\0')) {
        tracing::warn!("forwarded command arguments were malformed or excessive");
        return None;
    }
    let cwd = string_field(line, "cwd").map(PathBuf::from);
    if cwd.as_ref().is_some_and(|path| {
        let raw = path.to_string_lossy();
        raw.len() > 32 * 1024 || raw.contains('\0')
    }) {
        tracing::warn!("forwarded command working directory was malformed or excessive");
        return None;
    }
    Some((argv, cwd))
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

fn integer_field(line: &str, key: &str) -> Option<i64> {
    let start = line.find(&format!("\"{key}\":"))? + key.len() + 3;
    let bytes = line.as_bytes();
    let end = (start..bytes.len())
        .find(|index| !bytes[*index].is_ascii_digit() && bytes[*index] != b'-')
        .unwrap_or(bytes.len());
    line.get(start..end)?.parse().ok()
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
    use clap::Parser as _;
    use scrozz_core::{
        ColorSpace, Frame, PhysicalSize, PixelFormat, Provenance, ScaleFactor, WindowId,
    };

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn large_capture(marker: u8) -> Capture {
        let edge = 1_024;
        Capture {
            frame: Frame {
                data: vec![marker; edge * edge * 4],
                size: PhysicalSize::new(edge as f64, edge as f64),
                stride: edge * 4,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            provenance: Provenance::Window,
            target: scrozz_core::CaptureTarget::Window(WindowId(format!("large-{marker}"))),
        }
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
    fn one_request_retains_at_most_one_owned_full_resolution_capture() {
        let first = large_capture(1);
        let first_pixels = first.frame.data.as_ptr();
        let mut slot = None;
        retain_capture(&mut slot, CaptureKind::Region, first).expect("first capture");
        let retained = slot.as_ref().expect("capture retained");
        assert_eq!(
            retained.capture.frame.data.as_ptr(),
            first_pixels,
            "capture pixels must move into the request slot rather than clone"
        );

        let error = retain_capture(&mut slot, CaptureKind::Window, large_capture(2))
            .expect_err("a second full-resolution frame in one request must be rejected");
        assert!(error.to_string().contains("more than one"), "{error}");
    }

    #[test]
    fn acceptance_completes_before_a_success_reply_and_runs_once() {
        let command = Cli::try_parse_from([
            "scrozz",
            "capture",
            "--region",
            "0,0,10,10",
            "--output",
            "capture.png",
        ])
        .expect("valid command")
        .command
        .expect("capture command");
        let (reply, response) = sync_channel(1);
        let request = Request {
            argv: argv(&["capture", "--region", "0,0,10,10"]),
            cwd: None,
            reply,
        };
        let accepted = std::cell::Cell::new(0_u8);
        let result = request.finish(
            Some(command),
            text(0, "captured".to_owned()),
            Some(ForwardedCapture {
                kind: CaptureKind::Region,
                capture: large_capture(3),
            }),
            |_, captured| {
                assert!(matches!(
                    response.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ));
                let captured = captured.expect("owned capture reaches acceptance");
                assert_eq!(captured.kind, CaptureKind::Region);
                accepted.set(accepted.get() + 1);
                Ok(())
            },
        );

        assert!(matches!(result, Some(Command::Capture(_))));
        assert_eq!(accepted.get(), 1, "acceptance must be exactly once");
        assert_eq!(response.recv().expect("success reply").code, 0);
    }

    #[test]
    fn acceptance_failure_replaces_success_before_the_reply() {
        let command = Cli::try_parse_from(["scrozz", "capture", "--window", "Editor"])
            .expect("valid command")
            .command
            .expect("capture command");
        let (reply, response) = sync_channel(1);
        let request = Request {
            argv: argv(&["capture", "--window", "Editor"]),
            cwd: None,
            reply,
        };

        let result = request.finish(
            Some(command),
            text(0, "captured".to_owned()),
            Some(ForwardedCapture {
                kind: CaptureKind::Window,
                capture: large_capture(4),
            }),
            |_, captured| {
                assert!(captured.is_some());
                Err(CliError::ipc("capture admission was full"))
            },
        );

        assert!(
            result.is_none(),
            "failed acceptance is not a successful command"
        );
        let response = response.recv().expect("failure reply");
        assert_ne!(response.code, 0);
        assert!(String::from_utf8_lossy(&response.payload).contains("capture admission was full"));
    }

    #[test]
    fn an_empty_argument_vector_is_answered_not_ignored() {
        let (command, response, captured) = run(&[], None);
        assert!(command.is_none());
        assert_eq!(response.code, 2);
        assert!(!response.payload.is_empty());
        assert!(captured.is_none());
    }

    #[test]
    fn an_unparseable_command_answers_with_claps_own_message() {
        let (command, response, captured) = run(&argv(&["nonsuch"]), None);
        assert!(command.is_none(), "there is no command to name");
        assert_eq!(response.code, 2);
        assert_eq!(response.stream, StreamKind::Text);
        let message = String::from_utf8_lossy(&response.payload);
        assert!(message.contains("nonsuch"), "{message}");
        assert!(captured.is_none());
    }

    #[test]
    fn a_forwarded_failure_carries_the_same_exit_code_as_a_local_one() {
        // `list displays` needs a backend, which is guarded off by default, so
        // this is a stable failure that does not touch the screen.
        let (command, response, captured) = run(&argv(&["list", "displays"]), None);
        assert!(matches!(command, Some(Command::List(_))));
        assert_ne!(
            response.code, 0,
            "a guarded backend must not report success"
        );
        assert!(captured.is_none());
    }

    #[test]
    fn a_forwarded_json_failure_is_an_envelope_not_a_sentence() {
        let (_, response, _) = run(&argv(&["--json", "list", "displays"]), None);
        assert_eq!(response.stream, StreamKind::Json);
        let body = String::from_utf8_lossy(&response.payload);
        assert!(body.starts_with('{'), "{body}");
        assert!(body.contains("\"ok\":false"), "{body}");
        assert!(body.contains("\"command\":\"list.displays\""), "{body}");
    }

    #[test]
    fn a_forwarded_success_is_reported_verbatim() {
        // `capture --dry-run` reaches no backend, so it succeeds anywhere.
        let (_, response, captured) = run(&argv(&["capture", "--dry-run"]), None);
        assert_eq!(response.code, 0);
        assert_eq!(response.stream, StreamKind::Text);
        let body = String::from_utf8_lossy(&response.payload);
        assert!(body.contains("Would capture"), "{body}");
        assert!(
            !body.ends_with('\n'),
            "the trailing newline is the wire's job"
        );
        assert!(captured.is_none());
    }

    #[test]
    fn a_quiet_forwarded_command_says_nothing() {
        let (_, response, _) = run(&argv(&["--quiet", "capture", "--dry-run"]), None);
        assert_eq!(response.code, 0);
        assert!(response.payload.is_empty());
    }

    #[test]
    fn a_json_forwarded_success_is_an_envelope() {
        let (_, response, _) = run(&argv(&["--json", "capture", "--dry-run"]), None);
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
        let (_, response, _) = run(&argv(&["--json", "capture", "--dry-run"]), None);
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
        drop(WorkingDirectory::enter(Some(&elsewhere)).expect("enter temporary directory"));
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
    fn endpoint_for(name: &str) -> (PathBuf, PathBuf) {
        let directory = scratch(name);
        let path = directory.join("instance.sock");
        (directory, path)
    }

    #[cfg(windows)]
    fn endpoint_for(name: &str) -> (PathBuf, PathBuf) {
        (
            PathBuf::new(),
            PathBuf::from(format!(
                r"\\.\pipe\scrozz-pinned-{name}-{}",
                std::process::id()
            )),
        )
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

        let (mut writer, mut reader) =
            std::os::unix::net::UnixStream::pair().expect("local socket pair");
        reader
            .set_read_timeout(Some(REQUEST_READ_POLL))
            .expect("read timeout");
        let sending = std::thread::spawn(move || {
            let announced = u32::try_from(ipc::MAX_REQUEST_BYTES + 1).expect("bounded");
            writer
                .write_all(&announced.to_le_bytes())
                .expect("write oversized frame prefix");
            writer.shutdown(Shutdown::Write).expect("finish request");
        });

        let stop = AtomicBool::new(false);
        assert!(read_request(&mut reader, &stop).is_none());
        sending.join().expect("writer");
    }

    #[cfg(unix)]
    #[test]
    fn a_peer_that_never_finishes_is_bounded_by_the_read_timeout() {
        let (_writer, mut reader) =
            std::os::unix::net::UnixStream::pair().expect("local socket pair");
        reader
            .set_read_timeout(Some(REQUEST_READ_POLL))
            .expect("read timeout");
        let started = std::time::Instant::now();
        let stop = AtomicBool::new(false);
        assert!(read_request(&mut reader, &stop).is_none());
        assert!(
            started.elapsed() < REQUEST_READ_TIMEOUT + Duration::from_secs(1),
            "socket read exceeded its configured timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_drip_cannot_restart_the_wall_clock_deadline() {
        use std::io::Write;

        let (mut writer, mut reader) =
            std::os::unix::net::UnixStream::pair().expect("local socket pair");
        reader
            .set_read_timeout(Some(REQUEST_READ_POLL))
            .expect("read timeout");
        let sending = std::thread::spawn(move || {
            for _ in 0..20 {
                if writer.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        let started = std::time::Instant::now();
        let stop = AtomicBool::new(false);
        assert!(read_request(&mut reader, &stop).is_none());
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

    #[cfg(any(unix, windows))]
    #[test]
    fn a_forwarded_command_reaches_the_server_and_is_answered() {
        let _env = crate::test_env::lock();
        // The whole point, end to end: the client half in `ipc` talking to the
        // server half here, over a real socket.
        let (dir, path) = endpoint_for("round-trip");
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&wakes);
        let waker: SurfaceWaker = Arc::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let server = Server::bind_at_with_waker(path.clone(), Some(waker)).expect("binding");

        // No program name: `try_forward` sends `env::args().skip(1)`, and the
        // server is the side that has to know that.
        let sent = argv(&["capture", "--dry-run"]);
        crate::test_env::set(ipc::ENDPOINT_ENV, &path.to_string_lossy());
        let client = std::thread::spawn(move || ipc::forward(&sent));

        let request = loop {
            if let Some(request) = server.poll() {
                break request;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(request.argv.first().map(String::as_str), Some("capture"));
        assert!(wakes.load(std::sync::atomic::Ordering::Relaxed) > 0);
        let command = request.serve_with(|_, captured| {
            assert!(captured.is_none(), "a dry run has no pixels");
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
        #[cfg(unix)]
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_probe_does_not_poison_the_following_command() {
        let _env = crate::test_env::lock();
        let (_, path) = endpoint_for("probe");
        let server = Server::bind_at(path.clone()).expect("named-pipe listener");
        crate::test_env::set(ipc::ENDPOINT_ENV, &path.to_string_lossy());
        assert_eq!(ipc::probe(), ipc::Status::Running);

        let client = std::thread::spawn(|| ipc::forward(&argv(&["capture", "--dry-run"])));
        let request = loop {
            if let Some(request) = server.poll() {
                break request;
            }
            std::thread::sleep(WORKER_POLL);
        };
        assert!(matches!(
            request.serve_with(|_, captured| {
                assert!(captured.is_none());
                Ok(())
            }),
            Some(Command::Capture(_))
        ));
        assert_eq!(client.join().expect("client").expect("response").code, 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_client_may_pause_after_accept_before_sending_its_request() {
        use std::io::Write;

        let dir = scratch("delayed-write");
        let path = dir.join("delayed.sock");
        let server = Server::bind_at(path.clone()).expect("binding");
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = std::os::unix::net::UnixStream::connect(path).expect("connect");
                std::thread::sleep(Duration::from_millis(50));
                let request = ipc::encode_request(&argv(&["capture", "--dry-run"]), None);
                stream
                    .write_all(
                        &u32::try_from(request.len())
                            .expect("request length")
                            .to_le_bytes(),
                    )
                    .and_then(|()| stream.write_all(request.as_bytes()))
                    .expect("delayed framed write");
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
