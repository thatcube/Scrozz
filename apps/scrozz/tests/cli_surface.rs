//! Black-box tests: the binary is run as a user would run it.
//!
//! # Why these exist alongside the unit tests
//!
//! The in-source tests check that a function returns the right value. These check
//! the things that only exist once there is a real process: which file descriptor
//! the bytes landed on, what the shell saw as `$?`, and whether a stray log line
//! got into a machine-readable stream.
//!
//! Those are exactly the properties a script depends on, and none of them can be
//! observed from inside the code that produces them.
//!
//! # What is deliberately not here
//!
//! **No capture is ever taken.** A real capture needs a permission this machine
//! may not have granted and, on every platform, may put something on screen. So
//! failure is *simulated* through `SCROZZ_SIMULATE_ERROR`, and success is
//! exercised through the paths that touch no display server: `--dry-run`,
//! `hotkey generate-config`, `settings get`.
//!
//! **The GUI is never launched.** `scrozz gui` and a bare `scrozz` are absent
//! from this file on purpose. A test suite that opens a window is a test suite
//! that cannot be run while someone is working.
//!
//! Every invocation passes `--no-ipc` where a running instance could otherwise
//! answer, so a Scrozz already running on the developer's machine cannot change
//! the result.

use std::{
    ffi::OsStr,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

fn isolated_settings_path() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!(
            "scrozz-cli-surface-{}-{sequence}",
            std::process::id()
        ))
        .join("settings.json")
}

