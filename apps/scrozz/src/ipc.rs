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
//! A running instance listens on a Unix-domain socket or a protected Windows
//! named pipe. A CLI invocation that would benefit from the running app's state
//! hands its whole `argv` over and relays the answer back. The forked process is
//! a thin remote control, so
//! `scrozz capture --json` produces byte-identical output whether or not the app
//! happens to be running — which is the property that makes scripting against it
//! safe.
//!
//! # The wire format, and why it is not JSON both ways
//!
//! ```text
//! -->  <u32 length><{"schema":2,"argv":["capture","--json"],"cwd":"/home/u"}\n>
//! <--  <u32 length><SCROZZ/2 0 json\n<payload bytes>>
//! -->  <u32 length><SCROZZ/2 ACK>
//! ```
//!
//! The request is one line of JSON; the response is a plain-text header followed
//! by opaque bytes. That asymmetry is deliberate:
//!
//! - The payload may be a PNG (`--stdout`). Base64-ing it through a JSON
//!   envelope would cost memory and a decoder for no benefit.
//! - The client must relay the payload **unmodified**. If it had to parse and
//!   re-serialise JSON, the forwarded output could differ from the local output
//!   in key order or float formatting, and the contract in D11 would be a lie.
//! - It needs no JSON *parser* on the client side at all, only the writer this
//!   crate already has.
//!
//! The Windows endpoint is local-only, rejects remote clients, grants access
//! only to SYSTEM and the current user SID, and is verified by the client before
//! any request bytes are sent.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use crate::{
    cli::Command,
    fault::{CliError, CliResult},
    json::Json,
};

/// The protocol version token that opens every response.
pub const PROTOCOL_TOKEN: &str = "SCROZZ/2";

/// The request schema version.
pub const REQUEST_SCHEMA: i64 = 2;

/// Overrides the endpoint, for tests and for unusual sandboxes.
pub const ENDPOINT_ENV: &str = "SCROZZ_IPC_SOCKET";

pub(crate) const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_RESPONSE_BYTES: usize = 512 * 1024 * 1024;
pub(crate) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const FRAME_PREFIX_BYTES: usize = size_of::<u32>();
const ACK: &[u8] = b"SCROZZ/2 ACK";

