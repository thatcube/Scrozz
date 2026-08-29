//! History as the app actually uses it: insert, read back, page, search, delete.

use scrozz_annotate::{Annotation, Style};
use scrozz_core::{DisplayId, LogicalPoint, LogicalRect, LogicalSize, Opacity, PinScale, PinState};
use scrozz_store::{
    DocumentState, History as _, ImageState, MediaKind, NewCapture, Page, SearchQuery, SqliteStore,
    Store as _, Timestamp,
    test_support::{ScratchDir, richly_annotated_document, sample_document, scratch_dir},
};

fn store(label: &str) -> (ScratchDir, SqliteStore) {
    let dir = scratch_dir(label);
    let store = SqliteStore::open(dir.path()).expect("history opens");
    (dir, store)
}

#[test]
fn a_capture_survives_a_round_trip_through_history() {
    let (_dir, mut store) = store("round-trip");
    let document = richly_annotated_document(7);
    let original_pixels = document.source.frame.data.clone();

    let id = store
        .insert(
            NewCapture::new(&document)
                .from_app("Safari")
                .titled("Quarterly invoice")
                .with_ocr("Total due: 1,240.00"),
        )
        .expect("insert");

    let record = store.record(&id).expect("read").expect("present");
    assert_eq!(record.app_name.as_deref(), Some("Safari"));
    assert_eq!(record.window_title.as_deref(), Some("Quarterly invoice"));
    assert_eq!(record.annotation_count, 3);
    assert!(record.image.is_present());
    assert!(!record.pinned);

    let state = store.document(&id).expect("read").expect("present");
    let DocumentState::Complete(back) = state else {
        panic!("a freshly inserted capture must still have its pixels");
    };
    assert_eq!(
        back.source.frame.data, original_pixels,
        "D14: the source image is never mutated"
    );
    assert_eq!(back.annotations().len(), 3);
    assert!(back.beautification().is_some());
}

#[test]
fn a_malformed_frame_is_refused_rather_than_stored() {
    let (_dir, mut store) = store("malformed");
    let mut document = sample_document(8, 8, 1, 0);
    document.source.frame.stride = 3; // 8px of RGBA cannot fit in 3 bytes.

    let err = store
        .insert(NewCapture::new(&document))
        .expect_err("a frame that lies about its geometry must not enter history");
    assert!(format!("{err}").contains("stride"), "{err}");
    assert_eq!(store.count().expect("count"), 0);
}

#[test]
fn identical_captures_share_one_blob_on_disk() {
    let (_dir, mut store) = store("dedupe");
    let document = sample_document(16, 16, 3, 0);

    let first = store.insert(NewCapture::new(&document)).expect("insert");
    let second = store.insert(NewCapture::new(&document)).expect("insert");
    assert_ne!(first, second, "the same pixels are still two captures");

    let one_image = document.source.frame.data.len() as u64;
    assert_eq!(
        store.stored_image_bytes().expect("size"),
        one_image,
        "content addressing means re-capturing an unchanged window is nearly free"
    );
    assert_eq!(store.count().expect("count"), 2);
}

#[test]
fn history_lists_newest_first_and_pages_without_repeating() {
    let (_dir, mut store) = store("paging");
    let base = 1_700_000_000_000;

    for i in 0..12 {
        let document = sample_document(8, 8, i as u8, 0);
        store
            .insert(
                NewCapture::new(&document)
                    .taken_at(Timestamp(base + i * 1_000))
                    .titled(format!("capture {i}")),
            )
            .expect("insert");
    }

    let first = store.page(Page::new(5, 0)).expect("page");
    let second = store.page(Page::new(5, 5)).expect("page");
    let third = store.page(Page::new(5, 10)).expect("page");
    assert_eq!((first.len(), second.len(), third.len()), (5, 5, 2));

    assert_eq!(first[0].window_title.as_deref(), Some("capture 11"));
    assert_eq!(third[1].window_title.as_deref(), Some("capture 0"));

    let mut seen: Vec<_> = first
        .iter()
        .chain(&second)
        .chain(&third)
        .map(|r| r.id.clone())
        .collect();
    let total = seen.len();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    seen.dedup();
    assert_eq!(seen.len(), total, "paging must not repeat or drop a row");
}