fn clean_settings(path: &std::path::Path) {
    if let Some(root) = path.parent() {
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Runs the binary and returns everything the shell would have seen.
fn scrozz<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let settings = isolated_settings_path();
    let output = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args(args)
        // Otherwise the developer's own RUST_LOG decides whether the assertions
        // about stderr hold.
        .env_remove("RUST_LOG")
        .env_remove("SCROZZ_SIMULATE_ERROR")
        .env_remove("SCROZZ_UNSTABLE_BACKENDS")
        .env("SCROZZ_SETTINGS_FILE", &settings)
        .output()
        .expect("the binary should run");
    clean_settings(&settings);
    output
}

/// Runs the binary with one simulated failure injected.
fn scrozz_failing<I, S>(kind: &str, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let settings = isolated_settings_path();
    let output = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args(args)
        .env_remove("RUST_LOG")
        .env_remove("SCROZZ_UNSTABLE_BACKENDS")
        .env("SCROZZ_SETTINGS_FILE", &settings)
        .env("SCROZZ_SIMULATE_ERROR", kind)
        .output()
        .expect("the binary should run");
    clean_settings(&settings);
    output
}

fn code(out: &Output) -> i32 {
    out.status
        .code()
        .expect("the process should not be signalled")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("stdout should be UTF-8 here")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// Help and version
// ---------------------------------------------------------------------------

#[test]
fn help_goes_to_stdout_and_succeeds() {
    let out = scrozz(["--help"]);
    assert_eq!(code(&out), 0);
    assert!(out.stderr.is_empty(), "help must not use stderr");
    let text = stdout(&out);
    // The reason the CLI exists at all should be discoverable from `--help`,
    // because that is the only documentation many people will read.
    assert!(text.contains("hotkey"), "{text}");
    assert!(text.contains("Hyprland"), "{text}");
}

#[test]
fn version_goes_to_stdout_and_succeeds() {
    let out = scrozz(["--version"]);
    assert_eq!(code(&out), 0);
    let expected = option_env!("SCROZZ_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    assert!(stdout(&out).contains(expected));
}

#[test]
fn every_subcommand_has_working_help() {
    for args in [
        vec!["capture", "--help"],
        vec!["record", "--help"],
        vec!["list", "--help"],
        vec!["list", "displays", "--help"],
        vec!["list", "windows", "--help"],
        vec!["history", "--help"],
        vec!["history", "list", "--help"],
        vec!["history", "get", "--help"],
        vec!["history", "delete", "--help"],
        vec!["history", "pin", "--help"],
        vec!["ocr", "--help"],
        vec!["share", "--help"],
        vec!["settings", "--help"],
        vec!["settings", "get", "--help"],
        vec!["settings", "set", "--help"],
        vec!["hotkey", "--help"],
        vec!["hotkey", "generate-config", "--help"],
        vec!["gui", "--help"],
    ] {
        let out = scrozz(&args);
        assert_eq!(code(&out), 0, "{args:?} should print help and exit 0");
        assert!(!out.stdout.is_empty(), "{args:?} printed no help");
    }
}

#[cfg(not(feature = "cloud"))]
#[test]
fn default_binary_explains_that_cloud_networking_is_optional() {
    let path = std::env::temp_dir().join(format!(
        "scrozz-cloud-feature-smoke-{}.png",
        std::process::id()
    ));
    std::fs::write(&path, b"feature boundary only").unwrap();
    let out = scrozz(["share", path.to_str().unwrap()]);
    std::fs::remove_file(path).unwrap();
    assert_eq!(code(&out), 5);
    let text = stderr(&out);
    assert!(text.contains("--features cloud"), "{text}");
    assert!(!text.to_ascii_lowercase().contains("secret access key"));
}

#[test]
fn json_share_missing_file_fails_before_feature_config_credentials_or_network() {
    let missing = std::env::temp_dir().join(format!(
        "scrozz-definitely-missing-{}-credential-sentinel.png",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&missing);
    let out = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args(["--json", "--no-ipc", "share", missing.to_str().unwrap()])
        .env("SCROZZ_S3_ACCESS_KEY_ID", "access-id-must-not-appear")
        .env("SCROZZ_S3_SECRET_ACCESS_KEY", "secret-key-must-not-appear")
        .env("SCROZZ_S3_ENDPOINT", "https://network-must-not-run.invalid")
        .env_remove("RUST_LOG")
        .output()
        .expect("the binary should run");
    assert_eq!(code(&out), 2);
    assert!(out.stderr.is_empty(), "JSON runtime errors use stdout");
    let text = stdout(&out);
    assert!(text.contains(r#""ok":false"#), "{text}");
    assert!(text.contains(r#""kind":"usage""#), "{text}");
    for forbidden in [
        "Unsupported",
        "--features cloud",
        "access-id-must-not-appear",
        "secret-key-must-not-appear",
        "network-must-not-run.invalid",
    ] {
        assert!(!text.contains(forbidden), "{forbidden:?} leaked in {text}");
    }
}

#[test]
fn settings_set_persists_non_secret_values_and_reports_their_source() {
    let root = std::env::temp_dir().join(format!(
        "scrozz-cli-settings-persistence-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("settings.json");
    let set = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args([
            "--json",
            "--no-ipc",
            "settings",
            "set",
            "cloud.bucket",
            "screenshots",
        ])
        .env("SCROZZ_SETTINGS_FILE", &path)
        .output()
        .unwrap();
    assert_eq!(code(&set), 0, "{}", stderr(&set));
    let get = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args(["--json", "--no-ipc", "settings", "get", "cloud.bucket"])
        .env("SCROZZ_SETTINGS_FILE", &path)
        .output()
        .unwrap();
    assert_eq!(code(&get), 0, "{}", stderr(&get));
    let text = stdout(&get);
    assert!(text.contains(r#""value":"screenshots""#), "{text}");
    assert!(text.contains(r#""source":"user""#), "{text}");
    let stored = std::fs::read_to_string(&path).unwrap();
    for forbidden in ["secret_access_key", "session_token", "\"password\""] {
        assert!(!stored.contains(forbidden), "{stored}");
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "cloud")]
#[test]
fn share_puts_to_loopback_s3_and_emits_stable_json_without_logging_secrets() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        process::Stdio,
        time::Duration,
    };

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let count = stream.read(&mut chunk).unwrap();
            request.extend_from_slice(&chunk[..count]);
            let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..headers_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() >= headers_end + 4 + length {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        request
    });

    let file = std::env::temp_dir().join(format!(
        "scrozz-cli-share-{}-capture.png",
        std::process::id()
    ));
    std::fs::write(&file, b"\x89PNG\r\n\x1a\nloopback-image").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .arg("--json")
        .arg("share")
        .arg(&file)
        .arg("--expires")
        .arg("1h")
        .arg("--secret-key-stdin")
        .env_remove("RUST_LOG")
        .env_remove("SCROZZ_S3_CREDENTIAL_COMMAND")
        .env_remove("SCROZZ_S3_CREDENTIAL_ARGS")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .env("SCROZZ_S3_PROVIDER", "minio")
        .env("SCROZZ_S3_ENDPOINT", format!("http://{address}"))
        .env("SCROZZ_S3_BUCKET", "fake")
        .env("SCROZZ_S3_EXPIRES", "invalid-environment-value")
        .env("SCROZZ_S3_ACCESS_KEY_ID", "loopback-access")
        .env("SCROZZ_S3_SECRET_ACCESS_KEY", "loopback-secret")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&file);
    let request = String::from_utf8_lossy(&server.join().unwrap()).into_owned();

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
    let json = stdout(&output);
    assert!(json.contains(r#""command":"share""#), "{json}");
    assert!(json.contains(r#""provider":"minio""#), "{json}");
    assert!(json.contains(r#""seconds":3600"#), "{json}");
    assert!(json.contains("X-Amz-Signature"), "{json}");
    assert!(request.starts_with("PUT /fake/captures/"));
    assert!(request.to_ascii_lowercase().contains("authorization:"));
    assert!(!request.contains("loopback-secret"));
    assert!(!json.contains("loopback-secret"));
}

// ---------------------------------------------------------------------------
// Usage errors — exit 2
// ---------------------------------------------------------------------------

#[test]
fn a_bad_invocation_exits_two_and_writes_to_stderr() {
    for args in [
        vec!["--nonsense"],
        vec!["screenshot"],
        vec!["capture", "--format", "tiff"],
        vec!["capture", "--region", "not-a-region"],
        vec!["capture", "--region", "0,0,100,100", "--display", "1"],
        vec!["capture", "--quality", "0"],
        vec!["--no-ipc", "capture", "--delay", "1e300", "--dry-run"],
        vec!["list"],
        vec!["settings", "set", "only-a-key"],
        vec!["-v", "-q", "list", "displays"],
    ] {
        let out = scrozz(&args);
        assert_eq!(code(&out), 2, "{args:?} should be a usage error");
        assert!(
            out.stdout.is_empty(),
            "{args:?} put a usage error on stdout"
        );
        assert!(!out.stderr.is_empty(), "{args:?} explained nothing");
    }
}

#[test]
fn a_usage_error_never_contaminates_stdout_even_in_json_mode() {
    // A script doing `scrozz --json ... | jq` must get either a document or
    // nothing — never half a document followed by a clap diagnostic.
    let out = scrozz(["--json", "capture", "--format", "tiff"]);
    assert_eq!(code(&out), 2);
    assert!(out.stdout.is_empty());
}

// ---------------------------------------------------------------------------
// The exit-code contract
// ---------------------------------------------------------------------------

/// Every simulated error class, with the code a script may rely on.
const CLASSES: &[(&str, i32)] = &[
    ("usage", 2),
    ("cancelled", 3),
    ("permission-denied", 4),
    ("unsupported", 5),
    ("target-gone", 6),
    ("invalid-request", 7),
    ("codec", 8),
    ("storage", 9),
    ("io", 10),
    ("platform", 11),
    ("not-implemented", 12),
    ("ipc-failed", 13),
];

#[test]
fn each_error_class_has_its_own_exit_code() {
    for (kind, expected) in CLASSES {
        let out = scrozz_failing(kind, ["capture", "--no-ipc"]);
        assert_eq!(code(&out), *expected, "{kind} should exit {expected}");
    }
}

#[test]
fn the_exit_codes_are_all_distinct() {
    // Duplicating one would silently destroy a script's ability to branch on it.
    let mut seen: Vec<i32> = CLASSES.iter().map(|(_, c)| *c).collect();
    seen.sort_unstable();
    let count = seen.len();
    seen.dedup();
    assert_eq!(seen.len(), count, "two error classes share an exit code");
}

#[test]
fn an_unknown_simulated_class_is_a_loud_usage_error() {
    // Guards the test suite itself: a typo in a class name must fail, not
    // silently pass by taking the success path.
    let out = scrozz_failing("no-such-class", ["capture", "--no-ipc"]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("no-such-class"), "{}", stderr(&out));
}

// ---------------------------------------------------------------------------
// Cancellation is not a fault (D11 / D15)
// ---------------------------------------------------------------------------

#[test]
fn pressing_escape_is_silent_and_distinguishable() {
    let out = scrozz_failing("cancelled", ["capture", "--no-ipc"]);
    assert_eq!(code(&out), 3, "cancellation has its own code");
    assert!(out.stdout.is_empty(), "cancelling should print nothing");
    assert!(
        out.stderr.is_empty(),
        "cancelling is not an error: {}",
        stderr(&out)
    );
}

#[test]
fn cancellation_is_still_reported_in_json_because_a_script_asked() {
    let out = scrozz_failing("cancelled", ["--json", "capture", "--no-ipc"]);
    assert_eq!(code(&out), 3);
    let text = stdout(&out);
    assert!(text.contains(r#""cancelled":true"#), "{text}");
    assert!(text.contains(r#""ok":false"#), "{text}");
    // The distinction that matters: not a fault, so not something to alert on.
    assert!(text.contains(r#""actionable":false"#), "{text}");
}

// ---------------------------------------------------------------------------
// Permission denial renders as guidance, never a trace (D15)
// ---------------------------------------------------------------------------

#[test]
fn a_permission_denial_names_the_settings_pane() {
    let out = scrozz_failing("permission-denied", ["capture", "--no-ipc"]);
    assert_eq!(code(&out), 4);

    let text = stderr(&out);
    let pane = if cfg!(target_os = "macos") {
        "System Settings"
    } else if cfg!(target_os = "windows") {
        "Settings"
    } else {
        "portal"
    };
    assert!(
        text.contains(pane),
        "the remedy must name where to go: {text}"
    );

    // The thing D15 exists to prevent.
    assert!(!text.contains("panicked"), "{text}");
    assert!(!text.contains("RUST_BACKTRACE"), "{text}");
    assert!(!text.contains("Error {"), "a Debug dump leaked: {text}");
}

#[test]
fn the_remedy_survives_into_json_as_a_field() {
    // So a GUI or a script can act on it rather than scraping prose.
    let out = scrozz_failing("permission-denied", ["--json", "capture", "--no-ipc"]);
    let text = stdout(&out);
    assert!(text.contains(r#""kind":"permission-denied""#), "{text}");
    assert!(text.contains(r#""remedy":"#), "{text}");
    assert!(text.contains(r#""capability":"#), "{text}");
    assert!(text.contains(r#""actionable":true"#), "{text}");
}

// ---------------------------------------------------------------------------
// A platform gap explains itself (D8)
// ---------------------------------------------------------------------------

#[test]
fn an_unsupported_operation_gives_the_why_and_the_alternative() {
    let out = scrozz_failing("unsupported", ["capture", "--no-ipc"]);
    assert_eq!(code(&out), 5);

    let text = stderr(&out);
    assert!(text.contains("Wayland"), "the why is missing: {text}");
    assert!(
        text.contains("Capture a display instead"),
        "the alternative is missing, which is the whole point: {text}"
    );
    assert!(
        text.contains("not a fault"),
        "a limitation must not read as a bug: {text}"
    );
}

#[test]
fn an_unfinished_backend_is_a_clean_refusal_rather_than_a_panic() {
    // Most of the workspace is `todo!()` today. A user who reaches one must get
    // a code and a sentence, not exit 101 and a backtrace.
    let out = scrozz(["capture", "--region", "0,0,10,10", "--no-ipc"]);
    assert_ne!(code(&out), 101, "a todo!() escaped: {}", stderr(&out));
    assert!(!stderr(&out).contains("panicked"), "{}", stderr(&out));
}

// ---------------------------------------------------------------------------
// Stream discipline
// ---------------------------------------------------------------------------

#[test]
fn json_mode_emits_exactly_one_document_on_stdout() {
    let out = scrozz([
        "--json",
        "hotkey",
        "generate-config",
        "--compositor",
        "sway",
    ]);
    assert_eq!(code(&out), 0);

    let text = stdout(&out);
    assert_eq!(
        text.lines().count(),
        1,
        "a JSON document must be one line: {text}"
    );
    assert!(text.ends_with('\n'), "it must be newline-terminated");
    assert!(text.starts_with('{'), "{text}");
}

#[test]
fn logs_never_reach_stdout_however_loud_the_verbosity() {
    let settings = isolated_settings_path();
    let out = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args(["-vvv", "--json", "settings", "get"])
        .env_remove("SCROZZ_SIMULATE_ERROR")
        .env("SCROZZ_SETTINGS_FILE", &settings)
        .env("RUST_LOG", "trace")
        .output()
        .expect("the binary should run");
    clean_settings(&settings);

    let text = stdout(&out);
    assert_eq!(
        text.lines().count(),
        1,
        "logging leaked into stdout: {text}"
    );
    assert!(text.starts_with('{'), "{text}");
}

#[test]
fn quiet_silences_the_human_report_but_not_the_exit_code() {
    let out = scrozz_failing("storage", ["--quiet", "capture", "--no-ipc"]);
    assert_eq!(code(&out), 9, "--quiet must not change the outcome");
}

#[test]
fn human_mode_keeps_errors_off_stdout() {
    for (kind, _) in CLASSES {
        let out = scrozz_failing(kind, ["capture", "--no-ipc"]);
        assert!(
            out.stdout.is_empty(),
            "{kind} wrote an error to stdout: {}",
            stdout(&out)
        );
    }
}

#[test]
fn json_mode_keeps_the_report_off_stderr() {
    for (kind, _) in CLASSES {
        let out = scrozz_failing(kind, ["--json", "capture", "--no-ipc"]);
        assert!(
            out.stderr.is_empty(),
            "{kind} duplicated its report onto stderr: {}",
            stderr(&out)
        );
    }
}

// ---------------------------------------------------------------------------
// The JSON envelope is a public API
// ---------------------------------------------------------------------------

/// Checks for one literal `"key":value` pair in the compact envelope.
///
/// A real parser would be better; this crate deliberately has no JSON dependency,
/// and the writer emits a known compact shape, so a substring check is honest
/// about what it is testing: the exact bytes a consumer will see.
fn has_field(document: &str, key: &str, value: &str) -> bool {
    document.contains(&format!(r#""{key}":{value}"#))
}

#[test]
fn success_and_failure_share_one_envelope_shape() {
    let ok = stdout(&scrozz(["--json", "settings", "get"]));
    let bad = stdout(&scrozz_failing("codec", ["--json", "capture", "--no-ipc"]));

    for document in [&ok, &bad] {
        for key in ["schema", "ok", "command", "data", "error"] {
            assert!(
                document.contains(&format!(r#""{key}":"#)),
                "the envelope is missing {key}: {document}"
            );
        }
    }

    // Key order is part of the contract: a diff of two runs should not churn.
    assert!(
        ok.starts_with(r#"{"schema":1,"ok":true,"command":"settings.get","#),
        "{ok}"
    );
    assert!(
        bad.starts_with(r#"{"schema":1,"ok":false,"command":"capture","data":null,"#),
        "{bad}"
    );
}

#[test]
fn the_schema_version_is_pinned() {
    // Changing this is a breaking change for every script in the wild, so it
    // should require deliberately editing a test that says so.
    let text = stdout(&scrozz(["--json", "settings", "get"]));
    assert!(has_field(&text, "schema", "1"), "{text}");
}

#[test]
fn the_command_slug_identifies_the_subcommand_precisely() {
    for (args, slug) in [
        (vec!["--json", "settings", "get"], "settings.get"),
        (
            vec![
                "--json",
                "hotkey",
                "generate-config",
                "--compositor",
                "sway",
            ],
            "hotkey.generate-config",
        ),
    ] {
        let out = scrozz(&args);
        let text = stdout(&out);
        assert!(
            has_field(&text, "command", &format!(r#""{slug}""#)),
            "{args:?} reported the wrong command: {text}"
        );
    }
}

#[test]
fn an_error_object_always_carries_the_code_it_exited_with() {
    for (kind, expected) in CLASSES {
        let out = scrozz_failing(kind, ["--json", "capture", "--no-ipc"]);
        let text = stdout(&out);
        assert!(
            has_field(&text, "code", &expected.to_string()),
            "{kind}: the JSON code disagrees with the exit status: {text}"
        );
        assert_eq!(code(&out), *expected);
    }
}

#[test]
fn json_output_contains_no_raw_control_characters() {
    // A literal newline or tab inside a string would make the document
    // unparseable; the writer escapes them, and this proves it end to end.
    let out = scrozz([
        "--json",
        "hotkey",
        "generate-config",
        "--compositor",
        "hyprland",
    ]);
    let body = stdout(&out);
    let body = body.trim_end_matches('\n');
    assert!(
        !body.chars().any(|c| (c as u32) < 0x20),
        "an unescaped control character reached the output"
    );
}

// ---------------------------------------------------------------------------
// Paths that touch no display server
// ---------------------------------------------------------------------------

#[test]
fn a_dry_run_reports_the_plan_and_captures_nothing() {
    let out = scrozz([
        "--json",
        "capture",
        "--region",
        "10,20,300,400",
        "--cursor",
        "--format",
        "jpeg",
        "--no-ipc",
        "--dry-run",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let text = stdout(&out);
    assert!(has_field(&text, "dry_run", "true"), "{text}");
    assert!(has_field(&text, "cursor", "true"), "{text}");
    assert!(has_field(&text, "format", r#""jpeg""#), "{text}");
    assert!(has_field(&text, "width", "300.0"), "{text}");
}

#[test]
fn a_dry_run_reports_the_resolved_beautification_plan() {
    let out = scrozz([
        "--json",
        "capture",
        "--region",
        "0,0,300,200",
        "--beautify",
        "social",
        "--background",
        "#11223380",
        "--frame-aspect",
        "wide",
        "--alignment",
        "bottom-right",
        "--border",
        "2",
        "--no-ipc",
        "--dry-run",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    // Beautification requires no `--format`, so the plan still reports the
    // ordinary default format; only the beautification fields are new.
    assert!(has_field(&text, "format", r#""png""#), "{text}");
    assert!(has_field(&text, "auto_balance", "true"), "{text}");
    assert!(has_field(&text, "aspect", r#""wide""#), "{text}");
    assert!(has_field(&text, "alignment", r#""bottomright""#), "{text}");
    assert!(has_field(&text, "background", r##""#11223380""##), "{text}");
}

#[test]
fn d9_refuses_window_beautification_before_touching_a_backend() {
    let out = scrozz([
        "capture",
        "--window",
        "Safari",
        "--beautify",
        "clean",
        "--no-ipc",
        "--dry-run",
    ]);
    assert_eq!(code(&out), 7, "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("window"), "{text}");
    assert!(text.contains("D9"), "{text}");
}

#[test]
fn explicit_smart_frame_dry_run_is_allowed_for_a_window_outer_canvas() {
    // The reviewed D9 carve-out: an explicit `--smart-frame` may still add an
    // outer canvas to a window capture, unlike `--beautify` above, because it
    // adds no inset/corner/shadow/border that would touch native pixels.
    let out = scrozz([
        "--json",
        "capture",
        "--window",
        "Safari",
        "--smart-frame",
        "--no-ipc",
        "--dry-run",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(has_field(&text, "smart_frame", "true"), "{text}");
}

#[test]
fn all_in_one_selector_controls_survive_the_real_cli_boundary() {
    let out = scrozz([
        "--json",
        "capture",
        "--interactive",
        "all-in-one",
        "--fixed-size",
        "1200x630",
        "--aspect",
        "40:21",
        "--freeze=false",
        "--magnifier=false",
        "--crosshair=false",
        "--retake",
        "--no-ipc",
        "--dry-run",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let text = stdout(&out);
    assert!(has_field(&text, "kind", r#""interactive""#), "{text}");
    assert!(has_field(&text, "mode", r#""all-in-one""#), "{text}");
    assert!(has_field(&text, "freeze", "false"), "{text}");
    assert!(has_field(&text, "magnifier", "false"), "{text}");
    assert!(has_field(&text, "crosshair", "false"), "{text}");
    assert!(has_field(&text, "retake", "true"), "{text}");
    assert!(
        text.contains(r#""fixed_size":{"width":1200.0,"height":630.0}"#),
        "{text}"
    );
}

#[test]
fn interactive_window_is_two_arguments_not_a_private_flag() {
    let out = scrozz([
        "--json",
        "capture",
        "--interactive",
        "window",
        "--no-ipc",
        "--dry-run",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(has_field(&text, "mode", r#""window""#), "{text}");

    let obsolete = scrozz(["capture", "--interactive-window", "--dry-run"]);
    assert_eq!(code(&obsolete), 2);
}

#[test]
fn generate_config_emits_something_a_compositor_will_accept() {
    let sway = stdout(&scrozz([
        "hotkey",
        "generate-config",
        "--compositor",
        "sway",
    ]));
    assert!(sway.contains("bindsym"), "{sway}");
    assert!(sway.contains("exec scrozz capture"), "{sway}");
    // Mod4 rather than "Super": sway's config language, not ours.
    assert!(sway.contains("Mod4"), "{sway}");

    let hypr = stdout(&scrozz([
        "hotkey",
        "generate-config",
        "--compositor",
        "hyprland",
    ]));
    assert!(hypr.contains("bind ="), "{hypr}");
    assert!(hypr.contains("SUPER"), "{hypr}");
}

#[test]
fn generate_config_honours_a_custom_binary_path() {
    // Whoever pastes this may have installed Scrozz somewhere unusual.
    let out = scrozz([
        "hotkey",
        "generate-config",
        "--compositor",
        "sway",
        "--exec",
        "/opt/scrozz/bin/scrozz",
    ]);
    assert!(stdout(&out).contains("/opt/scrozz/bin/scrozz capture"));
}

#[test]
fn settings_get_lists_the_schema_without_touching_anything() {
    let out = scrozz(["--json", "settings", "get"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(r#""key":"#), "{text}");
    assert!(text.contains(r#""default":"#), "{text}");
}

#[test]
fn an_unknown_setting_key_suggests_the_nearest_real_one() {
    let out = scrozz(["settings", "get", "capture.formt"]);
    assert_ne!(code(&out), 0);
    let text = stderr(&out);
    assert!(
        text.contains("capture.format"),
        "a near-miss should be suggested: {text}"
    );
}

// ---------------------------------------------------------------------------
// Single instance
// ---------------------------------------------------------------------------

#[test]
fn no_ipc_keeps_the_work_in_this_process() {
    // The assertion that matters is negative: with --no-ipc no socket is
    // consulted, so the result cannot depend on whether a GUI is running.
    let out = scrozz(["--json", "--no-ipc", "settings", "get"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

#[test]
fn a_command_that_needs_the_running_app_says_so_rather_than_hanging() {
    // `record --stop` has nothing to stop in this process. It must fail
    // immediately and legibly.
    let settings = isolated_settings_path();
    let out = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args(["record", "--stop"])
        .env_remove("SCROZZ_SIMULATE_ERROR")
        .env("SCROZZ_SETTINGS_FILE", &settings)
        .env("SCROZZ_IPC_SOCKET", "/nonexistent/scrozz-test/absent.sock")
        .output()
        .expect("the binary should run");
    clean_settings(&settings);

    assert_ne!(code(&out), 0);
    assert_ne!(code(&out), 101, "{}", stderr(&out));
    assert!(!out.stderr.is_empty(), "it failed without explaining why");
}

#[test]
fn a_dry_run_recording_resolves_its_whole_plan_without_recording() {
    // The default target is the active display, not an interactive picker: a
    // bare `scrozz record` has to start recording something, and opening a
    // selector from a script is the wrong answer.
    let settings = isolated_settings_path();
    let out = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args([
            "--json",
            "record",
            "--dry-run",
            "--all-displays",
            "--fps",
            "60",
        ])
        .env_remove("SCROZZ_SIMULATE_ERROR")
        .env("SCROZZ_SETTINGS_FILE", &settings)
        .env("SCROZZ_IPC_SOCKET", "/nonexistent/scrozz-test/absent.sock")
        .output()
        .expect("the binary should run");
    clean_settings(&settings);

    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(body.contains("\"dry_run\":true"), "{body}");
    assert!(body.contains("\"fps\":60"), "{body}");
    assert!(body.contains("all-displays"), "{body}");
    assert!(
        !body.contains("not implemented"),
        "a dry run must resolve the whole plan: {body}"
    );
}

#[test]
fn recording_never_reports_itself_as_unimplemented() {
    // The one regression this guards: `record` used to resolve everything and
    // then answer `NotImplemented`, which is exit code 69 and reads to a script
    // as "this build cannot record at all".
    let settings = isolated_settings_path();
    let out = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args(["record", "--dry-run"])
        .env_remove("SCROZZ_SIMULATE_ERROR")
        .env("SCROZZ_SETTINGS_FILE", &settings)
        .env("SCROZZ_IPC_SOCKET", "/nonexistent/scrozz-test/absent.sock")
        .output()
        .expect("the binary should run");
    clean_settings(&settings);

    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out));
    assert!(
        !said.contains("scrozz-record"),
        "recording must not name itself as an unfinished provider: {said}"
    );
}
