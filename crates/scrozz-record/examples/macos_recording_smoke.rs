//! Explicit native smoke harness; never runs as part of `cargo test`.

#[cfg(target_os = "macos")]
use std::error::Error;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
use objc2::{MainThreadMarker, MainThreadOnly};
#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask, NSWindow,
    NSWindowStyleMask,
};
#[cfg(target_os = "macos")]
use objc2_core_foundation::CGPoint;
#[cfg(target_os = "macos")]
use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString};
#[cfg(target_os = "macos")]
use scrozz_core::{CaptureTarget, WindowId};
#[cfg(target_os = "macos")]
use scrozz_record::{
    RecordingRequest, RecordingSettings, RecordingState, active_input_monitors,
    edit::VideoDocument, settings::KeystrokeScope,
};

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the native recording smoke harness is macOS-only");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    let result = objc2::rc::autoreleasepool(|_| run());
    if let Err(failure) = result {
        eprintln!("recording smoke failed: {failure}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn run() -> Result<(), Box<dyn Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("window-disappearance") => smoke_window_disappearance(),
        Some("microphone") => smoke_microphone(),
        Some("interactions") => smoke_interactions(),
        _ => Err(invalid(
            "usage: macos_recording_smoke <window-disappearance|microphone|interactions>",
        )),
    }
}

