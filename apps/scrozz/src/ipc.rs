//! Talking to an already-running Scrozz.
//!
//! # The problem
//!
//! Scrozz is invisible at rest (D27): a menu-bar item with no window and no Dock
//! icon. On sway and Hyprland the *only* way to trigger a capture is a
//! compositor keybinding that runs `scrozz capture` (D11). Put those together and
//! every hotkey press starts a **second process**.
//!
//! Left alone, that second process is a second application: its own capture
//! stack, its own store handle, its own overlay. Pressing the hotkey twice would
//! produce two captures that never appear in the same history, and two writers
//! against one SQLite file.
//!
//! # The design
//!
//! A running instance listens on a socket. Only an operation that requires live
//! process-owned state is handed over; currently that is `record --stop`. Pure
//! capture, OCR, barcode, history, settings, and query commands stay in the
//! calling process, avoiding a GUI-thread hop and preserving native path and
//! diagnostic behavior.
//!
//! # The wire format, and why it is not JSON both ways
//!
//! ```text
//! -->  {"schema":3,"protocol":"SCROZZ/3","kind":"command",...}\n
//! <--  SCROZZ/3 0 json 12 0\n
//! <--  <12 stdout bytes><0 stderr bytes>
//! ```
//!
//! The request is one line of JSON; the response is a plain-text header followed
//! by length-delimited, opaque stdout and stderr bytes. That asymmetry is
//! deliberate:
//!
//! - The payload may be a PNG (`--stdout`). Base64-ing it through a JSON
//!   envelope would cost memory and a decoder for no benefit.
//! - The client must relay both payloads **unmodified**. If it had to parse and
//!   re-serialise JSON, the forwarded output could differ from the local output
//!   in key order or float formatting, and the contract in D11 would be a lie.
//! - Human failures retain rich diagnostics on stderr while JSON failures remain
//!   machine-readable on stdout.
//!
//! # What is implemented here
//!
//! The protocol, the endpoint rules, encoding, parsing, and the policy for which
//! commands forward. The **server** belongs to the GUI, which owns the event loop
//! and the store.

#[cfg(unix)]
use std::time::{Duration, Instant};
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use crate::{
    cli::Command,
    fault::{CliError, CliResult},
    json::Json,
};

/// The protocol version token that opens every response.
pub const PROTOCOL_TOKEN: &str = "SCROZZ/3";

/// The request schema version.
pub const REQUEST_SCHEMA: i64 = 3;

/// First wire argument, deliberately rejected by older clap parsers.
///
/// The client negotiates before sending a command, and this sentinel is the
/// fail-safe for a daemon replacement between negotiation and dispatch: an
/// older daemon sees an unknown option rather than executing the remaining argv.
pub const REQUEST_PROTOCOL_ARG: &str = "--scrozz-ipc=SCROZZ/3";

/// Maximum request frame accepted by the GUI.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Maximum combined response frame accepted by a client.
const MAX_RESPONSE_BYTES: usize = 512 * 1024 * 1024;

/// The response header is five short, whitespace-delimited fields.
const MAX_RESPONSE_HEADER_BYTES: usize = 128;

/// No IPC peer gets to hold a process forever.
#[cfg(unix)]
const IO_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(unix)]
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Overrides the endpoint, for tests and for unusual sandboxes.
pub const ENDPOINT_ENV: &str = "SCROZZ_IPC_SOCKET";

/// The encoding of a relayed stdout payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// One JSON document. Goes to stdout verbatim.
    Json,
    /// Human-readable text. Goes to stdout verbatim.
    Text,
    /// Raw bytes, as `--stdout` produces.
    Binary,
}

impl StreamKind {
    /// The token used on the wire.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Text => "text",
            Self::Binary => "binary",
        }
    }

    fn parse(token: &str) -> Option<Self> {
        match token {
            "json" => Some(Self::Json),
            "text" => Some(Self::Text),
            "binary" => Some(Self::Binary),
            _ => None,
        }
    }
}

/// A relayed result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The exit code the running instance produced. Adopted verbatim, so a
    /// script sees the same code whether the work happened here or there.
    pub code: u8,
    /// The encoding of stdout, used for diagnostics and binary-safe relaying.
    pub stream: StreamKind,
    /// Standard output, byte for byte as the running instance produced it.
    pub stdout: Vec<u8>,
    /// Standard error, byte for byte as the running instance produced it.
    pub stderr: Vec<u8>,
}

