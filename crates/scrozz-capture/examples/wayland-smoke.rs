//! End-to-end Wayland capture smoke test.
//!
//! Everything in `linux::wayland` that can be tested without a compositor is
//! tested in `tests/linux.rs` and runs on any machine. This is the other half:
//! the part that can only be proved by a real portal, a real compositor and a
//! real PipeWire daemon, all of which are absent from CI.
//!
//! # What it actually proves
//!
//! Running this successfully proves the things a unit test structurally cannot:
//!
//! - The C ABI declarations in `pipewire::sys` match the installed
//!   `libpipewire-0.3.so.0`. A wrong struct layout there compiles perfectly and
//!   then reads a garbage pointer at runtime.
//! - The hand-encoded SPA POD in `pipewire::pod` is one the server accepts.
//!   A malformed POD is not rejected with an error; the stream simply never
//!   reaches `Streaming`, so it looks exactly like a hang.
//! - Offering no DMA-BUF modifier really does make the server fall back to
//!   shared memory, rather than failing to negotiate at all.
//! - The portal dialog appears, and dismissing it produces
//!   [`Error::Cancelled`] rather than a scary failure.
//! - A restore token is issued, stored, and accepted on the next run, so the
//!   second capture is not preceded by a second dialog.
//!
//! # Why it is an example and not a test
//!
//! Two reasons. It is interactive — a portal dialog needs a human, and `cargo
//! test` capturing stdout hides the instructions. And a `#[test]` that skips on
//! a headless machine reports "ok", which is success-shaped output for a thing
//! that never ran; that is precisely the outcome decision D8 exists to prevent.
//!
//! Run it through `tools/wayland-smoke.sh`, which knows the skip conditions and
//! exits 77 rather than 0 when they apply.

use std::env;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use scrozz_core::{CaptureBackend, CaptureRequest, CaptureTarget, Error, Frame};
use tracing_subscriber::EnvFilter;

/// The exit status for "this could not run here".
///
/// 77 is the automake convention for a skipped test. It matters that it is not
/// 0: a CI job that prints "skipped" and exits 0 is indistinguishable from one
/// that passed, and the whole point of this file is to not claim Wayland capture
/// works when nothing checked.
const EXIT_SKIP: u8 = 77;

fn main() -> ExitCode {
    init_tracing();
    let require = env::args().any(|argument| argument == "--require");

    if let Some(reason) = unrunnable() {
        if require {
            eprintln!("wayland-smoke: FAIL (--require): {reason}");
            return ExitCode::FAILURE;
        }

        eprintln!("wayland-smoke: SKIP: {reason}");
        eprintln!("  Pass --require to treat this as a failure instead.");
        return ExitCode::from(EXIT_SKIP);
    }

    match run() {
        Ok(()) => {
            eprintln!("wayland-smoke: PASS");
            ExitCode::SUCCESS
        }
        Err(Outcome::Skipped(reason)) if !require => {
            eprintln!("wayland-smoke: SKIP: {reason}");
            ExitCode::from(EXIT_SKIP)
        }
        Err(outcome) => {
            eprintln!("wayland-smoke: FAIL: {outcome}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("scrozz_capture=debug,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Why this machine cannot run the smoke test, if it cannot.
///
/// Checked before touching the backend so the skip reason names the missing
/// piece, rather than surfacing as a generic capture failure ten seconds later.
fn unrunnable() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some(format!(
            "this is a Linux-only test and the host is {}",
            env::consts::OS
        ));
    }

    if env::var_os("WAYLAND_DISPLAY").is_none() {
        return Some(
            "WAYLAND_DISPLAY is unset, so there is no Wayland session to capture from. \
             On an X11 session the X11 backend is used instead and this test does not apply."
                .into(),
        );
    }

    // A portal needs a session bus to live on. Without one, nothing downstream
    // can work and the failure would otherwise read as a portal bug.
    if env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
        && !runtime_path("bus").is_some_and(|path| std::path::Path::new(&path).exists())
    {
        return Some(
            "no D-Bus session bus was found, so xdg-desktop-portal cannot be reached. \
             This is normal in a container or over a bare SSH session."
                .into(),
        );
    }

    None
}

/// A path inside `XDG_RUNTIME_DIR`, if that is set.
fn runtime_path(leaf: &str) -> Option<String> {
    env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(|directory| format!("{directory}/{leaf}"))
}

/// How the smoke test ended.
enum Outcome {
    /// A precondition discovered late — after the backend was built.
    Skipped(String),
    /// A real failure.
    Failed(String),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Skipped(reason) | Self::Failed(reason) => write!(f, "{reason}"),
        }
    }
}

