//! Drag-out: naming, format negotiation, the promise, and the session.
//!
//! Everything that runs by default is pure logic. No window server, no
//! pasteboard, no mouse, and above all **no live drag** — a drag captures the
//! pointer for as long as the button is held, and a test suite that grabs the
//! machine's mouse is a test suite nobody runs twice.
//!
//! The AppKit tests at the bottom are `#[ignore]`d. They exercise the promise
//! delegate directly — construct the provider, ask it for a filename, ask it to
//! write — which is exactly the code path a drop target triggers, minus the
//! dragging. They never order a window front and never call
//! `beginDraggingSession`.
//!
//! What is *not* covered here, and cannot be: that Finder, Slack or Figma
//! actually accept the drop. That needs a human with a mouse.

use scrozz_core::{Error, LogicalPoint, LogicalRect, LogicalSize};
use scrozz_shell::drag::{
    DragFormat, DragOrigin, DragOutcome, DragPayload, DragPreview, DragSession, FALLBACK_STEM,
    MAX_FILE_NAME_BYTES, NativeSurface, PromisedFile, byte_source, card_rect_in_view, check_origin,
    sanitise_stem,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Height of the view in every geometry fixture.
///
/// Deliberately un-round, and deliberately *not* a plausible card height: a
/// flip that accidentally uses the card's own height still passes when the two
/// happen to match.
const VIEW_HEIGHT: f64 = 843.0;

fn rect(x: f64, y: f64, width: f64, height: f64) -> LogicalRect {
    LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(width, height))
}

/// A surface handle that is non-null but never dereferenced.
///
/// `check_origin` only compares against null; nothing in the default tests goes
/// near AppKit, so a fabricated address is both safe and sufficient.
fn fake_surface() -> NativeSurface {
    // SAFETY: the pointer is never dereferenced. It exists so `is_null` is
    // false; every code path in these tests stops at that check.
    unsafe { NativeSurface::from_raw(0x1000 as *mut std::ffi::c_void) }
}