#[test]
fn search_finds_captures_by_app_title_text_and_date() {
    let (_dir, mut store) = store("search");
    let base = 1_700_000_000_000;
    let day = 86_400_000;

    let a = sample_document(8, 8, 1, 0);
    let b = sample_document(8, 8, 2, 0);
    let c = sample_document(8, 8, 3, 0);

    let safari = store
        .insert(
            NewCapture::new(&a)
                .from_app("Safari")
                .titled("Invoice 4021")
                .with_ocr("Amount due: 1240")
                .taken_at(Timestamp(base)),
        )
        .expect("insert");
    let terminal = store
        .insert(
            NewCapture::new(&b)
                .from_app("Terminal")
                .titled("cargo test")
                .with_ocr("test result: ok. 46 passed")
                .taken_at(Timestamp(base + day)),
        )
        .expect("insert");
    let figma = store
        .insert(
            NewCapture::new(&c)
                .from_app("Figma")
                .titled("Präsentation — Entwurf")
                .taken_at(Timestamp(base + 2 * day)),
        )
        .expect("insert");

    let by_app = store
        .search(&SearchQuery::all().app("safari"))
        .expect("search");
    assert_eq!(by_app.len(), 1);
    assert_eq!(by_app[0].id, safari);

    let by_title = store
        .search(&SearchQuery::all().titled("cargo"))
        .expect("search");
    assert_eq!(by_title.len(), 1);
    assert_eq!(by_title[0].id, terminal);

    let by_ocr = store
        .search(&SearchQuery::all().ocr("46 passed"))
        .expect("search");
    assert_eq!(by_ocr.len(), 1);
    assert_eq!(by_ocr[0].id, terminal);

    let by_text = store
        .search(&SearchQuery::all().text("1240"))
        .expect("free text spans app, title and OCR");
    assert_eq!(by_text.len(), 1);
    assert_eq!(by_text[0].id, safari);

    let folded = store
        .search(&SearchQuery::all().text("präsentation"))
        .expect("search");
    assert_eq!(
        folded.len(),
        1,
        "case folding must handle more than ASCII: {folded:?}"
    );
    assert_eq!(folded[0].id, figma);

    let window = store
        .search(
            &SearchQuery::all()
                .after(Timestamp(base + day))
                .before(Timestamp(base + day + 1)),
        )
        .expect("search");
    assert_eq!(window.len(), 1);
    assert_eq!(window[0].id, terminal);

    let nothing = store
        .search(&SearchQuery::all().text("no such capture"))
        .expect("search");
    assert!(nothing.is_empty());
}

#[test]
fn media_kind_round_trips_filters_and_defaults_to_screenshot() {
    let (dir, mut store) = store("media-kind");
    let screenshot = sample_document(8, 8, 1, 0);
    let video = sample_document(8, 8, 2, 0);
    let gif = sample_document(8, 8, 3, 0);

    let screenshot_id = store
        .insert(NewCapture::new(&screenshot).taken_at(Timestamp(1)))
        .expect("screenshot");
    let video_id = store
        .insert(
            NewCapture::of_kind(&video, MediaKind::Video)
                .taken_at(Timestamp(2))
                .from_app("Recorder"),
        )
        .expect("video");
    let gif_id = store
        .insert(
            NewCapture::of_kind(&gif, MediaKind::Gif)
                .taken_at(Timestamp(3))
                .from_app("Recorder"),
        )
        .expect("gif");

    assert_eq!(
        store
            .record(&screenshot_id)
            .expect("read")
            .expect("present")
            .media_kind,
        MediaKind::Screenshot
    );
    assert_eq!(
        store
            .search(&SearchQuery::all().kind(MediaKind::Video))
            .expect("videos")
            .iter()
            .map(|record| &record.id)
            .collect::<Vec<_>>(),
        vec![&video_id]
    );
    assert_eq!(
        store
            .search(&SearchQuery::all().kind(MediaKind::Gif))
            .expect("gifs")
            .iter()
            .map(|record| &record.id)
            .collect::<Vec<_>>(),
        vec![&gif_id]
    );

    drop(store);
    let store = SqliteStore::open(dir.path()).expect("reopen");
    assert_eq!(
        store
            .record(&video_id)
            .expect("read")
            .expect("present")
            .media_kind,
        MediaKind::Video
    );
}