/// Whether an instance is reachable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// A socket exists and accepted a connection.
    Running,
    /// No instance is listening. Not an error: the overwhelmingly common case
    /// is a one-shot CLI invocation on a machine where the GUI was never started.
    NotRunning,
    /// The endpoint exists but could not be used, e.g. wrong permissions.
    Unusable(String),
}

/// Whether a command should be handed to a running instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forwarding {
    /// Forward when an instance is running; do it locally otherwise.
    Prefer,
    /// Forward or fail. Only for commands that are meaningless in isolation.
    Require,
    /// Always local. Forwarding would add a hop and a failure mode for nothing.
    Never,
}

/// The forwarding policy for a command.
///
/// The rule: forward only work that cannot function without live state owned by
/// the existing process, and keep everything else local.
#[must_use]
pub fn policy(command: &Command) -> Forwarding {
    match command {
        // A recording is a live process owned by whoever started it. Stopping it
        // from a second process is not merely preferable, it is the only thing
        // that can work — which is exactly the hotkey case on wlroots, where
        // `record --stop` is bound to a key and always arrives in a fresh
        // process.
        Command::Record(args) if args.stop => Forwarding::Require,

        // No other command currently acts on live GUI-owned state. Running each
        // in its caller keeps native paths lossless and prevents capture, OCR,
        // encoding, SQLite, or subprocess work from occupying the GUI listener.
        Command::Capture(_)
        | Command::Record(_)
        | Command::History(_)
        | Command::Ocr(_)
        | Command::Barcodes(_)
        | Command::Settings(_)
        | Command::List(_)
        | Command::Hotkey(_)
        | Command::Gui => Forwarding::Never,
    }
}

/// Where the socket lives.
///
/// Runtime directories rather than a config directory because the socket is
/// per-boot, per-user state that must not survive a reboot, and because on Linux
/// `XDG_RUNTIME_DIR` is already `0700` and on tmpfs.
#[must_use]
pub fn endpoint() -> PathBuf {
    if let Ok(explicit) = std::env::var(ENDPOINT_ENV)
        && !explicit.is_empty()
    {
        return PathBuf::from(explicit);
    }
    default_endpoint()
}

#[cfg(target_os = "linux")]
fn default_endpoint() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR").map_or_else(
        |_| PathBuf::from(format!("/tmp/scrozz-{}/instance.sock", user_token())),
        |dir| PathBuf::from(dir).join("scrozz/instance.sock"),
    )
}

#[cfg(target_os = "macos")]
fn default_endpoint() -> PathBuf {
    // `TMPDIR` on macOS is already a private per-user directory under
    // `/var/folders`, which is the closest equivalent to `XDG_RUNTIME_DIR`.
    let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join(format!("scrozz-{}/instance.sock", user_token()))
}

#[cfg(target_os = "windows")]
fn default_endpoint() -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\scrozz-{}", user_token()))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn default_endpoint() -> PathBuf {
    PathBuf::from(format!("/tmp/scrozz-{}/instance.sock", user_token()))
}

/// A per-user component for the endpoint path.
///
/// Two users on one machine must not collide on a shared-temp path, and one
/// must never be able to hand the other's Scrozz a command.
fn user_token() -> String {
    for key in ["USER", "LOGNAME", "USERNAME"] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            return value
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
        }
    }
    "default".to_string()
}

/// Encodes a request.
///
/// The full argument list after the program name is forwarded rather than a
/// parsed structure so the running instance parses with the same code path a
/// local run uses. A parser on each side is two parsers that can disagree.
#[must_use]
pub fn encode_request<S: AsRef<OsStr>>(argv: &[S], cwd: Option<&Path>) -> String {
    encode_request_kind("command", argv, cwd)
}

pub(crate) fn encode_hello() -> String {
    encode_request_kind("hello", &[] as &[OsString], None)
}

fn encode_request_kind<S: AsRef<OsStr>>(kind: &str, argv: &[S], cwd: Option<&Path>) -> String {
    let request = Json::obj([
        ("schema", Json::Int(REQUEST_SCHEMA)),
        ("protocol", Json::str(PROTOCOL_TOKEN)),
        ("kind", Json::str(kind)),
        // Older daemons parse only this textual argv and reject the unknown
        // sentinel before they can execute a command from a newer client.
        ("argv", Json::arr([Json::str(REQUEST_PROTOCOL_ARG)])),
        ("os_encoding", Json::str(os_encoding())),
        (
            "arguments",
            Json::arr(argv.iter().map(|argument| encode_os(argument.as_ref()))),
        ),
        // Relative `--output` paths resolve against the *caller's* directory,
        // not the daemon's. Without this, `scrozz capture -o shot.png` would
        // silently write somewhere else once the GUI is running.
        ("cwd", Json::opt(cwd, |path| encode_os(path.as_os_str()))),
    ]);
    format!("{}\n", request.to_compact_string())
}