fn run() -> Result<(), Outcome> {
    let backend = scrozz_capture::backend().map_err(classify)?;
    eprintln!("wayland-smoke: backend is {}", backend.name());

    if !backend.name().starts_with("xdg-desktop-portal") {
        return Err(Outcome::Skipped(format!(
            "the selected backend is {}, not the Wayland portal. WAYLAND_DISPLAY was set, so \
             this most likely means the compositor was detected as something else.",
            backend.name()
        )));
    }

    check_window_enumeration_is_honest(backend.as_ref())?;
    check_all_displays_is_honest(backend.as_ref())?;

    let displays = backend.displays().map_err(classify)?;
    eprintln!("wayland-smoke: {} display(s) reported", displays.len());
    let Some(display) = displays.iter().find(|display| display.is_primary) else {
        return Err(Outcome::Failed(
            "no display was reported as primary, so there is nothing to capture".into(),
        ));
    };

    eprintln!();
    eprintln!("  A screen-sharing dialog should appear now.");
    eprintln!("  Pick a monitor and confirm. Dismissing it is also a valid result:");
    eprintln!("  this test checks that cancellation is reported as cancellation.");
    eprintln!();

    let request = CaptureRequest::new(CaptureTarget::Display(display.id.clone()));
    let capture = match backend.capture(&request) {
        Ok(capture) => capture,
        Err(Error::Cancelled) => {
            return Err(Outcome::Skipped(
                "the portal dialog was dismissed. Cancellation was reported correctly, which is \
                 itself worth knowing, but no pixels were captured."
                    .into(),
            ));
        }
        Err(error) => return Err(classify(error)),
    };

    report(&capture.frame)?;

    // The second capture is the interesting one: if the restore token was
    // stored and accepted, no dialog appears. A second dialog here means token
    // persistence is broken, which is invisible to any offline test.
    eprintln!();
    eprintln!("  Capturing a second time. If restore tokens work, there will be NO dialog.");
    eprintln!();

    let mut source = match scrozz_capture::frame_session(request.clone()) {
        Ok(source) => source,
        Err(Error::Cancelled) => {
            return Err(Outcome::Failed(
                "the second capture was cancelled, which proves a second dialog appeared. Either \
                 the compositor does not persist tokens, or token storage is broken. Check \
                 $XDG_STATE_HOME/scrozz/portal-tokens."
                    .into(),
            ));
        }
        Err(error) => return Err(classify(error)),
    };
    eprintln!("wayland-smoke: reusable source is {}", source.name());
    let second = source.capture_frame().map_err(classify)?;
    report(&second)?;
    eprintln!(
        "wayland-smoke: second capture succeeded. If no dialog appeared, restore tokens are \
         working."
    );

    eprintln!();
    eprintln!("  Keeping that portal and PipeWire session open for one more frame.");
    eprintln!("  Keep this terminal visible on the selected monitor.");
    eprintln!("  It will print changing markers while the next frame is requested.");
    eprintln!();
    let (third, attempts) = capture_changed_frame(source.as_mut(), &second)?;
    report(&third)?;
    eprintln!(
        "wayland-smoke: repeated capture delivered changed pixels after {attempts} request(s) \
         without reopening the portal or PipeWire stream"
    );
    drop(source);
    eprintln!("wayland-smoke: reusable session dropped; teardown trace should now be complete");

    Ok(())
}

fn capture_changed_frame(
    source: &mut dyn scrozz_capture::FrameSession,
    previous: &Frame,
) -> Result<(Frame, usize), Outcome> {
    let stop = Arc::new(AtomicBool::new(false));
    let marker_stop = Arc::clone(&stop);
    let marker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        for tick in 1..=50 {
            if marker_stop.load(Ordering::Relaxed) {
                break;
            }
            eprintln!("wayland-smoke: visible freshness marker {tick:02}");
            thread::sleep(Duration::from_millis(100));
        }
    });

    let deadline = Instant::now() + Duration::from_secs(6);
    let result = (1..)
        .map(|attempt| {
            source
                .capture_frame()
                .map(|frame| (frame, attempt))
                .map_err(classify)
        })
        .find_map(|result| match result {
            Ok((frame, attempt)) if frames_differ(previous, &frame) => Some(Ok((frame, attempt))),
            Ok(_) if Instant::now() < deadline => None,
            Ok(_) => Some(Err(Outcome::Failed(
                "repeated capture returned only pixel-identical frames for 6 seconds. Keep the \
                 smoke-test terminal visible on the selected monitor so its freshness markers \
                 produce compositor damage."
                    .into(),
            ))),
            Err(error) => Some(Err(error)),
        })
        .expect("the capture loop settles at its deadline");

    stop.store(true, Ordering::Relaxed);
    if marker.join().is_err() {
        return Err(Outcome::Failed(
            "the visible freshness-marker thread panicked".into(),
        ));
    }

    result
}

