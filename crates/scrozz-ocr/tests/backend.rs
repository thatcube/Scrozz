//! Backend availability and error mapping.
//!
//! The Linux half runs on Linux; the macOS half runs a genuine recognition. On
//! Windows this reduces to the availability check, because a cross-compiled
//! type check cannot execute anything (see `docs/platforms.md`).

use scrozz_core::{ColorSpace, Frame, PhysicalSize, PixelFormat, ScaleFactor};
use scrozz_ocr::{Ocr, Options, SystemOcr, UpscalePolicy};

fn blank(width: u32, height: u32, scale: f64) -> Frame {
    let stride = width as usize * 4;
    Frame {
        data: vec![255; stride * height as usize],
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride,
        format: PixelFormat::Bgra8,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::new(scale),
    }
}

#[test]
fn availability_matches_the_runtime_that_will_be_used() {
    #[cfg(target_os = "macos")]
    assert!(SystemOcr::is_runtime_available());

    #[cfg(all(target_os = "linux", feature = "tesseract"))]
    assert_eq!(
        SystemOcr::is_runtime_available(),
        SystemOcr::new()
            .available_languages()
            .is_ok_and(|languages| !languages.is_empty())
    );

    #[cfg(not(any(
        target_os = "macos",
        target_os = "windows",
        all(target_os = "linux", feature = "tesseract")
    )))]
    assert!(!SystemOcr::is_runtime_available());
}

#[test]
fn build_capability_remains_const_compatible() {
    const BUILT_WITH_OCR: bool = SystemOcr::is_available();
    assert_eq!(
        BUILT_WITH_OCR,
        cfg!(any(
            target_os = "macos",
            target_os = "windows",
            all(target_os = "linux", feature = "tesseract")
        ))
    );
}

#[cfg(all(target_os = "linux", feature = "tesseract"))]
#[test]
fn public_availability_reacts_to_same_process_runtime_changes() {
    use std::{os::unix::fs::PermissionsExt as _, process::Command};

    let root = std::env::temp_dir().join(format!(
        "scrozz-public-ocr-availability-{}",
        std::process::id()
    ));
    let available = root.join("available");
    let missing = root.join("missing");
    let _ = std::fs::remove_dir_all(&root);
    for directory in [&available, &missing] {
        std::fs::create_dir_all(directory.join("tessdata")).expect("tessdata directory");
    }
    let executable = available.join("tesseract");
    std::fs::write(
        &executable,
        "#!/bin/sh\nprintf 'List of available languages (1):\\neng\\n'\n",
    )
    .expect("fake Tesseract");
    let mut permissions = std::fs::metadata(&executable)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).expect("executable fixture");

    let status = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--ignored", "--exact", "public_availability_probe_child"])
        .env("SCROZZ_AVAILABLE_TESSERACT", &available)
        .env("SCROZZ_MISSING_TESSERACT", &missing)
        .status()
        .expect("availability child");
    assert!(status.success());
    std::fs::remove_dir_all(root).expect("remove fixture");
}

#[cfg(all(target_os = "linux", feature = "tesseract"))]
#[test]
#[ignore = "launched in isolation by public_availability_reacts_to_same_process_runtime_changes"]
fn public_availability_probe_child() {
    let available = std::env::var_os("SCROZZ_AVAILABLE_TESSERACT").expect("available runtime");
    let missing = std::env::var_os("SCROZZ_MISSING_TESSERACT").expect("missing runtime");

    // SAFETY: this ignored probe runs as the only test in a dedicated child
    // process, so no other thread can read or write the process environment.
    unsafe { std::env::set_var(scrozz_ocr::TESSERACT_DIRECTORY_ENV, &available) };
    assert!(SystemOcr::is_runtime_available());
    // SAFETY: same isolated child-process guarantee as above.
    unsafe { std::env::set_var(scrozz_ocr::TESSERACT_DIRECTORY_ENV, &missing) };
    assert!(!SystemOcr::is_runtime_available());
    // SAFETY: same isolated child-process guarantee as above.
    unsafe { std::env::set_var(scrozz_ocr::TESSERACT_DIRECTORY_ENV, &available) };
    assert!(SystemOcr::is_runtime_available());

    std::fs::remove_file(std::path::Path::new(&available).join("tesseract"))
        .expect("remove runtime");
    assert!(
        !SystemOcr::is_runtime_available(),
        "removing the selected executable must invalidate a prior positive probe"
    );
}

