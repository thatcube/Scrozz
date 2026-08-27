//! History as the app actually uses it: insert, read back, page, search, delete.

use scrozz_annotate::{Annotation, Style};
use scrozz_core::LogicalPoint;
use scrozz_store::{
    DocumentState, History as _, ImageState, NewCapture, Page, SearchQuery, SqliteStore, Store as _,
    Timestamp,
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

    let DocumentState::Complete(mut live) =
        store.document(&id).expect("read").expect("present")
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

    let DocumentState::Complete(reloaded) =
        store.document(&id).expect("read").expect("present")
    else {
        panic!("pixels must still be present");
    };
    assert_eq!(reloaded.annotations().len(), 1);
    assert_eq!(
        reloaded.source.frame.data, pixels,
        "D14: annotations are an overlay; the capture underneath is untouched"
    );
    assert_eq!(
        store.record(&id).expect("read").expect("present").annotation_count,
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
    assert!(!store.delete(&doomed).expect("delete"), "deleting twice is not an error");

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
