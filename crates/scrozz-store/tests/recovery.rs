//! Losing a user's capture history is unforgivable, so the index is a cache.
//!
//! Everything durable lives in the per-capture JSON records and the
//! content-addressed blobs beside them. These tests destroy the SQLite file in
//! the ways it actually gets destroyed — truncation, garbage, a half-finished
//! write — and insist the history comes back.

use scrozz_store::{
    CaptureId, DocumentState, History as _, ImageState, NewCapture, SearchQuery, SqliteStore,
    Store as _, StoredRecord, Timestamp,
    test_support::{ScratchDir, richly_annotated_document, sample_document, scratch_dir},
};
use std::fs;
use std::io::Write as _;

/// Builds a store with a known, checkable history.
fn seeded(label: &str) -> (ScratchDir, Vec<CaptureId>) {
    let dir = scratch_dir(label);
    let mut store = SqliteStore::open(dir.path()).expect("open");
    let base = 1_700_000_000_000;
    let mut ids = Vec::new();

    for n in 0..5u8 {
        let document = sample_document(8, 8, n, 2);
        ids.push(
            store
                .insert(
                    NewCapture::new(&document)
                        .taken_at(Timestamp(base + i64::from(n) * 1_000))
                        .from_app("Safari")
                        .titled(format!("tab {n}"))
                        .with_ocr(format!("body text {n}")),
                )
                .expect("insert"),
        );
    }

    let rich = richly_annotated_document(42);
    ids.push(
        store
            .insert(
                NewCapture::new(&rich)
                    .taken_at(Timestamp(base + 9_000))
                    .titled("the annotated one"),
            )
            .expect("insert"),
    );
    store.set_pinned(&ids[0], true).expect("pin");
    (dir, ids)
}

fn assert_history_intact(store: &mut SqliteStore, ids: &[CaptureId]) {
    assert_eq!(store.count().expect("count") as usize, ids.len());
    for (n, id) in ids.iter().take(5).enumerate() {
        let record = store
            .record(id)
            .expect("read")
            .expect("every capture must come back");
        assert_eq!(record.app_name.as_deref(), Some("Safari"));
        assert_eq!(record.window_title.as_deref(), Some(&*format!("tab {n}")));
        assert_eq!(record.ocr_text.as_deref(), Some(&*format!("body text {n}")));
        assert_eq!(record.annotation_count, 2, "the edits came back too");
        assert!(record.image.is_present(), "and so did the pixels");
        assert!(store.image(id).expect("read").is_some());
    }
    assert!(
        store
            .record(&ids[0])
            .expect("read")
            .expect("present")
            .pinned,
        "a pin is part of the history, not part of the cache"
    );

    let rich = ids.last().expect("rich capture");
    let DocumentState::Complete(document) = store.document(rich).expect("read").expect("present")
    else {
        panic!("the pixels are still there, so the document must be complete");
    };
    assert!(document.len() >= 3, "the full annotation set survived");
    assert!(
        document.beautification().is_some(),
        "beautification is part of the document and must survive too"
    );
}

#[test]
fn a_truncated_index_is_rebuilt_from_the_durable_records() {
    let (dir, ids) = seeded("truncated-index");
    let index = SqliteStore::open(dir.path())
        .expect("open")
        .layout()
        .index_path();

    // Chop the file in half: a classic result of losing power mid-checkpoint.
    let bytes = fs::read(&index).expect("read index");
    assert!(bytes.len() > 4_096, "need a database worth truncating");
    fs::write(&index, &bytes[..bytes.len() / 2]).expect("truncate");

    let mut store = SqliteStore::open(dir.path()).expect("a corrupt index must not be fatal");
    assert_history_intact(&mut store, &ids);
}

