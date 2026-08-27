//! Bounded single-instance IPC shared by the CLI and the running GUI.
//!
//! Requests and responses use versioned, length-prefixed frames. Responses carry
//! stdout and stderr separately so forwarding preserves the exact local process
//! contract, including raw output, guidance, cancellation, and exit status.

use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    cli::{Command, SystemCommand},
    fault::{CliError, CliResult},
    url::UrlAction,
};

/// The protocol version token embedded in every response payload.
pub const PROTOCOL_TOKEN: &str = "SCROZZ/2";
/// The request schema version.
pub const REQUEST_SCHEMA: i64 = 2;
/// Overrides the endpoint, for tests and unusual sandboxes.
pub const ENDPOINT_ENV: &str = "SCROZZ_IPC_SOCKET";

pub(crate) const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_RESPONSE_BYTES: usize = 512 * 1024 * 1024;
pub(crate) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1_500);
const RETRY_DELAY: Duration = Duration::from_millis(2);
const FRAME_PREFIX_BYTES: usize = size_of::<u32>();
const RESPONSE_HEADER_BYTES: usize = PROTOCOL_TOKEN.len() + 1 + size_of::<u32>() * 2;
const ACK: &[u8] = b"SCROZZ/2 ACK";

/// A relayed process result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The exit code produced by the running instance.
    pub code: u8,
    /// Bytes written to stdout.
    pub stdout: Vec<u8>,
    /// Bytes written to stderr.
    pub stderr: Vec<u8>,
}

/// One validated request decoded from the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedRequest {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    schema: i64,
    argv: Vec<String>,
    cwd: Option<String>,
}

/// Whether an instance is reachable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The endpoint accepted a connection.
    Running,
    /// No instance is listening.
    NotRunning,
    /// The endpoint exists but cannot safely be used.
    Unusable(String),
}

/// Whether a command should be handed to a running instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forwarding {
    /// Forward when an instance is running; do it locally otherwise.
    Prefer,
    /// Forward or fail.
    Require,
    /// Always local.
    Never,
}

/// The forwarding policy for a command.
#[must_use]
pub fn policy(command: &Command) -> Forwarding {
    match command {
        Command::Record(args) if args.stop => Forwarding::Require,
        Command::Capture(_) | Command::Record(_) => Forwarding::Prefer,
        Command::History(_) | Command::Ocr(_) => Forwarding::Prefer,
        Command::Settings(args) if args.is_write() => Forwarding::Prefer,
        Command::Url(args) if args.writes_settings() => Forwarding::Prefer,
        Command::System(args) if matches!(&args.command, SystemCommand::Notify { .. }) => {
            Forwarding::Prefer
        }
        Command::Settings(_)
        | Command::List(_)
        | Command::Hotkey(_)
        | Command::Autostart(_)
        | Command::Url(_)
        | Command::Update(_)
        | Command::System(_)
        | Command::Gui => Forwarding::Never,
    }
}

/// The per-user IPC endpoint.
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

fn user_token() -> String {
    for key in ["USER", "LOGNAME", "USERNAME"] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            let token: String = value
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !token.is_empty() {
                return token;
            }
        }
    }
    "default".to_owned()
}

/// Encodes one complete request frame.
pub fn encode_request(argv: &[String], cwd: Option<&Path>) -> CliResult<Vec<u8>> {
    let payload = serde_json::to_vec(&WireRequest {
        schema: REQUEST_SCHEMA,
        argv: argv.to_vec(),
        cwd: cwd.map(|path| path.to_string_lossy().into_owned()),
    })
    .map_err(|e| CliError::ipc(format!("could not encode the IPC request: {e}")))?;
    encode_frame(&payload, MAX_REQUEST_BYTES, "request")
}

