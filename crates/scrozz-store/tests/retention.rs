//! Decision D23: documents are kept forever, images are evicted against a cap.
//!
//! Every test here exists because the obvious alternative — deleting whole
//! captures older than some age — is what most tools ship, and it throws away
//! the edit history along with the pixels.

use scrozz_annotate::{Annotation, Style};
use scrozz_core::LogicalPoint;
use scrozz_store::{
    CaptureId, DocumentState, History as _, ImageState, NewCapture, RetentionPolicy, SearchQuery,
    SqliteStore, Store as _, Timestamp,
    test_support::{ScratchDir, sample_document, scratch_dir},
};

/// Each capture is 16×16 RGBA, so exactly 1 KiB of pixels.
const IMAGE_BYTES: u64 = 16 * 16 * 4;

fn store(label: &str) -> (ScratchDir, SqliteStore) {
    let dir = scratch_dir(label);
    let store = SqliteStore::open(dir.path()).expect("history opens");
    (dir, store)
}

/// Inserts `count` distinct captures a second apart, oldest first.
fn fill(store: &mut SqliteStore, count: u8) -> Vec<CaptureId> {
    let base = 1_700_000_000_000;
    (0..count)
        .map(|i| {
            let document = sample_document(16, 16, i, 1);
            store
                .insert(
                    NewCapture::new(&document)
                        .taken_at(Timestamp(base + i64::from(i) * 1_000))
                        .titled(format!("capture {i}")),
                )
                .expect("insert")
        })
        .collect()
}

#[test]
fn a_history_under_the_cap_is_left_completely_alone() {
    let (_dir, mut store) = store("under-cap");
    let ids = fill(&mut store, 5);

    let report = store
        .evict(&RetentionPolicy {
            max_image_bytes: 10 * IMAGE_BYTES,
        })
        .expect("retention");

    assert!(!report.evicted_anything());
    assert_eq!(report.bytes_remaining, 5 * IMAGE_BYTES);
    for id in &ids {
        assert!(
            store
                .record(id)
                .expect("read")
                .expect("present")
                .image
                .is_present()
        );
    }
}

#[test]
fn eviction_removes_the_oldest_first_and_stops_at_the_cap() {
    let (_dir, mut store) = store("oldest-first");
    let ids = fill(&mut store, 10);

    // Room for six images. Four must go, and they must be the four oldest.
    let report = store
        .evict(&RetentionPolicy {
            max_image_bytes: 6 * IMAGE_BYTES,
        })
        .expect("retention");

    assert_eq!(report.evicted.len(), 4, "{report:?}");
    assert_eq!(report.evicted, ids[..4].to_vec(), "oldest first, in order");
    assert_eq!(report.bytes_reclaimed, 4 * IMAGE_BYTES);
    assert_eq!(report.bytes_remaining, 6 * IMAGE_BYTES);
    assert!(!report.cap_unreachable);

    assert_eq!(
        store.stored_image_bytes().expect("size"),
        6 * IMAGE_BYTES,
        "the cap has to be respected in reality, not just in the report"
    );
    for id in &ids[..4] {
        assert!(
            !store
                .record(id)
                .expect("read")
                .expect("present")
                .image
                .is_present()
        );
    }
    for id in &ids[4..] {
        assert!(
            store
                .record(id)
                .expect("read")
                .expect("present")
                .image
                .is_present()
        );
    }
}

#[test]
fn an_evicted_capture_still_lists_with_its_edits_intact() {
    let (_dir, mut store) = store("edits-survive");
    let base = 1_700_000_000_000;

    let mut document = sample_document(16, 16, 1, 0);
    document.add(
        Annotation::Text {
            at: LogicalPoint::new(4.0, 4.0),
            content: "the important bit".into(),
        },
        Style::stroked(),
    );
    document.add_default(Annotation::Arrow {
        from: LogicalPoint::new(0.0, 0.0),
        to: LogicalPoint::new(8.0, 8.0),
    });

    let old = store
        .insert(
            NewCapture::new(&document)
                .taken_at(Timestamp(base))
                .from_app("Mail")
                .titled("Contract")
                .with_ocr("Signature required"),
        )
        .expect("insert");
    let recent = sample_document(16, 16, 2, 0);
    store
        .insert(NewCapture::new(&recent).taken_at(Timestamp(base + 60_000)))
        .expect("insert");

    store
        .evict(&RetentionPolicy {
            max_image_bytes: IMAGE_BYTES,
        })
        .expect("retention");

    // Still in history.
    assert_eq!(store.count().expect("count"), 2);
    assert!(store.list().expect("list").contains(&old));

    let record = store.record(&old).expect("read").expect("still listed");
    assert_eq!(record.app_name.as_deref(), Some("Mail"));
    assert_eq!(record.window_title.as_deref(), Some("Contract"));
    assert_eq!(record.ocr_text.as_deref(), Some("Signature required"));
    assert_eq!(
        record.annotation_count, 2,
        "D23: the document is kept forever"
    );
    assert_eq!(
        record.frame.size.width, 16.0,
        "the geometry outlives the pixels, so the UI can still lay it out"
    );
    match record.image {
        ImageState::Evicted { at, ref was_hash } => {
            assert!(at.0 > 0);
            assert_eq!(was_hash.len(), 64, "we still know what the pixels were");
        }
        other => panic!("expected an eviction, got {other:?}"),
    }

    // And still searchable.
    let found = store
        .search(&SearchQuery::all().text("signature"))
        .expect("search");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, old);

    // And its edits still load.
    let DocumentState::ImageEvicted(evicted) =
        store.document(&old).expect("read").expect("present")
    else {
        panic!("the pixels were evicted, so a complete document would be a lie");
    };
    assert_eq!(evicted.data.annotations.len(), 2);
    assert!(store.image(&old).expect("read").is_none());
}

