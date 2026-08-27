//! Concurrent history mutations must not recreate a deleted durable record.

use std::sync::{Arc, Barrier};

use scrozz_annotate::{Annotation, Style};
use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};
use scrozz_store::{
    History as _, NewCapture, SqliteStore,
    test_support::{sample_document, scratch_dir},
};

#[test]
fn concurrent_save_and_delete_never_resurrect_a_capture() {
    let dir = scratch_dir("save-delete-race");

    for iteration in 0..24_u8 {
        let mut setup = SqliteStore::open(dir.path()).expect("setup store opens");
        let mut document = sample_document(16, 16, iteration, 0);
        document
            .add(
                Annotation::Rectangle(LogicalRect::new(
                    LogicalPoint::new(2.0, 2.0),
                    LogicalSize::new(8.0, 8.0),
                )),
                Style::stroked(),
            )
            .expect("fixture annotation id is available");
        let id = setup
            .insert(NewCapture::new(&document))
            .expect("fixture inserts");
        let data = document.data();
        drop(setup);

        let mut saver = SqliteStore::open(dir.path()).expect("save connection opens");
        let mut deleter = SqliteStore::open(dir.path()).expect("delete connection opens");
        let barrier = Arc::new(Barrier::new(3));
        let save_barrier = Arc::clone(&barrier);
        let delete_barrier = Arc::clone(&barrier);
        let save_id = id.clone();
        let delete_id = id.clone();

        let save = std::thread::spawn(move || {
            save_barrier.wait();
            saver.save_edits(&save_id, &data)
        });
        let delete = std::thread::spawn(move || {
            delete_barrier.wait();
            deleter.delete(&delete_id)
        });
        barrier.wait();

        let save_result = save.join().expect("save thread does not panic");
        assert!(
            delete
                .join()
                .expect("delete thread does not panic")
                .expect("delete succeeds"),
            "the capture existed before the race"
        );
        if let Err(error) = save_result {
            assert!(
                error.to_string().contains("no capture"),
                "the only valid save loss is deletion winning the writer lock: {error}"
            );
        }

        let store = SqliteStore::open(dir.path()).expect("verification store opens");
        assert!(
            store.record(&id).expect("index remains readable").is_none(),
            "iteration {iteration} resurrected the deleted index row"
        );
        assert!(
            store
                .layout()
                .read_record(&id)
                .expect("durable records remain readable")
                .is_none(),
            "iteration {iteration} resurrected the deleted sidecar"
        );
    }
}