/// How a payload should be written to stdout once relayed.
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
    /// How to write the payload.
    pub stream: StreamKind,
    /// The payload, byte for byte as the running instance produced it.
    pub payload: Vec<u8>,
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
/// The rule: forward anything that touches **shared mutable state** — the
/// capture history, the overlay, an in-progress recording — and keep anything
/// that is a pure query or a pure function local.
#[must_use]
pub fn policy(command: &Command) -> Forwarding {
    match command {
        // A recording is a live process owned by whoever started it. Stopping it
        // from a second process is not merely preferable, it is the only thing
        // that can work — which is exactly the hotkey case on wlroots, where
        // `record --stop` is bound to a key and always arrives in a fresh
        // process.
        Command::Record(args) if args.stop => Forwarding::Require,

        // These write to the store or put an overlay on screen. Two processes
        // doing either concurrently is the bug this whole module exists to
        // prevent.
        Command::Capture(_) | Command::Record(_) => Forwarding::Prefer,
        Command::History(_) => Forwarding::Prefer,
        Command::Ocr(_) => Forwarding::Prefer,
        Command::Settings(args) if args.is_write() => Forwarding::Prefer,

        // Pure reads and pure functions. `list` asks the compositor, not Scrozz;
        // `hotkey generate-config` is string formatting; `gui` is the thing that
        // would be forwarded to.
        Command::Settings(_) | Command::List(_) | Command::Hotkey(_) | Command::Gui => {
            Forwarding::Never
        }
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
    let user = windows_pipe::current_user_sid_string().unwrap_or_else(|_| user_token());
    PathBuf::from(format!(r"\\.\pipe\scrozz-{user}"))
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
/// The full `argv` is forwarded rather than a parsed structure so the running
/// instance parses with the same code path a local run uses. A parser on each
/// side is two parsers that can disagree.
#[must_use]
pub fn encode_request(argv: &[String], cwd: Option<&Path>) -> String {
    let request = Json::obj([
        ("schema", Json::Int(REQUEST_SCHEMA)),
        (
            "argv",
            Json::arr(argv.iter().map(|a| Json::str(a.as_str()))),
        ),
        // Relative `--output` paths resolve against the *caller's* directory,
        // not the daemon's. Without this, `scrozz capture -o shot.png` would
        // silently write somewhere else once the GUI is running.
        (
            "cwd",
            Json::opt(cwd, |p| Json::str(p.to_string_lossy().into_owned())),
        ),
    ]);
    format!("{}\n", request.to_compact_string())
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
    let split = bytes
        .iter()
        .position(|b| *b == b'\n')
        .ok_or_else(|| CliError::ipc("the running instance sent no response header"))?;

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

    Ok(Response {
        code,
        stream,
        payload: bytes[split + 1..].to_vec(),
    })
}

/// Encodes a response. Used by the server side, and by the round-trip tests.
#[must_use]
pub fn encode_response(response: &Response) -> Vec<u8> {
    let mut out = format!(
        "{PROTOCOL_TOKEN} {} {}\n",
        response.code,
        response.stream.token()
    )
    .into_bytes();
    out.extend_from_slice(&response.payload);
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
    use std::os::unix::net::UnixStream;

    if !path.exists() {
        return Status::NotRunning;
    }
    match UnixStream::connect(path) {
        Ok(_) => Status::Running,
        // A socket file with nothing behind it is the normal residue of a crash,
        // not a condition worth telling the user about.
        Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused) => Status::NotRunning,
        Err(e) => Status::Unusable(e.to_string()),
    }
}

#[cfg(windows)]
fn probe_at(path: &Path) -> Status {
    match windows_pipe::PipeStream::connect(path, std::time::Duration::from_millis(1_500)) {
        Ok(_) => Status::Running,
        Err(windows_pipe::ConnectError::NotRunning) => Status::NotRunning,
        Err(windows_pipe::ConnectError::Unusable(error)) => Status::Unusable(error),
    }
}

#[cfg(not(any(unix, windows)))]
fn probe_at(_path: &Path) -> Status {
    Status::NotRunning
}

/// Hands a command to a running instance.
///
/// # Errors
///
/// Returns [`CliError::Ipc`] when no instance is reachable or the exchange
/// fails. Once an endpoint has accepted a connection, callers surface transport
/// failures rather than retrying locally and risking duplicate side effects.
pub fn forward(argv: &[String]) -> CliResult<Response> {
    forward_to(&endpoint(), argv)
}

#[cfg(unix)]
fn forward_to(path: &Path, argv: &[String]) -> CliResult<Response> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(path).map_err(|e| {
        CliError::ipc(format!(
            "could not reach the running Scrozz at {}: {e}",
            path.display()
        ))
    })?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(100)))
        .and_then(|()| stream.set_write_timeout(Some(std::time::Duration::from_millis(100))))
        .map_err(|error| CliError::ipc(format!("could not bound the IPC socket: {error}")))?;
    exchange(&mut stream, argv)
}

#[cfg(windows)]
fn forward_to(path: &Path, argv: &[String]) -> CliResult<Response> {
    let mut stream =
        windows_pipe::PipeStream::connect(path, std::time::Duration::from_millis(1_500)).map_err(
            |error| match error {
                windows_pipe::ConnectError::NotRunning => {
                    CliError::ipc("no running Scrozz named-pipe server was found")
                }
                windows_pipe::ConnectError::Unusable(error) => CliError::ipc(error),
            },
        )?;
    exchange(&mut stream, argv)
}