fn png_bytes() -> Vec<u8> {
    // The 8-byte PNG signature. Real enough for the code under test, which
    // never decodes it — only counts it and hands it on.
    vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

#[test]
fn window_title_separators_do_not_become_directories() {
    // The motivating case: a capture named after a Finder window. Left alone,
    // "Finder — /Users/brandon" would ask the drop target to write into a
    // directory that is not theirs to write into.
    let stem = sanitise_stem("Finder — /Users/brandon/Desktop");
    assert!(
        !stem.contains('/'),
        "{stem} still contains a path separator"
    );
    assert!(
        !stem.contains('\\'),
        "{stem} still contains a path separator"
    );
    assert_eq!(stem, "Finder — Users brandon Desktop");
}

#[test]
fn every_hostile_character_is_neutralised() {
    // `:` is the classic-Mac separator and still shows up as `/` in Finder;
    // `<>:"|?*` are outright illegal on Windows, and a promised file can land
    // on a Windows share.
    let stem = sanitise_stem(r#"a/b\c:d<e>f"g|h?i*j"#);
    for bad in ['/', '\\', ':', '<', '>', '"', '|', '?', '*'] {
        assert!(!stem.contains(bad), "{stem} still contains {bad:?}");
    }
    assert_eq!(stem, "a b c d e f g h i j");
}

#[test]
fn control_characters_including_nul_are_removed() {
    // A NUL truncates the name at the C-string boundary in some receivers and
    // is rejected outright by others.
    let stem = sanitise_stem("Chat\u{0}window\twith\nnewlines\r\u{7}");
    assert_eq!(stem, "Chat window with newlines");
}

#[test]
fn a_capture_cannot_become_a_hidden_file() {
    // A window title beginning with a dot would otherwise produce a file the
    // user drops and then cannot find.
    assert_eq!(sanitise_stem("...secret"), "secret");
    assert_eq!(sanitise_stem(".."), FALLBACK_STEM);
}

#[test]
fn trailing_dots_and_spaces_are_stripped() {
    // Windows silently drops both, so "Report." and "Report" would collide
    // after the user has already dropped one of them.
    assert_eq!(sanitise_stem("Report..."), "Report");
    assert_eq!(sanitise_stem("Report   "), "Report");
    assert_eq!(sanitise_stem("Report . . "), "Report");
}

#[test]
fn whitespace_runs_collapse() {
    // `split_whitespace` treats U+00A0 as whitespace, so a title padded with
    // non-breaking spaces collapses like any other. That is what we want: the
    // name should read the same however the source app spelled its gaps.
    assert_eq!(sanitise_stem("Slack   \u{a0}  DM"), "Slack DM");
    assert_eq!(sanitise_stem("a  b   c"), "a b c");
}

#[test]
fn nothing_usable_falls_back_rather_than_producing_an_empty_name() {
    for raw in ["", "   ", "\t\n", "///", "***", "\u{0}"] {
        assert_eq!(
            sanitise_stem(raw),
            FALLBACK_STEM,
            "{raw:?} should have fallen back"
        );
    }
}

#[test]
fn unicode_survives_intact() {
    // Emoji and CJK are legal in filenames everywhere Scrozz runs. Mangling
    // them would be a bug, not a safety measure.
    assert_eq!(
        sanitise_stem("スクリーンショット 📸"),
        "スクリーンショット 📸"
    );
}

// ---------------------------------------------------------------------------
// File names
// ---------------------------------------------------------------------------

#[test]
fn file_name_is_stem_plus_extension() {
    let file = PromisedFile::new("Design review", DragFormat::Png, byte_source(|| Ok(vec![])));
    assert_eq!(file.file_name(), "Design review.png");
}

#[test]
fn file_name_sanitises_at_construction() {
    let file = PromisedFile::new("a/b", DragFormat::Png, byte_source(|| Ok(vec![])));
    assert_eq!(file.stem(), "a b");
    assert_eq!(file.file_name(), "a b.png");
}

#[test]
fn an_absurd_title_is_truncated_to_a_legal_length() {
    let long = "x".repeat(4096);
    let file = PromisedFile::new(&long, DragFormat::Png, byte_source(|| Ok(vec![])));
    let name = file.file_name();
    assert!(
        name.len() <= MAX_FILE_NAME_BYTES,
        "{} bytes exceeds the {MAX_FILE_NAME_BYTES}-byte limit",
        name.len()
    );
    assert!(
        name.ends_with(".png"),
        "extension was truncated away: {name}"
    );
}

#[test]
fn truncation_never_splits_a_codepoint() {
    // Every char here is 4 bytes, so a byte-wise cut lands mid-codepoint
    // roughly three times in four. An invalid-UTF-8 filename is refused by
    // some drop targets and silently mangled by others.
    let long = "🐢".repeat(300);
    let file = PromisedFile::new(&long, DragFormat::Mp4, byte_source(|| Ok(vec![])));
    let name = file.file_name();

    assert!(name.len() <= MAX_FILE_NAME_BYTES);
    assert!(name.ends_with(".mp4"));

    let stem = name.trim_end_matches(".mp4");
    assert!(!stem.is_empty());
    assert!(
        stem.chars().all(|c| c == '🐢'),
        "truncation produced a partial character: {stem:?}"
    );
}

#[test]
fn truncation_that_leaves_nothing_usable_falls_back() {
    // A stem that is one enormous run of dots and spaces past the budget
    // trims away to nothing; the name must still be a name.
    let long = format!("{}{}", "y".repeat(400), ".".repeat(64));
    let file = PromisedFile::new(&long, DragFormat::Png, byte_source(|| Ok(vec![])));
    let name = file.file_name();
    assert!(name.len() <= MAX_FILE_NAME_BYTES);
    assert!(!name.starts_with('.'));
    assert!(name.ends_with(".png"));
}

// ---------------------------------------------------------------------------
// Formats
// ---------------------------------------------------------------------------

#[test]
fn every_format_agrees_with_itself() {
    for format in [
        DragFormat::Png,
        DragFormat::Jpeg,
        DragFormat::Webp,
        DragFormat::Gif,
        DragFormat::Mp4,
    ] {
        assert!(!format.extension().is_empty());
        assert!(!format.extension().starts_with('.'), "{format:?}");
        assert!(
            format.uti().contains('.'),
            "{format:?} UTI is not reverse-DNS: {}",
            format.uti()
        );
        assert!(
            format.mime().contains('/'),
            "{format:?} MIME lacks a type/subtype: {}",
            format.mime()
        );
        assert_eq!(
            DragFormat::from_extension(format.extension()),
            Some(format),
            "{format:?} does not round-trip through its own extension"
        );
    }
}

#[test]
fn only_video_is_file_or_nothing() {
    // Everything a decoder will hand back as a single frame can also ride the
    // pasteboard as image data — a GIF included, since AppKit reads one and
    // takes the first frame. A recording cannot: there is no video flavour any
    // real receiver accepts, so an MP4 is file-or-nothing.
    assert!(DragFormat::Png.is_still_image());
    assert!(DragFormat::Jpeg.is_still_image());
    assert!(DragFormat::Webp.is_still_image());
    assert!(DragFormat::Gif.is_still_image());
    assert!(!DragFormat::Mp4.is_still_image());
}

#[test]
fn extensions_are_recognised_however_they_are_written() {
    // The extension can arrive from a settings string, a filename, or a user
    // typing it. All three spellings show up.
    assert_eq!(DragFormat::from_extension("PNG"), Some(DragFormat::Png));
    assert_eq!(DragFormat::from_extension(".png"), Some(DragFormat::Png));
    assert_eq!(DragFormat::from_extension(".PnG"), Some(DragFormat::Png));
    assert_eq!(DragFormat::from_extension("jpg"), Some(DragFormat::Jpeg));
    assert_eq!(DragFormat::from_extension("jpeg"), Some(DragFormat::Jpeg));
    assert_eq!(DragFormat::from_extension("m4v"), Some(DragFormat::Mp4));
    assert_eq!(DragFormat::from_extension("heic"), None);
    assert_eq!(DragFormat::from_extension(""), None);
}

#[test]
fn png_is_the_type_apple_and_the_web_agree_on() {
    // Hard-coded on purpose. `public.png` and `image/png` are what the
    // pasteboard and XDND advertise; a typo here is invisible at compile time
    // and shows up as "Slack ignores the drop".
    assert_eq!(DragFormat::Png.uti(), "public.png");
    assert_eq!(DragFormat::Png.mime(), "image/png");
    assert_eq!(DragFormat::Jpeg.uti(), "public.jpeg");
    assert_eq!(DragFormat::Mp4.uti(), "public.mpeg-4");
}

// ---------------------------------------------------------------------------
// The promise
// ---------------------------------------------------------------------------

#[test]
fn nothing_is_produced_until_the_receiver_asks() {
    // The property the whole design exists for: constructing a payload must
    // not encode, must not write, must not touch the disk.
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);

    let payload = DragPayload::png_capture(
        "Capture",
        byte_source(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(png_bytes())
        }),
    );

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the payload encoded eagerly; the file would be written before the drop"
    );

    let bytes = payload
        .file()
        .produce()
        .expect("promise should be keepable");
    assert_eq!(bytes, png_bytes());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_promise_can_be_kept_more_than_once() {
    // A receiver may ask for the file and the image, and some ask twice.
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let file = PromisedFile::new(
        "Capture",
        DragFormat::Png,
        byte_source(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(png_bytes())
        }),
    );

    assert_eq!(file.produce().unwrap(), png_bytes());
    assert_eq!(file.produce().unwrap(), png_bytes());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_promise_that_cannot_be_kept_reports_why() {
    // The capture was evicted, or the encoder failed. The drop target has to
    // be told; silently writing an empty file is worse than failing.
    let file = PromisedFile::new(
        "Capture",
        DragFormat::Png,
        byte_source(|| Err(Error::InvalidRequest("capture was evicted".to_owned()))),
    );

    let err = file.produce().expect_err("failure must propagate");
    assert!(
        err.to_string().contains("evicted"),
        "the reason was lost: {err}"
    );
}