#[test]
fn an_index_full_of_garbage_is_rebuilt_from_the_durable_records() {
    let (dir, ids) = seeded("garbage-index");
    let index = SqliteStore::open(dir.path())
        .expect("open")
        .layout()
        .index_path();

    // Not a database at all. SQLite rejects this at open, before any query.
    let mut file = fs::File::create(&index).expect("create");
    file.write_all(&[0x5A; 32_768]).expect("write garbage");
    file.sync_all().expect("sync");
    drop(file);

    let mut store = SqliteStore::open(dir.path()).expect("garbage must not be fatal");
    assert_history_intact(&mut store, &ids);
}

#[test]
fn a_deleted_index_is_rebuilt_from_the_durable_records() {
    let (dir, ids) = seeded("deleted-index");
    let index = SqliteStore::open(dir.path())
        .expect("open")
        .layout()
        .index_path();
    for suffix in ["", "-wal", "-shm"] {
        let path = index.with_file_name(format!(
            "{}{suffix}",
            index.file_name().expect("name").to_string_lossy()
        ));
        let _ = fs::remove_file(path);
    }

    let mut store = SqliteStore::open(dir.path()).expect("a missing index must not be fatal");
    assert_history_intact(&mut store, &ids);
}

#[test]
fn the_corrupt_file_is_quarantined_rather_than_silently_destroyed() {
    let (dir, _ids) = seeded("quarantine");
    let index = SqliteStore::open(dir.path())
        .expect("open")
        .layout()
        .index_path();
    fs::write(&index, b"this is not a database").expect("corrupt");

    let _store = SqliteStore::open(dir.path()).expect("open");

    let quarantined: Vec<_> = fs::read_dir(dir.path())
        .expect("read root")
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"))
        .collect();
    assert_eq!(
        quarantined.len(),
        1,
        "the broken file is kept for forensics, not deleted"
    );
    assert_eq!(
        fs::read(quarantined[0].path()).expect("read"),
        b"this is not a database",
        "quarantine must preserve the bytes exactly"
    );
}

#[test]
fn reconciling_a_healthy_store_reports_a_clean_bill_of_health() {
    let (dir, ids) = seeded("recovery-report");
    let mut store = SqliteStore::open(dir.path()).expect("open");

    let report = store.reconcile().expect("reconcile");

    assert_eq!(report.records_recovered, ids.len());
    assert_eq!(report.records_unreadable, 0);
    assert_eq!(
        report.rows_dropped, 0,
        "nothing was stale, so nothing was dropped"
    );
    assert_eq!(
        report.blobs_found,
        ids.len(),
        "one blob per distinct capture"
    );
    assert!(report.bytes_found > 0);
    assert_eq!(report.images_missing, 0);
    assert!(
        report.quarantined.is_none(),
        "reconciling does not move anything aside"
    );
    assert_eq!(store.count().expect("count") as usize, ids.len());
}

#[test]
fn a_record_written_before_the_commit_landed_is_adopted_on_the_next_open() {
    // The crash window: the durable record is on disk but the index row is not.
    // The record is the source of truth, so the capture must appear anyway.
    let dir = scratch_dir("orphan-record");
    let mut store = SqliteStore::open(dir.path()).expect("open");
    let document = sample_document(8, 8, 3, 1);
    let known = store.insert(NewCapture::new(&document)).expect("insert");

    // Produce a real sidecar the honest way, in a store of its own, then move
    // only the JSON across — no blob, no index row. That is precisely the state
    // a crash between `write_record` and `COMMIT` leaves behind.
    let elsewhere = scratch_dir("orphan-record-source");
    let mut source = SqliteStore::open(elsewhere.path()).expect("open");
    let orphan = richly_annotated_document(11);
    let orphan_id = source
        .insert(
            NewCapture::new(&orphan)
                .taken_at(Timestamp(1_700_000_500_000))
                .titled("written but never committed"),
        )
        .expect("insert");
    let from = source.layout().record_path(&orphan_id).expect("path");
    let to = store.layout().record_path(&orphan_id).expect("path");
    let record = StoredRecord::from_json(&fs::read(&from).expect("read")).expect("parse");
    fs::copy(&from, &to).expect("plant the orphan record");
    drop(source);
    drop(store);

    let mut store = SqliteStore::open(dir.path()).expect("reopen");

    assert_eq!(store.count().expect("count"), 2_u64);
    let adopted = store
        .record(&orphan_id)
        .expect("read")
        .expect("the orphan must be adopted");
    assert_eq!(adopted.window_title, record.window_title);
    assert!(adopted.annotation_count >= 3);
    assert!(store.record(&known).expect("read").is_some());

    // Its pixels were never written, so it is honest about that rather than
    // claiming an image it cannot produce.
    assert!(matches!(
        adopted.image,
        ImageState::Absent | ImageState::Evicted { .. }
    ));
    assert!(store.image(&orphan_id).expect("read").is_none());
}