#[cfg(unix)]
const fn os_encoding() -> &'static str {
    "unix-bytes"
}

#[cfg(windows)]
const fn os_encoding() -> &'static str {
    "windows-wide"
}

#[cfg(not(any(unix, windows)))]
const fn os_encoding() -> &'static str {
    "utf8-bytes"
}

#[cfg(unix)]
fn encode_os(value: &OsStr) -> Json {
    use std::os::unix::ffi::OsStrExt as _;
    Json::arr(
        value
            .as_bytes()
            .iter()
            .map(|byte| Json::Int(i64::from(*byte))),
    )
}

#[cfg(windows)]
fn encode_os(value: &OsStr) -> Json {
    use std::os::windows::ffi::OsStrExt as _;
    Json::arr(value.encode_wide().map(|unit| Json::Int(i64::from(unit))))
}

#[cfg(not(any(unix, windows)))]
fn encode_os(value: &OsStr) -> Json {
    Json::arr(
        value
            .to_string_lossy()
            .as_bytes()
            .iter()
            .map(|byte| Json::Int(i64::from(*byte))),
    )
}

/// Decodes one lossless operating-system string from a request.
pub(crate) fn decode_os(value: &serde_json::Value, encoding: &str) -> CliResult<OsString> {
    if encoding != os_encoding() {
        return Err(CliError::ipc(format!(
            "the request uses {encoding:?} strings, this platform requires {:?}",
            os_encoding()
        )));
    }
    let units = value
        .as_array()
        .ok_or_else(|| CliError::ipc("an operating-system string was not an array"))?;

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        let bytes = units
            .iter()
            .map(|unit| {
                unit.as_u64()
                    .and_then(|unit| u8::try_from(unit).ok())
                    .ok_or_else(|| CliError::ipc("a Unix path byte was outside 0..=255"))
            })
            .collect::<CliResult<Vec<_>>>()?;
        Ok(OsString::from_vec(bytes))
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt as _;
        let wide = units
            .iter()
            .map(|unit| {
                unit.as_u64()
                    .and_then(|unit| u16::try_from(unit).ok())
                    .ok_or_else(|| CliError::ipc("a Windows path unit was outside 0..=65535"))
            })
            .collect::<CliResult<Vec<_>>>()?;
        Ok(OsString::from_wide(&wide))
    }

    #[cfg(not(any(unix, windows)))]
    {
        let bytes = units
            .iter()
            .map(|unit| {
                unit.as_u64()
                    .and_then(|unit| u8::try_from(unit).ok())
                    .ok_or_else(|| CliError::ipc("a path byte was outside 0..=255"))
            })
            .collect::<CliResult<Vec<_>>>()?;
        String::from_utf8(bytes)
            .map(OsString::from)
            .map_err(|_| CliError::ipc("an operating-system string was not UTF-8"))
    }
}

/// Parses a response.
///
/// # Errors
///
/// Returns [`CliError::Ipc`] if the header is missing, malformed, or announces a
/// protocol version this build does not speak. A garbled response is reported as
/// an IPC fault rather than passed through, because relaying an unparsed payload
/// with a guessed exit code is how a broken daemon becomes a silent data bug.
pub fn parse_response(bytes: &[u8]) -> CliResult<Response> {
    let header = parse_response_header(bytes)?
        .ok_or_else(|| CliError::ipc("the running instance sent no response header"))?;
    let body = &bytes[header.body_offset..];
    let expected_len = header
        .stdout_len
        .checked_add(header.stderr_len)
        .ok_or_else(|| CliError::ipc("the response payload lengths overflowed"))?;
    if bytes.len() != header.frame_len {
        return Err(CliError::ipc(format!(
            "the response announced {expected_len} payload bytes but sent {}",
            body.len()
        )));
    }
    let (stdout, stderr) = body.split_at(header.stdout_len);
    Ok(Response {
        code: header.code,
        stream: header.stream,
        stdout: stdout.to_vec(),
        stderr: stderr.to_vec(),
    })
}

struct ResponseHeader {
    code: u8,
    stream: StreamKind,
    stdout_len: usize,
    stderr_len: usize,
    body_offset: usize,
    frame_len: usize,
}

