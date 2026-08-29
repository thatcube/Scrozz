//! The schema has to climb forward across app upgrades without losing history.
//!
//! A user's capture history is not reproducible: if a migration drops it, it is
//! gone. These tests exercise the ladder directly rather than through the store,
//! because the failure they guard against only shows up on a database that was
//! written by an older build.

use rusqlite::Connection;
use scrozz_store::{
    History as _, NewCapture, SqliteStore, Store as _, StoreLayout, Timestamp,
    schema::{MIGRATIONS, Migration, latest_version, migrate, schema_version},
    test_support::{sample_document, sample_record, scratch_dir},
};

#[test]
fn a_fresh_database_climbs_to_the_latest_version() {
    let mut conn = Connection::open_in_memory().expect("memory database");
    assert_eq!(schema_version(&conn).expect("version"), 0);

    let reached = migrate(&mut conn, MIGRATIONS).expect("migrate");

    assert_eq!(reached, latest_version(MIGRATIONS));
    assert_eq!(schema_version(&conn).expect("version"), reached);
    assert!(reached >= 1, "there is at least one rung");
}

#[test]
fn migrating_an_already_current_database_is_a_no_op() {
    let mut conn = Connection::open_in_memory().expect("memory database");
    let first = migrate(&mut conn, MIGRATIONS).expect("migrate");
    let second = migrate(&mut conn, MIGRATIONS).expect("migrate again");
    let third = migrate(&mut conn, MIGRATIONS).expect("and again");

    assert_eq!(first, second);
    assert_eq!(second, third);
}

#[test]
fn the_ladder_has_no_gaps_duplicates_or_backward_steps() {
    for (expected, migration) in (1..).zip(MIGRATIONS.iter()) {
        assert_eq!(
            migration.version, expected,
            "migrations must be a dense ascending ladder starting at 1"
        );
        assert!(
            !migration.sql.trim().is_empty(),
            "migration {} has no statements",
            migration.version
        );
    }
    assert_eq!(latest_version(MIGRATIONS), MIGRATIONS.len() as u32);
}

#[test]
fn an_old_database_climbs_forward_with_every_row_intact() {
    // Stand up a database at rung 1 only — the shape an older build left behind —
    // then put real rows in it before letting the current build migrate it.
    let dir = scratch_dir("old-database");
    let mut store = SqliteStore::open(dir.path()).expect("open");
    let base = 1_700_000_000_000;
    let ids: Vec<_> = (0..4)
        .map(|i| {
            let document = sample_document(8, 8, i, 1);
            store
                .insert(
                    NewCapture::new(&document)
                        .taken_at(Timestamp(base + i64::from(i)))
                        .from_app("Xcode")
                        .titled(format!("window {i}")),
                )
                .expect("insert")
        })
        .collect();
    store.set_pinned(&ids[0], true).expect("pin");
    let current = store.schema_version().expect("version");
    let index = store.layout().index_path();
    drop(store);

    // Wind the recorded version backwards. The tables are already current, so a
    // correct migration ladder must be safe to re-apply; an incorrect one that
    // does `CREATE TABLE` without `IF NOT EXISTS` fails loudly right here.
    let conn = Connection::open(&index).expect("raw open");
    conn.pragma_update(None, "user_version", 0).expect("rewind");
    drop(conn);

    let store = SqliteStore::open(dir.path()).expect("reopen migrates");

    assert_eq!(store.schema_version().expect("version"), current);
    assert_eq!(store.count().expect("count"), 4, "no history was lost");
    for (i, id) in ids.iter().enumerate() {
        let record = store.record(id).expect("read").expect("present");
        assert_eq!(
            record.window_title.as_deref(),
            Some(&*format!("window {i}"))
        );
        assert_eq!(record.app_name.as_deref(), Some("Xcode"));
        assert_eq!(record.annotation_count, 1);
        assert!(record.image.is_present());
    }
    assert!(
        store
            .record(&ids[0])
            .expect("read")
            .expect("present")
            .pinned,
        "a pin set by the old build survives the upgrade"
    );
}