fn decode_request_payload(payload: &[u8]) -> CliResult<DecodedRequest> {
    let request: WireRequest = serde_json::from_slice(payload)
        .map_err(|e| CliError::ipc(format!("the IPC request was not valid JSON: {e}")))?;
    if request.schema != REQUEST_SCHEMA {
        return Err(CliError::ipc(format!(
            "the caller uses request schema {}, this build uses {REQUEST_SCHEMA}",
            request.schema
        )));
    }
    if request.argv.len() > 4_096 {
        return Err(CliError::ipc("the IPC request carried too many arguments"));
    }
    if request.argv.iter().any(|argument| argument.contains('\0')) {
        return Err(CliError::ipc("an IPC argument contained a NUL byte"));
    }
    if request
        .cwd
        .as_deref()
        .is_some_and(|cwd| cwd.len() > 32 * 1024 || cwd.contains('\0'))
    {
        return Err(CliError::ipc(
            "the IPC working directory was malformed or too long",
        ));
    }

    Ok(DecodedRequest {
        argv: request.argv,
        cwd: request.cwd.map(PathBuf::from),
    })
}

/// Encodes one complete response frame.
pub fn encode_response(response: &Response) -> CliResult<Vec<u8>> {
    let output_bytes = response
        .stdout
        .len()
        .checked_add(response.stderr.len())
        .ok_or_else(|| CliError::ipc("the IPC response length overflowed"))?;
    if output_bytes > MAX_RESPONSE_BYTES {
        return Err(CliError::ipc(format!(
            "the IPC response is {output_bytes} bytes; the limit is {MAX_RESPONSE_BYTES}"
        )));
    }

    let stdout_len = u32::try_from(response.stdout.len())
        .map_err(|_| CliError::ipc("stdout is too large for the IPC protocol"))?;
    let stderr_len = u32::try_from(response.stderr.len())
        .map_err(|_| CliError::ipc("stderr is too large for the IPC protocol"))?;
    let mut payload = Vec::with_capacity(RESPONSE_HEADER_BYTES + output_bytes);
    payload.extend_from_slice(PROTOCOL_TOKEN.as_bytes());
    payload.push(response.code);
    payload.extend_from_slice(&stdout_len.to_le_bytes());
    payload.extend_from_slice(&stderr_len.to_le_bytes());
    payload.extend_from_slice(&response.stdout);
    payload.extend_from_slice(&response.stderr);
    encode_frame(
        &payload,
        MAX_RESPONSE_BYTES + RESPONSE_HEADER_BYTES,
        "response",
    )
}

/// Parses one complete response frame.
pub fn parse_response(frame: &[u8]) -> CliResult<Response> {
    let payload = decode_frame(
        frame,
        MAX_RESPONSE_BYTES + RESPONSE_HEADER_BYTES,
        "response",
    )?;
    decode_response_payload(payload)
}

fn decode_response_payload(payload: &[u8]) -> CliResult<Response> {
    if payload.len() < RESPONSE_HEADER_BYTES {
        return Err(CliError::ipc("the IPC response header was truncated"));
    }
    let (token, rest) = payload.split_at(PROTOCOL_TOKEN.len());
    if token != PROTOCOL_TOKEN.as_bytes() {
        return Err(CliError::ipc(format!(
            "the running instance speaks {:?}, this build speaks {PROTOCOL_TOKEN:?}; \
             the two are different versions of Scrozz",
            String::from_utf8_lossy(token)
        )));
    }

    let code = rest[0];
    let stdout_len = u32::from_le_bytes(
        rest[1..5]
            .try_into()
            .map_err(|_| CliError::ipc("the stdout length was truncated"))?,
    ) as usize;
    let stderr_len = u32::from_le_bytes(
        rest[5..9]
            .try_into()
            .map_err(|_| CliError::ipc("the stderr length was truncated"))?,
    ) as usize;
    let expected = RESPONSE_HEADER_BYTES
        .checked_add(stdout_len)
        .and_then(|size| size.checked_add(stderr_len))
        .ok_or_else(|| CliError::ipc("the IPC response lengths overflowed"))?;
    if expected != payload.len() {
        return Err(CliError::ipc(format!(
            "the IPC response announced {expected} bytes but carried {}",
            payload.len()
        )));
    }
    let output_bytes = stdout_len
        .checked_add(stderr_len)
        .ok_or_else(|| CliError::ipc("the IPC response output lengths overflowed"))?;
    if output_bytes > MAX_RESPONSE_BYTES {
        return Err(CliError::ipc("the IPC response exceeded the output limit"));
    }

    let output = &payload[RESPONSE_HEADER_BYTES..];
    Ok(Response {
        code,
        stdout: output[..stdout_len].to_vec(),
        stderr: output[stdout_len..].to_vec(),
    })
}

