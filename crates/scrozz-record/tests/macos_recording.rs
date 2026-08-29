//! Permission-safe end-to-end checks for the macOS recording backend.
//!
//! The real capture test is opt-in because ordinary tests must never touch the
//! screen service or request microphone access. A missing Screen Recording
//! grant is an expected machine outcome and is reported as a skip.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use scrozz_core::{
    CaptureTarget, ColorSpace, Display, Error, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalPoint, PhysicalSize, PixelFormat, ScaleFactor,
};
use scrozz_record::{
    OverlayLayer, OverlaySource, RecordingRequest, RecordingResolution, SessionEvent, VideoCodec,
};

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

struct SolidOverlays {
    calls: Arc<AtomicU64>,
}

impl OverlaySource for SolidOverlays {
    fn layers(&mut self, _elapsed: Duration, _canvas: PhysicalSize) -> Vec<OverlayLayer> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Vec::from([OverlayLayer {
            content: Frame {
                data: [0_u8, 0, 255, 255].repeat(16 * 16),
                size: PhysicalSize::new(16.0, 16.0),
                stride: 16 * 4,
                format: PixelFormat::Bgra8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            origin: PhysicalPoint::new(4.0, 4.0),
            opacity: 0.75,
            adaptive_contrast: false,
        }])
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
    std::thread::sleep(Duration::from_millis(350));
    assert!(matches!(session.poll(), Some(SessionEvent::FirstFrame)));
    assert!(
        session.poll().is_none(),
        "the native first-frame signal must be one-shot"
    );
    let before_pause = session.engine_elapsed_secs().unwrap_or_default();
    session.pause().expect("pause recording");
    std::thread::sleep(Duration::from_millis(200));
    let after_pause = session.engine_elapsed_secs().unwrap_or_default();
    assert!(
        (after_pause - before_pause).abs() < 0.03,
        "elapsed time advanced while paused: {before_pause:?} to {after_pause:?}"
    );
    session.resume().expect("resume recording");
    std::thread::sleep(Duration::from_millis(350));
    let elapsed = session.engine_elapsed_secs().unwrap_or_default();
    let recording = attempt!("recording stop", session.stop());

    let size = recording.metadata.size.expect("native size");
    assert!(recording.metadata.frames.is_some_and(|frames| frames > 0));
    assert!(
        recording
            .metadata
            .audio_channels
            .is_some_and(|channels| channels > 0)
    );
    assert!(!recording.is_partial(), "{:?}", recording.completion);
    assert!(size.width >= 2.0 && size.height >= 2.0);
    assert_eq!(size.width as u32 % 2, 0);
    assert_eq!(size.height as u32 % 2, 0);
    let expected_width = (display.bounds.size.width * display.scale.get()) as u32 & !1;
    let expected_height = (display.bounds.size.height * display.scale.get()) as u32 & !1;
    assert_eq!(size.width as u32, expected_width);
    assert_eq!(size.height as u32, expected_height);
    assert!(
        (recording.duration_secs - elapsed).abs() < 0.2,
        "file duration {} disagrees with elapsed {elapsed:?}",
        recording.duration_secs
    );
    assert!(
        std::fs::metadata(&recording.path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false),
        "recording file is missing or empty"
    );
    assert_no_asset_writer_sidecars(&recording.path);
}

#[test]
fn opt_in_smoke_rebases_video_only_frames_after_pause() {
    if std::env::var_os("SCROZZ_RECORD_SMOKE").as_deref() != Some("1".as_ref()) {
        println!("skipping video-only pause smoke: set SCROZZ_RECORD_SMOKE=1 to enable it");
        return;
    }

    let backend = scrozz_capture::backend().expect("macOS capture backend");
    let displays = attempt!("display enumeration", backend.displays());
    let Some(display) = displays.first() else {
        println!("skipping video-only pause smoke: no displays are attached");
        return;
    };
    let output = TempRecording(smoke_path("scrozz-video-only-pause-smoke"));
    let mut request = RecordingRequest::new(CaptureTarget::Display(display.id.clone()));
    request.destination = Some(output.0.clone());
    request.system_audio = false;
    request.microphone = false;
    request.video_codec = VideoCodec::H264;
    request.resolution = RecordingResolution::ScalePercent(50);

    let mut session = attempt!("video-only recording start", scrozz_record::start(&request));
    std::thread::sleep(Duration::from_millis(350));
    session.pause().expect("pause video-only recording");
    std::thread::sleep(Duration::from_millis(200));
    session.resume().expect("resume video-only recording");
    std::thread::sleep(Duration::from_millis(350));
    let recording = attempt!("video-only recording stop", session.stop());

    assert!(recording.metadata.frames.is_some_and(|frames| frames > 0));
    assert!(!recording.is_partial(), "{:?}", recording.completion);
    assert!(recording.duration_secs > 0.0);
    assert_no_asset_writer_sidecars(&recording.path);
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
    let size = recording.metadata.size.expect("native size");
    assert_eq!(size.width as u32, expected_width);
    assert_eq!(size.height as u32, expected_height);
    assert!(recording.metadata.frames.is_some_and(|frames| frames > 0));
    assert_eq!(recording.metadata.audio_channels, Some(0));
    assert!(!recording.is_partial(), "{:?}", recording.completion);
    assert!(
        std::fs::metadata(&recording.path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false),
        "region recording file is missing or empty"
    );
}

#[test]
fn opt_in_smoke_records_all_displays_and_a_region_across_their_seam() {
    if std::env::var_os("SCROZZ_RECORD_SMOKE").as_deref() != Some("1".as_ref()) {
        println!("skipping multi-display smoke: set SCROZZ_RECORD_SMOKE=1 to enable it");
        return;
    }

    let backend = scrozz_capture::backend().expect("macOS capture backend");
    let displays = attempt!("display enumeration", backend.displays());
    if displays.len() < 2 {
        println!("skipping multi-display smoke: fewer than two displays are attached");
        return;
    }

    let output = TempRecording(smoke_path("scrozz-all-displays-smoke"));
    let mut request = RecordingRequest::new(CaptureTarget::AllDisplays);
    request.destination = Some(output.0.clone());
    request.resolution = RecordingResolution::MaxShortestEdge(720);
    request.microphone = false;
    let session = attempt!(
        "all-display recording start",
        scrozz_record::start(&request)
    );
    std::thread::sleep(Duration::from_millis(450));
    let recording = attempt!("all-display recording stop", session.stop());
    assert!(recording.metadata.frames.is_some_and(|frames| frames > 0));
    assert!(!recording.is_partial(), "{:?}", recording.completion);
    let size = recording.metadata.size.expect("native size");
    assert!(size.width.min(size.height) <= 720.0);

    let Some((region, expected_scale)) = seam_region(&displays) else {
        println!("skipping spanning-region smoke: attached displays have no shared edge");
        return;
    };
    let output = TempRecording(smoke_path("scrozz-spanning-region-smoke"));
    let mut request = RecordingRequest::new(CaptureTarget::Region(region));
    request.destination = Some(output.0.clone());
    request.microphone = false;
    let session = attempt!(
        "spanning-region recording start",
        scrozz_record::start(&request)
    );
    std::thread::sleep(Duration::from_millis(450));
    let recording = attempt!("spanning-region recording stop", session.stop());
    assert!(recording.metadata.frames.is_some_and(|frames| frames > 0));
    assert!(!recording.is_partial(), "{:?}", recording.completion);
    let size = recording.metadata.size.expect("native size");
    assert_eq!(
        size.width as u32,
        (region.size.width * expected_scale) as u32 & !1
    );
    assert_eq!(
        size.height as u32,
        (region.size.height * expected_scale) as u32 & !1
    );
}

#[test]
fn opt_in_smoke_uses_the_explicit_hevc_hardware_path() {
    if std::env::var_os("SCROZZ_RECORD_SMOKE").as_deref() != Some("1".as_ref()) {
        println!("skipping HEVC smoke: set SCROZZ_RECORD_SMOKE=1 to enable it");
        return;
    }
    let backend = scrozz_capture::backend().expect("macOS capture backend");
    let displays = attempt!("display enumeration", backend.displays());
    let Some(display) = displays.first() else {
        println!("skipping HEVC smoke: no displays are attached");
        return;
    };

    let output = TempRecording(smoke_path("scrozz-hevc-smoke"));
    let mut request = RecordingRequest::new(CaptureTarget::Region(inset_region(display)));
    request.destination = Some(output.0.clone());
    request.video_codec = VideoCodec::Hevc;
    request.microphone = false;
    let session = attempt!("HEVC recording start", scrozz_record::start(&request));
    std::thread::sleep(Duration::from_millis(450));
    let recording = attempt!("HEVC recording stop", session.stop());

    assert!(recording.metadata.frames.is_some_and(|frames| frames > 0));
    assert!(!recording.is_partial(), "{:?}", recording.completion);
}

#[test]
fn opt_in_smoke_composites_a_non_empty_overlay_into_native_frames() {
    if std::env::var_os("SCROZZ_RECORD_SMOKE").as_deref() != Some("1".as_ref()) {
        println!("skipping overlay smoke: set SCROZZ_RECORD_SMOKE=1 to enable it");
        return;
    }
    let backend = scrozz_capture::backend().expect("macOS capture backend");
    let displays = attempt!("display enumeration", backend.displays());
    let Some(display) = displays.first() else {
        println!("skipping overlay smoke: no displays are attached");
        return;
    };

    let calls = Arc::new(AtomicU64::new(0));
    let output = TempRecording(smoke_path("scrozz-overlay-smoke"));
    let mut request = RecordingRequest::new(CaptureTarget::Region(inset_region(display)));
    request.destination = Some(output.0.clone());
    request.microphone = false;
    let session = attempt!(
        "overlay recording start",
        scrozz_record::start_with_overlays(
            &request,
            Box::new(SolidOverlays {
                calls: Arc::clone(&calls),
            }),
        )
    );
    std::thread::sleep(Duration::from_millis(450));
    let recording = attempt!("overlay recording stop", session.stop());

    assert!(
        calls.load(Ordering::Relaxed) > 1,
        "time-varying overlays must continue on ScreenCaptureKit idle frames"
    );
    assert!(recording.metadata.frames.is_some_and(|frames| frames > 0));
    assert!(!recording.is_partial(), "{:?}", recording.completion);
}

fn smoke_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{label}-{nonce}.mp4"))
}

