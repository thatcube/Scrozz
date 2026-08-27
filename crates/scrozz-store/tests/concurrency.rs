//! Decision D11 puts the CLI and the GUI on the same history at the same time.
//!
//! That makes concurrent access a requirement rather than a hypothetical, so
//! these tests run genuinely parallel connections against one store and insist
//! that no caller ever sees `SQLITE_BUSY` leak out as an error.

use std::sync::{Arc, Barrier};
use std::thread;

use scrozz_store::{
    History as _, NewCapture, RetentionPolicy, SearchQuery, SqliteStore, Store as _, Timestamp,
    test_support::{sample_document, scratch_dir},
};

#[test]
fn two_processes_can_write_to_one_history_at_the_same_time() {
    let dir = scratch_dir("parallel-writers");
    // Create the store once so both threads open an existing, migrated file —
    // which is exactly what a second app launch does.
    drop(SqliteStore::open(dir.path()).expect("create"));

    const WRITERS: u8 = 4;
    const EACH: u8 = 12;
    let gate = Arc::new(Barrier::new(WRITERS as usize));
    let root = dir.path().to_path_buf();

    let handles: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let gate = Arc::clone(&gate);
            let root = root.clone();
            thread::spawn(move || {
                let mut store = SqliteStore::open(&root).expect("open");
                gate.wait();
                for n in 0..EACH {
                    // Distinct pixels per capture, so nothing is deduplicated
                    // away and the byte accounting stays checkable.
                    let document = sample_document(8, 8, writer * EACH + n, 1);
                    store
                        .insert(
                            NewCapture::new(&document)
                                .from_app(format!("writer {writer}"))
                                .titled(format!("{writer}-{n}"))
                                .taken_at(Timestamp(1_700_000_000_000 + i64::from(n))),
                        )
                        .expect("a busy database must be waited out, not surfaced");
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("writer thread");
    }

    let store = SqliteStore::open(dir.path()).expect("reopen");
    let expected = usize::from(WRITERS) * usize::from(EACH);
    assert_eq!(
        store.count().expect("count") as usize,
        expected,
        "every insert from every writer must be present"
    );
    for writer in 0..WRITERS {
        let mine = store
            .search(&SearchQuery::all().app(format!("writer {writer}")))
            .expect("search");
        assert_eq!(mine.len(), usize::from(EACH), "writer {writer} lost rows");
    }
    assert_eq!(
        store
            .layout()
            .documents_dir()
            .read_dir()
            .expect("dir")
            .count(),
        expected,
        "one durable record per capture, no matter who wrote it"
    );
}

#[test]
fn a_reader_sees_a_consistent_history_while_a_writer_is_busy() {
    let dir = scratch_dir("reader-during-write");
    let mut seed = SqliteStore::open(dir.path()).expect("open");
    let document = sample_document(8, 8, 200, 2);
    let known = seed
        .insert(NewCapture::new(&document).titled("already here"))
        .expect("insert");
    drop(seed);

    let root = dir.path().to_path_buf();
    let gate = Arc::new(Barrier::new(2));

    let writer_gate = Arc::clone(&gate);
    let writer_root = root.clone();
    let writer = thread::spawn(move || {
        let mut store = SqliteStore::open(&writer_root).expect("open");
        writer_gate.wait();
        for n in 0..40u8 {
            let document = sample_document(8, 8, n, 1);
            store
                .insert(NewCapture::new(&document).titled(format!("new {n}")))
                .expect("insert");
        }
    });

    let reader_gate = Arc::clone(&gate);
    let reader = thread::spawn(move || {
        let store = SqliteStore::open(&root).expect("open");
        reader_gate.wait();
        for _ in 0..80 {
            // WAL means a reader is never blocked by the writer; it just sees a
            // snapshot. What must never happen is an error or a missing row.
            let record = store
                .record(&known)
                .expect("read must not fail under write load")
                .expect("the pre-existing capture cannot vanish");
            assert_eq!(record.window_title.as_deref(), Some("already here"));
            assert!(store.count().expect("count") >= 1);
            let listed = store.list().expect("list");
            assert!(listed.contains(&known));
        }
    });

    writer.join().expect("writer");
    reader.join().expect("reader");

    let store = SqliteStore::open(dir.path()).expect("reopen");
    assert_eq!(store.count().expect("count"), 41_u64);
}

#[test]
fn retention_running_in_one_process_does_not_break_inserts_in_another() {
    // The realistic version of this: the GUI is enforcing the size cap on a
    // timer while the CLI takes a screenshot.
    let dir = scratch_dir("retention-during-insert");
    let mut seed = SqliteStore::open(dir.path()).expect("open");
    for n in 0..20u8 {
        let document = sample_document(8, 8, 100 + n, 0);
        seed.insert(NewCapture::new(&document)).expect("insert");
    }
    drop(seed);

    let root = dir.path().to_path_buf();
    let gate = Arc::new(Barrier::new(2));

    let evictor_root = root.clone();
    let evictor_gate = Arc::clone(&gate);
    let evictor = thread::spawn(move || {
        let mut store = SqliteStore::open(&evictor_root).expect("open");
        evictor_gate.wait();
        for _ in 0..10 {
            store
                .evict(&RetentionPolicy {
                    max_image_bytes: 4 * 8 * 8 * 4,
                })
                .expect("retention must tolerate a concurrent writer");
        }
    });

    let inserter_gate = Arc::clone(&gate);
    let inserter = thread::spawn(move || {
        let mut store = SqliteStore::open(&root).expect("open");
        inserter_gate.wait();
        let mut ids = Vec::new();
        for n in 0..20u8 {
            let document = sample_document(8, 8, n, 1);
            ids.push(
                store
                    .insert(NewCapture::new(&document).titled(format!("fresh {n}")))
                    .expect("an insert must never lose to eviction"),
            );
        }
        ids
    });

    evictor.join().expect("evictor");
    let fresh = inserter.join().expect("inserter");

    let mut store = SqliteStore::open(dir.path()).expect("reopen");
    assert_eq!(store.count().expect("count"), 40_u64, "nothing was deleted");
    for id in &fresh {
        let record = store
            .record(id)
            .expect("read")
            .expect("a capture inserted during eviction must still exist");
        assert_eq!(record.annotation_count, 1, "its edits are intact");
    }
    // Whatever the interleaving, an image that is still claimed as present must
    // actually be readable. That is the invariant a race would break.
    for id in store.list().expect("list") {
        let record = store.record(&id).expect("read").expect("present");
        if record.image.is_present() {
            assert!(
                store.image(&id).expect("read").is_some(),
                "capture {id:?} claims pixels that are not on disk"
            );
        }
    }
}

#[test]
fn concurrent_pin_and_read_never_produce_a_torn_view() {
    let dir = scratch_dir("pin-race");
    let mut seed = SqliteStore::open(dir.path()).expect("open");
    let document = sample_document(8, 8, 7, 1);
    let id = seed.insert(NewCapture::new(&document)).expect("insert");
    drop(seed);

    let root = dir.path().to_path_buf();
    let gate = Arc::new(Barrier::new(2));

    let toggler_root = root.clone();
    let toggler_id = id.clone();
    let toggler_gate = Arc::clone(&gate);
    let toggler = thread::spawn(move || {
        let mut store = SqliteStore::open(&toggler_root).expect("open");
        toggler_gate.wait();
        for n in 0..60 {
            store
                .set_pinned(&toggler_id, n % 2 == 0)
                .expect("pin toggle");
        }
    });

    let reader_gate = Arc::clone(&gate);
    let reader = thread::spawn(move || {
        let store = SqliteStore::open(&root).expect("open");
        reader_gate.wait();
        for _ in 0..60 {
            let record = store.record(&id).expect("read").expect("present");
            // Whichever side of the toggle we land on, the rest of the row must
            // be coherent — a half-written update would show up here.
            assert!(record.image.is_present());
            assert_eq!(record.annotation_count, 1);
            assert_eq!(
                record
                    .frame
                    .expect("image capture should have frame metadata")
                    .size
                    .width,
                8.0
            );
        }
    });

    toggler.join().expect("toggler");
    reader.join().expect("reader");
}

#[test]
fn many_readers_do_not_block_each_other() {
    let dir = scratch_dir("parallel-readers");
    let mut seed = SqliteStore::open(dir.path()).expect("open");
    for n in 0..30u8 {
        let document = sample_document(8, 8, n, 1);
        seed.insert(
            NewCapture::new(&document)
                .from_app("Terminal")
                .with_ocr(format!("line {n}")),
        )
        .expect("insert");
    }
    drop(seed);

    let root = dir.path().to_path_buf();
    let gate = Arc::new(Barrier::new(6));
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let root = root.clone();
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let store = SqliteStore::open(&root).expect("open");
                gate.wait();
                for _ in 0..20 {
                    assert_eq!(store.count().expect("count"), 30_u64);
                    let hits = store
                        .search(&SearchQuery::all().app("Terminal"))
                        .expect("search");
                    assert_eq!(hits.len(), 30);
                    let page = store.page(scrozz_store::Page::new(5, 0)).expect("page");
                    assert_eq!(page.len(), 5);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("reader thread");
    }
}
