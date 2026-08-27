//! The single-instance listener.
//!
//! # The problem this solves
//!
//! Once the menu-bar app is running it owns live state a second process cannot
//! see, such as an in-progress recording. Operations against that state must
//! happen inside the owning process; pure capture, OCR, barcode, history,
//! settings, and query commands deliberately remain local.
//!
//! [`crate::ipc`] already has the client half — [`crate::ipc::forward`] is what
//! the terminal process calls, and `main` already routes through it. This module
//! is the half that answers, and it lives here rather than in `ipc.rs` because
//! it only exists while the GUI does.
//!
//! # Fidelity
//!
//! A forwarded operation must produce byte-identical output to a local one; the
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

#[cfg(unix)]
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::{
    cli::{Cli, Command},
    commands,
    fault::{CliError, CliResult},
    ipc::{self, Response, StreamKind},
    report::{error_envelope, success_envelope},
};

/// A request from another process, waiting for its answer.
pub struct Request {
    /// The argument vector after the caller's program name.
    pub argv: Vec<OsString>,
    /// The losslessly encoded caller directory carried by the protocol.
    ///
    /// It is not applied while the only forwardable operation is pathless
    /// `record --stop`; changing cwd in a multithreaded GUI would be unsafe.
    pub cwd: Option<PathBuf>,
    /// A negotiation/rejection response that must be sent without dispatch.
    preflight: Option<Response>,
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("argv", &self.argv)
            .field("cwd", &self.cwd)
            .field("preflight", &self.preflight.is_some())
            .finish_non_exhaustive()
    }
}

impl Request {
    /// Runs the command and prepares its answer for the response worker.
    ///
    /// Consumes the request, because the socket must be closed either way: a
    /// branch that forgot to reply would leave the terminal hanging on a read
    /// that never returns.
    ///
    /// Returns what the command was, so the app can decide whether it also has
    /// local work to do — showing a card for a forwarded capture, or quitting.
    pub fn execute(mut self) -> (Option<Command>, Reply) {
        let (command, response) = if let Some(response) = self.preflight.take() {
            (None, response)
        } else {
            run(&self.argv, self.cwd.as_deref())
        };
        (command, self.into_reply(response))
    }

    #[cfg(unix)]
    fn into_reply(self, response: Response) -> Reply {
        Reply {
            stream: self.stream,
            bytes: ipc::encode_response(&response),
        }
    }

    #[cfg(not(unix))]
    fn into_reply(self, _response: Response) -> Reply {
        Reply {}
    }

    #[cfg(test)]
    fn serve(self) -> Option<Command> {
        let (command, reply) = self.execute();
        reply.send();
        command
    }
}

/// An encoded answer waiting for the dedicated response worker.
pub struct Reply {
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
    #[cfg(unix)]
    bytes: Vec<u8>,
}

impl Reply {
    /// Writes the answer without occupying the capture worker.
    #[cfg(unix)]
    pub fn send(mut self) {
        use std::{
            io::{ErrorKind, Write},
            net::Shutdown,
        };

        const RESPONSE_DEADLINE: Duration = Duration::from_secs(5);

        if let Err(error) = self.stream.set_nonblocking(false) {
            tracing::warn!("could not prepare an IPC response stream: {error}");
            return;
        }

        let deadline = Instant::now() + RESPONSE_DEADLINE;
        let mut written = 0;
        while written < self.bytes.len() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                tracing::warn!("an IPC response exceeded its 5-second deadline");
                return;
            };
            if remaining.is_zero() {
                tracing::warn!("an IPC response exceeded its 5-second deadline");
                return;
            }
            if let Err(error) = self.stream.set_write_timeout(Some(remaining)) {
                tracing::warn!("could not bound an IPC response write: {error}");
                return;
            }
            match self.stream.write(&self.bytes[written..]) {
                Ok(0) => {
                    tracing::warn!("an IPC client stopped accepting its response");
                    return;
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => {
                    tracing::warn!("could not answer a forwarded command: {error}");
                    return;
                }
            }
        }

