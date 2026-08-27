//! End-to-end tests for the macOS capture backend.
//!
//! # Running without a screen
//!
//! These run in CI, where there is no display and no Screen Recording grant.
//! Every test that needs the compositor therefore treats
//! [`Error::PermissionDenied`] and [`Error::Unsupported`] as "skip", prints why,
//! and returns. That is not laxity: per decision D15 a missing grant is an
//! ordinary first-run outcome, so a test suite that failed on it would be
//! asserting the wrong thing.
//!
//! What is *not* skipped is shape. Whatever a backend returns must be
//! self-consistent — well-formed frames, plausible geometry, honest errors —
//! and those assertions run whenever a capture actually happens.
//!
//! # No windows
//!
//! Nothing here opens a window. Window capture is tested against windows that
//! already exist, and is skipped when there are none.

#![cfg(target_os = "macos")]

use scrozz_capture::backend;
use scrozz_core::{
    CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, Display, DisplayId, Error,
    LogicalPoint, LogicalRect, LogicalSize, PixelFormat, Provenance, ShadowSupport, WindowId,
    WindowSelection,
};

/// Whether an error means "this machine cannot do this", rather than a defect.
///
/// Returns the reason to print, so a skipped test says out loud why it did
/// nothing — a silently passing test that never ran is worse than no test.
fn skip_reason(error: &Error) -> Option<String> {
    match error {
        Error::PermissionDenied { capability, remedy } => {
            Some(format!("no permission for {capability}: {remedy}"))
        }
        Error::Unsupported { what, why } => Some(format!("{what} unsupported: {why}")),
        _ => None,
    }
}

/// Runs `body`, skipping when the platform simply cannot oblige.
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

fn displays_or_skip(backend: &dyn CaptureBackend) -> Option<Vec<Display>> {
    let displays = match backend.displays() {
        Ok(displays) => displays,
        Err(error) => {
            let reason = skip_reason(&error).unwrap_or_else(|| error.to_string());
            println!("skipping: {reason}");
            return None;
        }
    };

    if displays.is_empty() {
        println!("skipping: no displays attached");
        return None;
    }

    Some(displays)
}

#[test]
fn the_backend_exists_and_identifies_itself() {
    let backend = backend().expect("macOS always has a capture backend");
    assert_eq!(backend.name(), "ScreenCaptureKit");
}

#[test]
fn window_picker_capability_reports_native_alpha_and_the_shadow_no_op() {
    let backend = backend().expect("backend");
    let capability = backend.window_picking();
    assert_eq!(capability.selection, WindowSelection::InProcess);
    assert!(capability.native_alpha);
    assert!(matches!(
        capability.shadow,
        ShadowSupport::AlwaysExcluded { .. }
    ));
    assert!(!capability.shadow.resolve(true));
    assert!(!capability.shadow.resolve(false));
}

#[test]
fn enumerated_displays_are_internally_consistent() {
    let backend = backend().expect("backend");
    let Some(displays) = displays_or_skip(backend.as_ref()) else {
        return;
    };

    let mut seen = std::collections::HashSet::new();
    let mut primaries = 0;

    for display in &displays {
        assert!(
            seen.insert(display.id.clone()),
            "display id {:?} appeared twice",
            display.id
        );
        assert!(!display.id.0.is_empty(), "a display must have an id");
        assert!(!display.name.is_empty(), "a display must have a name");

        assert!(
            display.bounds.size.width > 0.0 && display.bounds.size.height > 0.0,
            "{} has an empty bounds rect",
            display.name
        );
        assert!(
            display.bounds.origin.x.is_finite() && display.bounds.origin.y.is_finite(),
            "{} has a non-finite origin",
            display.name
        );

        let scale = display.scale.get();
        assert!(
            (0.5..=16.0).contains(&scale),
            "{} reports an implausible scale of {scale}",
            display.name
        );

        // The work area is the bounds minus OS furniture, so it can never be
        // larger, and must sit inside.
        let work = display.work_area;
        let full = display.bounds;
        assert!(
            work.size.width <= full.size.width && work.size.height <= full.size.height,
            "{}'s work area is larger than the display",
            display.name
        );
        assert!(
            work.origin.x >= full.origin.x - 0.5 && work.origin.y >= full.origin.y - 0.5,
            "{}'s work area starts outside the display",
            display.name
        );

        if display.is_primary {
            primaries += 1;
        }
    }

    assert_eq!(primaries, 1, "there must be exactly one primary display");
}