#[test]
fn count_matching_ignores_pagination_and_apps_are_distinct_and_sorted() {
    let (_dir, mut store) = store("count-and-apps");
    for (index, app) in ["Terminal", "Safari", "terminal", "Mail"]
        .into_iter()
        .enumerate()
    {
        let document = sample_document(8, 8, index as u8, 0);
        store
            .insert(
                NewCapture::new(&document)
                    .from_app(app)
                    .with_ocr("needle")
                    .taken_at(Timestamp(index as i64)),
            )
            .expect("insert");
    }
    let no_app = sample_document(8, 8, 9, 0);
    store.insert(NewCapture::new(&no_app)).expect("insert");

    let query = SearchQuery::all().text("needle").paged(Page::new(1, 2));
    assert_eq!(store.search(&query).expect("page").len(), 1);
    assert_eq!(
        store.count_matching(&query).expect("matching count"),
        4,
        "the total must not shrink to the requested page"
    );
    assert_eq!(
        store.apps().expect("apps"),
        vec!["Mail", "Safari", "Terminal"],
        "case-only duplicates and missing app names do not become filter choices"
    );
}

#[test]
fn search_treats_sql_wildcards_as_ordinary_characters() {
    let (_dir, mut store) = store("wildcards");
    let a = sample_document(8, 8, 1, 0);
    let b = sample_document(8, 8, 2, 0);

    store
        .insert(NewCapture::new(&a).titled("report_2024"))
        .expect("insert");
    store
        .insert(NewCapture::new(&b).titled("reportX2024"))
        .expect("insert");

    let found = store
        .search(&SearchQuery::all().titled("report_2024"))
        .expect("search");
    assert_eq!(
        found.len(),
        1,
        "an underscore the user typed is an underscore, not a SQL wildcard"
    );
    assert_eq!(found[0].window_title.as_deref(), Some("report_2024"));

    let percent = store
        .search(&SearchQuery::all().titled("%"))
        .expect("search");
    assert!(
        percent.is_empty(),
        "a bare % must match nothing, not everything"
    );
}

#[test]
fn editing_a_capture_persists_and_never_touches_its_pixels() {
    let (_dir, mut store) = store("editing");
    let document = sample_document(16, 16, 5, 0);
    let pixels = document.source.frame.data.clone();
    let id = store.insert(NewCapture::new(&document)).expect("insert");

    let DocumentState::Complete(mut live) = store.document(&id).expect("read").expect("present")
    else {
        panic!("pixels must be present");
    };
    live.add(
        Annotation::Arrow {
            from: LogicalPoint::new(1.0, 1.0),
            to: LogicalPoint::new(9.0, 9.0),
        },
        Style::stroked(),
    );
    store.save_document(&id, &live).expect("save");

    let DocumentState::Complete(reloaded) = store.document(&id).expect("read").expect("present")
    else {
        panic!("pixels must still be present");
    };
    assert_eq!(reloaded.annotations().len(), 1);
    assert_eq!(
        reloaded.source.frame.data, pixels,
        "D14: annotations are an overlay; the capture underneath is untouched"
    );
    assert_eq!(
        store
            .record(&id)
            .expect("read")
            .expect("present")
            .annotation_count,
        1,
        "the index must reflect the edit without a reconcile"
    );
}