fn encode_frame(payload: &[u8], maximum: usize, name: &str) -> CliResult<Vec<u8>> {
    if payload.len() > maximum {
        return Err(CliError::ipc(format!(
            "the IPC {name} is {} bytes; the limit is {maximum}",
            payload.len()
        )));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| CliError::ipc(format!("the IPC {name} is too large to frame")))?;
    let mut frame = Vec::with_capacity(FRAME_PREFIX_BYTES + payload.len());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

fn decode_frame<'a>(frame: &'a [u8], maximum: usize, name: &str) -> CliResult<&'a [u8]> {
    let prefix: [u8; FRAME_PREFIX_BYTES] = frame
        .get(..FRAME_PREFIX_BYTES)
        .ok_or_else(|| CliError::ipc(format!("the IPC {name} frame was truncated")))?
        .try_into()
        .map_err(|_| CliError::ipc(format!("the IPC {name} length was malformed")))?;
    let length = u32::from_le_bytes(prefix) as usize;
    if length > maximum {
        return Err(CliError::ipc(format!(
            "the IPC {name} announced {length} bytes; the limit is {maximum}"
        )));
    }
    if frame.len() != FRAME_PREFIX_BYTES + length {
        return Err(CliError::ipc(format!(
            "the IPC {name} announced {length} bytes but its frame carried {}",
            frame.len().saturating_sub(FRAME_PREFIX_BYTES)
        )));
    }
    Ok(&frame[FRAME_PREFIX_BYTES..])
}