#[test]
fn edits_can_still_be_made_to_a_capture_whose_image_is_gone() {
    let (dir, mut store) = store("edit-after-eviction");
    let document = sample_document(16, 16, 1, 1);
    let id = store.insert(NewCapture::new(&document)).expect("insert");

    store
        .evict(&RetentionPolicy { max_image_bytes: 0 })
        .expect("retention");

    let DocumentState::ImageEvicted(evicted) = store.document(&id).expect("read").expect("present")
    else {
        panic!("pixels should be gone");
    };
    let mut data = evicted.data;
    data.annotations.pop();
    store.save_edits(&id, &data).expect("save");

    drop(store);
    let mut store = SqliteStore::open(dir.path()).expect("reopen");
    assert_eq!(
        store
            .record(&id)
            .expect("read")
            .expect("present")
            .annotation_count,
        0,
        "an edit to an evicted capture must persist like any other"
    );
    assert!(matches!(
        store.document(&id).expect("read").expect("present"),
        DocumentState::ImageEvicted(_)
    ));
}

#[test]
fn pinned_captures_are_never_evicted_even_when_that_breaks_the_cap() {
    let (_dir, mut store) = store("pinned-wins");
    let ids = fill(&mut store, 6);

    // Pin the two oldest — exactly the ones eviction would otherwise take.
    store.set_pinned(&ids[0], true).expect("pin");
    store.set_pinned(&ids[1], true).expect("pin");

    let report = store
        .evict(&RetentionPolicy { max_image_bytes: 0 })
        .expect("retention");

    assert_eq!(report.pinned_bytes, 2 * IMAGE_BYTES);
    assert!(
        report.cap_unreachable,
        "the cap could not be met without touching pinned captures, and must say so"
    );
    assert_eq!(report.bytes_remaining, 2 * IMAGE_BYTES);
    assert_eq!(
        store.stored_image_bytes().expect("size"),
        2 * IMAGE_BYTES,
        "D23 is unambiguous: pinned wins over the cap"
    );

    for id in &ids[..2] {
        assert!(
            store
                .record(id)
                .expect("read")
                .expect("present")
                .image
                .is_present(),
            "a pinned capture kept its pixels"
        );
        assert!(store.image(id).expect("read").is_some());
    }
    for id in &ids[2..] {
        assert!(
            !store
                .record(id)
                .expect("read")
                .expect("present")
                .image
                .is_present()
        );
    }
}

#[test]
fn unpinning_makes_a_capture_evictable_again() {
    let (_dir, mut store) = store("unpin");
    let ids = fill(&mut store, 3);
    store.set_pinned(&ids[0], true).expect("pin");

    store
        .evict(&RetentionPolicy { max_image_bytes: 0 })
        .expect("retention");
    assert!(
        store
            .record(&ids[0])
            .expect("read")
            .expect("present")
            .image
            .is_present()
    );

    store.set_pinned(&ids[0], false).expect("unpin");
    let report = store
        .evict(&RetentionPolicy { max_image_bytes: 0 })
        .expect("retention");

    assert_eq!(report.evicted, vec![ids[0].clone()]);
    assert_eq!(store.stored_image_bytes().expect("size"), 0);
}

#[test]
fn eviction_never_removes_a_capture_or_its_document() {
    let (dir, mut store) = store("nothing-deleted");
    let ids = fill(&mut store, 8);

    store
        .evict(&RetentionPolicy { max_image_bytes: 0 })
        .expect("retention");

    assert_eq!(
        store.count().expect("count"),
        8,
        "no capture may be deleted"
    );
    assert_eq!(store.list().expect("list").len(), 8);

    let documents = std::fs::read_dir(store.layout().documents_dir())
        .expect("documents directory")
        .count();
    assert_eq!(
        documents, 8,
        "the durable records are what D23 keeps forever"
    );

    for id in &ids {
        assert_eq!(
            store
                .record(id)
                .expect("read")
                .expect("present")
                .annotation_count,
            1
        );
    }

    let images = store.layout().images_dir();
    let leftover: usize = walk_files(&images);
    assert_eq!(leftover, 0, "the pixels, and only the pixels, are gone");
}