#[test]
fn pinning_survives_a_reopen() {
    let (dir, mut store) = store("pinning");
    let document = sample_document(8, 8, 9, 0);
    let id = store.insert(NewCapture::new(&document)).expect("insert");

    store.set_pinned(&id, true).expect("pin");
    assert!(store.record(&id).expect("read").expect("present").pinned);

    drop(store);
    let store = SqliteStore::open(dir.path()).expect("reopen");
    assert!(
        store.record(&id).expect("read").expect("present").pinned,
        "a pin the user set must not evaporate on restart"
    );

    let pinned = store
        .search(&SearchQuery::all().pinned_only())
        .expect("search");
    assert_eq!(pinned.len(), 1);
}

#[test]
fn screen_pin_state_is_authoritative_in_the_sidecar_and_unpin_clears_it() {
    let (dir, mut store) = store("screen-pinning");
    let document = sample_document(800, 600, 9, 0);
    let id = store.insert(NewCapture::new(&document)).expect("insert");
    let mut pin = PinState::new(
        LogicalRect::new(
            LogicalPoint::new(101.5, 42.0),
            LogicalSize::new(400.0, 300.0),
        ),
        PinScale::new(0.5),
        Some(DisplayId("external".into())),
    );
    pin.opacity = Opacity::new(0.62);
    pin.locked = true;

    store
        .set_screen_pin(&id, Some(&pin))
        .expect("persist screen pin");
    let sidecar = store
        .layout()
        .read_record(&id)
        .expect("read sidecar")
        .expect("sidecar exists");
    assert_eq!(sidecar.screen_pin.as_ref(), Some(&pin));
    assert!(sidecar.pinned, "an on-screen pin must be retention-exempt");

    drop(store);
    let mut store = SqliteStore::open(dir.path()).expect("reopen");
    let restored = store.record(&id).expect("read").expect("present");
    assert_eq!(restored.screen_pin.as_ref(), Some(&pin));
    assert!(restored.pinned);

    store.set_pinned(&id, false).expect("unpin");
    let cleared = store.record(&id).expect("read").expect("present");
    assert!(!cleared.pinned);
    assert_eq!(
        cleared.screen_pin, None,
        "a CLI-style unpin must not leave a window that restores on restart"
    );
}

#[test]
fn first_cache_bootstrap_repairs_legacy_stale_pins_when_record_counts_match() {
    let (dir, store) = store("screen-pin-equal-count-recovery");
    let document = sample_document(800, 600, 11, 0);
    let id = {
        let mut store = store;
        store.insert(NewCapture::new(&document)).expect("insert")
    };
    let pin = PinState::new(
        LogicalRect::new(
            LogicalPoint::new(90.0, 60.0),
            LogicalSize::new(400.0, 300.0),
        ),
        PinScale::new(0.5),
        Some(DisplayId("main".into())),
    );

    let layout = scrozz_store::StoreLayout::new(dir.path());
    let conn = rusqlite::Connection::open(layout.index_path()).expect("open raw index");
    conn.execute(
        "DELETE FROM store_meta WHERE key = 'pin_cache_sidecars_v1'",
        [],
    )
    .expect("simulate a store from before pin cache bootstrap");
    drop(conn);
    let mut sidecar = layout
        .read_record(&id)
        .expect("read sidecar")
        .expect("sidecar exists");
    sidecar.pinned = true;
    sidecar.screen_pin = Some(pin.clone());
    layout
        .write_record(&sidecar)
        .expect("simulate a crash after the sidecar rename");

    let store = SqliteStore::open(dir.path()).expect("reopen repairs the cache");
    let restored = store.record(&id).expect("read").expect("present");
    assert!(restored.pinned);
    assert_eq!(restored.screen_pin, Some(pin));
    assert_eq!(
        store
            .search(&SearchQuery::all().pinned_only())
            .expect("pinned query")
            .len(),
        1,
        "retention queries must see the repaired cache"
    );
}