pub(crate) fn receive_request(
    input: &mut impl Read,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> CliResult<DecodedRequest> {
    let payload = read_frame(
        input,
        MAX_REQUEST_BYTES,
        "request",
        deadline,
        Some(shutdown),
    )?;
    decode_request_payload(&payload)
}

pub(crate) fn send_response(
    output: &mut impl Write,
    response: &Response,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> CliResult<()> {
    let frame = encode_response(response)?;
    write_all_until(output, &frame, "response", deadline, Some(shutdown))
}

pub(crate) fn receive_ack(
    input: &mut impl Read,
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

fn send_ack(output: &mut impl Write, deadline: Instant) -> CliResult<()> {
    let frame = encode_frame(ACK, ACK.len(), "acknowledgement")?;
    write_all_until(output, &frame, "acknowledgement", deadline, None)
}

fn read_response(input: &mut impl Read, deadline: Instant) -> CliResult<Response> {
    let payload = read_frame(
        input,
        MAX_RESPONSE_BYTES + RESPONSE_HEADER_BYTES,
        "response",
        deadline,
        None,
    )?;
    decode_response_payload(&payload)
}

fn read_frame(
    input: &mut impl Read,
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
    input: &mut impl Read,
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
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                std::thread::sleep(RETRY_DELAY);
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

fn write_all_until(
    output: &mut impl Write,
    mut source: &[u8],
    name: &str,
    deadline: Instant,
    shutdown: Option<&AtomicBool>,
) -> CliResult<()> {
    while !source.is_empty() {
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
        match output.write(source) {
            Ok(0) => {
                return Err(CliError::ipc(format!(
                    "the IPC {name} writer stopped making progress"
                )));
            }
            Ok(written) => source = &source[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                std::thread::sleep(RETRY_DELAY);
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

/// Whether an instance is listening.
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
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Status::NotRunning
        }
        Err(error) => Status::Unusable(error.to_string()),
    }
}

#[cfg(windows)]
fn probe_at(path: &Path) -> Status {
    match windows_pipe::PipeStream::connect(path, CONNECT_TIMEOUT) {
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
pub fn forward(argv: &[String]) -> CliResult<Response> {
    forward_to(&endpoint(), argv)
}

/// Hands one allow-listed URL action to a running instance.
pub fn forward_url(action: UrlAction) -> CliResult<Response> {
    forward(&url_arguments(action))
}

fn url_arguments(action: UrlAction) -> Vec<String> {
    action
        .arguments()
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect()
}

fn exchange(stream: &mut (impl Read + Write), argv: &[String]) -> CliResult<Response> {
    let cwd = std::env::current_dir().ok();
    let request = encode_request(argv, cwd.as_deref())?;
    write_all_until(
        stream,
        &request,
        "request",
        Instant::now() + TRANSFER_TIMEOUT,
        None,
    )?;
    let response = read_response(stream, Instant::now() + COMMAND_TIMEOUT)?;
    send_ack(stream, Instant::now() + TRANSFER_TIMEOUT)?;
    Ok(response)
}

#[cfg(unix)]
fn forward_to(path: &Path, argv: &[String]) -> CliResult<Response> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(path).map_err(|error| {
        CliError::ipc(format!(
            "could not reach the running Scrozz at {}: {error}",
            path.display()
        ))
    })?;
    configure_unix_stream(&stream)?;
    exchange(&mut stream, argv)
}

#[cfg(unix)]
pub(crate) fn configure_unix_stream(stream: &std::os::unix::net::UnixStream) -> CliResult<()> {
    let poll = Some(Duration::from_millis(100));
    stream
        .set_read_timeout(poll)
        .and_then(|()| stream.set_write_timeout(poll))
        .map_err(|error| CliError::ipc(format!("could not bound the IPC socket: {error}")))
}

#[cfg(windows)]
fn forward_to(path: &Path, argv: &[String]) -> CliResult<Response> {
    let mut stream =
        windows_pipe::PipeStream::connect(path, CONNECT_TIMEOUT).map_err(|error| match error {
            windows_pipe::ConnectError::NotRunning => {
                CliError::ipc("no running Scrozz named-pipe server was found")
            }
            windows_pipe::ConnectError::Unusable(error) => CliError::ipc(error),
        })?;
    exchange(&mut stream, argv)
}

#[cfg(not(any(unix, windows)))]
fn forward_to(_path: &Path, _argv: &[String]) -> CliResult<Response> {
    Err(CliError::ipc(
        "single-instance forwarding is not supported on this platform",
    ))
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
                    GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
                },
                GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
                SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
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

    #[derive(Debug)]
    pub enum ConnectError {
        NotRunning,
        Unusable(String),
    }

    struct OwnedHandle(HANDLE);

    // Windows kernel handles may be transferred between threads. This wrapper
    // owns exactly one handle and closes it once.
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

    struct LocalWideString(PWSTR);

    impl Drop for LocalWideString {
        fn drop(&mut self) {
            let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0.cast()))) };
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
                        std::thread::sleep(super::RETRY_DELAY);
                    }
                    Err(_) => match unsafe { GetLastError() } {
                        ERROR_PIPE_CONNECTED => return Ok(()),
                        ERROR_PIPE_LISTENING => {
                            std::thread::sleep(super::RETRY_DELAY);
                        }
                        ERROR_NO_DATA => {
                            let _ = unsafe { DisconnectNamedPipe(self.handle.0) };
                            std::thread::sleep(super::RETRY_DELAY);
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
                        verify_server_owner(stream.handle.0)?;
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
                ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED => {
                    Err(io::Error::from(io::ErrorKind::UnexpectedEof))
                }
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
        sid_to_string(user.User.Sid).map_err(|error| {
            CliError::ipc(format!("could not format the current user SID: {error}"))
        })
    }

    fn verify_server_owner(handle: HANDLE) -> Result<(), ConnectError> {
        let expected =
            current_user_sid_string().map_err(|error| ConnectError::Unusable(error.to_string()))?;
        let actual = server_owner_sid(handle).map_err(ConnectError::Unusable)?;
        if actual == expected {
            Ok(())
        } else {
            Err(ConnectError::Unusable(
                "refusing a Scrozz named pipe not owned by the current user".to_owned(),
            ))
        }
    }

    fn server_owner_sid(handle: HANDLE) -> Result<String, String> {
        let mut owner = PSID::default();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(&raw mut owner),
                None,
                None,
                None,
                Some(&raw mut descriptor),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!(
                "could not inspect the Scrozz named-pipe owner: {}",
                io::Error::from_raw_os_error(status.0 as i32)
            ));
        }
        if descriptor.is_invalid() {
            return Err(
                "Windows returned no security descriptor for the Scrozz named pipe".to_owned(),
            );
        }
        let _descriptor = SecurityDescriptor(descriptor);
        if owner.is_invalid() {
            return Err("Windows returned no owner for the Scrozz named pipe".to_owned());
        }
        sid_to_string(owner)
            .map_err(|error| format!("the Scrozz named-pipe owner SID was invalid: {error}"))
    }

    fn sid_to_string(sid: PSID) -> Result<String, String> {
        let mut string = PWSTR::null();
        unsafe { ConvertSidToStringSidW(sid, &mut string) }.map_err(|error| error.to_string())?;
        let string = LocalWideString(string);
        unsafe { string.0.to_string() }.map_err(|error| error.to_string())
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
    use std::io::Cursor;

    use clap::Parser as _;

    use super::*;
    use crate::cli::Cli;

    fn command_of(argv: &[&str]) -> Command {
        Cli::try_parse_from(argv).unwrap().command.unwrap()
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn forwarding_policy_serializes_mutable_state() {
        assert_eq!(
            policy(&command_of(&["scrozz", "record", "--stop"])),
            Forwarding::Require
        );
        for args in [
            vec!["scrozz", "capture"],
            vec!["scrozz", "record"],
            vec!["scrozz", "history", "list"],
            vec!["scrozz", "ocr"],
            vec!["scrozz", "settings", "set", "capture.format", "png"],
            vec!["scrozz", "url", "enable"],
            vec![
                "scrozz", "system", "notify", "--title", "Scrozz", "--body", "Ready",
            ],
        ] {
            assert_eq!(policy(&command_of(&args)), Forwarding::Prefer, "{args:?}");
        }
    }

    #[test]
    fn pure_operations_stay_local() {
        for args in [
            vec!["scrozz", "list", "displays"],
            vec!["scrozz", "settings", "get"],
            vec![
                "scrozz",
                "hotkey",
                "generate-config",
                "--compositor",
                "sway",
            ],
            vec!["scrozz", "update", "status"],
            vec!["scrozz", "system", "status"],
            vec!["scrozz", "gui"],
        ] {
            assert_eq!(policy(&command_of(&args)), Forwarding::Never, "{args:?}");
        }
    }

    #[test]
    fn url_ipc_uses_only_fixed_action_arguments() {
        assert_eq!(
            url_arguments(UrlAction::CaptureRegion),
            argv(&["capture", "--interactive", "region"])
        );
        assert_eq!(
            url_arguments(UrlAction::RecordStop),
            argv(&["record", "--stop"])
        );
    }

    #[test]
    fn requests_round_trip_special_characters() {
        let original = argv(&[
            "capture",
            "--window",
            "quotes \" newline\n backslash\\ and écran ✅",
        ]);
        let frame = encode_request(&original, Some(Path::new("/home/u"))).unwrap();
        let payload = decode_frame(&frame, MAX_REQUEST_BYTES, "request").unwrap();
        let decoded = decode_request_payload(payload).unwrap();
        assert_eq!(decoded.argv, original);
        assert_eq!(decoded.cwd.as_deref(), Some(Path::new("/home/u")));
    }

    #[test]
    fn request_schema_and_unknown_fields_are_rejected() {
        for payload in [
            br#"{"schema":1,"argv":[],"cwd":null}"#.as_slice(),
            br#"{"schema":2,"argv":[],"cwd":null,"command":"anything"}"#.as_slice(),
        ] {
            assert!(decode_request_payload(payload).is_err());
        }
    }

    #[test]
    fn stdout_stderr_and_binary_bytes_round_trip() {
        let original = Response {
            code: 7,
            stdout: vec![0, 1, b'\n', 255],
            stderr: b"guidance\n".to_vec(),
        };
        let encoded = encode_response(&original).unwrap();
        assert_eq!(parse_response(&encoded).unwrap(), original);
    }

    #[test]
    fn truncated_and_trailing_frames_are_rejected() {
        let response = Response {
            code: 0,
            stdout: b"ok\n".to_vec(),
            stderr: Vec::new(),
        };
        let mut encoded = encode_response(&response).unwrap();
        encoded.pop();
        assert!(parse_response(&encoded).is_err());
        encoded.push(0);
        encoded.push(0);
        assert!(parse_response(&encoded).is_err());
    }

    #[test]
    fn oversized_announced_frames_are_rejected_before_allocation() {
        let announced = u32::try_from(MAX_REQUEST_BYTES + 1).unwrap();
        let mut cursor = Cursor::new(announced.to_le_bytes());
        let error = read_frame(
            &mut cursor,
            MAX_REQUEST_BYTES,
            "request",
            Instant::now() + TRANSFER_TIMEOUT,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("limit"), "{error}");
    }

    #[test]
    fn short_reads_and_writes_transfer_the_entire_frame() {
        struct ShortWriter(Vec<u8>);
        impl Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let count = bytes.len().min(3);
                self.0.extend_from_slice(&bytes[..count]);
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let frame = encode_request(&argv(&["capture"]), None).unwrap();
        let mut writer = ShortWriter(Vec::new());
        write_all_until(
            &mut writer,
            &frame,
            "request",
            Instant::now() + TRANSFER_TIMEOUT,
            None,
        )
        .unwrap();
        assert_eq!(writer.0, frame);

        let mut cursor = Cursor::new(frame);
        let shutdown = AtomicBool::new(false);
        let request =
            receive_request(&mut cursor, Instant::now() + TRANSFER_TIMEOUT, &shutdown).unwrap();
        assert_eq!(request.argv, argv(&["capture"]));
    }

    #[test]
    fn endpoint_override_is_exact() {
        let _env = crate::test_env::lock();
        crate::test_env::set(ENDPOINT_ENV, "/run/custom/scrozz.sock");
        assert_eq!(endpoint(), PathBuf::from("/run/custom/scrozz.sock"));
    }

    #[test]
    fn user_tokens_cannot_escape_a_path() {
        let _env = crate::test_env::lock();
        crate::test_env::set("USER", "../../etc/evil");
        let token = user_token();
        assert!(!token.contains('/'), "{token}");
        assert!(!token.contains('.'), "{token}");
    }

    #[cfg(unix)]
    #[test]
    fn missing_unix_endpoint_is_not_running_and_cannot_forward() {
        let missing = Path::new("/nonexistent/scrozz-test/instance.sock");
        assert_eq!(probe_at(missing), Status::NotRunning);
        assert_eq!(
            forward_to(missing, &argv(&["capture"])).unwrap_err().exit(),
            crate::exit::Exit::IpcFailed
        );
    }
}