#[cfg(target_os = "windows")]
#[test]
fn windows_availability_matches_the_selected_artifact_backend() {
    match SystemOcr::engine_name().expect("package identity query") {
        "windows-media-ocr" | "tesseract" => assert_eq!(
            SystemOcr::is_runtime_available(),
            SystemOcr::new()
                .available_languages()
                .is_ok_and(|languages| !languages.is_empty())
        ),
        other => panic!("unexpected Windows OCR backend {other}"),
    }
}

#[test]
fn options_default_to_accurate_and_no_language_correction() {
    let options = Options::default();
    assert_eq!(options.accuracy, scrozz_ocr::Accuracy::Accurate);
    assert!(
        !options.language_correction,
        "screenshots are full of identifiers; correcting them into English is worse than the raw read"
    );
    assert_eq!(options.upscale, UpscalePolicy::Automatic);
    assert!(options.languages.is_empty());
    assert!(!options.automatic_language_detection);
    assert_eq!(options.line_breaks, scrozz_ocr::LineBreaks::Preserve);
}

#[test]
fn options_builders_compose() {
    let options = Options::new()
        .with_languages(["en-US".to_string(), "de-DE".to_string()])
        .with_accuracy(scrozz_ocr::Accuracy::Fast)
        .with_upscale(UpscalePolicy::Off)
        .with_language_correction(true)
        .with_automatic_language_detection(false)
        .with_line_breaks(scrozz_ocr::LineBreaks::Collapse);
    assert_eq!(options.languages, ["en-US", "de-DE"]);
    assert_eq!(options.accuracy, scrozz_ocr::Accuracy::Fast);
    assert_eq!(options.upscale, UpscalePolicy::Off);
    assert!(options.language_correction);
    assert_eq!(options.line_breaks, scrozz_ocr::LineBreaks::Collapse);
}

/// A malformed frame must be rejected the same way everywhere, before any
/// platform call gets a chance to read past the end of the buffer.
#[test]
fn a_short_buffer_never_panics() {
    let mut frame = blank(64, 64, 1.0);
    frame.data.truncate(16);
    let result = SystemOcr::new().recognize(&frame);
    assert!(
        result.is_err(),
        "a short buffer must be an error, not a crash"
    );
}

#[test]
fn a_zero_sized_frame_never_panics() {
    let frame = blank(0, 0, 1.0);
    assert!(SystemOcr::new().recognize(&frame).is_err());
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    all(target_os = "linux", feature = "tesseract")
)))]
mod no_system_engine {
    use super::{Ocr, SystemOcr, blank};
    use scrozz_core::Error;