#[test]
fn clean_reopen_does_not_rewrite_every_pin_cache() {
    let (dir, mut store) = store("screen-pin-clean-reopen");
    let document = sample_document(800, 600, 13, 0);
    let id = store.insert(NewCapture::new(&document)).expect("insert");
    let index = store.layout().index_path();
    drop(store);

    let conn = rusqlite::Connection::open(index).expect("open raw index");
    conn.execute_batch(
        "CREATE TRIGGER reject_startup_pin_rewrite
         BEFORE UPDATE OF pinned ON captures
         BEGIN
             SELECT RAISE(FAIL, 'startup rewrote a clean pin cache');
         END;",
    )
    .expect("install rewrite detector");
    drop(conn);

    let store = SqliteStore::open(dir.path()).expect("clean reopen avoids cache writes");
    assert!(store.record(&id).expect("read").is_some());
}

#[test]
fn history_queries_read_screen_pin_state_from_the_index_cache() {
    let (_dir, mut store) = store("screen-pin-index-read");
    let document = sample_document(800, 600, 14, 0);
    let id = store.insert(NewCapture::new(&document)).expect("insert");
    let pin = PinState::new(
        LogicalRect::new(
            LogicalPoint::new(100.0, 70.0),
            LogicalSize::new(400.0, 300.0),
        ),
        PinScale::new(0.5),
        Some(DisplayId("main".into())),
    );
    store
        .set_screen_pin(&id, Some(&pin))
        .expect("persist screen pin");

    let sidecar = store.layout().record_path(&id).expect("sidecar path");
    std::fs::remove_file(sidecar).expect("temporarily remove authoritative sidecar");

    let record = store.record(&id).expect("indexed read").expect("present");
    assert_eq!(record.screen_pin, Some(pin.clone()));
    let found = store
        .search(&SearchQuery::all().pinned_only())
        .expect("indexed search");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].screen_pin, Some(pin));
}

#[test]
fn a_failed_pin_cache_write_is_recovered_from_the_durable_sidecar() {
    let (dir, mut store) = store("screen-pin-cache-failure");
    let document = sample_document(800, 600, 12, 0);
    let id = store.insert(NewCapture::new(&document)).expect("insert");
    let index = store.layout().index_path();
    drop(store);

    let conn = rusqlite::Connection::open(&index).expect("open raw index");
    conn.execute_batch(
        "CREATE TRIGGER reject_screen_pin
         BEFORE INSERT ON capture_pins
         BEGIN
             SELECT RAISE(FAIL, 'forced pin cache failure');
         END;",
    )
    .expect("install failure trigger");
    drop(conn);

    let mut store = SqliteStore::open(dir.path()).expect("open before pinning");
    let pin = PinState::new(
        LogicalRect::new(
            LogicalPoint::new(100.0, 70.0),
            LogicalSize::new(400.0, 300.0),
        ),
        PinScale::new(0.5),
        Some(DisplayId("main".into())),
    );
    let error = store
        .set_screen_pin(&id, Some(&pin))
        .expect_err("the forced cache failure must be reported");
    assert!(
        error.to_string().contains("index reconciliation"),
        "{error}"
    );
    drop(store);

    let conn = rusqlite::Connection::open(&index).expect("open raw index");
    conn.execute("DROP TRIGGER reject_screen_pin", [])
        .expect("remove failure trigger");
    drop(conn);

    let store = SqliteStore::open(dir.path()).expect("reopen repairs from sidecar");
    let restored = store.record(&id).expect("read").expect("present");
    assert!(restored.pinned);
    assert_eq!(restored.screen_pin, Some(pin));
}

#[test]
fn pinning_an_unknown_capture_is_an_error_not_a_silent_no_op() {
    let (_dir, mut store) = store("pin-missing");
    let err = store
        .set_pinned(&scrozz_store::CaptureId("nope".into()), true)
        .expect_err("must report the miss");
    assert!(format!("{err}").contains("nope"), "{err}");
}

#[test]
fn deleting_a_capture_removes_its_record_and_its_pixels() {
    let (dir, mut store) = store("delete");
    let a = sample_document(16, 16, 1, 0);
    let b = sample_document(16, 16, 2, 0);
    let doomed = store.insert(NewCapture::new(&a)).expect("insert");
    let kept = store.insert(NewCapture::new(&b)).expect("insert");

    assert!(store.delete(&doomed).expect("delete"));
    assert!(
        !store.delete(&doomed).expect("delete"),
        "deleting twice is not an error"
    );

    assert!(store.record(&doomed).expect("read").is_none());
    assert!(store.record(&kept).expect("read").is_some());
    assert_eq!(
        store.stored_image_bytes().expect("size"),
        b.source.frame.data.len() as u64
    );

    let documents = std::fs::read_dir(store.layout().documents_dir())
        .expect("documents directory")
        .count();
    assert_eq!(documents, 1, "delete must remove the durable record too");
}