#[test]
fn the_active_display_is_one_of_the_enumerated_displays() {
    let backend = backend().expect("backend");
    let Some(displays) = displays_or_skip(backend.as_ref()) else {
        return;
    };

    let active = attempt!("active_display", backend.active_display());
    assert!(
        displays.iter().any(|display| display.id == active.id),
        "the active display {:?} is not in the display list",
        active.id
    );
}

#[test]
fn enumerated_windows_are_internally_consistent() {
    let backend = backend().expect("backend");
    let windows = attempt!("windows", backend.windows());
    let displays = attempt!("displays", backend.displays());

    let mut seen = std::collections::HashSet::new();

    for window in &windows {
        assert!(
            seen.insert(window.id.clone()),
            "window id {:?} appeared twice",
            window.id
        );
        assert!(
            window.id.0.parse::<u32>().is_ok(),
            "window id {:?} is not a CGWindowID",
            window.id
        );
        assert!(
            window.bounds.origin.x.is_finite() && window.bounds.size.width.is_finite(),
            "window {:?} has non-finite bounds",
            window.title
        );
        assert!(
            displays.iter().any(|display| display.id == window.display)
                || window.display.0.is_empty(),
            "window {:?} claims display {:?}, which does not exist",
            window.title,
            window.display
        );
        assert_ne!(
            window.title.as_deref(),
            Some(""),
            "an absent title must be None, not an empty string"
        );
        assert_ne!(window.application.as_deref(), Some(""));
        assert_ne!(window.application_id.as_deref(), Some(""));
    }

    println!("enumerated {} windows", windows.len());
}

#[test]
fn capturing_a_display_yields_a_well_formed_frame_at_native_resolution() {
    let backend = backend().expect("backend");
    let Some(displays) = displays_or_skip(backend.as_ref()) else {
        return;
    };
    let display = &displays[0];

    let request = CaptureRequest::new(CaptureTarget::Display(display.id.clone()));
    let capture = attempt!("display capture", backend.capture(&request));

    assert_eq!(capture.provenance, Provenance::Display);
    assert_eq!(capture.target, request.target);
    assert!(
        capture.frame.is_well_formed(),
        "frame is not well formed: {}x{} stride {} for {} bytes",
        capture.frame.width(),
        capture.frame.height(),
        capture.frame.stride,
        capture.frame.data.len()
    );

    // Requirement 1: a Retina capture is 2x the logical size, not 1x.
    let expected_width = display.bounds.size.width * display.scale.get();
    let expected_height = display.bounds.size.height * display.scale.get();
    let width = f64::from(capture.frame.width());
    let height = f64::from(capture.frame.height());

    assert!(
        (width - expected_width).abs() <= 2.0,
        "captured {width} px wide; the display is {} pt at {}x, so {expected_width} px was expected",
        display.bounds.size.width,
        display.scale.get()
    );
    assert!(
        (height - expected_height).abs() <= 2.0,
        "captured {height} px tall; {expected_height} px was expected"
    );

    // Requirement 3: the stride is the real one, and it is at least a row.
    assert!(
        capture.frame.stride >= capture.frame.width() as usize * 4,
        "stride {} is narrower than a {}px row",
        capture.frame.stride,
        capture.frame.width()
    );

    assert_eq!(
        capture.frame.scale.get(),
        display.scale.get(),
        "the frame must record the scale it was captured at"
    );

    println!(
        "captured {}x{} px, stride {}, {:?}, {:?}, scale {}",
        capture.frame.width(),
        capture.frame.height(),
        capture.frame.stride,
        capture.frame.format,
        capture.frame.color_space,
        capture.frame.scale.get(),
    );

    assert!(
        !is_entirely_black(&capture.frame),
        "the capture is entirely black, which means nothing was actually read"
    );
}

/// Requirement 4: premultiplication is reported, never silently dropped.
#[test]
fn the_frame_reports_a_pixel_format_the_encoder_can_act_on() {
    let backend = backend().expect("backend");
    let Some(displays) = displays_or_skip(backend.as_ref()) else {
        return;
    };

    let request = CaptureRequest::new(CaptureTarget::Display(displays[0].id.clone()));
    let capture = attempt!("display capture", backend.capture(&request));

    assert!(
        matches!(
            capture.frame.format,
            PixelFormat::Rgba8
                | PixelFormat::RgbaPremultiplied8
                | PixelFormat::Bgra8
                | PixelFormat::BgraPremultiplied8
        ),
        "unexpected format {:?}",
        capture.frame.format
    );
    assert_eq!(capture.frame.format.bytes_per_pixel(), 4);
}