#[test]
fn png_capture_offers_the_image_as_well_as_the_file() {
    // Apps that take an image but not a file — Keynote, some mail composers —
    // must still work, so the same bytes are offered both ways.
    let payload = DragPayload::png_capture("Capture", byte_source(|| Ok(png_bytes())));
    assert_eq!(payload.file().format(), DragFormat::Png);
    assert!(
        payload.image().is_some(),
        "an image-only receiver would refuse this drop"
    );
    assert_eq!(payload.image().unwrap()().unwrap(), png_bytes());
}

#[test]
fn a_video_capture_offers_no_still_image() {
    // Handing a receiver an MP4 under `public.png` produces a broken image,
    // which reads to the user as a corrupt capture.
    let payload = DragPayload::new(PromisedFile::new(
        "Screen recording",
        DragFormat::Mp4,
        byte_source(|| Ok(vec![0; 4])),
    ));
    assert!(payload.image().is_none());
    assert!(payload.preview_png().is_none());
}

// ---------------------------------------------------------------------------
// Preview
// ---------------------------------------------------------------------------

#[test]
fn a_preview_needs_pixels_and_a_size() {
    let size = LogicalSize::new(320.0, 200.0);
    let preview = DragPreview::from_png(png_bytes(), size).expect("valid preview");
    assert_eq!(preview.png(), png_bytes().as_slice());
    assert_eq!(preview.size().width, 320.0);

    assert!(
        matches!(
            DragPreview::from_png(vec![], size),
            Err(Error::InvalidRequest(_))
        ),
        "empty bytes should be refused, not drawn as nothing"
    );
    assert!(
        matches!(
            DragPreview::from_png(png_bytes(), LogicalSize::new(0.0, 0.0)),
            Err(Error::InvalidRequest(_))
        ),
        "a zero-size preview is an invisible drag image"
    );
}