#[test]
fn deleting_one_of_two_identical_captures_keeps_the_shared_blob() {
    let (_dir, mut store) = store("delete-shared");
    let document = sample_document(16, 16, 4, 0);
    let first = store.insert(NewCapture::new(&document)).expect("insert");
    let second = store.insert(NewCapture::new(&document)).expect("insert");

    assert!(store.delete(&first).expect("delete"));

    let bytes = store.image(&second).expect("read").expect("still there");
    assert_eq!(
        bytes, document.source.frame.data,
        "deduplication must not let one delete take another capture's pixels"
    );
}

#[test]
fn recognised_text_can_be_attached_after_the_fact() {
    let (_dir, mut store) = store("ocr-later");
    let document = sample_document(8, 8, 6, 0);
    let id = store.insert(NewCapture::new(&document)).expect("insert");

    assert!(
        store
            .search(&SearchQuery::all().text("deploy"))
            .expect("search")
            .is_empty()
    );

    store
        .set_ocr_text(&id, Some("Deploy to production"))
        .expect("set OCR");

    let found = store
        .search(&SearchQuery::all().text("deploy"))
        .expect("search");
    assert_eq!(found.len(), 1, "OCR must be searchable as soon as it lands");
    assert_eq!(found[0].id, id);

    store.set_ocr_text(&id, None).expect("clear OCR");
    assert!(
        store
            .search(&SearchQuery::all().text("deploy"))
            .expect("search")
            .is_empty()
    );
}

#[test]
fn history_is_ordered_by_capture_time_not_by_insertion_order() {
    let (_dir, mut store) = store("ordering");
    let base = 1_700_000_000_000;

    let older = sample_document(8, 8, 1, 0);
    let newer = sample_document(8, 8, 2, 0);

    // Imported out of order, as a backfill would do.
    let newer_id = store
        .insert(NewCapture::new(&newer).taken_at(Timestamp(base + 10_000)))
        .expect("insert");
    let older_id = store
        .insert(NewCapture::new(&older).taken_at(Timestamp(base)))
        .expect("insert");

    let listed = store.list().expect("list");
    assert_eq!(listed, vec![newer_id, older_id]);
}

#[test]
fn an_image_state_reports_what_actually_happened_to_the_pixels() {
    let (_dir, mut store) = store("image-state");
    let document = sample_document(8, 8, 8, 0);
    let id = store.insert(NewCapture::new(&document)).expect("insert");

    match store.record(&id).expect("read").expect("present").image {
        ImageState::Present { byte_len, ref hash } => {
            assert_eq!(byte_len, document.source.frame.data.len() as u64);
            assert_eq!(hash.len(), 64);
        }
        other => panic!("expected present pixels, got {other:?}"),
    }
}

// ─── Video recording tests ──────────────────────────────────────────────────

use scrozz_store::{NewRecording, VideoCompletion, VideoMetadata, VideoSalvageability};

fn sample_video_file(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("recording.mov");
    std::fs::write(&path, b"fake video content").expect("write test video");
    std::fs::canonicalize(&path).expect("canonicalize")
}

fn sample_video_metadata(path: &std::path::Path) -> VideoMetadata {
    VideoMetadata {
        path: path.to_path_buf(),
        duration_secs: 6.75,
        engine: "AVFoundation".into(),
        completion: VideoCompletion::Complete,
        size: Some(scrozz_core::PhysicalSize::new(1728.0, 1116.0)),
        frames: Some(405),
        audio_channels: Some(2),
        file_size_bytes: Some(123_456),
        codec: Some("h264".to_owned()),
        quality: Some("balanced".to_owned()),
        resolution: Some("native".to_owned()),
    }
}