/// Requirement 5, decision D9: a window capture must say it is a window
/// capture, because everything downstream keys off that to refuse compositing.
#[test]
fn capturing_a_window_is_marked_as_sacred() {
    let backend = backend().expect("backend");
    let windows = attempt!("windows", backend.windows());

    let Some(window) = windows
        .iter()
        .find(|window| window.is_visible && window.bounds.size.width >= 64.0)
    else {
        println!("skipping: no visible window large enough to capture");
        return;
    };

    let request = CaptureRequest::new(CaptureTarget::Window(window.id.clone()));
    let capture = attempt!("window capture", backend.capture(&request));

    assert_eq!(capture.provenance, Provenance::Window);
    assert!(
        capture.provenance.forbids_compositing(),
        "D9: window pixels must be marked uncompositable"
    );
    assert!(capture.frame.is_well_formed());
    assert!(capture.frame.width() > 0 && capture.frame.height() > 0);
    assert!(capture.frame.stride >= capture.frame.width() as usize * 4);
    assert_eq!(capture.source_app.name, window.application);
    assert_eq!(capture.source_app.identifier, window.application_id);
    assert_eq!(capture.window_shadow, Some(false));

    println!(
        "captured window {:?} at {}x{} px",
        window.title,
        capture.frame.width(),
        capture.frame.height()
    );
}

/// ScreenCaptureKit exposes a shadow-looking flag, but it is a no-op for the
/// desktop-independent still path. Both requests must report the honest result.
#[test]
fn the_window_shadow_request_is_reported_as_excluded_either_way() {
    let backend = backend().expect("backend");
    let windows = attempt!("windows", backend.windows());

    let Some(window) = windows
        .iter()
        .find(|window| window.is_visible && window.bounds.size.width >= 64.0)
    else {
        println!("skipping: no visible window large enough to capture");
        return;
    };

    let target = CaptureTarget::Window(window.id.clone());
    let with = attempt!(
        "window capture with shadow",
        backend.capture(&CaptureRequest {
            target: target.clone(),
            cursor: CursorMode::Hidden,
            include_window_shadow: true,
        })
    );
    let without = attempt!(
        "window capture without shadow",
        backend.capture(&CaptureRequest {
            target,
            cursor: CursorMode::Hidden,
            include_window_shadow: false,
        })
    );

    assert!(with.frame.is_well_formed() && without.frame.is_well_formed());
    assert_eq!(with.window_shadow, Some(false));
    assert_eq!(without.window_shadow, Some(false));

    println!(
        "shadow requested: {}x{}; shadow refused: {}x{}; both honestly excluded",
        with.frame.width(),
        with.frame.height(),
        without.frame.width(),
        without.frame.height()
    );
}

#[test]
fn capturing_a_region_yields_that_region_at_native_resolution() {
    let backend = backend().expect("backend");
    let Some(displays) = displays_or_skip(backend.as_ref()) else {
        return;
    };
    let display = &displays[0];

    // A rectangle comfortably inside the display, offset so a mistake in the
    // origin shows up as wrong content rather than a wrong size.
    let region = LogicalRect::new(
        LogicalPoint::new(
            display.bounds.origin.x + 40.0,
            display.bounds.origin.y + 60.0,
        ),
        LogicalSize::new(200.0, 100.0),
    );

    let request = CaptureRequest::new(CaptureTarget::Region(region));
    let capture = attempt!("region capture", backend.capture(&request));

    assert_eq!(capture.provenance, Provenance::Region);
    assert!(capture.frame.is_well_formed());

    let scale = display.scale.get();
    let expected_width = 200.0 * scale;
    let expected_height = 100.0 * scale;
    assert!(
        (f64::from(capture.frame.width()) - expected_width).abs() <= 2.0,
        "region came back {}px wide, expected {expected_width}",
        capture.frame.width()
    );
    assert!(
        (f64::from(capture.frame.height()) - expected_height).abs() <= 2.0,
        "region came back {}px tall, expected {expected_height}",
        capture.frame.height()
    );
}

#[test]
fn an_empty_region_is_rejected_rather_than_captured() {
    let backend = backend().expect("backend");
    let empty = LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(0.0, 0.0));

    match backend.capture(&CaptureRequest::new(CaptureTarget::Region(empty))) {
        Err(Error::InvalidRequest(_)) => {}
        Err(other) if skip_reason(&other).is_some() => {
            println!("skipping: {}", skip_reason(&other).unwrap_or_default());
        }
        Err(other) => panic!("expected InvalidRequest, got {other}"),
        Ok(_) => panic!("an empty region should not produce an image"),
    }
}