fn parse_response_header(bytes: &[u8]) -> CliResult<Option<ResponseHeader>> {
    let Some(split) = bytes.iter().position(|b| *b == b'\n') else {
        if bytes.len() > MAX_RESPONSE_HEADER_BYTES {
            return Err(CliError::ipc(format!(
                "the response header exceeded the {MAX_RESPONSE_HEADER_BYTES}-byte limit"
            )));
        }
        return Ok(None);
    };
    if split > MAX_RESPONSE_HEADER_BYTES {
        return Err(CliError::ipc(format!(
            "the response header exceeded the {MAX_RESPONSE_HEADER_BYTES}-byte limit"
        )));
    }

    let header = std::str::from_utf8(&bytes[..split])
        .map_err(|_| CliError::ipc("the response header was not valid UTF-8"))?;

    let mut fields = header.split_whitespace();
    let token = fields
        .next()
        .ok_or_else(|| CliError::ipc("the response header was empty"))?;
    if token != PROTOCOL_TOKEN {
        return Err(CliError::ipc(format!(
            "the running instance speaks {token:?}, this build speaks {PROTOCOL_TOKEN:?}; \
             the two are different versions of Scrozz"
        )));
    }

    let code: u8 = fields
        .next()
        .ok_or_else(|| CliError::ipc("the response header carried no exit code"))?
        .parse()
        .map_err(|_| CliError::ipc("the response header carried a malformed exit code"))?;

    let stream = fields
        .next()
        .ok_or_else(|| CliError::ipc("the response header named no stream kind"))?;
    let stream = StreamKind::parse(stream)
        .ok_or_else(|| CliError::ipc(format!("unknown stream kind {stream:?}")))?;

    let stdout_len = parse_payload_length(fields.next(), "stdout")?;
    let stderr_len = parse_payload_length(fields.next(), "stderr")?;
    if fields.next().is_some() {
        return Err(CliError::ipc(
            "the response header carried unexpected trailing fields",
        ));
    }

    let expected_len = stdout_len
        .checked_add(stderr_len)
        .ok_or_else(|| CliError::ipc("the response payload lengths overflowed"))?;
    if expected_len > MAX_RESPONSE_BYTES {
        return Err(CliError::ipc(format!(
            "the response announced {expected_len} payload bytes, exceeding the \
             {MAX_RESPONSE_BYTES}-byte response limit"
        )));
    }
    let body_offset = split + 1;
    let frame_len = body_offset
        .checked_add(expected_len)
        .ok_or_else(|| CliError::ipc("the response frame length overflowed"))?;
    if frame_len > MAX_RESPONSE_BYTES {
        return Err(CliError::ipc(format!(
            "the response frame requires {frame_len} bytes, exceeding the \
             {MAX_RESPONSE_BYTES}-byte response limit"
        )));
    }

    Ok(Some(ResponseHeader {
        code,
        stream,
        stdout_len,
        stderr_len,
        body_offset,
        frame_len,
    }))
}

fn parse_payload_length(field: Option<&str>, destination: &str) -> CliResult<usize> {
    field
        .ok_or_else(|| {
            CliError::ipc(format!(
                "the response header carried no {destination} payload length"
            ))
        })?
        .parse()
        .map_err(|_| {
            CliError::ipc(format!(
                "the response header carried a malformed {destination} payload length"
            ))
        })
}

/// Encodes a response. Used by the server side, and by the round-trip tests.
#[must_use]
pub fn encode_response(response: &Response) -> Vec<u8> {
    let mut out = format!(
        "{PROTOCOL_TOKEN} {} {} {} {}\n",
        response.code,
        response.stream.token(),
        response.stdout.len(),
        response.stderr.len()
    )
    .into_bytes();
    out.extend_from_slice(&response.stdout);
    out.extend_from_slice(&response.stderr);
    out
}

/// Whether an instance is listening.
///
/// Deliberately cheap and side-effect free: it must be safe to call on the hot
/// path of every hotkey press.
#[must_use]
pub fn probe() -> Status {
    probe_at(&endpoint())
}

#[cfg(unix)]
fn probe_at(path: &Path) -> Status {
    if !path.exists() {
        return Status::NotRunning;
    }
    match connect_until(path, Instant::now() + PROBE_TIMEOUT) {
        Ok(_) => Status::Running,
        // A socket file with nothing behind it is the normal residue of a crash,
        // not a condition worth telling the user about.
        Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused) => Status::NotRunning,
        Err(e) => Status::Unusable(e.to_string()),
    }
}

#[cfg(not(unix))]
fn probe_at(_path: &Path) -> Status {
    // Named pipes need platform calls this crate has no dependency for. Until
    // the GUI ships a listener there is nothing to connect to anyway.
    Status::NotRunning
}