#[test]
fn a_recording_round_trips_through_history() {
    let (dir, mut store) = store("recording-round-trip");
    let video_path = sample_video_file(dir.path());
    let meta = sample_video_metadata(&video_path);

    let id = store
        .insert_recording(
            NewRecording::new(meta.clone())
                .from_app("Scrozz")
                .with_app_identifier("com.scrozz.app")
                .titled("My recording")
                .with_window_shadow(false)
                .taken_at(Timestamp(1_700_000_000_000)),
        )
        .expect("insert recording");

    let record = store.record(&id).expect("read").expect("present");
    assert_eq!(record.media_kind, MediaKind::Video);
    assert_eq!(record.app_name.as_deref(), Some("Scrozz"));
    assert_eq!(record.app_identifier.as_deref(), Some("com.scrozz.app"));
    assert_eq!(record.window_title.as_deref(), Some("My recording"));
    assert_eq!(record.window_shadow, Some(false));
    assert!(record.frame.is_none());
    assert_eq!(record.image, ImageState::Absent);
    let video = record.video.expect("video metadata present");
    assert_eq!(video.duration_secs, 6.75);
    assert_eq!(video.engine, "AVFoundation");
    assert!(matches!(video.completion, VideoCompletion::Complete));
    assert_eq!(video.size.map(|s| s.width), Some(1728.0));
    assert_eq!(video.frames, Some(405));
    assert_eq!(video.audio_channels, Some(2));
    assert_eq!(video.file_size_bytes, Some(123_456));
    assert_eq!(video.codec.as_deref(), Some("h264"));
    assert_eq!(video.quality.as_deref(), Some("balanced"));
    assert_eq!(video.resolution.as_deref(), Some("native"));
}

#[test]
fn recording_insert_rejects_empty_path() {
    let (_dir, mut store) = store("recording-empty-path");
    let meta = VideoMetadata {
        path: std::path::PathBuf::new(),
        duration_secs: 1.0,
        engine: "test".into(),
        completion: VideoCompletion::Complete,
        size: None,
        frames: None,
        audio_channels: None,
        file_size_bytes: None,
        codec: None,
        quality: None,
        resolution: None,
    };
    let err = store
        .insert_recording(NewRecording::new(meta))
        .expect_err("empty path must fail");
    assert!(format!("{err}").contains("empty"), "{err}");
}

#[test]
fn recording_insert_rejects_relative_path() {
    let (_dir, mut store) = store("recording-relative-path");
    let meta = VideoMetadata {
        path: "relative/video.mov".into(),
        duration_secs: 1.0,
        engine: "test".into(),
        completion: VideoCompletion::Complete,
        size: None,
        frames: None,
        audio_channels: None,
        file_size_bytes: None,
        codec: None,
        quality: None,
        resolution: None,
    };
    let err = store
        .insert_recording(NewRecording::new(meta))
        .expect_err("relative path must fail");
    assert!(format!("{err}").contains("absolute"), "{err}");
}

#[test]
fn recording_insert_rejects_nonexistent_path() {
    let (_dir, mut store) = store("recording-missing-path");
    let meta = VideoMetadata {
        path: "/definitely/not/a/real/file.mov".into(),
        duration_secs: 1.0,
        engine: "test".into(),
        completion: VideoCompletion::Complete,
        size: None,
        frames: None,
        audio_channels: None,
        file_size_bytes: None,
        codec: None,
        quality: None,
        resolution: None,
    };
    let err = store
        .insert_recording(NewRecording::new(meta))
        .expect_err("nonexistent path must fail");
    assert!(
        format!("{err}").contains("canonical") || format!("{err}").contains("accessible"),
        "{err}"
    );
}

