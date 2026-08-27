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
    process::{Command, Output},
};

/// Runs the binary and returns everything the shell would have seen.
fn scrozz<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args(args)
        // Otherwise the developer's own RUST_LOG decides whether the assertions
        // about stderr hold.
        .env_remove("RUST_LOG")
        .env_remove("SCROZZ_SIMULATE_ERROR")
        .env_remove("SCROZZ_UNSTABLE_BACKENDS")
        .output()
        .expect("the binary should run")
}

/// Runs the binary with one simulated failure injected.
fn scrozz_failing<I, S>(kind: &str, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args(args)
        .env_remove("RUST_LOG")
        .env_remove("SCROZZ_UNSTABLE_BACKENDS")
        .env("SCROZZ_SIMULATE_ERROR", kind)
        .output()
        .expect("the binary should run")
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
    assert!(stdout(&out).contains(env!("CARGO_PKG_VERSION")));
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
        text.contains("--interactive window"),
        "the alternative is missing, which is the whole point: {text}"
    );
    assert!(
        text.contains("not a fault"),
        "a limitation must not read as a bug: {text}"
    );
}

#[test]
fn capture_validation_is_clean_without_touching_the_screen() {
    let out = scrozz(["capture", "--region", "0,0,10,10", "--no-ipc", "--dry-run"]);
    assert_eq!(code(&out), 0, "capture planning failed: {}", stderr(&out));
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
    let out = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args(["-vvv", "--json", "settings", "get"])
        .env_remove("SCROZZ_SIMULATE_ERROR")
        .env("RUST_LOG", "trace")
        .output()
        .expect("the binary should run");

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
    let out = Command::new(env!("CARGO_BIN_EXE_scrozz"))
        .args(["record", "--stop"])
        .env_remove("SCROZZ_SIMULATE_ERROR")
        .env("SCROZZ_IPC_SOCKET", "/nonexistent/scrozz-test/absent.sock")
        .output()
        .expect("the binary should run");

    assert_ne!(code(&out), 0);
    assert_ne!(code(&out), 101, "{}", stderr(&out));
    assert!(!out.stderr.is_empty(), "it failed without explaining why");
}