/// Hands a command to a running instance.
///
/// # Errors
///
/// Returns [`CliError::Ipc`] when no instance is reachable or the exchange
/// fails. Callers whose policy is [`Forwarding::Prefer`] treat that as a signal
/// to do the work locally; only [`Forwarding::Require`] surfaces it.
pub fn forward(argv: &[OsString]) -> CliResult<Response> {
    forward_to(&endpoint(), argv)
}

#[cfg(unix)]
pub(crate) fn connect_until(
    path: &Path,
    deadline: Instant,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use socket2::{Domain, SockAddr, Socket, Type};
    use std::io::{Error, ErrorKind};
    use std::os::fd::OwnedFd;

    let timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| Error::new(ErrorKind::TimedOut, "the IPC connect deadline expired"))?;
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.connect_timeout(&SockAddr::unix(path)?, timeout)?;
    let descriptor: OwnedFd = socket.into();
    let stream = std::os::unix::net::UnixStream::from(descriptor);
    // `socket2::connect_timeout` may leave the descriptor nonblocking. The
    // exchange below uses absolute-deadline socket timeouts, which require a
    // blocking stream on macOS.
    stream.set_nonblocking(false)?;
    Ok(stream)
}

#[cfg(unix)]
fn forward_to(path: &Path, argv: &[OsString]) -> CliResult<Response> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let hello = exchange_until(path, &encode_hello(), deadline)?;
    if hello.code != 0 {
        return Err(CliError::ipc(format!(
            "the running instance rejected protocol negotiation: {}",
            String::from_utf8_lossy(&hello.stderr).trim()
        )));
    }

    let cwd = std::env::current_dir().ok();
    exchange_until(path, &encode_request(argv, cwd.as_deref()), deadline)
}

#[cfg(all(unix, test))]
fn exchange(path: &Path, request: &str) -> CliResult<Response> {
    exchange_until(path, request, Instant::now() + IO_TIMEOUT)
}

#[cfg(unix)]
fn exchange_until(path: &Path, request: &str, deadline: Instant) -> CliResult<Response> {
    use std::{
        io::{ErrorKind, Read, Write},
        net::Shutdown,
    };

    if request.len() > MAX_REQUEST_BYTES {
        return Err(CliError::ipc(format!(
            "the command requires {} request bytes, exceeding the \
             {MAX_REQUEST_BYTES}-byte IPC limit",
            request.len()
        )));
    }

    let mut stream = connect_until(path, deadline).map_err(|e| {
        CliError::ipc(format!(
            "could not reach the running Scrozz at {}: {e}",
            path.display()
        ))
    })?;

    let bytes = request.as_bytes();
    let mut written = 0;
    while written < bytes.len() {
        let remaining = remaining(deadline)?;
        stream
            .set_write_timeout(Some(remaining))
            .map_err(|e| CliError::ipc(format!("could not bound the IPC request write: {e}")))?;
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(CliError::ipc(
                    "the running instance stopped accepting the IPC request",
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                return Err(deadline_exceeded());
            }
            Err(error) => {
                return Err(CliError::ipc(format!(
                    "could not send the IPC request: {error}"
                )));
            }
        }
    }
    // Half-close so the far side sees EOF and knows the request is complete
    // without needing a length prefix.
    stream
        .shutdown(Shutdown::Write)
        .map_err(|e| CliError::ipc(format!("could not finish the request: {e}")))?;

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let remaining = remaining(deadline)?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|e| CliError::ipc(format!("could not bound the IPC response read: {e}")))?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                if buffer.len().saturating_add(count) > MAX_RESPONSE_BYTES {
                    return Err(CliError::ipc(format!(
                        "the running instance exceeded the {MAX_RESPONSE_BYTES}-byte response limit"
                    )));
                }
                buffer.extend_from_slice(&chunk[..count]);
                if parse_response_header(&buffer)?
                    .is_some_and(|header| buffer.len() >= header.frame_len)
                {
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                return Err(deadline_exceeded());
            }
            Err(error) => {
                return Err(CliError::ipc(format!(
                    "could not read the IPC response: {error}"
                )));
            }
        }
    }

    parse_response(&buffer)
}

#[cfg(unix)]
fn remaining(deadline: Instant) -> CliResult<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(deadline_exceeded)
}

#[cfg(unix)]
fn deadline_exceeded() -> CliError {
    CliError::ipc("the IPC exchange exceeded its absolute deadline")
}