#[test]
fn legacy_null_video_json_rows_remain_listable() {
    let (dir, mut store) = store("legacy-null-video");
    // Insert a normal screenshot — it has video: None (NULL video_json).
    let document = sample_document(8, 8, 1, 0);
    let id = store.insert(NewCapture::new(&document)).expect("insert");

    let record = store.record(&id).expect("read").expect("present");
    assert!(record.video.is_none());
    assert_eq!(record.media_kind, MediaKind::Screenshot);

    // The capture is listable.
    let all = store.search(&SearchQuery::all()).expect("search");
    assert_eq!(all.len(), 1);
    assert!(all[0].video.is_none());
}

#[test]
fn unknown_video_json_degrades_only_that_row() {
    let (dir, mut store) = store("unknown-video-json");
    let video_path = sample_video_file(dir.path());
    let id = store
        .insert_recording(NewRecording::new(sample_video_metadata(&video_path)))
        .expect("insert recording");
    let screenshot = store
        .insert(NewCapture::new(&sample_document(8, 8, 2, 0)))
        .expect("insert screenshot");
    let index = store.layout().index_path().to_path_buf();
    drop(store);

    let conn = rusqlite::Connection::open(index).expect("open raw index");
    conn.execute(
        "UPDATE captures SET video_json = ?1 WHERE id = ?2",
        rusqlite::params![
            r#"{"completion":"Complete","path":"/future/video.mov"}"#,
            id.0
        ],
    )
    .expect("write future metadata");
    drop(conn);

    let reopened = SqliteStore::open(dir.path()).expect("reopen");
    let page = reopened.search(&SearchQuery::all()).expect("list history");
    assert_eq!(page.len(), 2);
    assert!(
        page.iter()
            .find(|record| record.id == id)
            .is_some_and(|record| record.video.is_none())
    );
    assert!(page.iter().any(|record| record.id == screenshot));
}

#[test]
fn deleting_a_recording_does_not_delete_the_durable_media_file() {
    let (dir, mut store) = store("recording-delete-preserves-media");
    let video_path = sample_video_file(dir.path());
    let meta = sample_video_metadata(&video_path);

    let id = store
        .insert_recording(NewRecording::new(meta).taken_at(Timestamp(1_700_000_000_000)))
        .expect("insert recording");

    assert!(store.delete(&id).expect("delete"));
    assert!(
        video_path.exists(),
        "deletion must never unlink the externally-owned durable media file"
    );
}

#[test]
fn partial_video_completion_round_trips() {
    let (dir, mut store) = store("recording-partial");
    let video_path = sample_video_file(dir.path());
    let meta = VideoMetadata {
        path: video_path.clone(),
        duration_secs: 3.0,
        engine: "test".into(),
        completion: VideoCompletion::Partial {
            salvageability: VideoSalvageability::Playable,
            reason: "encoder interrupted".into(),
        },
        size: None,
        frames: None,
        audio_channels: None,
        file_size_bytes: None,
        codec: None,
        quality: None,
        resolution: None,
    };
    let id = store
        .insert_recording(NewRecording::new(meta))
        .expect("insert");

    let record = store.record(&id).expect("read").expect("present");
    match &record.video.expect("metadata").completion {
        VideoCompletion::Partial {
            salvageability,
            reason,
        } => {
            assert!(salvageability.is_playable());
            assert!(reason.contains("interrupted"));
        }
        other => panic!("expected partial, got {other:?}"),
    }
}

#[test]
fn sidecar_recovery_preserves_recording_metadata() {
    let (dir, mut store) = store("recording-recovery");
    let video_path = sample_video_file(dir.path());
    let meta = sample_video_metadata(&video_path);

    let id = store
        .insert_recording(
            NewRecording::new(meta.clone())
                .from_app("Scrozz")
                .taken_at(Timestamp(1_700_000_000_000)),
        )
        .expect("insert");

    // Force a full index rebuild from sidecars.
    let report = store.recover().expect("recovery");
    assert_eq!(report.records_recovered, 1);

    let record = store.record(&id).expect("read").expect("present");
    assert_eq!(record.media_kind, MediaKind::Video);
    let video = record.video.expect("metadata survives recovery");
    assert_eq!(video.duration_secs, 6.75);
    assert_eq!(video.engine, "AVFoundation");
}
