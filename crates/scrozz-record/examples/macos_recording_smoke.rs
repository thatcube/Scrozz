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
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString};
#[cfg(target_os = "macos")]
use scrozz_core::{CaptureTarget, WindowId};
#[cfg(target_os = "macos")]
use scrozz_record::{
    CameraDeviceId, CameraRequest, RecordingRequest, RecordingState,
    settings::{CameraSettings, CameraShape, OverlayAnchor},
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
        Some("camera") => smoke_camera(),
        Some("camera-preview") => smoke_camera_preview(),
        _ => Err(invalid(
            "usage: macos_recording_smoke <window-disappearance|microphone|camera|camera-preview>",
        )),
    }
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
fn smoke_camera() -> Result<(), Box<dyn Error>> {
    require_opt_in("SCROZZ_RECORD_CAMERA_SMOKE", "camera smoke")?;
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| invalid("camera smoke must start on the process main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();
    app.activate();

    let backend = scrozz_capture::backend()?;
    let display = backend
        .displays()?
        .into_iter()
        .next()
        .ok_or_else(|| invalid("camera smoke requires one attached display"))?;
    let output = TempRecording::new("scrozz-camera-smoke");
    let mut request = RecordingRequest::new(CaptureTarget::Display(display.id));
    request.destination = Some(output.path.clone());
    let settings = CameraSettings {
        enabled: true,
        position: OverlayAnchor::BottomRight,
        shape: CameraShape::Circle,
        ..CameraSettings::default()
    };
    let mut camera = CameraRequest::new(settings);
    if let Ok(device) = std::env::var("SCROZZ_CAMERA_DEVICE") {
        camera = camera.with_device(CameraDeviceId::new(device)?);
    }

    request.camera = Some(camera);

    let mut session = scrozz_record::start(&request)?;
    std::thread::sleep(Duration::from_secs(2));
    let status = session
        .camera_status()
        .ok_or_else(|| invalid("camera smoke exposed no runtime status"))?;
    if !status.active || !status.privacy_indicator_visible || status.frames_received == 0 {
        return Err(invalid(&format!(
            "camera smoke was not visibly active: {status:?}"
        )));
    }
    session.update_camera(CameraSettings {
        presenter: true,
        presenter_screen: true,
        ..settings
    })?;
    std::thread::sleep(Duration::from_secs(2));
    let recording = session.stop()?;
    let metadata = recording
        .metadata
        .camera
        .as_deref()
        .ok_or_else(|| invalid("camera smoke output omitted camera metadata"))?;
    if !metadata.presenter || recording.metadata.frames.unwrap_or(0) == 0 {
        return Err(invalid(
            "camera smoke did not preserve the live presenter transition",
        ));
    }
    if let Some(reason) = recording.partial_reason() {
        return Err(invalid(&format!(
            "camera smoke returned only partial output: {reason}"
        )));
    }
    println!(
        "camera smoke encoded {} frames with {} dropped camera frames at {}",
        recording.metadata.frames.unwrap_or(0),
        metadata.dropped_frames,
        recording.path.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn smoke_camera_preview() -> Result<(), Box<dyn Error>> {
    require_opt_in("SCROZZ_CAMERA_PREVIEW_SMOKE", "camera preview smoke")?;
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| invalid("camera preview smoke must start on the process main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();
    app.activate();

    let settings = CameraSettings {
        enabled: true,
        ..CameraSettings::default()
    };
    let mut request = CameraRequest::new(settings);
    if let Ok(device) = std::env::var("SCROZZ_CAMERA_DEVICE") {
        request = request.with_device(CameraDeviceId::new(device)?);
    }
    let mut preview = scrozz_record::start_camera_preview(&request)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let frame = loop {
        pump_app_events(&app, Duration::from_millis(20));
        if let Some(frame) = preview.poll() {
            break frame;
        }
        if Instant::now() >= deadline {
            return Err(invalid(
                "camera preview produced no frame within 10 seconds",
            ));
        }
    };
    if !frame.status.active || !frame.status.privacy_indicator_visible {
        return Err(invalid("camera preview privacy indicator was not active"));
    }
    let width = frame.frame.pixels.width();
    let height = frame.frame.pixels.height();
    preview.stop();
    println!("camera preview smoke captured {width}x{height}");
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