fn assert_no_asset_writer_sidecars(path: &std::path::Path) {
    let file_name = path.file_name().expect("recording file name");
    let prefix = format!("{}.sb-", file_name.to_string_lossy());
    let parent = path.parent().expect("recording parent");
    let leaked = std::fs::read_dir(parent)
        .expect("recording parent remains readable")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.file_name())
        .any(|name| name.to_string_lossy().starts_with(&prefix));
    assert!(
        !leaked,
        "AVAssetWriter sidecar leaked for {}",
        path.display()
    );
}

fn inset_region(display: &Display) -> LogicalRect {
    let width = display.bounds.size.width.clamp(80.0, 320.0) - 40.0;
    let height = display.bounds.size.height.clamp(80.0, 200.0) - 40.0;
    LogicalRect::new(
        LogicalPoint::new(
            display.bounds.origin.x + 20.0,
            display.bounds.origin.y + 20.0,
        ),
        LogicalSize::new(width, height),
    )
}

fn seam_region(displays: &[Display]) -> Option<(LogicalRect, f64)> {
    const HALF_SPAN: f64 = 20.0;
    const EDGE_EPSILON: f64 = 0.5;
    for (index, first) in displays.iter().enumerate() {
        for second in &displays[index + 1..] {
            let first_right = first.bounds.origin.x + first.bounds.size.width;
            let second_right = second.bounds.origin.x + second.bounds.size.width;
            let first_bottom = first.bounds.origin.y + first.bounds.size.height;
            let second_bottom = second.bounds.origin.y + second.bounds.size.height;
            let overlap_y_start = first.bounds.origin.y.max(second.bounds.origin.y);
            let overlap_y_end = first_bottom.min(second_bottom);
            let overlap_x_start = first.bounds.origin.x.max(second.bounds.origin.x);
            let overlap_x_end = first_right.min(second_right);
            let scale = first.scale.get().min(second.scale.get());

            if (first_right - second.bounds.origin.x).abs() <= EDGE_EPSILON
                && overlap_y_end - overlap_y_start >= HALF_SPAN * 2.0
            {
                return Some((
                    LogicalRect::new(
                        LogicalPoint::new(first_right - HALF_SPAN, overlap_y_start),
                        LogicalSize::new(HALF_SPAN * 2.0, HALF_SPAN * 2.0),
                    ),
                    scale,
                ));
            }
            if (second_right - first.bounds.origin.x).abs() <= EDGE_EPSILON
                && overlap_y_end - overlap_y_start >= HALF_SPAN * 2.0
            {
                return Some((
                    LogicalRect::new(
                        LogicalPoint::new(second_right - HALF_SPAN, overlap_y_start),
                        LogicalSize::new(HALF_SPAN * 2.0, HALF_SPAN * 2.0),
                    ),
                    scale,
                ));
            }
            if (first_bottom - second.bounds.origin.y).abs() <= EDGE_EPSILON
                && overlap_x_end - overlap_x_start >= HALF_SPAN * 2.0
            {
                return Some((
                    LogicalRect::new(
                        LogicalPoint::new(overlap_x_start, first_bottom - HALF_SPAN),
                        LogicalSize::new(HALF_SPAN * 2.0, HALF_SPAN * 2.0),
                    ),
                    scale,
                ));
            }
            if (second_bottom - first.bounds.origin.y).abs() <= EDGE_EPSILON
                && overlap_x_end - overlap_x_start >= HALF_SPAN * 2.0
            {
                return Some((
                    LogicalRect::new(
                        LogicalPoint::new(overlap_x_start, second_bottom - HALF_SPAN),
                        LogicalSize::new(HALF_SPAN * 2.0, HALF_SPAN * 2.0),
                    ),
                    scale,
                ));
            }
        }
    }
    None
}