        // The client reads to EOF, so it only sees the answer once we close.
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    /// No listener exists on this platform, so no response can be pending.
    #[cfg(not(unix))]
    pub const fn send(self) {}
}

/// Executes a forwarded argument vector, exactly as a local run would.
///
/// Separated from [`Request`] so the fidelity rules can be tested without a
/// socket, which is the part most likely to drift from
/// [`crate::report::Reporter::emit`].
fn run(argv: &[OsString], _cwd: Option<&Path>) -> (Option<Command>, Response) {
    use clap::Parser as _;

    if argv.is_empty() {
        // A forwarded invocation with nothing in it cannot be honoured, and
        // clap would answer with the help text rather than saying so.
        return (
            None,
            stderr(2, "the forwarded command had no arguments\n".to_owned()),
        );
    }

    // The wire carries the arguments *after* the program name — `ipc::forward`
    // is handed `env::args().skip(1)`. clap always treats its first element as
    // the program name and discards it, so without a placeholder the real
    // subcommand is eaten and every forwarded command parses as a bare
    // `scrozz`. That failure is invisible: the response comes back well-formed,
    // for the wrong command.
    let mut with_argv0 = Vec::with_capacity(argv.len() + 1);
    with_argv0.push(OsString::from("scrozz"));
    with_argv0.extend_from_slice(argv);

    let cli = match Cli::try_parse_from(&with_argv0) {
        Ok(cli) => cli,
        // clap's own rejection. There is no slug to report it under, because we
        // never got as far as knowing which subcommand was meant.
        Err(err) => return (None, stderr(2, err.to_string())),
    };

    let command = cli.command.clone().unwrap_or(Command::Gui);
    let slug = command.slug();
    if ipc::policy(&command) == ipc::Forwarding::Never {
        let error = CliError::ipc(format!(
            "{} must run in the calling process rather than the GUI listener",
            command.slug()
        ));
        return (Some(command), response_for_error(&cli, &slug, &error));
    }

    // The only forwardable operation is `record --stop`, which takes no path.
    // Never change a multithreaded GUI process's current directory: cwd is
    // process-global, so even a perfectly restored temporary switch races every
    // other thread resolving a relative path.
    let result = cli.validate().and_then(|()| commands::dispatch(&command));

    let response = match result {
        Ok(report) => {
            if let Some(bytes) = report.raw {
                Response {
                    code: 0,
                    stream: StreamKind::Binary,
                    stdout: bytes,
                    stderr: if cli.global.quiet || report.human.is_empty() {
                        Vec::new()
                    } else {
                        line(report.human).into_bytes()
                    },
                }
            } else if cli.global.json {
                json(
                    0,
                    line(success_envelope(&slug, report.data).to_compact_string()),
                )
            } else if cli.global.quiet {
                text(0, String::new())
            } else {
                text(0, line(report.human))
            }
        }
        Err(err) => response_for_error(&cli, &slug, &err),
    };

    (Some(command), response)
}

fn response_for_error(cli: &Cli, slug: &str, error: &CliError) -> Response {
    let code = error.exit().code();
    if cli.global.json {
        json(code, line(error_envelope(slug, error).to_compact_string()))
    } else {
        stderr(code, error.to_human())
    }
}

fn text(code: u8, body: String) -> Response {
    Response {
        code,
        stream: StreamKind::Text,
        stdout: body.into_bytes(),
        stderr: Vec::new(),
    }
}

fn json(code: u8, body: String) -> Response {
    Response {
        code,
        stream: StreamKind::Json,
        stdout: body.into_bytes(),
        stderr: Vec::new(),
    }
}

fn stderr(code: u8, body: String) -> Response {
    Response {
        code,
        stream: StreamKind::Text,
        stdout: Vec::new(),
        stderr: body.into_bytes(),
    }
}

fn line(mut body: String) -> String {
    if !body.is_empty() {
        body.truncate(body.trim_end().len());
        body.push('\n');
    }
    body
}

/// The listener a running GUI holds.
pub struct Server {
    path: PathBuf,
    #[cfg(unix)]
    listener: std::os::unix::net::UnixListener,
    #[cfg(unix)]
    pending: VecDeque<Pending>,
    #[cfg(unix)]
    _instance_lock: std::fs::File,
}