    /// Decision D8: say what is missing and what to install. An empty `Vec`
    /// would look like "no text found", which is a lie.
    #[test]
    fn recognition_reports_an_honest_gap() {
        let err = SystemOcr::new()
            .recognize(&blank(200, 100, 1.0))
            .expect_err("no system engine exists on this platform");
        match err {
            Error::Unsupported { what, why } => {
                assert!(what.contains("text recognition"), "what = {what:?}");
                let why = why.to_lowercase();
                assert!(
                    why.contains("tesseract"),
                    "the message must name something installable: {why:?}"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    /// Even the malformed-frame path must not reach a panic.
    #[test]
    fn a_bad_frame_is_still_an_error() {
        let mut frame = blank(64, 64, 1.0);
        frame.data.clear();
        assert!(SystemOcr::new().recognize(&frame).is_err());
    }
}

/// Offscreen text rendering for the Vision tests. Lives in `tests/backend/`
/// so Cargo does not treat it as an integration test target of its own; the
/// explicit path is needed because `tests/backend.rs` is a crate root, whose
/// modules resolve against `tests/`.
#[cfg(target_os = "macos")]
#[path = "backend/canvas.rs"]
mod canvas;

#[cfg(target_os = "macos")]
mod vision {
    use super::{Ocr, Options, SystemOcr};
    use scrozz_core::{ColorSpace, Frame, PhysicalSize, PixelFormat, ScaleFactor};
    use scrozz_ocr::plain_text;

    use super::canvas;

    const TOP_WORD: &str = "HELLO";
    const BOTTOM_WORD: &str = "WORLD";

    /// Renders two known words at known heights into a 1× frame.
    ///
    /// 1× on purpose: it exercises the upscale path, which is exactly the code
    /// that a Retina-only test would never touch.
    fn two_line_frame() -> Frame {
        let (width, height) = (600u32, 400u32);
        let mut canvas = canvas::Canvas::new(width, height);
        canvas.fill_white();
        canvas.draw_text(TOP_WORD, 40.0, 300.0, 64.0);
        canvas.draw_text(BOTTOM_WORD, 40.0, 60.0, 64.0);
        assert!(
            canvas.ink() > 200,
            "nothing was drawn; the test would be vacuous"
        );

        Frame {
            data: canvas.into_rgba8(),
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride: width as usize * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::new(1.0),
        }
    }

    /// Which renderer ran is worth knowing: if Apple finally removes the
    /// deprecated text API the suite keeps passing on the blocky fallback font,
    /// and this is the only place that says so.
    #[test]
    fn reports_which_renderer_drew_the_fixture() {
        let mut canvas = canvas::Canvas::new(200, 80);
        canvas.fill_white();
        let renderer = canvas.draw_text("HELLO", 10.0, 20.0, 48.0);
        assert!(canvas.ink() > 100, "{renderer:?} produced no ink");
        eprintln!("fixture rendered with {renderer:?}");
    }

    #[test]
    fn recognises_rendered_text() {
        let frame = two_line_frame();
        let blocks = SystemOcr::new().recognize(&frame).expect("recognition");
        let text = plain_text(&blocks).to_uppercase();
        assert!(
            text.contains(TOP_WORD),
            "expected {TOP_WORD:?} in {text:?} ({} blocks)",
            blocks.len()
        );
        assert!(
            text.contains(BOTTOM_WORD),
            "expected {BOTTOM_WORD:?} in {text:?}"
        );
    }

    /// End-to-end proof that the bottom-left flip is not inverted: the word
    /// drawn near the top of the image must come back with the smaller `y`.
    #[test]
    fn coordinates_are_not_vertically_flipped() {
        let frame = two_line_frame();
        let blocks = SystemOcr::new().recognize(&frame).expect("recognition");

        let find = |needle: &str| {
            blocks
                .iter()
                .find(|b| b.text.to_uppercase().contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} missing from {blocks:#?}"))
        };
        let top = find(TOP_WORD);
        let bottom = find(BOTTOM_WORD);

        assert!(
            top.bounds.origin.y < bottom.bounds.origin.y,
            "{TOP_WORD} was drawn above {BOTTOM_WORD}; got y {} vs {}",
            top.bounds.origin.y,
            bottom.bounds.origin.y
        );
        // Drawn at CG y=300 of 400, i.e. in the upper half of the image.
        assert!(
            top.bounds.origin.y < 200.0,
            "{TOP_WORD} should land in the top half, got y = {}",
            top.bounds.origin.y
        );
        assert!(
            bottom.bounds.origin.y > 200.0,
            "{BOTTOM_WORD} should land in the bottom half, got y = {}",
            bottom.bounds.origin.y
        );
    }

    /// Coordinates come back in the frame's *logical* space, so the UI can draw
    /// a box straight over the pixels the user sees.
    #[test]
    fn bounds_stay_inside_the_frame() {
        let frame = two_line_frame();
        let blocks = SystemOcr::new().recognize(&frame).expect("recognition");
        assert!(!blocks.is_empty());
        for block in &blocks {
            let b = block.bounds;
            assert!(b.origin.x >= 0.0 && b.origin.y >= 0.0, "{b:?}");
            assert!(b.origin.x + b.size.width <= 600.0 + 1e-6, "{b:?}");
            assert!(b.origin.y + b.size.height <= 400.0 + 1e-6, "{b:?}");
            assert!(!b.is_empty(), "an empty box is useless to the UI: {b:?}");
        }
    }

    /// At 2× the same drawing has to land at half the logical coordinates.
    #[test]
    fn logical_coordinates_respect_the_scale_factor() {
        let mut frame = two_line_frame();
        let one_x = SystemOcr::new().recognize(&frame).expect("recognition");

        frame.scale = ScaleFactor::new(2.0);
        let two_x = SystemOcr::new().recognize(&frame).expect("recognition");

        let top_of = |blocks: &[scrozz_ocr::TextBlock]| {
            blocks
                .iter()
                .find(|b| b.text.to_uppercase().contains(TOP_WORD))
                .map(|b| b.bounds.origin.y)
                .expect("top word")
        };
        let (a, b) = (top_of(&one_x), top_of(&two_x));
        assert!(
            (a / 2.0 - b).abs() < 2.0,
            "2x logical coordinates should be half of 1x: {a} vs {b}"
        );
    }

    /// Confidence drives what the UI is willing to show, so it has to be a real
    /// number from the engine rather than a placeholder.
    #[test]
    fn confidence_is_reported_and_in_range() {
        let frame = two_line_frame();
        let blocks = SystemOcr::new().recognize(&frame).expect("recognition");
        assert!(!blocks.is_empty());
        for block in &blocks {
            assert!(
                block.confidence > 0.0 && block.confidence <= 1.0,
                "confidence out of range: {} for {:?}",
                block.confidence,
                block.text
            );
        }
        let best = blocks.iter().map(|b| b.confidence).fold(0.0_f32, f32::max);
        assert!(
            best > 0.3,
            "clean rendered text should read confidently, got {best}"
        );
    }

    /// Reading order must survive the round trip, or copying pastes a bag of
    /// words.
    #[test]
    fn text_comes_back_in_reading_order() {
        let frame = two_line_frame();
        let blocks = SystemOcr::new().recognize(&frame).expect("recognition");
        let text = plain_text(&blocks).to_uppercase();
        let top = text.find(TOP_WORD).expect("top word");
        let bottom = text.find(BOTTOM_WORD).expect("bottom word");
        assert!(
            top < bottom,
            "expected {TOP_WORD} before {BOTTOM_WORD} in {text:?}"
        );
        assert!(
            text.contains('\n'),
            "two rows should be two lines: {text:?}"
        );
    }

    /// Small text in a tall capture — a menu bar, a breadcrumb, a status line —
    /// is the text a screenshot tool is most often asked about, and it is what
    /// Vision's documented `minimumTextHeight` default (a fraction of image
    /// height) would discard. 18pt in a 1200px window is 1.5% of the height.
    #[test]
    fn small_text_in_a_tall_capture_is_still_found() {
        let (width, height) = (700u32, 1200u32);
        let mut canvas = canvas::Canvas::new(width, height);
        canvas.fill_white();
        // Near the top, the way a menu bar or breadcrumb is.
        canvas.draw_text("SCROLL", 24.0, f64::from(height) - 40.0, 18.0);

        let frame = Frame {
            data: canvas.into_rgba8(),
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride: width as usize * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::new(1.0),
        };

        let blocks = SystemOcr::new().recognize(&frame).expect("recognition");
        let text = plain_text(&blocks).to_uppercase();
        assert!(
            text.contains("SCROLL"),
            "small text was dropped; minimumTextHeight is filtering it out. Got {text:?}"
        );
    }

    /// An empty screenshot is not an error — it is a screenshot with no text.
    #[test]
    fn a_blank_image_yields_no_blocks_and_no_error() {
        let frame = super::blank(300, 200, 1.0);
        let blocks = SystemOcr::new()
            .recognize(&frame)
            .expect("blank is not a failure");
        assert!(
            blocks.is_empty(),
            "found text in a blank image: {blocks:#?}"
        );
    }

    /// Requesting a language the installed system does not have must degrade to
    /// automatic detection, not fail the whole request.
    #[test]
    fn an_unknown_language_does_not_fail_the_request() {
        let frame = two_line_frame();
        let ocr = SystemOcr::with_options(
            Options::new().with_languages(["zz-ZZ".to_string(), "en-US".to_string()]),
        );
        let blocks = ocr
            .recognize(&frame)
            .expect("unknown tags must be filtered, not fatal");
        let text = plain_text(&blocks).to_uppercase();
        assert!(text.contains(TOP_WORD), "got {text:?}");
    }

    #[test]
    fn fast_mode_also_works() {
        let frame = two_line_frame();
        let ocr = SystemOcr::with_options(Options::new().with_accuracy(scrozz_ocr::Accuracy::Fast));
        let blocks = ocr.recognize(&frame).expect("fast recognition");
        let text = plain_text(&blocks).to_uppercase();
        assert!(
            text.contains(TOP_WORD) || text.contains(BOTTOM_WORD),
            "got {text:?}"
        );
    }

    /// The convenience wrapper must agree with the block-level API.
    #[test]
    fn recognize_text_matches_plain_text() {
        let frame = two_line_frame();
        let ocr = SystemOcr::new();
        let blocks = ocr.recognize(&frame).expect("recognition");
        assert_eq!(
            ocr.recognize_text(&frame).expect("text"),
            plain_text(&blocks)
        );
    }
}
