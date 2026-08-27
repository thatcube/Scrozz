//! Permission-safe end-to-end checks for the macOS recording backend.
//!
//! The real capture test is opt-in because ordinary tests must never touch the
//! screen service or request microphone access. A missing Screen Recording
//! grant is an expected machine outcome and is reported as a skip.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use scrozz_core::{CaptureTarget, Error, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize};
use scrozz_record::{OverlayLayer, OverlaySource, RecordingRequest, RecordingState};

struct TempRecording(PathBuf);

impl Drop for TempRecording {
    fn drop(&mut self) {
        if let Err(failure) = std::fs::remove_file(&self.0)
            && failure.kind() != std::io::ErrorKind::NotFound
        {
            panic!(
                "failed to remove smoke recording {}: {failure}",
                self.0.display()
            );
        }
    }
}

struct EmptyOverlays;

impl OverlaySource for EmptyOverlays {
    fn layers(&mut self, _elapsed: Duration, _canvas: PhysicalSize) -> Vec<OverlayLayer> {
        Vec::new()
    }
}

fn skip_reason(error: &Error) -> Option<String> {
    match error {
        Error::PermissionDenied { capability, remedy } => {
            Some(format!("no permission for {capability}: {remedy}"))
        }
        Error::Unsupported { what, why } => Some(format!("{what} unsupported: {why}")),
        _ => None,
    }
}

macro_rules! attempt {
    ($label:literal, $body:expr) => {
        match $body {
            Ok(value) => value,
            Err(error) => match skip_reason(&error) {
                Some(reason) => {
                    println!("skipping {}: {reason}", $label);
                    return;
                }
                None => panic!("{} failed: {error}", $label),
            },
        }
    };
}

#[test]
fn invalid_requests_fail_before_any_permission_service_is_touched() {
    let mut request = RecordingRequest::new(CaptureTarget::AllDisplays);
    request.fps = 0;
    request.microphone = true;

    assert!(matches!(
        scrozz_record::start(&request),
        Err(Error::InvalidRequest(_))
    ));
}

#[test]
fn opt_in_smoke_records_and_rebases_a_short_pause() {
    if std::env::var_os("SCROZZ_RECORD_SMOKE").as_deref() != Some("1".as_ref()) {
        println!("skipping recording smoke: set SCROZZ_RECORD_SMOKE=1 to enable it");
        return;
    }

    let backend = scrozz_capture::backend().expect("macOS capture backend");
    let displays = attempt!("display enumeration", backend.displays());
    let Some(display) = displays.first() else {
        println!("skipping recording smoke: no displays are attached");
        return;
    };
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after Unix epoch")
        .as_nanos();
    let output =
        TempRecording(std::env::temp_dir().join(format!("scrozz-recording-smoke-{nonce}.mp4")));
    let mut request = RecordingRequest::new(CaptureTarget::Display(display.id.clone()));
    request.destination = Some(output.0.clone());
    request.system_audio = true;
    request.microphone = false;
    request.show_cursor = true;

    let mut session = attempt!("recording start", scrozz_record::start(&request));
    assert_eq!(session.state(), RecordingState::Recording);
    std::thread::sleep(Duration::from_millis(350));
    let before_pause = session.elapsed();
    session.pause().expect("pause recording");
    assert_eq!(session.state(), RecordingState::Paused);
    std::thread::sleep(Duration::from_millis(200));
    let after_pause = session.elapsed();
    assert!(
        after_pause.abs_diff(before_pause) < Duration::from_millis(30),
        "elapsed time advanced while paused: {before_pause:?} to {after_pause:?}"
    );
    session.resume().expect("resume recording");
    std::thread::sleep(Duration::from_millis(350));
    let elapsed = session.elapsed();
    let recording = attempt!("recording stop", session.stop());

    assert!(recording.frames > 0);
    assert!(recording.has_audio);
    assert!(recording.partial.is_none(), "{:?}", recording.partial);
    assert!(recording.size.width >= 2.0 && recording.size.height >= 2.0);
    assert_eq!(recording.size.width as u32 % 2, 0);
    assert_eq!(recording.size.height as u32 % 2, 0);
    let expected_width = (display.bounds.size.width * display.scale.get()) as u32 & !1;
    let expected_height = (display.bounds.size.height * display.scale.get()) as u32 & !1;
    assert_eq!(recording.size.width as u32, expected_width);
    assert_eq!(recording.size.height as u32, expected_height);
    assert!(
        (recording.duration_secs - elapsed.as_secs_f64()).abs() < 0.2,
        "file duration {} disagrees with elapsed {elapsed:?}",
        recording.duration_secs
    );
    assert!(
        std::fs::metadata(&recording.path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false),
        "recording file is missing or empty"
    );
}

#[test]
fn opt_in_smoke_records_a_region_through_the_empty_overlay_fast_path() {
    if std::env::var_os("SCROZZ_RECORD_SMOKE").as_deref() != Some("1".as_ref()) {
        println!("skipping region recording smoke: set SCROZZ_RECORD_SMOKE=1 to enable it");
        return;
    }

    let backend = scrozz_capture::backend().expect("macOS capture backend");
    let displays = attempt!("display enumeration", backend.displays());
    let Some(display) = displays.first() else {
        println!("skipping region recording smoke: no displays are attached");
        return;
    };
    if display.bounds.size.width < 100.0 || display.bounds.size.height < 100.0 {
        println!("skipping region recording smoke: display is too small");
        return;
    }

    let logical_width = display.bounds.size.width.min(320.0) - 40.0;
    let logical_height = display.bounds.size.height.min(200.0) - 40.0;
    let region = LogicalRect::new(
        LogicalPoint::new(
            display.bounds.origin.x + 20.0,
            display.bounds.origin.y + 20.0,
        ),
        LogicalSize::new(logical_width, logical_height),
    );
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after Unix epoch")
        .as_nanos();
    let output = TempRecording(
        std::env::temp_dir().join(format!("scrozz-region-recording-smoke-{nonce}.mp4")),
    );
    let mut request = RecordingRequest::new(CaptureTarget::Region(region));
    request.destination = Some(output.0.clone());
    request.microphone = false;

    let session = attempt!(
        "region recording start",
        scrozz_record::start_with_overlays(&request, Box::new(EmptyOverlays))
    );
    std::thread::sleep(Duration::from_millis(350));
    let recording = attempt!("region recording stop", session.stop());

    let expected_width = (logical_width * display.scale.get()) as u32 & !1;
    let expected_height = (logical_height * display.scale.get()) as u32 & !1;
    assert_eq!(recording.size.width as u32, expected_width);
    assert_eq!(recording.size.height as u32, expected_height);
    assert!(recording.frames > 0);
    assert!(!recording.has_audio);
    assert!(recording.partial.is_none(), "{:?}", recording.partial);
    assert!(
        std::fs::metadata(&recording.path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false),
        "region recording file is missing or empty"
    );
}