#[test]
fn deduplicated_pixels_survive_until_the_last_capture_referring_to_them_is_evicted() {
    let (_dir, mut store) = store("shared-blob");
    let base = 1_700_000_000_000;
    let document = sample_document(16, 16, 1, 0);

    let older = store
        .insert(NewCapture::new(&document).taken_at(Timestamp(base)))
        .expect("insert");
    let newer = store
        .insert(NewCapture::new(&document).taken_at(Timestamp(base + 1_000)))
        .expect("insert");
    assert_eq!(store.stored_image_bytes().expect("size"), IMAGE_BYTES);

    // Cap of zero, but only the older capture is over the line first.
    let report = store
        .evict(&RetentionPolicy { max_image_bytes: 0 })
        .expect("retention");
    assert_eq!(report.evicted, vec![older.clone(), newer.clone()]);
    assert_eq!(
        report.bytes_reclaimed, IMAGE_BYTES,
        "one shared blob is reclaimed once, not twice"
    );
    assert_eq!(store.stored_image_bytes().expect("size"), 0);
    assert!(store.image(&older).expect("read").is_none());
    assert!(store.image(&newer).expect("read").is_none());
}

#[test]
fn a_shared_blob_is_kept_while_a_pinned_capture_still_needs_it() {
    let (_dir, mut store) = store("shared-pinned");
    let base = 1_700_000_000_000;
    let document = sample_document(16, 16, 2, 0);

    let older = store
        .insert(NewCapture::new(&document).taken_at(Timestamp(base)))
        .expect("insert");
    let newer = store
        .insert(NewCapture::new(&document).taken_at(Timestamp(base + 1_000)))
        .expect("insert");
    store.set_pinned(&newer, true).expect("pin");

    let report = store
        .evict(&RetentionPolicy { max_image_bytes: 0 })
        .expect("retention");

    assert_eq!(report.evicted, vec![older.clone()]);
    assert_eq!(
        report.bytes_reclaimed, 0,
        "the blob is still referenced by a pinned capture"
    );
    assert!(
        store.image(&newer).expect("read").is_some(),
        "evicting one capture must not pull the pixels out from under a pinned one"
    );
    assert!(store.image(&older).expect("read").is_none());
}

#[test]
fn the_default_policy_is_ten_gigabytes_and_a_small_history_never_reaches_it() {
    let (_dir, mut store) = store("default-policy");
    fill(&mut store, 4);

    assert_eq!(
        RetentionPolicy::default().max_image_bytes,
        10 * 1024 * 1024 * 1024
    );
    store
        .enforce_retention(&RetentionPolicy::default())
        .expect("retention");
    assert_eq!(store.stored_image_bytes().expect("size"), 4 * IMAGE_BYTES);
}

#[test]
fn eviction_state_survives_a_reopen() {
    let (dir, mut store) = store("eviction-persists");
    let ids = fill(&mut store, 4);
    store
        .evict(&RetentionPolicy {
            max_image_bytes: 2 * IMAGE_BYTES,
        })
        .expect("retention");
    drop(store);

    let store = SqliteStore::open(dir.path()).expect("reopen");
    assert_eq!(store.count().expect("count"), 4);
    assert_eq!(store.stored_image_bytes().expect("size"), 2 * IMAGE_BYTES);
    assert!(
        !store
            .record(&ids[0])
            .expect("read")
            .expect("present")
            .image
            .is_present()
    );
    assert!(
        store
            .record(&ids[3])
            .expect("read")
            .expect("present")
            .image
            .is_present()
    );
}

#[test]
fn images_only_is_the_opt_in_and_history_shows_evicted_captures_by_default() {
    let (_dir, mut store) = store("images-only");
    fill(&mut store, 4);
    store
        .evict(&RetentionPolicy {
            max_image_bytes: 2 * IMAGE_BYTES,
        })
        .expect("retention");

    let everything = store.search(&SearchQuery::all()).expect("search");
    assert_eq!(
        everything.len(),
        4,
        "the default view must not hide evicted captures"
    );

    let with_pixels = store
        .search(&SearchQuery::all().images_only())
        .expect("search");
    assert_eq!(with_pixels.len(), 2);
}

#[test]
fn repeated_enforcement_is_idempotent() {
    let (_dir, mut store) = store("idempotent");
    fill(&mut store, 5);
    let policy = RetentionPolicy {
        max_image_bytes: 2 * IMAGE_BYTES,
    };

    let first = store.evict(&policy).expect("retention");
    let second = store.evict(&policy).expect("retention");

    assert_eq!(first.evicted.len(), 3);
    assert!(
        !second.evicted_anything(),
        "a second pass at the same cap must find nothing to do"
    );
    assert_eq!(second.bytes_remaining, 2 * IMAGE_BYTES);
}

fn walk_files(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            if entry.path().is_dir() {
                walk_files(&entry.path())
            } else {
                1
            }
        })
        .sum()
}