#[test]
fn an_index_row_whose_record_vanished_is_dropped_instead_of_haunting_history() {
    let dir = scratch_dir("orphan-row");
    let mut store = SqliteStore::open(dir.path()).expect("open");
    let keep_doc = sample_document(8, 8, 1, 1);
    let keep = store.insert(NewCapture::new(&keep_doc)).expect("insert");
    let lose_doc = sample_document(8, 8, 2, 1);
    let lose = store.insert(NewCapture::new(&lose_doc)).expect("insert");

    fs::remove_file(store.layout().record_path(&lose).expect("path")).expect("delete record");

    let report = store.reconcile().expect("reconcile");

    assert_eq!(report.rows_dropped, 1);
    assert_eq!(store.count().expect("count"), 1_u64);
    assert!(store.record(&lose).expect("read").is_none());
    assert!(store.record(&keep).expect("read").is_some());
}

#[test]
fn a_missing_blob_downgrades_that_capture_and_leaves_everything_else_alone() {
    let dir = scratch_dir("missing-blob");
    let mut store = SqliteStore::open(dir.path()).expect("open");
    let mut ids = Vec::new();
    for n in 0..3u8 {
        let document = sample_document(8, 8, n, 1);
        ids.push(
            store
                .insert(NewCapture::new(&document).titled(format!("shot {n}")))
                .expect("insert"),
        );
    }

    let hash = match store.record(&ids[1]).expect("read").expect("present").image {
        ImageState::Present { hash, .. } => hash,
        other => panic!("expected pixels, got {other:?}"),
    };
    fs::remove_file(store.layout().blob_path(&hash).expect("path")).expect("delete blob");

    // Reading it must not error — it self-heals to the evicted state, which is
    // the truth: the capture is still history, it just lost its pixels.
    assert!(store.image(&ids[1]).expect("read must not fail").is_none());

    let record = store.record(&ids[1]).expect("read").expect("still listed");
    assert!(!record.image.is_present());
    assert_eq!(record.annotation_count, 1, "its edits are untouched");
    assert_eq!(store.count().expect("count"), 3_u64);
    for id in [&ids[0], &ids[2]] {
        assert!(
            store.image(id).expect("read").is_some(),
            "neighbours are fine"
        );
    }

    // And the downgrade is durable, not just a per-read fudge.
    drop(store);
    let store = SqliteStore::open(dir.path()).expect("reopen");
    assert!(
        !store
            .record(&ids[1])
            .expect("read")
            .expect("present")
            .image
            .is_present()
    );
}

#[test]
fn a_single_unreadable_record_does_not_take_the_rest_of_history_down() {
    let dir = scratch_dir("bad-record");
    let mut store = SqliteStore::open(dir.path()).expect("open");
    let mut ids = Vec::new();
    for n in 0..4u8 {
        let document = sample_document(8, 8, n, 1);
        ids.push(
            store
                .insert(NewCapture::new(&document).titled(format!("shot {n}")))
                .expect("insert"),
        );
    }
    let index = store.layout().index_path();
    let victim = store.layout().record_path(&ids[2]).expect("path");
    drop(store);

    fs::write(&victim, b"{ not json at all").expect("corrupt record");
    fs::write(&index, b"nor is this a database").expect("corrupt index");

    // Rebuilding has to walk every record. One bad file must cost one capture,
    // not the whole history.
    let store = SqliteStore::open(dir.path()).expect("open");
    assert_eq!(store.count().expect("count"), 3_u64);
    for id in [&ids[0], &ids[1], &ids[3]] {
        assert!(store.record(id).expect("read").is_some());
    }
    assert!(store.record(&ids[2]).expect("read").is_none());
}