fn frames_differ(previous: &Frame, next: &Frame) -> bool {
    previous.width() != next.width()
        || previous.height() != next.height()
        || previous.stride != next.stride
        || previous.format != next.format
        || previous.color_space != next.color_space
        || previous.scale != next.scale
        || previous.data != next.data
}

/// All-display capture must fail before the portal can show a picker.
fn check_all_displays_is_honest(backend: &(dyn CaptureBackend + 'static)) -> Result<(), Outcome> {
    let request = CaptureRequest::new(CaptureTarget::AllDisplays);
    match backend.capture(&request) {
        Err(Error::Unsupported { what, why }) => {
            if !why.contains("before opening the portal picker") {
                return Err(Outcome::Failed(format!(
                    "all-display capture was refused, but did not guarantee pre-prompt refusal: \
                     {what}: {why}"
                )));
            }
            eprintln!("wayland-smoke: all-display capture correctly refused before prompting");
            Ok(())
        }
        Err(other) => Err(Outcome::Failed(format!(
            "all-display capture failed with {other}, but should be Unsupported before any portal \
             call"
        ))),
        Ok(_) => Err(Outcome::Failed(
            "all-display capture returned only part of the desktop instead of refusing incomplete \
             multi-stream composition"
                .into(),
        )),
    }
}

/// Wayland has no window enumeration protocol, and per decision D8 the backend
/// says so rather than inventing a list. Confirming that on a real compositor
/// keeps a future "improvement" from quietly reintroducing a fake one.
fn check_window_enumeration_is_honest(
    backend: &(dyn CaptureBackend + 'static),
) -> Result<(), Outcome> {
    match backend.windows() {
        Err(Error::Unsupported { what, why }) => {
            eprintln!("wayland-smoke: window enumeration correctly refused — {what}: {why}");
            Ok(())
        }
        Err(other) => Err(Outcome::Failed(format!(
            "window enumeration failed with {other}, but it should be Unsupported: Wayland has \
             no protocol for it, so the refusal is the correct answer"
        ))),
        Ok(windows) => Err(Outcome::Failed(format!(
            "window enumeration returned {} window(s). Wayland has no enumeration protocol, so \
             this list cannot be real — see decision D8.",
            windows.len()
        ))),
    }
}

/// Prints what came back and checks it is a plausible screenshot.
///
/// A frame that is entirely one colour is the classic symptom of taking the
/// first buffer PipeWire offers, which on an idle desktop is empty. It passes
/// every structural check and is a black rectangle.
fn report(frame: &Frame) -> Result<(), Outcome> {
    eprintln!(
        "wayland-smoke: {}x{} px, stride {}, {:?}, {:?}, scale {}",
        frame.width(),
        frame.height(),
        frame.stride,
        frame.format,
        frame.color_space,
        frame.scale.get()
    );

    if !frame.is_well_formed() {
        return Err(Outcome::Failed(format!(
            "the frame is not well formed: {} bytes for {}x{} at stride {}",
            frame.data.len(),
            frame.width(),
            frame.height(),
            frame.stride
        )));
    }

    if frame.data.is_empty() {
        return Err(Outcome::Failed("the frame has no pixels at all".into()));
    }

    let first = &frame.data[..4.min(frame.data.len())];
    if frame
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| pixel == first)
    {
        return Err(Outcome::Failed(format!(
            "every pixel is {first:?}. A uniform frame usually means an empty PipeWire buffer \
             was accepted instead of waiting for real content — unless the screen genuinely is \
             one flat colour, in which case open a window and re-run."
        )));
    }

    eprintln!("wayland-smoke: pixels vary, so this is a real frame");
    Ok(())
}

/// Turns a capture error into a skip or a failure.
///
/// `Unsupported` is the one that is genuinely ambiguous: it is how a missing
/// PipeWire, a missing portal, and a compositor that cannot do the job all
/// arrive. None of those are defects in this code, so they skip — but the
/// message is printed in full so the operator can see which it was.
fn classify(error: Error) -> Outcome {
    match error {
        Error::Cancelled => Outcome::Skipped("the request was cancelled".into()),
        Error::Unsupported { what, why } => {
            Outcome::Skipped(format!("{what} is unsupported on this machine — {why}"))
        }
        Error::PermissionDenied { capability, remedy } => Outcome::Skipped(format!(
            "screen capture permission was refused for {capability}: {remedy}"
        )),
        other => Outcome::Failed(other.to_string()),
    }
}