#[cfg(unix)]
struct Pending {
    stream: std::os::unix::net::UnixStream,
    raw: Vec<u8>,
    accepted: Instant,
}

#[cfg(unix)]
const MAX_PENDING: usize = 64;
#[cfg(unix)]
const ACCEPT_BUDGET: usize = 8;
#[cfg(unix)]
const PENDING_BUDGET: usize = 8;

#[cfg(unix)]
enum PendingProgress {
    Waiting,
    Drop,
    Ready,
    Reject(String),
}

#[cfg(unix)]
impl Pending {
    fn advance(&mut self) -> PendingProgress {
        use std::io::{ErrorKind, Read};

        const READ_BUDGET: usize = 64 * 1024;
        const CONNECTION_DEADLINE: Duration = Duration::from_secs(5);

        if self.accepted.elapsed() >= CONNECTION_DEADLINE {
            return PendingProgress::Reject(
                "the IPC request did not complete within 5 seconds".to_owned(),
            );
        }

        let mut read = 0;
        let mut chunk = [0u8; 4096];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) if self.raw.is_empty() => return PendingProgress::Drop,
                Ok(0) => {
                    return PendingProgress::Reject(
                        "the IPC request ended before its newline frame".to_owned(),
                    );
                }
                Ok(count) => {
                    self.raw.extend_from_slice(&chunk[..count]);
                    read += count;
                    if self.raw.len() > ipc::MAX_REQUEST_BYTES {
                        return PendingProgress::Reject(format!(
                            "the IPC request exceeded the {}-byte limit",
                            ipc::MAX_REQUEST_BYTES
                        ));
                    }
                    if let Some(end) = self.raw.iter().position(|byte| *byte == b'\n') {
                        if end + 1 != self.raw.len() {
                            return PendingProgress::Reject(
                                "the IPC request carried bytes after its newline frame".to_owned(),
                            );
                        }
                        return PendingProgress::Ready;
                    }
                    if read >= READ_BUDGET {
                        return PendingProgress::Waiting;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    return PendingProgress::Waiting;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => {
                    return PendingProgress::Reject(format!(
                        "could not read the IPC request: {error}"
                    ));
                }
            }
        }
    }
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
        Self::bind_at(ipc::endpoint())
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
        use std::fs::OpenOptions;
        use std::os::unix::net::UnixListener;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CliError::ipc(format!(
                    "could not make {} for the instance socket: {e}",
                    parent.display()
                ))
            })?;
        }

        let lock_path = instance_lock_path(&path);
        let instance_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| {
                CliError::ipc(format!(
                    "could not open the instance lock {}: {error}",
                    lock_path.display()
                ))
            })?;
        instance_lock.try_lock().map_err(|error| {
            CliError::ipc(format!(
                "another Scrozz is starting or running (instance lock {}: {error})",
                lock_path.display()
            ))
        })?;

        clear_stale(&path)?;

        let listener = UnixListener::bind(&path)
            .map_err(|e| CliError::ipc(format!("could not listen at {}: {e}", path.display())))?;
        listener.set_nonblocking(true).map_err(|e| {
            CliError::ipc(format!("could not make the instance socket pollable: {e}"))
        })?;

        tracing::debug!(path = %path.display(), "listening for forwarded commands");
        Ok(Self {
            path,
            listener,
            pending: VecDeque::new(),
            _instance_lock: instance_lock,
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
        tracing::warn!(
            "this build has no named-pipe listener, so live-state operations \
             such as `scrozz record --stop` cannot be forwarded"
        );
        Ok(Self { path })
    }

    /// Takes one pending request, if there is one. Never blocks.
    #[cfg(unix)]
    pub fn poll(&mut self) -> Option<Request> {
        use std::io::ErrorKind;

        for _ in 0..ACCEPT_BUDGET {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.pending.len() >= MAX_PENDING {
                        tracing::warn!(
                            "refusing an IPC connection because {MAX_PENDING} are already pending"
                        );
                        drop(stream);
                        continue;
                    }
                    if let Err(error) = stream.set_nonblocking(true) {
                        tracing::warn!("could not make an IPC connection nonblocking: {error}");
                        continue;
                    }
                    self.pending.push_back(Pending {
                        stream,
                        raw: Vec::new(),
                        accepted: Instant::now(),
                    });
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => {
                    tracing::warn!("could not accept a forwarded command: {error}");
                    break;
                }
            }
        }

        let pending_budget = self.pending.len().min(PENDING_BUDGET);
        for _ in 0..pending_budget {
            let mut pending = self
                .pending
                .pop_front()
                .expect("the pending budget was derived from the queue length");
            match pending.advance() {
                PendingProgress::Waiting => self.pending.push_back(pending),
                PendingProgress::Drop => {}
                PendingProgress::Ready => return Some(parse_request(pending)),
                PendingProgress::Reject(message) => {
                    return Some(rejected_request(pending.stream, message));
                }
            }
        }
        None
    }

    /// Nothing arrives without a listener.
    #[cfg(not(unix))]
    #[must_use]
    pub const fn poll(&mut self) -> Option<Request> {
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
        // Otherwise the next launch finds a socket file with nothing behind it
        // and has to decide whether it is stale — solvable, but this is free.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Removes a socket file left behind by a crash.
///
/// The caller holds the instance lock across this check and the subsequent
/// bind, so another launcher cannot race between the probe and removal.
#[cfg(unix)]
fn clear_stale(path: &Path) -> CliResult<()> {
    use std::{io::ErrorKind, os::unix::fs::FileTypeExt as _};

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(CliError::ipc(format!(
                "could not inspect the existing instance endpoint {}: {error}",
                path.display()
            )));
        }
    };

    if metadata.file_type().is_socket() {
        match ipc::connect_until(path, Instant::now() + Duration::from_millis(250)) {
            Ok(_) => {
                return Err(CliError::ipc(format!(
                    "another Scrozz is already running and listening at {}",
                    path.display()
                )));
            }
            Err(error) if error.kind() == ErrorKind::ConnectionRefused => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(CliError::ipc(format!(
                    "could not prove the existing instance socket {} is stale; \
                     refusing to remove it: {error}",
                    path.display()
                )));
            }
        }
    }

    std::fs::remove_file(path).map_err(|error| {
        CliError::ipc(format!(
            "could not remove the stale instance socket {}: {error}",
            path.display()
        ))
    })?;
    Ok(())
}