#[cfg(target_os = "macos")]
fn smoke_interactions() -> Result<(), Box<dyn Error>> {
    require_opt_in(
        "SCROZZ_RECORD_INTERACTION_SMOKE",
        "recording-interaction smoke",
    )?;
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| invalid("interaction smoke must start on the process main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let backend = scrozz_capture::backend()?;
    let display = backend.active_display()?;
    let output = TempRecording::new("scrozz-interaction-smoke");
    let mut settings = RecordingSettings::shipped();
    settings.countdown.enabled = false;
    settings.clicks.enabled = true;
    settings.keystrokes.enabled = true;
    settings.keystrokes.scope = KeystrokeScope::ModifiersOnly;
    settings.cursor_smoothing = true;
    settings.after_capture.open_editor = true;
    let mut request =
        RecordingRequest::from_settings(CaptureTarget::Display(display.id), &settings);
    request.destination = Some(output.path.clone());
    if active_input_monitors() != 0 {
        return Err(invalid(
            "an input hook was active before the smoke recording",
        ));
    }
    let session = scrozz_record::start_with_settings(&request, &settings)?;
    if active_input_monitors() != 1 {
        return Err(invalid(
            "the recording did not install exactly one input hook",
        ));
    }
    std::thread::sleep(Duration::from_millis(250));
    post_interaction_smoke_events()?;
    std::thread::sleep(Duration::from_secs(2));
    let recording = session.stop()?;
    if active_input_monitors() != 0 {
        return Err(invalid("the input hook survived recording finalisation"));
    }
    let summary = recording
        .interactions()
        .ok_or_else(|| invalid("the recording retained no interaction timeline"))?
        .summary();
    if summary.clicks == 0 || summary.keystrokes == 0 || summary.cursor_samples == 0 {
        return Err(invalid(&format!(
            "native interaction capture was incomplete: {summary:?}"
        )));
    }
    if !summary.editable || summary.truncated {
        return Err(invalid(&format!(
            "native interaction timeline is not safely editable: {summary:?}"
        )));
    }
    let document = VideoDocument::open_native(recording.clone())?;
    if document.duration().is_zero() {
        return Err(invalid("the retained private source is not playable"));
    }
    println!(
        "interaction smoke rendered clicks, modifiers, and cursor samples; hook count returned to zero: {summary:?}"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn post_interaction_smoke_events() -> Result<(), Box<dyn Error>> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .ok_or_else(|| invalid("CoreGraphics did not create a smoke event source"))?;
    let cursor = CGEvent::new(None)
        .map(|event| CGEvent::location(Some(&event)))
        .unwrap_or_else(|| CGPoint::new(32.0, 32.0));
    for event_type in [CGEventType::LeftMouseDown, CGEventType::LeftMouseUp] {
        let event =
            CGEvent::new_mouse_event(Some(&source), event_type, cursor, CGMouseButton::Left)
                .ok_or_else(|| invalid("CoreGraphics did not create a smoke mouse event"))?;
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    for down in [true, false] {
        let event = CGEvent::new_keyboard_event(Some(&source), 40, down)
            .ok_or_else(|| invalid("CoreGraphics did not create a smoke key event"))?;
        CGEvent::set_flags(Some(&event), CGEventFlags::MaskCommand);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn smoke_window_disappearance() -> Result<(), Box<dyn Error>> {
    require_opt_in("SCROZZ_RECORD_WINDOW_SMOKE", "window-disappearance smoke")?;
    let mtm = MainThreadMarker::new().ok_or_else(|| {
        invalid("window-disappearance smoke must start on the process main thread")
    })?;
    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();

    // SAFETY: main-thread AppKit construction with a live application.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(160.0, 160.0), NSSize::new(480.0, 280.0)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the retained window must outlive close so SCK can report inactivity.
    unsafe {
        window.setReleasedWhenClosed(false);
    }
    window.setTitle(&NSString::from_str("Scrozz Recording Disappearance Smoke"));
    window.center();
    window.orderFrontRegardless();
    window.display();
    app.activate();
    app.updateWindows();
    pump_app_events(&app, Duration::from_millis(500));

    let window_number = u32::try_from(window.windowNumber())
        .map_err(|_| invalid("AppKit returned an invalid smoke window number"))?;
    let output = TempRecording::new("scrozz-window-disappearance-smoke");
    let mut request =
        RecordingRequest::new(CaptureTarget::Window(WindowId(window_number.to_string())));
    request.destination = Some(output.path.clone());
    request.microphone = false;
    let session = scrozz_record::start(&request)?;
    // Run beyond the writer's five-second fragment interval so interruption can
    // prove that a previously written MP4 fragment remains salvageable.
    let origin = window.frame().origin;
    for step in 0..65 {
        let offset = f64::from(step % 2) * 2.0;
        window.setFrameOrigin(NSPoint::new(origin.x + offset, origin.y));
        pump_app_events(&app, Duration::from_millis(100));
    }

    window.orderOut(None);
    window.close();
    app.updateWindows();
    let deadline = Instant::now() + Duration::from_secs(3);
    while session.state() != RecordingState::Stopped && Instant::now() < deadline {
        pump_app_events(&app, Duration::from_millis(20));
    }
    if session.state() != RecordingState::Stopped {
        return Err(invalid(
            "ScreenCaptureKit did not report the closed window as inactive",
        ));
    }

    let recording = session.stop()?;
    let reason = recording.partial_reason().ok_or_else(|| {
        invalid("window disappearance produced a complete result instead of a partial result")
    })?;
    let frames = recording.metadata.frames.unwrap_or(0);
    if frames == 0 {
        return Err(invalid(
            "window disappearance smoke encoded no video frames",
        ));
    }
    println!(
        "window disappearance preserved {} frames at {}: {reason}",
        frames,
        recording.path.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn pump_app_events(app: &NSApplication, duration: Duration) {
    let deadline = Instant::now() + duration;
    // SAFETY: Foundation initializes this process-global run-loop mode.
    let mode = unsafe { NSDefaultRunLoopMode };
    while Instant::now() < deadline {
        let expiration = NSDate::dateWithTimeIntervalSinceNow(0.01);
        if let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&expiration),
            mode,
            true,
        ) {
            app.sendEvent(&event);
        }
        app.updateWindows();
    }
}

#[cfg(target_os = "macos")]
fn smoke_microphone() -> Result<(), Box<dyn Error>> {
    require_opt_in("SCROZZ_RECORD_MIC_SMOKE", "microphone smoke")?;
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| invalid("microphone smoke must start on the process main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let backend = scrozz_capture::backend()?;
    let display = backend
        .displays()?
        .into_iter()
        .next()
        .ok_or_else(|| invalid("microphone smoke requires one attached display"))?;
    let output = TempRecording::new("scrozz-microphone-smoke");
    let mut request = RecordingRequest::new(CaptureTarget::Display(display.id));
    request.destination = Some(output.path.clone());
    request.microphone = true;
    request.system_audio = false;
    let session = scrozz_record::start(&request)?;
    std::thread::sleep(Duration::from_secs(2));
    let recording = session.stop()?;
    if !recording
        .metadata
        .audio_channels
        .is_some_and(|channels| channels > 0)
    {
        return Err(invalid(
            "microphone smoke completed without an encoded audio track",
        ));
    }
    if let Some(reason) = recording.partial_reason() {
        return Err(invalid(&format!(
            "microphone smoke returned only partial output: {reason}"
        )));
    }
    println!(
        "microphone smoke encoded {} frames with audio at {}",
        recording.metadata.frames.unwrap_or(0),
        recording.path.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn require_opt_in(variable: &str, label: &str) -> Result<(), Box<dyn Error>> {
    if std::env::var_os(variable).as_deref() == Some("1".as_ref()) {
        Ok(())
    } else {
        Err(invalid(&format!(
            "{label} is disabled; set {variable}=1 explicitly"
        )))
    }
}

#[cfg(target_os = "macos")]
fn invalid(message: &str) -> Box<dyn Error> {
    std::io::Error::other(message.to_owned()).into()
}

#[cfg(target_os = "macos")]
struct TempRecording {
    path: PathBuf,
}

#[cfg(target_os = "macos")]
impl TempRecording {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after Unix epoch")
            .as_nanos();
        Self {
            path: std::env::temp_dir().join(format!("{label}-{nonce}.mp4")),
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for TempRecording {
    fn drop(&mut self) {
        if let Err(failure) = std::fs::remove_file(&self.path)
            && failure.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "warning: failed to remove smoke output {}: {failure}",
                self.path.display()
            );
        }
    }
}