#[test]
fn a_database_from_a_newer_build_is_refused_rather_than_mangled() {
    let dir = scratch_dir("future-database");
    let store = SqliteStore::open(dir.path()).expect("open");
    let current = store.schema_version().expect("version");
    let index = store.layout().index_path();
    drop(store);

    let conn = Connection::open(&index).expect("raw open");
    conn.pragma_update(None, "user_version", current + 7)
        .expect("bump");
    drop(conn);

    let error = SqliteStore::open(dir.path()).expect_err("a future schema must be refused");
    let message = error.to_string().to_lowercase();
    assert!(
        message.contains("newer") || message.contains("future"),
        "the error should explain the version mismatch, got {error}"
    );
}

#[test]
fn a_v1_database_and_record_load_after_migration() {
    let dir = scratch_dir("v1-load");
    let layout = StoreLayout::new(dir.path());
    layout.ensure_dirs().expect("dirs");
    let mut record = sample_record("Safari", 1_700_000_000_000);
    record.window_title = Some("legacy record".into());
    let id = scrozz_store::CaptureId(record.id.clone());
    layout.write_record(&record).expect("record");

    let index = layout.index_path();
    let mut conn = Connection::open(&index).expect("open");
    migrate(&mut conn, &MIGRATIONS[..1]).expect("v1 schema");
    conn.execute(
        "INSERT INTO captures (
             id, created_at, stored_at, pinned, app_name, window_title, provenance,
             target_json, frame_json, image_hash, image_bytes, image_evicted_at, ocr_text,
             annotation_count, search_fold, app_fold, title_fold, ocr_fold
         ) VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7, ?8, NULL, 0, NULL, NULL, 0, ?9, ?10, ?11, NULL)",
        rusqlite::params![
            id.0,
            record.created_at,
            record.stored_at,
            record.app_name,
            record.window_title,
            record.provenance.as_token(),
            serde_json::to_string(&record.target).expect("target"),
            serde_json::to_string(&record.frame).expect("frame"),
            record.search_text(),
            record.app_name.as_ref().map(|t| t.to_lowercase()),
            record.window_title.as_ref().map(|t| t.to_lowercase()),
        ],
    )
    .expect("insert v1 row");
    drop(conn);

    let store = SqliteStore::open(dir.path()).expect("migrate and open");
    assert_eq!(
        store.schema_version().expect("version"),
        latest_version(MIGRATIONS)
    );
    let loaded = store.record(&id).expect("read").expect("present");
    assert_eq!(loaded.window_title.as_deref(), Some("legacy record"));
    assert!(loaded.sharing.is_none(), "v1 rows had no share metadata");
}

#[test]
fn a_failing_migration_leaves_the_recorded_version_untouched() {
    let mut conn = Connection::open_in_memory().expect("memory database");
    migrate(&mut conn, MIGRATIONS).expect("migrate");
    let before = schema_version(&conn).expect("version");

    let mut broken: Vec<Migration> = MIGRATIONS.to_vec();
    broken.push(Migration {
        version: before + 1,
        name: "deliberately broken",
        sql: "CREATE TABLE ok_so_far (x INTEGER); \
              CREATE TABLE captures (this_will_collide INTEGER);",
    });

    let error = migrate(&mut conn, &broken).expect_err("the rung must fail");
    assert!(!error.to_string().is_empty());

    assert_eq!(
        schema_version(&conn).expect("version"),
        before,
        "a failed rung must not advance user_version"
    );
    let leaked: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'ok_so_far'",
            [],
            |row| row.get(0),
        )
        .expect("query");
    assert_eq!(
        leaked, 0,
        "the whole rung is one transaction, so a half-applied migration is impossible"
    );
}

#[test]
fn the_tables_the_store_depends_on_actually_exist_after_migration() {
    let mut conn = Connection::open_in_memory().expect("memory database");
    migrate(&mut conn, MIGRATIONS).expect("migrate");

    for table in ["captures", "blobs", "store_meta", "capture_shares"] {
        let found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(found, 1, "missing table {table}");
    }

    let indexes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name LIKE 'captures_%'",
            [],
            |row| row.get(0),
        )
        .expect("query");
    assert!(
        indexes >= 3,
        "history paging and retention both depend on indexes, found {indexes}"
    );
}