#[cfg(not(any(unix, windows)))]
fn forward_to(_path: &Path, _argv: &[String]) -> CliResult<Response> {
    Err(CliError::ipc(
        "single-instance forwarding is not supported on this platform",
    ))
}

fn exchange(
    stream: &mut (impl std::io::Read + std::io::Write),
    argv: &[String],
) -> CliResult<Response> {
    let request = encode_request(argv, std::env::current_dir().ok().as_deref());
    send_frame(
        stream,
        request.as_bytes(),
        MAX_REQUEST_BYTES,
        "request",
        Instant::now() + TRANSFER_TIMEOUT,
        None,
    )?;
    let bytes = read_frame(
        stream,
        MAX_RESPONSE_BYTES,
        "response",
        Instant::now() + COMMAND_TIMEOUT,
        None,
    )?;
    let response = parse_response(&bytes)?;
    send_frame(
        stream,
        ACK,
        ACK.len(),
        "acknowledgement",
        Instant::now() + TRANSFER_TIMEOUT,
        None,
    )?;
    Ok(response)
}

fn send_frame(
    output: &mut impl std::io::Write,
    payload: &[u8],
    maximum: usize,
    name: &str,
    deadline: Instant,
    shutdown: Option<&AtomicBool>,
) -> CliResult<()> {
    if payload.len() > maximum {
        return Err(CliError::ipc(format!(
            "the IPC {name} is {} bytes; the limit is {maximum}",
            payload.len()
        )));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| CliError::ipc(format!("the IPC {name} is too large to frame")))?;
    write_all_until(output, &length.to_le_bytes(), name, deadline, shutdown)?;
    write_all_until(output, payload, name, deadline, shutdown)?;
    output
        .flush()
        .map_err(|error| CliError::ipc(format!("could not flush the IPC {name}: {error}")))
}