#[cfg(unix)]
fn instance_lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

#[cfg(unix)]
fn parse_request(pending: Pending) -> Request {
    let value: serde_json::Value = match serde_json::from_slice(&pending.raw) {
        Ok(value) => value,
        Err(error) => {
            return rejected_request(
                pending.stream,
                format!("the IPC request was not valid JSON: {error}"),
            );
        }
    };
    let schema = value.get("schema").and_then(serde_json::Value::as_i64);
    let protocol = value.get("protocol").and_then(serde_json::Value::as_str);
    let guarded = value
        .get("argv")
        .and_then(serde_json::Value::as_array)
        .and_then(|argv| argv.first())
        .and_then(serde_json::Value::as_str)
        == Some(ipc::REQUEST_PROTOCOL_ARG);
    if schema != Some(ipc::REQUEST_SCHEMA) || protocol != Some(ipc::PROTOCOL_TOKEN) || !guarded {
        return Request {
            argv: Vec::new(),
            cwd: None,
            preflight: Some(protocol_rejection(schema, protocol, guarded)),
            stream: pending.stream,
        };
    }

    let encoding = match value.get("os_encoding").and_then(serde_json::Value::as_str) {
        Some(encoding) => encoding,
        None => return rejected_request(pending.stream, "missing os_encoding".to_owned()),
    };
    let arguments = match value.get("arguments").and_then(serde_json::Value::as_array) {
        Some(arguments) => arguments,
        None => return rejected_request(pending.stream, "missing arguments".to_owned()),
    };
    let argv = match arguments
        .iter()
        .map(|argument| ipc::decode_os(argument, encoding))
        .collect::<CliResult<Vec<_>>>()
    {
        Ok(argv) => argv,
        Err(error) => return rejected_request(pending.stream, error.to_human()),
    };
    let cwd = match value.get("cwd") {
        None | Some(serde_json::Value::Null) => None,
        Some(cwd) => match ipc::decode_os(cwd, encoding) {
            Ok(cwd) => Some(PathBuf::from(cwd)),
            Err(error) => return rejected_request(pending.stream, error.to_human()),
        },
    };
    let kind = value.get("kind").and_then(serde_json::Value::as_str);
    let preflight = match kind {
        Some("hello") if argv.is_empty() && cwd.is_none() => Some(text(0, String::new())),
        Some("command") => None,
        _ => Some(protocol_rejection(schema, protocol, guarded)),
    };
    Request {
        argv,
        cwd,
        preflight,
        stream: pending.stream,
    }
}

