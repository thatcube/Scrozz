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
//! A running instance listens on a socket. A CLI invocation that would benefit
//! from the running app's state hands its whole `argv` over and relays the
//! answer back. The forked process is a thin remote control, so
//! `scrozz capture --json` produces byte-identical output whether or not the app
//! happens to be running — which is the property that makes scripting against it
//! safe.
//!
//! # The wire format, and why it is not JSON both ways
//!
//! ```text
//! -->  {"schema":1,"argv":["capture","--json"],"cwd":"/home/u"}\n   (then EOF)
//! <--  SCROZZ/1 0 json\n
//! <--  <payload bytes until EOF>
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
//! # What is implemented here
//!
//! The protocol, the endpoint rules, encoding, parsing, and the policy for which
//! commands forward. The **server** belongs to the GUI, which owns the event loop
//! and the store; it does not exist yet, so `try_connect` reports
//! [`Status::NotRunning`] and every caller falls through to doing the work
//! locally. That is the correct behaviour today and stays correct afterwards.

use std::path::{Path, PathBuf};

use crate::{
    cli::Command,
    fault::{CliError, CliResult},
    json::Json,
};

/// The protocol version token that opens every response.
pub const PROTOCOL_TOKEN: &str = "SCROZZ/1";

/// The request schema version.
pub const REQUEST_SCHEMA: i64 = 1;

/// Overrides the endpoint, for tests and for unusual sandboxes.
pub const ENDPOINT_ENV: &str = "SCROZZ_IPC_SOCKET";

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
pub fn forward(argv: &[String]) -> CliResult<Response> {
    forward_to(&endpoint(), argv)
}

#[cfg(unix)]
fn forward_to(path: &Path, argv: &[String]) -> CliResult<Response> {
    use std::{
        io::{Read, Write},
        net::Shutdown,
        os::unix::net::UnixStream,
    };

    let mut stream = UnixStream::connect(path).map_err(|e| {
        CliError::ipc(format!(
            "could not reach the running Scrozz at {}: {e}",
            path.display()
        ))
    })?;

    let cwd = std::env::current_dir().ok();
    let request = encode_request(argv, cwd.as_deref());
    stream
        .write_all(request.as_bytes())
        .map_err(|e| CliError::ipc(format!("could not send the request: {e}")))?;
    // Half-close so the far side sees EOF and knows the request is complete
    // without needing a length prefix.
    stream
        .shutdown(Shutdown::Write)
        .map_err(|e| CliError::ipc(format!("could not finish the request: {e}")))?;

    let mut buffer = Vec::new();
    stream
        .read_to_end(&mut buffer)
        .map_err(|e| CliError::ipc(format!("could not read the response: {e}")))?;

    parse_response(&buffer)
}

#[cfg(not(unix))]
fn forward_to(_path: &Path, _argv: &[String]) -> CliResult<Response> {
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
        for args in [
            vec!["scrozz", "settings", "set", "capture.format", "png"],
            vec!["scrozz", "settings", "reset", "capture.format"],
            vec!["scrozz", "settings", "reset"],
        ] {
            assert_eq!(policy(&command_of(&args)), Forwarding::Prefer, "{args:?}");
        }
        for args in [
            vec!["scrozz", "settings", "get"],
            vec!["scrozz", "settings", "path"],
        ] {
            assert_eq!(policy(&command_of(&args)), Forwarding::Never, "{args:?}");
        }
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
            "{\"schema\":1,\"argv\":[\"capture\",\"--json\"],\"cwd\":\"/home/u\"}\n"
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
        let raw = b"SCROZZ/1 0 json\n{\"ok\":true}";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.code, 0);
        assert_eq!(response.stream, StreamKind::Json);
        assert_eq!(response.payload, br#"{"ok":true}"#);
    }

    #[test]
    fn a_binary_payload_survives_untouched() {
        let png = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff];
        let mut raw = b"SCROZZ/1 0 binary\n".to_vec();
        raw.extend_from_slice(&png);
        let response = parse_response(&raw).unwrap();
        assert_eq!(response.stream, StreamKind::Binary);
        assert_eq!(response.payload, png);
    }

    #[test]
    fn a_payload_containing_newlines_is_not_truncated() {
        let raw = b"SCROZZ/1 0 text\nline one\nline two\n";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.payload, b"line one\nline two\n");
    }

    #[test]
    fn an_empty_payload_is_valid() {
        let response = parse_response(b"SCROZZ/1 3 text\n").unwrap();
        assert_eq!(response.code, 3);
        assert!(response.payload.is_empty());
    }

    #[test]
    fn every_exit_code_relays_verbatim() {
        for code in crate::exit::Exit::all() {
            let raw = format!("SCROZZ/1 {} text\n", code.code());
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
        let err = parse_response(b"SCROZZ/2 0 json\n{}").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("SCROZZ/2"), "{message}");
        assert!(message.contains("SCROZZ/1"), "{message}");
        assert!(message.contains("different versions"), "{message}");
    }

    #[test]
    fn malformed_headers_are_rejected_one_by_one() {
        let cases: [(&[u8], &str); 4] = [
            (b"SCROZZ/1\n", "no exit code"),
            (b"SCROZZ/1 abc json\n", "malformed exit code"),
            (b"SCROZZ/1 0\n", "no stream kind"),
            (b"SCROZZ/1 0 pictures\n", "unknown stream kind"),
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
        assert!(parse_response(b"SCROZZ/1 300 json\n").is_err());
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