fn write_all_until(
    output: &mut impl std::io::Write,
    mut bytes: &[u8],
    name: &str,
    deadline: Instant,
    shutdown: Option<&AtomicBool>,
) -> CliResult<()> {
    while !bytes.is_empty() {
        if shutdown.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(CliError::ipc(format!(
                "stopped while writing the IPC {name}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(CliError::ipc(format!(
                "timed out while writing the IPC {name}"
            )));
        }
        match output.write(bytes) {
            Ok(0) => {
                return Err(CliError::ipc(format!(
                    "the IPC {name} writer stopped making progress"
                )));
            }
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => {
                return Err(CliError::ipc(format!(
                    "could not write the IPC {name}: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn read_frame(
    input: &mut impl std::io::Read,
    maximum: usize,
    name: &str,
    deadline: Instant,
    shutdown: Option<&AtomicBool>,
) -> CliResult<Vec<u8>> {
    let mut prefix = [0_u8; FRAME_PREFIX_BYTES];
    read_exact_until(input, &mut prefix, name, deadline, shutdown)?;
    let length = u32::from_le_bytes(prefix) as usize;
    if length > maximum {
        return Err(CliError::ipc(format!(
            "the IPC {name} announced {length} bytes; the limit is {maximum}"
        )));
    }
    let mut payload = vec![0_u8; length];
    read_exact_until(input, &mut payload, name, deadline, shutdown)?;
    Ok(payload)
}

fn read_exact_until(
    input: &mut impl std::io::Read,
    mut destination: &mut [u8],
    name: &str,
    deadline: Instant,
    shutdown: Option<&AtomicBool>,
) -> CliResult<()> {
    while !destination.is_empty() {
        if shutdown.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(CliError::ipc(format!(
                "stopped while reading the IPC {name}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(CliError::ipc(format!(
                "timed out while reading the IPC {name}"
            )));
        }
        match input.read(destination) {
            Ok(0) => {
                return Err(CliError::ipc(format!(
                    "the IPC {name} ended before the announced length"
                )));
            }
            Ok(read) => destination = &mut destination[read..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => {
                return Err(CliError::ipc(format!(
                    "could not read the IPC {name}: {error}"
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn receive_request_frame(
    input: &mut impl std::io::Read,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> CliResult<Vec<u8>> {
    read_frame(
        input,
        MAX_REQUEST_BYTES,
        "request",
        deadline,
        Some(shutdown),
    )
}

pub(crate) fn send_response_frame(
    output: &mut impl std::io::Write,
    response: &Response,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> CliResult<()> {
    let bytes = encode_response(response);
    send_frame(
        output,
        &bytes,
        MAX_RESPONSE_BYTES,
        "response",
        deadline,
        Some(shutdown),
    )
}

pub(crate) fn receive_ack(
    input: &mut impl std::io::Read,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> CliResult<()> {
    let ack = read_frame(
        input,
        ACK.len(),
        "acknowledgement",
        deadline,
        Some(shutdown),
    )?;
    if ack != ACK {
        return Err(CliError::ipc(
            "the IPC client sent an invalid response acknowledgement",
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) mod windows_pipe {
    use std::{
        ffi::OsStr,
        io::{self, Read, Write},
        os::windows::ffi::OsStrExt as _,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use windows::{
        Win32::{
            Foundation::{
                CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_NO_DATA,
                ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING,
                ERROR_PIPE_NOT_CONNECTED, ERROR_SUCCESS, GetLastError, HANDLE, HLOCAL, LocalFree,
            },
            Security::{
                Authorization::{
                    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                    GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
                },
                GetTokenInformation, IsValidSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
                PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
            },
            Storage::FileSystem::{
                CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_MODE,
                OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL, ReadFile, SECURITY_IDENTIFICATION,
                SECURITY_SQOS_PRESENT, WriteFile,
            },
            System::{
                Pipes::{
                    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_NOWAIT,
                    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
                    SetNamedPipeHandleState, WaitNamedPipeW,
                },
                Threading::{GetCurrentProcess, OpenProcessToken},
            },
        },
        core::{PCWSTR, PWSTR},
    };

    use crate::fault::{CliError, CliResult};

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
    const RETRY_DELAY: Duration = Duration::from_millis(2);

    #[derive(Debug)]
    pub enum ConnectError {
        NotRunning,
        Unusable(String),
    }

    struct OwnedHandle(HANDLE);

    unsafe impl Send for OwnedHandle {}

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }

    pub struct PipeListener {
        handle: OwnedHandle,
    }

    impl PipeListener {
        pub fn bind(path: &Path) -> CliResult<Self> {
            let name = pipe_name(path)?;
            let sid = current_user_sid_string()?;
            let descriptor = security_descriptor(&sid)?;
            let attributes = SECURITY_ATTRIBUTES {
                nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                    .map_err(|_| CliError::ipc("Windows security attributes are too large"))?,
                lpSecurityDescriptor: descriptor.0.0,
                bInheritHandle: false.into(),
            };
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    PIPE_BUFFER_BYTES,
                    PIPE_BUFFER_BYTES,
                    5_000,
                    Some(&raw const attributes),
                )
            };
            if handle.is_invalid() {
                return Err(CliError::ipc(format!(
                    "could not bind protected named pipe {}: {}",
                    path.display(),
                    io::Error::from_raw_os_error(last_error())
                )));
            }
            Ok(Self {
                handle: OwnedHandle(handle),
            })
        }

        pub fn accept(&mut self, shutdown: &Arc<AtomicBool>) -> CliResult<()> {
            loop {
                if shutdown.load(Ordering::Acquire) {
                    return Err(CliError::ipc("the IPC server is shutting down"));
                }
                match unsafe { ConnectNamedPipe(self.handle.0, None) } {
                    Ok(()) => {
                        // In non-blocking mode this means the pipe became
                        // available; ERROR_PIPE_CONNECTED proves attachment.
                        std::thread::sleep(RETRY_DELAY);
                    }
                    Err(_) => match unsafe { GetLastError() } {
                        ERROR_PIPE_CONNECTED => return Ok(()),
                        ERROR_PIPE_LISTENING => std::thread::sleep(RETRY_DELAY),
                        ERROR_NO_DATA => {
                            let _ = unsafe { DisconnectNamedPipe(self.handle.0) };
                            std::thread::sleep(RETRY_DELAY);
                        }
                        error => {
                            return Err(CliError::ipc(format!(
                                "could not accept a named-pipe client: {}",
                                io::Error::from_raw_os_error(error.0 as i32)
                            )));
                        }
                    },
                }
            }
        }

        pub fn disconnect(&mut self) {
            let _ = unsafe { DisconnectNamedPipe(self.handle.0) };
        }
    }

    impl Read for PipeListener {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            read_handle(self.handle.0, buffer)
        }
    }

    impl Write for PipeListener {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            write_handle(self.handle.0, buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub struct PipeStream {
        handle: OwnedHandle,
    }

    impl PipeStream {
        pub fn connect(path: &Path, timeout: Duration) -> Result<Self, ConnectError> {
            let name =
                pipe_name(path).map_err(|error| ConnectError::Unusable(error.to_string()))?;
            let deadline = Instant::now() + timeout;
            loop {
                let result = unsafe {
                    CreateFileW(
                        PCWSTR(name.as_ptr()),
                        GENERIC_READ | GENERIC_WRITE | READ_CONTROL.0,
                        FILE_SHARE_MODE(0),
                        None,
                        OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                        None,
                    )
                };
                match result {
                    Ok(handle) => {
                        let stream = Self {
                            handle: OwnedHandle(handle),
                        };
                        verify_pipe_owner(stream.handle.0)
                            .map_err(|error| ConnectError::Unusable(error.to_string()))?;
                        let mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
                        if let Err(error) = unsafe {
                            SetNamedPipeHandleState(
                                stream.handle.0,
                                Some(&raw const mode),
                                None,
                                None,
                            )
                        } {
                            return Err(ConnectError::Unusable(format!(
                                "could not configure the Scrozz named pipe: {error}"
                            )));
                        }
                        return Ok(stream);
                    }
                    Err(_) => match unsafe { GetLastError() } {
                        ERROR_FILE_NOT_FOUND => return Err(ConnectError::NotRunning),
                        ERROR_PIPE_BUSY if Instant::now() < deadline => {
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            let milliseconds = u32::try_from(remaining.as_millis())
                                .unwrap_or(u32::MAX)
                                .max(1);
                            if !unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), milliseconds) }
                                .as_bool()
                                && Instant::now() >= deadline
                            {
                                return Err(ConnectError::Unusable(
                                    "timed out waiting for the Scrozz named pipe".to_owned(),
                                ));
                            }
                        }
                        ERROR_PIPE_BUSY => {
                            return Err(ConnectError::Unusable(
                                "timed out waiting for the Scrozz named pipe".to_owned(),
                            ));
                        }
                        error => {
                            return Err(ConnectError::Unusable(format!(
                                "could not open {}: {}",
                                path.display(),
                                io::Error::from_raw_os_error(error.0 as i32)
                            )));
                        }
                    },
                }
            }
        }
    }

    impl Read for PipeStream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            read_handle(self.handle.0, buffer)
        }
    }

    impl Write for PipeStream {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            write_handle(self.handle.0, buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn verify_pipe_owner(handle: HANDLE) -> CliResult<()> {
        let expected = current_user_sid_string()?;
        let mut owner = PSID(std::ptr::null_mut());
        let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(&raw mut owner),
                None,
                None,
                None,
                Some(&raw mut descriptor),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(CliError::ipc(format!(
                "could not read the named-pipe owner: {}",
                io::Error::from_raw_os_error(status.0 as i32)
            )));
        }
        if descriptor.0.is_null() {
            return Err(CliError::ipc(
                "Windows returned no named-pipe security descriptor",
            ));
        }
        let _descriptor = SecurityDescriptor(descriptor);
        let actual = sid_string(owner, "named-pipe owner")?;
        if actual != expected {
            return Err(CliError::ipc(format!(
                "refusing named pipe owned by {actual}; expected current user {expected}"
            )));
        }
        Ok(())
    }

    fn read_handle(handle: HANDLE, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let limit = buffer.len().min(PIPE_BUFFER_BYTES as usize);
        let mut read = 0_u32;
        match unsafe { ReadFile(handle, Some(&mut buffer[..limit]), Some(&mut read), None) } {
            Ok(()) => Ok(read as usize),
            Err(_) => match unsafe { GetLastError() } {
                ERROR_NO_DATA | ERROR_PIPE_LISTENING => {
                    Err(io::Error::from(io::ErrorKind::WouldBlock))
                }
                ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED => Ok(0),
                error => Err(io::Error::from_raw_os_error(error.0 as i32)),
            },
        }
    }

    fn write_handle(handle: HANDLE, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let limit = buffer.len().min(PIPE_BUFFER_BYTES as usize);
        let mut written = 0_u32;
        match unsafe { WriteFile(handle, Some(&buffer[..limit]), Some(&mut written), None) } {
            Ok(()) if written == 0 => Err(io::Error::from(io::ErrorKind::WouldBlock)),
            Ok(()) => Ok(written as usize),
            Err(_) => match unsafe { GetLastError() } {
                ERROR_NO_DATA | ERROR_PIPE_BUSY => Err(io::Error::from(io::ErrorKind::WouldBlock)),
                ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED => {
                    Err(io::Error::from(io::ErrorKind::BrokenPipe))
                }
                error => Err(io::Error::from_raw_os_error(error.0 as i32)),
            },
        }
    }

    fn pipe_name(path: &Path) -> CliResult<Vec<u16>> {
        let name = path.to_string_lossy();
        if !name.starts_with(r"\\.\pipe\")
            || name.len() > 256
            || name.contains('\0')
            || name[r"\\.\pipe\".len()..].is_empty()
        {
            return Err(CliError::ipc(format!(
                "{} is not a valid local Windows named-pipe path",
                path.display()
            )));
        }
        Ok(OsStr::new(name.as_ref())
            .encode_wide()
            .chain(Some(0))
            .collect())
    }

    pub(super) fn current_user_sid_string() -> CliResult<String> {
        let mut token = HANDLE(std::ptr::null_mut());
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .map_err(|error| CliError::ipc(format!("could not open the user token: {error}")))?;
        let token = OwnedHandle(token);

        let mut required = 0_u32;
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut required) };
        if required == 0 {
            return Err(CliError::ipc(
                "Windows did not report the current user SID size",
            ));
        }
        let word = size_of::<usize>();
        let words = (required as usize).div_ceil(word);
        let mut storage = vec![0_usize; words];
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(storage.as_mut_ptr().cast()),
                required,
                &mut required,
            )
        }
        .map_err(|error| CliError::ipc(format!("could not read the current user SID: {error}")))?;
        let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
        sid_string(user.User.Sid, "current user")
    }

    fn sid_string(sid: PSID, context: &str) -> CliResult<String> {
        if sid.0.is_null() || !unsafe { IsValidSid(sid) }.as_bool() {
            return Err(CliError::ipc(format!(
                "Windows returned an invalid {context} SID"
            )));
        }
        let mut string = PWSTR::null();
        unsafe { ConvertSidToStringSidW(sid, &mut string) }.map_err(|error| {
            CliError::ipc(format!("could not format the {context} SID: {error}"))
        })?;
        let converted = unsafe { string.to_string() };
        let _ = unsafe { LocalFree(Some(HLOCAL(string.0.cast()))) };
        converted.map_err(|error| CliError::ipc(format!("the {context} SID was invalid: {error}")))
    }

    fn security_descriptor(sid: &str) -> CliResult<SecurityDescriptor> {
        let sddl = format!("O:{sid}D:P(A;;GA;;;SY)(A;;GA;;;{sid})");
        let wide: Vec<u16> = OsStr::new(&sddl).encode_wide().chain(Some(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|error| {
            CliError::ipc(format!(
                "could not create the user-only named-pipe ACL: {error}"
            ))
        })?;
        Ok(SecurityDescriptor(descriptor))
    }

    fn last_error() -> i32 {
        unsafe { GetLastError() }.0 as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;
    use std::io::Cursor;

    fn command_of(argv: &[&str]) -> Command {
        Cli::try_parse_from(argv).unwrap().command.unwrap()
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
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
    fn starting_a_recording_prefers_the_running_instance() {
        assert_eq!(
            policy(&command_of(&["scrozz", "record"])),
            Forwarding::Prefer
        );
    }

    #[test]
    fn captures_prefer_the_running_instance() {
        assert_eq!(
            policy(&command_of(&["scrozz", "capture"])),
            Forwarding::Prefer
        );
        assert_eq!(
            policy(&command_of(&["scrozz", "capture", "--region", "0,0,10,10"])),
            Forwarding::Prefer
        );
    }

    #[test]
    fn history_prefers_the_running_instance_because_of_the_store() {
        for args in [
            vec!["scrozz", "history", "list"],
            vec!["scrozz", "history", "delete", "abc"],
            vec!["scrozz", "history", "pin", "abc"],
        ] {
            assert_eq!(policy(&command_of(&args)), Forwarding::Prefer, "{args:?}");
        }
    }

    #[test]
    fn writing_a_setting_forwards_but_reading_one_does_not() {
        assert_eq!(
            policy(&command_of(&[
                "scrozz",
                "settings",
                "set",
                "capture.format",
                "png"
            ])),
            Forwarding::Prefer
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
        assert_eq!(
            request,
            "{\"schema\":2,\"argv\":[\"capture\",\"--json\"],\"cwd\":\"/home/u\"}\n"
        );
    }

    #[test]
    fn a_missing_working_directory_is_null_not_absent() {
        let request = encode_request(&argv(&["capture"]), None);
        assert!(request.contains(r#""cwd":null"#));
    }

    #[test]
    fn arguments_containing_quotes_survive_encoding() {
        let request = encode_request(&argv(&["capture", "--window", r#"He said "hi""#]), None);
        assert!(request.contains(r#"He said \"hi\""#));
        assert_eq!(request.matches('\n').count(), 1);
    }

    #[test]
    fn framed_transfers_handle_short_reads_and_writes() {
        struct ShortWriter(Vec<u8>);

        impl std::io::Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let count = bytes.len().min(3);
                self.0.extend_from_slice(&bytes[..count]);
                Ok(count)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let payload = encode_request(&argv(&["capture"]), None);
        let mut writer = ShortWriter(Vec::new());
        send_frame(
            &mut writer,
            payload.as_bytes(),
            MAX_REQUEST_BYTES,
            "request",
            Instant::now() + TRANSFER_TIMEOUT,
            None,
        )
        .expect("short writes complete");
        let decoded = read_frame(
            &mut Cursor::new(writer.0),
            MAX_REQUEST_BYTES,
            "request",
            Instant::now() + TRANSFER_TIMEOUT,
            None,
        )
        .expect("short frame reads complete");
        assert_eq!(decoded, payload.as_bytes());
    }

    #[test]
    fn oversized_frames_are_rejected_before_payload_allocation() {
        let announced = u32::try_from(MAX_REQUEST_BYTES + 1).expect("bounded");
        let error = read_frame(
            &mut Cursor::new(announced.to_le_bytes()),
            MAX_REQUEST_BYTES,
            "request",
            Instant::now() + TRANSFER_TIMEOUT,
            None,
        )
        .expect_err("oversized frame");
        assert!(error.to_string().contains("limit"), "{error}");
    }

    #[test]
    fn response_acknowledgements_are_exact_and_bounded() {
        let stop = AtomicBool::new(false);
        let mut frame = Vec::new();
        send_frame(
            &mut frame,
            ACK,
            ACK.len(),
            "acknowledgement",
            Instant::now() + TRANSFER_TIMEOUT,
            None,
        )
        .expect("valid ack frame");
        receive_ack(
            &mut Cursor::new(frame),
            Instant::now() + TRANSFER_TIMEOUT,
            &stop,
        )
        .expect("valid acknowledgement");

        let mut invalid = Vec::new();
        send_frame(
            &mut invalid,
            b"SCROZZ/2 NAK",
            ACK.len(),
            "acknowledgement",
            Instant::now() + TRANSFER_TIMEOUT,
            None,
        )
        .expect("invalid ack frame is still framed");
        assert!(
            receive_ack(
                &mut Cursor::new(invalid),
                Instant::now() + TRANSFER_TIMEOUT,
                &stop,
            )
            .is_err()
        );
    }

    #[test]
    fn an_argument_containing_a_newline_cannot_split_the_request() {
        // A window title really can contain a newline; if it broke the framing
        // it would be a remote-command-injection bug against the daemon.
        let request = encode_request(&argv(&["capture", "--window", "a\nb"]), None);
        assert_eq!(request.matches('\n').count(), 1);
        assert!(request.contains(r"a\nb"));
    }

    // -- response parsing --------------------------------------------------

    #[test]
    fn a_well_formed_response_parses() {
        let raw = b"SCROZZ/2 0 json\n{\"ok\":true}";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.code, 0);
        assert_eq!(response.stream, StreamKind::Json);
        assert_eq!(response.payload, br#"{"ok":true}"#);
    }

    #[test]
    fn a_binary_payload_survives_untouched() {
        let png = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff];
        let mut raw = b"SCROZZ/2 0 binary\n".to_vec();
        raw.extend_from_slice(&png);
        let response = parse_response(&raw).unwrap();
        assert_eq!(response.stream, StreamKind::Binary);
        assert_eq!(response.payload, png);
    }

    #[test]
    fn a_payload_containing_newlines_is_not_truncated() {
        let raw = b"SCROZZ/2 0 text\nline one\nline two\n";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.payload, b"line one\nline two\n");
    }

    #[test]
    fn an_empty_payload_is_valid() {
        let response = parse_response(b"SCROZZ/2 3 text\n").unwrap();
        assert_eq!(response.code, 3);
        assert!(response.payload.is_empty());
    }

    #[test]
    fn every_exit_code_relays_verbatim() {
        for code in crate::exit::Exit::all() {
            let raw = format!("SCROZZ/2 {} text\n", code.code());
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
        assert!(message.contains("SCROZZ/2"), "{message}");
        assert!(message.contains("different versions"), "{message}");
    }

    #[test]
    fn malformed_headers_are_rejected_one_by_one() {
        let cases: [(&[u8], &str); 4] = [
            (b"SCROZZ/2\n", "no exit code"),
            (b"SCROZZ/2 abc json\n", "malformed exit code"),
            (b"SCROZZ/2 0\n", "no stream kind"),
            (b"SCROZZ/2 0 pictures\n", "unknown stream kind"),
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
        assert!(parse_response(b"SCROZZ/2 300 json\n").is_err());
    }

    #[test]
    fn responses_round_trip() {
        for stream in [StreamKind::Json, StreamKind::Text, StreamKind::Binary] {
            let original = Response {
                code: 7,
                stream,
                payload: vec![0, 1, 2, b'\n', 255],
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