#[cfg(unix)]
fn rejected_request(stream: std::os::unix::net::UnixStream, message: String) -> Request {
    let error = CliError::ipc(message);
    Request {
        argv: Vec::new(),
        cwd: None,
        preflight: Some(stderr(error.exit().code(), error.to_human())),
        stream,
    }
}

fn protocol_rejection(schema: Option<i64>, protocol: Option<&str>, guarded: bool) -> Response {
    let error = CliError::ipc(format!(
        "the request protocol is incompatible (schema {schema:?}, protocol {protocol:?}, \
         guarded argv {guarded}); this instance requires schema {} and {}",
        ipc::REQUEST_SCHEMA,
        ipc::PROTOCOL_TOKEN
    ));
    stderr(error.exit().code(), error.to_human())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    fn request_json(line: &str) -> serde_json::Value {
        serde_json::from_str(line).expect("valid request JSON")
    }

    fn decoded_arguments(value: &serde_json::Value) -> Vec<OsString> {
        let encoding = value["os_encoding"].as_str().expect("OS encoding");
        value["arguments"]
            .as_array()
            .expect("argument array")
            .iter()
            .map(|argument| ipc::decode_os(argument, encoding).expect("encoded OS string"))
            .collect()
    }

    #[test]
    fn an_argv_array_round_trips_through_the_wire_format() {
        let sent = argv(&["scrozz", "capture", "--json"]);
        let line = ipc::encode_request(&sent, None);
        let value = request_json(&line);
        assert_eq!(decoded_arguments(&value), sent);
        assert_eq!(value["argv"][0], ipc::REQUEST_PROTOCOL_ARG);
        assert_eq!(value["schema"], ipc::REQUEST_SCHEMA);
        assert_eq!(value["protocol"], ipc::PROTOCOL_TOKEN);
    }

    #[test]
    fn a_cwd_round_trips_and_is_absent_when_not_sent() {
        let sent = argv(&["scrozz"]);
        let with = ipc::encode_request(&sent, Some(Path::new("/Users/someone/work")));
        let value = request_json(&with);
        assert_eq!(
            ipc::decode_os(&value["cwd"], value["os_encoding"].as_str().unwrap()).unwrap(),
            OsStr::new("/Users/someone/work")
        );

        let without = ipc::encode_request(&sent, None);
        assert!(request_json(&without)["cwd"].is_null());
    }

    #[test]
    fn an_argument_containing_a_quote_survives() {
        // The case a naive split on `","` gets wrong.
        let sent = argv(&["scrozz", "capture", "-o", r#"/a "quoted" name.png"#]);
        let line = ipc::encode_request(&sent, None);
        assert_eq!(decoded_arguments(&request_json(&line)), sent);
    }

    #[test]
    fn an_argument_containing_a_backslash_survives() {
        let sent = argv(&["scrozz", r"C:\shots\a.png"]);
        let line = ipc::encode_request(&sent, None);
        assert_eq!(decoded_arguments(&request_json(&line)), sent);
    }

    #[test]
    fn a_non_ascii_argument_survives() {
        let sent = argv(&["scrozz", "capture", "-o", "/captures/écran ✅.png"]);
        let line = ipc::encode_request(&sent, None);
        assert_eq!(decoded_arguments(&request_json(&line)), sent);
    }

    #[test]
    fn an_empty_argv_still_carries_the_protocol_guard() {
        let line = ipc::encode_request::<OsString>(&[], None);
        let value = request_json(&line);
        assert_eq!(value["argv"][0], ipc::REQUEST_PROTOCOL_ARG);
        assert!(decoded_arguments(&value).is_empty());
    }

    #[test]
    fn a_truncated_line_is_rejected_rather_than_panicking() {
        // This arrives from outside the process, so it must never abort us.
        for line in [
            r#"{"argv":["scrozz"#,
            r#"{"argv":["#,
            r#"{"cwd":"#,
            r#"{"cwd":"unterminated"#,
        ] {
            assert!(serde_json::from_str::<serde_json::Value>(line).is_err());
        }
    }

    #[test]
    fn a_missing_key_is_none() {
        let line = ipc::encode_request(&argv(&["scrozz"]), None);
        assert!(request_json(&line).get("nope").is_none());
    }

    #[test]
    fn an_empty_argument_vector_is_answered_not_ignored() {
        let (command, response) = run(&[], None);
        assert!(command.is_none());
        assert_eq!(response.code, 2);
        assert!(response.stdout.is_empty());
        assert!(!response.stderr.is_empty());
    }

    #[test]
    fn an_unparseable_command_answers_with_claps_own_message() {
        let (command, response) = run(&argv(&["nonsuch"]), None);
        assert!(command.is_none(), "there is no command to name");
        assert_eq!(response.code, 2);
        assert_eq!(response.stream, StreamKind::Text);
        assert!(response.stdout.is_empty());
        let message = String::from_utf8_lossy(&response.stderr);
        assert!(message.contains("nonsuch"), "{message}");
    }

    #[test]
    fn a_forwarded_failure_carries_the_same_exit_code_as_a_local_one() {
        // `list displays` needs a backend, which is guarded off by default, so
        // this is a stable failure that does not touch the screen.
        let (command, response) = run(&argv(&["list", "displays"]), None);
        assert!(matches!(command, Some(Command::List(_))));
        assert_ne!(
            response.code, 0,
            "a guarded backend must not report success"
        );
    }

    #[test]
    fn a_forwarded_json_failure_is_an_envelope_not_a_sentence() {
        let (_, response) = run(&argv(&["--json", "list", "displays"]), None);
        assert_eq!(response.stream, StreamKind::Json);
        assert!(response.stderr.is_empty());
        let body = String::from_utf8_lossy(&response.stdout);
        assert!(body.starts_with('{'), "{body}");
        assert!(body.contains("\"ok\":false"), "{body}");
        assert!(body.contains("\"command\":\"list.displays\""), "{body}");
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn a_forwarded_human_failure_keeps_rich_diagnostics_on_stderr() {
        let (_, response) = run(&argv(&["list", "displays"]), None);
        assert_ne!(response.code, 0);
        assert!(response.stdout.is_empty());
        let body = String::from_utf8_lossy(&response.stderr);
        assert!(body.starts_with("scrozz:"), "{body}");
        assert!(body.ends_with('\n'), "{body}");
    }

    #[test]
    fn a_non_forwardable_command_is_rejected_before_execution() {
        let (_, response) = run(&argv(&["capture", "--dry-run"]), None);
        assert_eq!(response.code, crate::exit::Exit::IpcFailed.code());
        assert_eq!(response.stream, StreamKind::Text);
        assert!(response.stdout.is_empty());
        let body = String::from_utf8_lossy(&response.stderr);
        assert!(body.contains("calling process"), "{body}");
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn a_quiet_non_forwardable_command_still_fails() {
        let (_, response) = run(&argv(&["--quiet", "capture", "--dry-run"]), None);
        assert_eq!(response.code, crate::exit::Exit::IpcFailed.code());
        assert!(response.stdout.is_empty());
        assert!(String::from_utf8_lossy(&response.stderr).contains("calling process"));
    }

    #[test]
    fn a_json_non_forwardable_command_is_an_error_envelope() {
        let (_, response) = run(&argv(&["--json", "capture", "--dry-run"]), None);
        assert_eq!(response.stream, StreamKind::Json);
        assert!(response.stderr.is_empty());
        let body = String::from_utf8_lossy(&response.stdout);
        assert!(body.contains("\"ok\":false"), "{body}");
        assert!(body.contains("\"command\":\"capture\""), "{body}");
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn a_response_survives_the_round_trip_the_client_will_make() {
        // The real check: whatever we produce, `ipc::parse_response` must read
        // back unchanged, or the terminal shows something different from a
        // local run.
        let (_, response) = run(&argv(&["--json", "capture", "--dry-run"]), None);
        let wire = ipc::encode_response(&response);
        let parsed = ipc::parse_response(&wire).expect("our own wire format must parse");
        assert_eq!(parsed.code, response.code);
        assert_eq!(parsed.stream, response.stream);
        assert_eq!(parsed.stdout, response.stdout);
        assert_eq!(parsed.stderr, response.stderr);
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scrozz-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    #[cfg(unix)]
    fn await_request(server: &mut Server) -> Request {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(request) = server.poll() {
                return request;
            }
            assert!(
                Instant::now() < deadline,
                "the test client did not deliver an IPC request"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[cfg(unix)]
    fn pending_command(label: &str) -> Pending {
        use std::{io::Write, net::Shutdown, os::unix::net::UnixStream};

        let (stream, mut client) = UnixStream::pair().expect("socket pair");
        stream.set_nonblocking(true).expect("nonblocking server");
        client
            .write_all(ipc::encode_request(&argv(&["record", "--stop", label]), None).as_bytes())
            .expect("pending request");
        client.shutdown(Shutdown::Write).expect("finish request");
        Pending {
            stream,
            raw: Vec::new(),
            accepted: Instant::now(),
        }
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
    fn an_unbound_socket_is_taken_over_after_connection_refusal() {
        use std::os::unix::net::UnixListener;

        let dir = scratch("stale-socket");
        let path = dir.join("stale.sock");
        drop(UnixListener::bind(&path).expect("stale socket fixture"));

        let server =
            Server::bind_at(path.clone()).expect("a definitively stale socket must be replaced");
        assert_eq!(server.path(), path);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn polling_an_idle_server_yields_nothing() {
        let dir = scratch("idle");
        let mut server = Server::bind_at(dir.join("idle.sock")).expect("binding");
        assert!(server.poll().is_none());
        assert!(server.poll().is_none());
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn pending_requests_are_serviced_in_arrival_order() {
        let dir = scratch("pending-order");
        let mut server = Server::bind_at(dir.join("order.sock")).expect("binding");
        for label in ["first", "second", "third"] {
            server.pending.push_back(pending_command(label));
        }

        let first = server.poll().expect("first request");
        let second = server.poll().expect("second request");
        assert_eq!(
            first.argv.last().map(OsString::as_os_str),
            Some(OsStr::new("first"))
        );
        assert_eq!(
            second.argv.last().map(OsString::as_os_str),
            Some(OsStr::new("second"))
        );

        drop(first);
        drop(second);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn one_poll_accepts_only_its_fixed_connection_budget() {
        use std::os::unix::net::UnixStream;

        let dir = scratch("accept-budget");
        let path = dir.join("budget.sock");
        let mut server = Server::bind_at(path.clone()).expect("binding");
        let clients = (0..ACCEPT_BUDGET + 3)
            .map(|_| UnixStream::connect(&path).expect("client connection"))
            .collect::<Vec<_>>();

        assert!(server.poll().is_none());
        assert_eq!(server.pending.len(), ACCEPT_BUDGET);

        drop(clients);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_partial_client_does_not_block_a_complete_client() {
        use std::{
            io::{Read, Write},
            net::Shutdown,
            os::unix::net::UnixStream,
            time::Instant,
        };

        let dir = scratch("partial-client");
        let path = dir.join("partial.sock");
        let mut server = Server::bind_at(path.clone()).expect("binding");
        let mut partial = UnixStream::connect(&path).expect("partial connection");
        partial
            .write_all(br#"{"schema":3"#)
            .expect("partial request");

        let started = Instant::now();
        assert!(server.poll().is_none());
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "poll must not wait for a slow client"
        );

        let complete = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(path).expect("complete connection");
                stream
                    .write_all(ipc::encode_hello().as_bytes())
                    .expect("hello");
                stream.shutdown(Shutdown::Write).expect("finish hello");
                let mut response = Vec::new();
                stream
                    .read_to_end(&mut response)
                    .expect("read hello response");
                ipc::parse_response(&response).expect("valid hello response")
            }
        });

        let request = await_request(&mut server);
        assert!(request.serve().is_none());
        assert_eq!(complete.join().expect("complete client").code, 0);

        drop(partial);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_dropped_client_does_not_hide_a_complete_request() {
        use std::{
            io::{Read, Write},
            net::Shutdown,
            os::unix::net::UnixStream,
        };

        let dir = scratch("dropped-client");
        let path = dir.join("dropped.sock");
        let mut server = Server::bind_at(path.clone()).expect("binding");

        drop(UnixStream::connect(&path).expect("dropped connection"));
        let mut complete = UnixStream::connect(&path).expect("complete connection");
        complete
            .write_all(ipc::encode_hello().as_bytes())
            .expect("hello");
        complete.shutdown(Shutdown::Write).expect("finish hello");

        let request = server
            .poll()
            .expect("poll must skip the dropped client and return the complete request");
        assert!(request.serve().is_none());

        let mut response = Vec::new();
        complete
            .read_to_end(&mut response)
            .expect("read hello response");
        assert_eq!(
            ipc::parse_response(&response)
                .expect("valid hello response")
                .code,
            0
        );

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
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
        let mut server = Server::bind_at(path.clone()).expect("binding");

        // No program name: `try_forward` sends `env::args().skip(1)`, and the
        // server is the side that has to know that.
        let sent = argv(&["record", "--stop"]);
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                // SAFETY-adjacent: the env var is process-global, but this test
                // is the only one using this endpoint name.
                unsafe { std::env::set_var(ipc::ENDPOINT_ENV, &path) };
                ipc::forward(&sent)
            }
        });

        let hello = await_request(&mut server);
        assert!(hello.argv.is_empty());
        assert!(hello.serve().is_none());

        let request = await_request(&mut server);
        assert_eq!(
            request.argv.first().map(OsString::as_os_str),
            Some(OsStr::new("record"))
        );
        let command = request.serve();
        assert!(matches!(command, Some(Command::Record(_))));

        let response = client
            .join()
            .expect("the client thread")
            .expect("a well-formed answer");
        assert_ne!(response.code, crate::exit::Exit::IpcFailed.code());
        assert!(
            !String::from_utf8_lossy(&response.stderr).contains("calling process"),
            "the only required forwarded operation must reach execution"
        );

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn an_old_request_is_rejected_before_dispatch() {
        use std::{
            io::{Read, Write},
            net::Shutdown,
            os::unix::net::UnixStream,
        };

        let dir = scratch("old-protocol");
        let path = dir.join("old.sock");
        let mut server = Server::bind_at(path.clone()).expect("binding");
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(path).expect("connect");
                stream
                    .write_all(
                        b"{\"schema\":1,\"argv\":[\"capture\",\"--dry-run\"],\"cwd\":null}\n",
                    )
                    .expect("write old request");
                stream.shutdown(Shutdown::Write).expect("finish request");
                let mut response = Vec::new();
                stream.read_to_end(&mut response).expect("read rejection");
                ipc::parse_response(&response).expect("current rejection response")
            }
        });

        let request = await_request(&mut server);
        assert!(
            request.serve().is_none(),
            "an incompatible request must not dispatch"
        );
        let response = client.join().expect("client thread");
        assert_ne!(response.code, 0);
        assert!(response.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&response.stderr).contains("incompatible"),
            "{response:?}"
        );

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