#[test]
fn a_payload_carries_its_preview_through() {
    let preview = DragPreview::from_png(png_bytes(), LogicalSize::new(64.0, 64.0)).unwrap();
    let payload =
        DragPayload::png_capture("Capture", byte_source(|| Ok(png_bytes()))).with_preview(preview);
    assert!(payload.preview().is_some());
    assert_eq!(payload.preview_png(), Some(png_bytes().as_slice()));
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

#[test]
fn a_flipped_view_needs_no_conversion() {
    // Scrozz and a flipped NSView agree: both measure downwards from the
    // top-left. Converting anyway would move the drag image.
    let card = rect(40.0, 120.0, 300.0, 180.0);
    let placed = card_rect_in_view(card, VIEW_HEIGHT, true);
    assert_eq!(placed.x, 40.0);
    assert_eq!(placed.y, 120.0);
    assert_eq!(placed.width, 300.0);
    assert_eq!(placed.height, 180.0);
}

#[test]
fn an_unflipped_view_measures_from_the_bottom() {
    // The bug this guards against does not fail loudly: the drag image simply
    // appears mirrored about the middle of the window, a long way from the
    // pointer, which reads as "the drag animation is broken".
    let card = rect(40.0, 120.0, 300.0, 180.0);
    let placed = card_rect_in_view(card, VIEW_HEIGHT, false);
    assert_eq!(placed.x, 40.0);
    assert_eq!(placed.y, VIEW_HEIGHT - 120.0 - 180.0);
    assert_eq!(placed.width, 300.0);
    assert_eq!(placed.height, 180.0);
}

#[test]
fn a_card_at_the_top_of_an_unflipped_view_sits_at_the_top() {
    // Sanity in the other direction: a card flush with the top edge must end
    // up flush with the top edge, not the bottom.
    let card = rect(0.0, 0.0, 100.0, 60.0);
    let placed = card_rect_in_view(card, VIEW_HEIGHT, false);
    assert_eq!(placed.y, VIEW_HEIGHT - 60.0);
}

// ---------------------------------------------------------------------------
// Origin validation
// ---------------------------------------------------------------------------

#[test]
fn a_valid_origin_passes() {
    let origin = DragOrigin::new(
        fake_surface(),
        rect(10.0, 10.0, 240.0, 150.0),
        LogicalPoint::new(120.0, 80.0),
    );
    assert!(check_origin(&origin).is_ok());
}

#[test]
fn a_null_surface_is_refused_before_anything_native_happens() {
    let origin = DragOrigin::new(
        NativeSurface::null(),
        rect(10.0, 10.0, 240.0, 150.0),
        LogicalPoint::new(120.0, 80.0),
    );
    let err = check_origin(&origin).expect_err("a null window handle cannot start a drag");
    assert!(matches!(err, Error::InvalidRequest(_)));
    assert!(err.to_string().contains("null"));
}

#[test]
fn an_empty_card_is_refused_rather_than_drawn_as_nothing() {
    // A zero-size drag image looks exactly like the drag silently failing to
    // start, so the diagnosis has to happen here.
    for card in [
        rect(10.0, 10.0, 0.0, 150.0),
        rect(10.0, 10.0, 240.0, 0.0),
        rect(10.0, 10.0, 0.0, 0.0),
    ] {
        let origin = DragOrigin::new(fake_surface(), card, LogicalPoint::new(1.0, 1.0));
        assert!(
            matches!(check_origin(&origin), Err(Error::InvalidRequest(_))),
            "{card:?} should have been refused"
        );
    }
}

// ---------------------------------------------------------------------------
// Session and outcome
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_session_is_running() {
    let session = DragSession::new();
    assert!(session.is_active());
    assert_eq!(session.outcome(), None);
}

#[test]
fn the_first_outcome_wins() {
    // AppKit can report an end and a failure for the same drag. The UI polls
    // once and must not see the answer change under it.
    let session = DragSession::new();
    session.finish(DragOutcome::Accepted(
        scrozz_shell::drag::DragOperation::Copy,
    ));
    session.finish(DragOutcome::Failed("late report".to_owned()));

    assert!(!session.is_active());
    assert_eq!(
        session.outcome(),
        Some(DragOutcome::Accepted(
            scrozz_shell::drag::DragOperation::Copy
        ))
    );
}

#[test]
fn a_session_can_be_born_finished() {
    // The stub backends return one of these, so the UI has a uniform thing to
    // poll whether or not the platform can drag.
    let session = DragSession::finished(DragOutcome::Cancelled);
    assert!(!session.is_active());
    assert_eq!(session.outcome(), Some(DragOutcome::Cancelled));
}

#[test]
fn only_an_acceptance_keeps_the_card_out_of_the_pile() {
    // D21: a drag that did not land leaves the pile exactly as it was.
    assert!(DragOutcome::Accepted(scrozz_shell::drag::DragOperation::Copy).is_accepted());
    assert!(!DragOutcome::Accepted(scrozz_shell::drag::DragOperation::Copy).should_restore_card());

    for outcome in [
        DragOutcome::Rejected,
        DragOutcome::Cancelled,
        DragOutcome::Failed("no".to_owned()),
    ] {
        assert!(!outcome.is_accepted(), "{outcome:?}");
        assert!(outcome.should_restore_card(), "{outcome:?}");
    }
}

// ---------------------------------------------------------------------------
// Capability
// ---------------------------------------------------------------------------

#[test]
fn a_backend_reports_what_it_can_do_without_being_asked_to_do_it() {
    // D8: an unavailable capability is an outcome, not a panic. The UI asks
    // before it offers the gesture.
    use scrozz_shell::drag::DragSource as _;

    let source = scrozz_shell::drag::native_drag_source();

    #[cfg(target_os = "macos")]
    {
        // Off the main thread this is the expected refusal, and it must say so
        // rather than crash.
        match source {
            Ok(source) => {
                let capability = source.capability();
                assert!(capability.promised_files);
                assert!(capability.image_data);
                assert!(capability.can_drag);
            }
            Err(Error::Platform(message)) => {
                assert!(message.contains("main thread"), "{message}");
            }
            Err(other) => panic!("unexpected failure: {other}"),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let source = source.expect("the planned backend always constructs");
        let capability = source.capability();
        assert!(!capability.promised_files);
        assert!(!capability.can_drag);
        assert!(
            !capability.detail.is_empty(),
            "an unavailable capability must explain itself"
        );
    }
}

// ---------------------------------------------------------------------------
// AppKit — run with `cargo test -p scrozz-shell --test drag -- --ignored`
// ---------------------------------------------------------------------------
//
// These need the main thread and a running AppKit, so they are opt-in. They do
// not order a window front, do not begin a dragging session, and do not touch
// the pointer. What they exercise is the pasteboard item itself: the thing a
// drop target reads once the user has let go.

#[cfg(target_os = "macos")]
mod appkit {
    use super::{DragPayload, byte_source, png_bytes};
    use scrozz_shell::drag::{ArtifactState, DragOperation, DragOutcome};
    use scrozz_shell::macos::drag::test_support::{ItemHarness, view_can_begin_drags};

    /// A scratch root inside cargo's own target directory.
    ///
    /// `CARGO_TARGET_TMPDIR` exists for exactly this and is swept by
    /// `cargo clean`. Deliberately not the system temp directory, which is
    /// where the real artifacts live: a test that wrote there could be swept by
    /// the orphan reaper mid-run, or sweep a real drag out from under the app.
    fn scratch(name: &str) -> std::path::PathBuf {
        let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch root");
        root
    }

    #[test]
    #[ignore = "needs the main thread and a running AppKit"]
    fn the_item_offers_a_file_url_first_and_an_image_beside_it() {
        let root = scratch("item-flavours");
        let payload = DragPayload::png_capture("Design review", byte_source(|| Ok(png_bytes())));
        let harness = ItemHarness::new(&payload, &root).expect("item");

        let types = harness.types();
        assert_eq!(
            types.first().map(String::as_str),
            Some("public.file-url"),
            "a screenshot drag must offer its file before its pixels: {types:?}"
        );
        assert!(
            types.iter().any(|ty| ty == "public.png"),
            "no image flavour was advertised: {types:?}"
        );
        assert!(
            types.iter().any(|ty| ty == "public.tiff"),
            "no TIFF flavour was advertised: {types:?}"
        );
        harness.finish();
    }

    #[test]
    #[ignore = "needs the main thread and a running AppKit"]
    fn the_advertised_url_points_at_the_file_that_was_written() {
        let root = scratch("item-url");
        let payload = DragPayload::png_capture("Design #1 100%", byte_source(|| Ok(png_bytes())));
        let harness = ItemHarness::new(&payload, &root).expect("item");

        let resolved = harness
            .file_url_path()
            .expect("the item must advertise a resolvable file URL");
        assert_eq!(
            std::path::Path::new(&resolved),
            harness.artifact().path(),
            "the URL a receiver reads is not the file that was written"
        );
        assert_eq!(
            std::fs::read(&resolved).expect("the advertised file must exist"),
            png_bytes(),
            "the advertised file does not hold the capture"
        );
        harness.finish();
    }

    #[test]
    #[ignore = "needs the main thread and a running AppKit"]
    fn the_pasteboard_offers_png_eagerly_and_tiff_lazily() {
        let root = scratch("item-image");
        let payload = DragPayload::png_capture("Flavours", byte_source(|| Ok(png_bytes())));
        let harness = ItemHarness::new(&payload, &root).expect("item");

        let png = harness
            .flavour("public.png")
            .expect("public.png must be offered");
        assert_eq!(png, png_bytes());

        // TIFF is what older AppKit receivers ask for. The bytes differ — they
        // are transcoded — so only presence and non-emptiness are checked.
        let tiff = harness.flavour("public.tiff");
        assert!(
            tiff.as_ref().is_none_or(|bytes| !bytes.is_empty()),
            "public.tiff was offered but produced nothing"
        );

        assert!(
            harness.flavour("public.avif").is_none(),
            "an unrequested flavour must not be fabricated"
        );
        harness.finish();
    }

    #[test]
    #[ignore = "needs the main thread and a running AppKit"]
    fn an_accepted_drop_leaves_the_file_where_the_receiver_can_read_it() {
        let root = scratch("item-lifetime");
        let payload = DragPayload::png_capture("Kept", byte_source(|| Ok(png_bytes())));
        let mut harness = ItemHarness::new(&payload, &root).expect("item");

        assert_eq!(harness.artifact().state(), ArtifactState::InFlight);
        harness
            .artifact_mut()
            .settle(&DragOutcome::Accepted(DragOperation::Copy));
        assert_eq!(harness.artifact().state(), ArtifactState::Retained);
        assert!(
            harness.artifact().exists(),
            "an accepted drop deleted the file the receiver was told to read"
        );
        harness.finish();
    }

    #[test]
    #[ignore = "needs the main thread and a running AppKit"]
    fn an_nsview_can_start_a_dragging_session() {
        // A precondition for the whole feature. This does *not* prove a
        // non-activating panel can be a drag source — see docs/drag-matrix.md.
        assert!(
            view_can_begin_drags(),
            "NSView does not respond to beginDraggingSessionWithItems:event:source:"
        );
    }
}

// ---------------------------------------------------------------------------
// Artifact lifetime
// ---------------------------------------------------------------------------
//
// The file a drop target is handed is a real file on disk with no owner but
// us. Delete it a moment early and the receiver reads nothing — which is the
// silent-failure shape this whole path exists to remove. Never delete it and
// the temp directory grows without bound. These tests pin down exactly when it
// goes.

mod artifact_lifetime {
    use super::png_bytes;
    use scrozz_shell::drag::artifact::{
        ArtifactState, DragArtifact, ORPHAN_MAX_AGE, RETENTION, artifact_root, state_after,
        sweep_orphans,
    };
    use scrozz_shell::drag::{DragOperation, DragOutcome};
    use std::time::{Duration, Instant, SystemTime};

    /// A private root per test, so parallel tests cannot sweep each other's
    /// files. Never the shared `artifact_root()`.
    fn root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "scrozz-artifact-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test root");
        dir
    }

    fn artifact(tag: &str) -> (std::path::PathBuf, DragArtifact) {
        let dir = root(tag);
        let a = DragArtifact::materialise(&dir, "Screenshot.png", &png_bytes())
            .expect("a temp file is writable");
        (dir, a)
    }

    // -- the pure rule ------------------------------------------------------

    #[test]
    fn an_accepted_drop_is_retained_and_everything_else_is_removed() {
        for (outcome, expected) in [
            (
                DragOutcome::Accepted(DragOperation::Copy),
                ArtifactState::Retained,
            ),
            (DragOutcome::Cancelled, ArtifactState::Removed),
            (DragOutcome::Failed("nope".into()), ArtifactState::Removed),
        ] {
            assert_eq!(
                state_after(ArtifactState::InFlight, &outcome),
                expected,
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn a_late_report_cannot_undo_a_settled_artifact() {
        // AppKit can report `endedAtPoint:` and a failure for the same drag.
        // A second opinion must not resurrect a deleted file, nor re-delete one
        // a receiver is still reading.
        for settled in [ArtifactState::Retained, ArtifactState::Removed] {
            for outcome in [
                DragOutcome::Accepted(DragOperation::Copy),
                DragOutcome::Cancelled,
                DragOutcome::Failed("late".into()),
            ] {
                assert_eq!(
                    state_after(settled, &outcome),
                    settled,
                    "{settled:?} then {outcome:?}"
                );
            }
        }
    }

    #[test]
    fn only_a_live_or_retained_artifact_expects_a_file() {
        assert!(ArtifactState::InFlight.expects_file());
        assert!(ArtifactState::Retained.expects_file());
        assert!(!ArtifactState::Removed.expects_file());
    }

    // -- the file on disk ---------------------------------------------------

    #[test]
    fn a_materialised_artifact_is_a_real_readable_file() {
        let (dir, a) = artifact("materialise");
        assert_eq!(a.state(), ArtifactState::InFlight);
        assert!(a.exists(), "the drop target has to be able to open it");
        assert_eq!(
            std::fs::read(a.path()).expect("readable"),
            png_bytes(),
            "byte-for-byte what was captured, not a re-encode"
        );
        assert_eq!(
            a.path().file_name().and_then(|n| n.to_str()),
            Some("Screenshot.png"),
            "the receiver shows this name to the user"
        );
        assert!(a.expires_at().is_none(), "nothing expires mid-drag");
        drop(a);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn two_artifacts_can_share_a_name_without_colliding() {
        // Both drags are of "Screenshot.png" and both receivers should see that
        // name. Private directories are what make that possible without
        // appending `-1` to a filename the user will read.
        let dir = root("collide");
        let a = DragArtifact::materialise(&dir, "Screenshot.png", &png_bytes()).expect("a");
        let b = DragArtifact::materialise(&dir, "Screenshot.png", &png_bytes()).expect("b");
        assert_ne!(a.path(), b.path());
        assert_eq!(a.path().file_name(), b.path().file_name());
        assert!(a.exists() && b.exists());
        drop((a, b));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cancelled_drag_deletes_the_file_at_once() {
        let (dir, mut a) = artifact("cancel");
        let path = a.path().to_path_buf();
        a.settle(&DragOutcome::Cancelled);
        assert_eq!(a.state(), ArtifactState::Removed);
        assert!(!path.exists(), "nothing read it, so nothing is waiting");
        assert!(a.expires_at().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_accepted_drop_keeps_the_file_until_the_retention_window_passes() {
        let (dir, mut a) = artifact("retain");
        let path = a.path().to_path_buf();
        let now = Instant::now();

        a.settle_at(&DragOutcome::Accepted(DragOperation::Copy), now);
        assert_eq!(a.state(), ArtifactState::Retained);
        assert!(path.exists(), "the receiver may not have read it yet");

        assert!(
            !a.sweep_at(now + RETENTION - Duration::from_secs(1)),
            "one second early is still too early"
        );
        assert!(path.exists());

        assert!(a.sweep_at(now + RETENTION + Duration::from_secs(1)));
        assert_eq!(a.state(), ArtifactState::Removed);
        assert!(!path.exists());

        // `sweep_at` answers "is this finished with?", not "did I delete
        // something?" — so it keeps saying yes, and a caller polling every
        // frame after the deadline does not have to remember it already asked.
        assert!(a.sweep_at(now + RETENTION * 10));
        assert!(!path.exists(), "and it is not recreated by asking twice");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_live_artifact_is_never_swept() {
        // The drag is still happening. Sweeping here is exactly the bug.
        let (dir, mut a) = artifact("live");
        assert!(!a.sweep_at(Instant::now() + RETENTION * 100));
        assert_eq!(a.state(), ArtifactState::InFlight);
        assert!(a.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn removing_an_artifact_is_idempotent() {
        let (dir, mut a) = artifact("idempotent");
        let path = a.path().to_path_buf();
        a.remove();
        a.remove();
        assert_eq!(a.state(), ArtifactState::Removed);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dropping_a_live_artifact_still_cleans_up() {
        // A panic mid-drag must not leave a screenshot in the temp directory
        // for the rest of the machine's uptime.
        let dir = root("drop");
        let path = {
            let a = DragArtifact::materialise(&dir, "Screenshot.png", &png_bytes()).expect("a");
            a.path().to_path_buf()
        };
        assert!(!path.exists(), "the guard ran on the way out");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_retained_artifact_survives_being_dropped() {
        // The receiver is reading it. Dropping our handle is not permission to
        // pull the file out from under them — the sweep decides that.
        let dir = root("retained-drop");
        let path = {
            let mut a = DragArtifact::materialise(&dir, "Screenshot.png", &png_bytes()).expect("a");
            a.settle(&DragOutcome::Accepted(DragOperation::Copy));
            a.path().to_path_buf()
        };
        assert!(path.exists(), "still readable after the handle went away");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- artifacts from a process that is gone ------------------------------

    #[test]
    fn an_old_orphan_is_swept_and_a_recent_one_is_left_alone() {
        let dir = root("orphans");
        let old = dir.join("stale");
        let fresh = dir.join("fresh");
        std::fs::create_dir_all(&old).expect("old");
        std::fs::create_dir_all(&fresh).expect("fresh");
        std::fs::write(old.join("a.png"), png_bytes()).expect("write");
        std::fs::write(fresh.join("b.png"), png_bytes()).expect("write");

        // Sweeping "now" ages nothing, so both survive.
        assert_eq!(sweep_orphans(&dir, SystemTime::now(), ORPHAN_MAX_AGE), 0);
        assert!(old.exists() && fresh.exists());

        // Sweeping from an hour and a half in the future ages both past the
        // window, so both go. Time is a parameter precisely so this is a test
        // and not a sleep.
        let later = SystemTime::now() + ORPHAN_MAX_AGE + Duration::from_secs(60);
        assert_eq!(sweep_orphans(&dir, later, ORPHAN_MAX_AGE), 2);
        assert!(!old.exists() && !fresh.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sweeping_a_root_that_does_not_exist_is_not_an_error() {
        // First run on a clean machine. There is nothing to sweep and that is
        // the normal case, not a failure.
        let missing = std::env::temp_dir().join("scrozz-artifact-test-absent-xyzzy");
        let _ = std::fs::remove_dir_all(&missing);
        assert_eq!(
            sweep_orphans(&missing, SystemTime::now(), ORPHAN_MAX_AGE),
            0
        );
    }

    #[test]
    fn every_artifact_lives_under_one_known_root() {
        // So a sweep can find them all, and so nothing is written somewhere a
        // later version would not know to look.
        let root = artifact_root();
        assert!(root.starts_with(std::env::temp_dir()));
        assert!(
            root.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("scrozz")),
            "{root:?} has to be identifiably ours"
        );
    }
}