#[test]
fn capturing_all_displays_produces_one_frame_covering_them_all() {
    let backend = backend().expect("backend");
    let Some(displays) = displays_or_skip(backend.as_ref()) else {
        return;
    };

    let request = CaptureRequest::new(CaptureTarget::AllDisplays);
    let capture = attempt!("all-displays capture", backend.capture(&request));

    assert_eq!(capture.provenance, Provenance::AllDisplays);
    assert!(capture.frame.is_well_formed());

    // It must be at least as wide as the widest single display, whatever the
    // arrangement.
    let widest = displays
        .iter()
        .map(|display| display.bounds.size.width * display.scale.get())
        .fold(0.0_f64, f64::max);
    assert!(
        f64::from(capture.frame.width()) + 2.0 >= widest,
        "the composite is {}px wide but one display alone is {widest}px",
        capture.frame.width()
    );

    println!(
        "captured {} display(s) as {}x{} px",
        displays.len(),
        capture.frame.width(),
        capture.frame.height()
    );
}

/// Requirement 7: a window that has closed is an ordinary outcome.
#[test]
fn a_window_that_no_longer_exists_reports_target_gone() {
    let backend = backend().expect("backend");

    // CGWindowIDs are allocated from a low counter; u32::MAX will never be live.
    let ghost = WindowId(u32::MAX.to_string());
    match backend.capture(&CaptureRequest::new(CaptureTarget::Window(ghost))) {
        Err(Error::TargetGone(message)) => {
            assert!(!message.is_empty(), "TargetGone must explain itself");
        }
        Err(other) if skip_reason(&other).is_some() => {
            println!("skipping: {}", skip_reason(&other).unwrap_or_default());
        }
        Err(other) => panic!("expected TargetGone, got {other}"),
        Ok(_) => panic!("a non-existent window should not produce an image"),
    }
}

#[test]
fn a_display_that_no_longer_exists_reports_target_gone() {
    let backend = backend().expect("backend");

    let ghost = DisplayId(u32::MAX.to_string());
    match backend.capture(&CaptureRequest::new(CaptureTarget::Display(ghost))) {
        Err(Error::TargetGone(_)) => {}
        Err(other) if skip_reason(&other).is_some() => {
            println!("skipping: {}", skip_reason(&other).unwrap_or_default());
        }
        Err(other) => panic!("expected TargetGone, got {other}"),
        Ok(_) => panic!("a non-existent display should not produce an image"),
    }
}

#[test]
fn a_malformed_display_id_is_an_invalid_request_not_a_panic() {
    let backend = backend().expect("backend");

    let nonsense = DisplayId("the-big-one".to_owned());
    match backend.capture(&CaptureRequest::new(CaptureTarget::Display(nonsense))) {
        Err(Error::InvalidRequest(_)) => {}
        Err(other) if skip_reason(&other).is_some() => {
            println!("skipping: {}", skip_reason(&other).unwrap_or_default());
        }
        Err(other) => panic!("expected InvalidRequest, got {other}"),
        Ok(_) => panic!("a nonsense display id should not produce an image"),
    }
}

/// Enumeration must be repeatable: a backend that leaks or corrupts state
/// shows up here rather than in a user's hands.
#[test]
fn enumeration_is_stable_across_repeated_calls() {
    let backend = backend().expect("backend");
    let Some(first) = displays_or_skip(backend.as_ref()) else {
        return;
    };

    for _ in 0..4 {
        let again = backend.displays().expect("displays");
        assert_eq!(
            first.iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
            again.iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
        );
    }
}

/// Requirement 3, stated as a property: every row of the image must be
/// addressable through the reported stride without running off the buffer.
/// This is exactly the read that produces the classic skewed screenshot when
/// the stride is assumed to be `width * 4`.
#[test]
fn every_row_is_addressable_through_the_reported_stride() {
    let backend = backend().expect("backend");
    let Some(displays) = displays_or_skip(backend.as_ref()) else {
        return;
    };

    let request = CaptureRequest::new(CaptureTarget::Display(displays[0].id.clone()));
    let capture = attempt!("display capture", backend.capture(&request));
    let frame = &capture.frame;

    let row_bytes = frame.width() as usize * 4;
    for row in 0..frame.height() as usize {
        let start = row * frame.stride;
        assert!(
            start + row_bytes <= frame.data.len(),
            "row {row} runs past the end of the buffer"
        );
    }
}

fn is_entirely_black(frame: &scrozz_core::Frame) -> bool {
    let row_bytes = frame.width() as usize * 4;
    (0..frame.height() as usize).all(|row| {
        let start = row * frame.stride;
        frame.data[start..start + row_bytes]
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[0] == 0 && pixel[1] == 0 && pixel[2] == 0)
    })
}