#[cfg(not(unix))]
fn forward_to(_path: &Path, _argv: &[OsString]) -> CliResult<Response> {
    Err(CliError::ipc(
        "handing a command to a running instance needs named-pipe support, \
         which this build does not have yet",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;

    fn command_of(argv: &[&str]) -> Command {
        Cli::try_parse_from(argv).unwrap().command.unwrap()
    }

    fn argv(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    // -- policy ------------------------------------------------------------

    #[test]
    fn stopping_a_recording_must_be_forwarded() {
        // The recorder lives in whichever process started it. A second process
        // cannot stop it locally, so there is nothing to fall back to.
        assert_eq!(
            policy(&command_of(&["scrozz", "record", "--stop"])),
            Forwarding::Require
        );
    }

    #[test]
    fn starting_a_recording_stays_local_until_the_gui_owns_a_live_session() {
        assert_eq!(
            policy(&command_of(&["scrozz", "record"])),
            Forwarding::Never
        );
    }

    #[test]
    fn captures_stay_local_instead_of_blocking_the_gui() {
        assert_eq!(
            policy(&command_of(&["scrozz", "capture"])),
            Forwarding::Never
        );
        assert_eq!(
            policy(&command_of(&["scrozz", "capture", "--region", "0,0,10,10"])),
            Forwarding::Never
        );
    }

    #[test]
    fn history_uses_the_store_from_the_calling_process() {
        for args in [
            vec!["scrozz", "history", "list"],
            vec!["scrozz", "history", "delete", "abc"],
            vec!["scrozz", "history", "pin", "abc"],
        ] {
            assert_eq!(policy(&command_of(&args)), Forwarding::Never, "{args:?}");
        }
    }

    #[test]
    fn settings_reads_and_writes_stay_local() {
        assert_eq!(
            policy(&command_of(&[
                "scrozz",
                "settings",
                "set",
                "capture.format",
                "png"
            ])),
            Forwarding::Never
        );
        assert_eq!(
            policy(&command_of(&["scrozz", "settings", "get"])),
            Forwarding::Never
        );
    }

    #[test]
    fn pure_queries_and_pure_functions_stay_local() {
        for args in [
            vec!["scrozz", "list", "displays"],
            vec!["scrozz", "list", "windows"],
            vec!["scrozz", "ocr", "--file", "image.png"],
            vec!["scrozz", "barcodes", "--file", "image.png"],
            vec![
                "scrozz",
                "hotkey",
                "generate-config",
                "--compositor",
                "sway",
            ],
            vec!["scrozz", "gui"],
        ] {
            assert_eq!(policy(&command_of(&args)), Forwarding::Never, "{args:?}");
        }
    }

    // -- request encoding --------------------------------------------------

    #[test]
    fn a_request_is_one_newline_terminated_line() {
        let request = encode_request(&argv(&["capture", "--json"]), Some(Path::new("/home/u")));
        assert_eq!(request.matches('\n').count(), 1);
        assert!(request.ends_with('\n'));
    }

    #[test]
    fn the_request_shape_is_pinned() {
        let request = encode_request(&argv(&["capture", "--json"]), Some(Path::new("/home/u")));
        let parsed: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(parsed["schema"], REQUEST_SCHEMA);
        assert_eq!(parsed["protocol"], PROTOCOL_TOKEN);
        assert_eq!(parsed["kind"], "command");
        assert_eq!(parsed["argv"][0], REQUEST_PROTOCOL_ARG);
        assert_eq!(parsed["os_encoding"], os_encoding());
        assert_eq!(
            decode_os(&parsed["arguments"][0], os_encoding()).unwrap(),
            "capture"
        );
        assert_eq!(
            decode_os(&parsed["arguments"][1], os_encoding()).unwrap(),
            "--json"
        );
        assert_eq!(
            decode_os(&parsed["cwd"], os_encoding()).unwrap(),
            Path::new("/home/u")
        );
    }

    #[test]
    fn negotiation_and_commands_are_safe_against_an_older_clap_parser() {
        assert!(encode_hello().contains(REQUEST_PROTOCOL_ARG));
        assert!(Cli::try_parse_from(["scrozz", REQUEST_PROTOCOL_ARG]).is_err());
    }

    #[test]
    fn a_missing_working_directory_is_null_not_absent() {
        let request = encode_request(&argv(&["capture"]), None);
        let parsed: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert!(parsed["cwd"].is_null());
    }

    #[test]
    fn arguments_containing_quotes_survive_encoding() {
        let request = encode_request(&argv(&["capture", "--window", r#"He said "hi""#]), None);
        let parsed: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(
            decode_os(&parsed["arguments"][2], os_encoding()).unwrap(),
            r#"He said "hi""#
        );
        assert_eq!(request.matches('\n').count(), 1);
    }

    #[test]
    fn an_argument_containing_a_newline_cannot_split_the_request() {
        // A window title really can contain a newline; if it broke the framing
        // it would be a remote-command-injection bug against the daemon.
        let request = encode_request(&argv(&["capture", "--window", "a\nb"]), None);
        assert_eq!(request.matches('\n').count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(
            decode_os(&parsed["arguments"][2], os_encoding()).unwrap(),
            "a\nb"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_arguments_round_trip_losslessly() {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let argument = OsString::from_vec(vec![b'n', b'a', b'm', b'e', 0xff]);
        let request = encode_request(std::slice::from_ref(&argument), None);
        let parsed: serde_json::Value = serde_json::from_str(&request).unwrap();
        let decoded = decode_os(&parsed["arguments"][0], os_encoding()).unwrap();

        assert_eq!(
            decoded.as_os_str().as_bytes(),
            argument.as_os_str().as_bytes()
        );
    }

    #[cfg(unix)]
    #[test]
    fn oversized_requests_fail_before_connecting() {
        let argument = OsString::from("x".repeat(MAX_REQUEST_BYTES));
        let request = encode_request(&[argument], None);
        assert!(request.len() > MAX_REQUEST_BYTES);

        let error = exchange(Path::new("/does/not/exist"), &request).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("request bytes"), "{message}");
        assert!(message.contains("IPC limit"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_drip_response_cannot_extend_the_absolute_deadline() {
        use std::{
            io::{Read, Write},
            os::unix::net::UnixListener,
        };

        let path = PathBuf::from(format!(
            "/tmp/scrozz-ipc-deadline-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("test listener");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test connection");
            let mut request = Vec::new();
            stream.read_to_end(&mut request).expect("test request");
            for byte in b"SCROZZ/3 0 text 0 0\n" {
                std::thread::sleep(Duration::from_millis(30));
                if stream.write_all(std::slice::from_ref(byte)).is_err() {
                    break;
                }
            }
        });

        let started = Instant::now();
        let error =
            exchange_until(&path, "{}\n", started + Duration::from_millis(120)).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the absolute deadline was not enforced"
        );
        assert!(
            error.to_string().contains("absolute deadline"),
            "unexpected error: {error}"
        );

        server.join().expect("test server");
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn a_complete_response_does_not_wait_for_the_peer_to_close() {
        use std::{
            io::{Read, Write},
            os::unix::net::UnixListener,
        };

        let path = PathBuf::from(format!(
            "/tmp/scrozz-ipc-complete-response-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("test listener");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test connection");
            let mut request = Vec::new();
            stream.read_to_end(&mut request).expect("test request");
            stream
                .write_all(b"SCROZZ/3 0 text 2 0\nok")
                .expect("complete response");
            std::thread::sleep(Duration::from_millis(500));
        });

        let started = Instant::now();
        let response = exchange_until(&path, "{}\n", started + Duration::from_millis(200)).unwrap();
        assert_eq!(response.stdout, b"ok");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "the client waited for EOF after receiving the complete frame"
        );

        server.join().expect("test server");
        let _ = std::fs::remove_file(path);
    }

    // -- response parsing --------------------------------------------------

    #[test]
    fn a_well_formed_response_parses() {
        let raw = b"SCROZZ/3 0 json 11 0\n{\"ok\":true}";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.code, 0);
        assert_eq!(response.stream, StreamKind::Json);
        assert_eq!(response.stdout, br#"{"ok":true}"#);
        assert!(response.stderr.is_empty());
    }

    #[test]
    fn a_binary_payload_survives_untouched() {
        let png = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff];
        let mut raw = b"SCROZZ/3 0 binary 10 0\n".to_vec();
        raw.extend_from_slice(&png);
        let response = parse_response(&raw).unwrap();
        assert_eq!(response.stream, StreamKind::Binary);
        assert_eq!(response.stdout, png);
        assert!(response.stderr.is_empty());
    }

    #[test]
    fn payloads_containing_newlines_are_not_truncated_or_mixed() {
        let raw = b"SCROZZ/3 7 text 18 11\nline one\nline two\nerror\nmore\n";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.stdout, b"line one\nline two\n");
        assert_eq!(response.stderr, b"error\nmore\n");
    }

    #[test]
    fn an_empty_payload_is_valid() {
        let response = parse_response(b"SCROZZ/3 3 text 0 0\n").unwrap();
        assert_eq!(response.code, 3);
        assert!(response.stdout.is_empty());
        assert!(response.stderr.is_empty());
    }

    #[test]
    fn every_exit_code_relays_verbatim() {
        for code in crate::exit::Exit::all() {
            let raw = format!("SCROZZ/3 {} text 0 0\n", code.code());
            let response = parse_response(raw.as_bytes()).unwrap();
            assert_eq!(response.code, code.code());
        }
    }

    #[test]
    fn a_missing_header_is_an_ipc_fault_not_a_guess() {
        let err = parse_response(b"no newline here").unwrap_err();
        assert_eq!(err.exit(), crate::exit::Exit::IpcFailed);
    }

    #[test]
    fn a_foreign_protocol_version_is_named_in_the_error() {
        let err = parse_response(b"SCROZZ/1 0 json\n{}").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("SCROZZ/1"), "{message}");
        assert!(message.contains("SCROZZ/3"), "{message}");
        assert!(message.contains("different versions"), "{message}");
    }

    #[test]
    fn malformed_headers_are_rejected_one_by_one() {
        let cases: [(&[u8], &str); 9] = [
            (b"SCROZZ/3\n", "no exit code"),
            (b"SCROZZ/3 abc json 0 0\n", "malformed exit code"),
            (b"SCROZZ/3 0\n", "no stream kind"),
            (b"SCROZZ/3 0 pictures 0 0\n", "unknown stream kind"),
            (b"SCROZZ/3 0 text\n", "no stdout payload length"),
            (b"SCROZZ/3 0 text abc 0\n", "malformed stdout"),
            (b"SCROZZ/3 0 text 0 abc\n", "malformed stderr"),
            (b"SCROZZ/3 0 text 0 0 extra\n", "unexpected trailing"),
            (b"SCROZZ/3 0 text 1 0\n", "announced 1 payload bytes"),
        ];
        for (raw, expected) in cases {
            let err = parse_response(raw).unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "{:?} said {err:?}",
                std::str::from_utf8(raw)
            );
        }
    }

    #[test]
    fn an_exit_code_beyond_a_byte_is_rejected() {
        assert!(parse_response(b"SCROZZ/3 300 json 0 0\n").is_err());
    }

    #[test]
    fn an_announced_oversized_response_is_rejected_without_a_payload() {
        let raw = format!("SCROZZ/3 0 binary {} 0\n", MAX_RESPONSE_BYTES + 1);
        let error = parse_response(raw.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("response limit"), "{error}");
    }

    #[test]
    fn responses_round_trip() {
        for stream in [StreamKind::Json, StreamKind::Text, StreamKind::Binary] {
            let original = Response {
                code: 7,
                stream,
                stdout: vec![0, 1, 2, b'\n', 255],
                stderr: b"diagnostic\n".to_vec(),
            };
            let encoded = encode_response(&original);
            assert_eq!(parse_response(&encoded).unwrap(), original);
        }
    }

    #[test]
    fn stream_tokens_are_distinct_and_stable() {
        assert_eq!(StreamKind::Json.token(), "json");
        assert_eq!(StreamKind::Text.token(), "text");
        assert_eq!(StreamKind::Binary.token(), "binary");
    }

    // -- endpoint ----------------------------------------------------------

    #[test]
    fn the_endpoint_can_be_overridden() {
        let _env = crate::test_env::lock();
        crate::test_env::set(ENDPOINT_ENV, "/run/custom/scrozz.sock");
        assert_eq!(endpoint(), PathBuf::from("/run/custom/scrozz.sock"));
    }

    #[test]
    fn the_default_endpoint_is_user_scoped() {
        let path = default_endpoint().to_string_lossy().into_owned();
        assert!(
            path.contains("scrozz"),
            "the endpoint should be identifiable: {path}"
        );
    }

    #[test]
    fn the_user_token_cannot_contain_path_separators() {
        // A crafted USER must not be able to steer the socket out of its
        // directory.
        let _env = crate::test_env::lock();
        crate::test_env::set("USER", "../../etc/evil");
        let token = user_token();
        assert!(!token.contains('/'), "{token}");
        assert!(!token.contains('.'), "{token}");
    }

    #[test]
    fn probing_a_path_that_does_not_exist_reports_not_running() {
        let missing = Path::new("/nonexistent/scrozz-test/instance.sock");
        assert_eq!(probe_at(missing), Status::NotRunning);
    }

    #[test]
    fn forwarding_to_a_dead_endpoint_is_an_ipc_error() {
        let err = forward_to(
            Path::new("/nonexistent/scrozz-test/instance.sock"),
            &argv(&["capture"]),
        )
        .unwrap_err();
        assert_eq!(err.exit(), crate::exit::Exit::IpcFailed);
    }
}
