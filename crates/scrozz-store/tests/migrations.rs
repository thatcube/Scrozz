//! The schema has to climb forward across app upgrades without losing history.
//!
//! A user's capture history is not reproducible: if a migration drops it, it is
//! gone. These tests exercise the ladder directly rather than through the store,
//! because the failure they guard against only shows up on a database that was
//! written by an older build.

use rusqlite::Connection;
use scrozz_store::{
    SqliteStore,
    schema::{MIGRATIONS, Migration, latest_version, migrate, schema_version},
    test_support::scratch_dir,
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
    let mut conn = Connection::open_in_memory().expect("memory database");
    migrate(&mut conn, &MIGRATIONS[..1]).expect("create v1 database");
    conn.execute(
        "INSERT INTO captures (
             id, created_at, stored_at, pinned, app_name, window_title, provenance,
             target_json, frame_json, image_bytes, annotation_count
         ) VALUES (
             'old', 1700000000000, 1700000000001, 1, 'Xcode', 'window 0',
             'window', '{}', '{}', 1024, 1
         )",
        [],
    )
    .expect("insert old row");

    assert_eq!(migrate(&mut conn, MIGRATIONS).expect("upgrade"), 3);
    let row: (
        i64,
        String,
        String,
        i64,
        Option<String>,
        Option<i64>,
        String,
    ) = conn
        .query_row(
            "SELECT pinned, app_name, window_title, annotation_count,
                    app_identifier, window_shadow, media_kind
             FROM captures WHERE id = 'old'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .expect("old row survives");
    assert_eq!(
        row,
        (
            1,
            "Xcode".into(),
            "window 0".into(),
            1,
            None,
            None,
            "screenshot".into(),
        )
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

    for table in ["captures", "blobs", "store_meta"] {
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