#[test]
fn rebuilding_preserves_search_and_ordering_not_just_row_counts() {
    let (dir, ids) = seeded("rebuild-search");
    let index = SqliteStore::open(dir.path())
        .expect("open")
        .layout()
        .index_path();
    fs::write(&index, b"broken").expect("corrupt");

    let store = SqliteStore::open(dir.path()).expect("open");

    let by_app = store
        .search(&SearchQuery::all().app("Safari"))
        .expect("search");
    assert_eq!(by_app.len(), 5);
    let by_ocr = store
        .search(&SearchQuery::all().ocr("body text 3"))
        .expect("search");
    assert_eq!(by_ocr.len(), 1);
    assert_eq!(by_ocr[0].id, ids[3]);

    let listed = store.list().expect("list");
    assert_eq!(
        listed.len(),
        ids.len(),
        "a rebuilt index still orders history newest first"
    );
    assert_eq!(listed[0], *ids.last().expect("newest"));
    assert_eq!(listed[listed.len() - 1], ids[0]);
}

#[test]
fn a_rebuilt_store_is_immediately_writable_again() {
    let (dir, ids) = seeded("rebuild-then-write");
    let index = SqliteStore::open(dir.path())
        .expect("open")
        .layout()
        .index_path();
    fs::write(&index, b"broken").expect("corrupt");

    let mut store = SqliteStore::open(dir.path()).expect("open");
    let document = sample_document(8, 8, 99, 1);
    let fresh = store
        .insert(NewCapture::new(&document).titled("after the crash"))
        .expect("insert into a rebuilt store");

    assert_eq!(store.count().expect("count") as usize, ids.len() + 1);
    drop(store);

    let store = SqliteStore::open(dir.path()).expect("reopen");
    assert_eq!(store.count().expect("count") as usize, ids.len() + 1);
    assert_eq!(
        store
            .record(&fresh)
            .expect("read")
            .expect("present")
            .window_title
            .as_deref(),
        Some("after the crash")
    );
}

#[test]
fn garbage_collection_removes_only_pixels_nothing_refers_to() {
    let dir = scratch_dir("collect-garbage");
    let mut store = SqliteStore::open(dir.path()).expect("open");
    let document = sample_document(8, 8, 5, 1);
    let id = store.insert(NewCapture::new(&document)).expect("insert");

    // A blob left behind by a crash between writing pixels and committing a row.
    let bytes = [7u8; 512];
    let stray = scrozz_store::hash::content_hash(&bytes);
    store
        .layout()
        .write_blob(&stray, &bytes)
        .expect("write stray blob");
    assert!(store.layout().blob_path(&stray).expect("path").exists());

    let reclaimed = store.collect_garbage().expect("collect");

    assert_eq!(reclaimed, 512);
    assert!(!store.layout().blob_path(&stray).expect("path").exists());
    assert!(
        store.image(&id).expect("read").is_some(),
        "a referenced blob must never be collected"
    );
}

#[test]
fn an_explicit_recovery_rebuilds_and_says_where_the_old_index_went() {
    // The "my history looks wrong" button. It must never delete anything, so
    // the previous index is moved aside and reported, not removed.
    let (dir, ids) = seeded("explicit-recovery");
    let mut store = SqliteStore::open(dir.path()).expect("open");

    let report = store.recover().expect("recover");

    assert_eq!(report.records_recovered, ids.len());
    assert_eq!(report.records_unreadable, 0);
    let quarantined = report.quarantined.expect("the old index is kept");
    assert!(quarantined.is_file());
    assert_history_intact(&mut store, &ids);
}
