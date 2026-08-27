//! The index schema and its forward migration path.
//!
//! # Why migrations are a ladder rather than a `CREATE TABLE`
//!
//! This database has to survive every future version of the app. A store that
//! only knows how to create today's schema has exactly two options when it
//! meets yesterday's file: refuse to start, or delete it. Both lose a user's
//! capture history, which decision D23 spends its whole argument saying must
//! not happen.
//!
//! So the schema is a numbered list. `PRAGMA user_version` records how far a
//! file has climbed, each rung runs inside its own transaction, and a rung that
//! fails leaves the file exactly where it was rather than half-migrated.
//!
//! A file from the *future* — written by a newer build — is refused outright.
//! Opening it read-write and guessing would corrupt data that a downgrade is
//! supposed to be able to hand back.

use rusqlite::{Connection, TransactionBehavior};
use scrozz_core::{Error, Result};

/// One rung of the ladder.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// The `user_version` this migration brings a file to. Strictly ascending.
    pub version: u32,
    /// What it does, for logs and for the failure message.
    pub name: &'static str,
    /// The statements to run.
    pub sql: &'static str,
}

/// The schema, in order.
///
/// Every statement is `IF NOT EXISTS`, so re-running a rung against a file that
/// already has it is harmless. That matters more than it looks: the one way a
/// `user_version` and a schema drift apart is an interrupted upgrade on a
/// filesystem that lied about `fsync`, and idempotent rungs make that
/// recoverable instead of fatal.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial history index",
        sql: r"
        -- One row per capture. Rows are NEVER deleted by retention: decision
        -- D23 evicts `image_hash`, not the capture.
        CREATE TABLE IF NOT EXISTS captures (
            id                TEXT    NOT NULL PRIMARY KEY,
            created_at        INTEGER NOT NULL,
            stored_at         INTEGER NOT NULL,
            pinned            INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
            app_name          TEXT,
            app_identifier    TEXT,
            window_title      TEXT,
            window_shadow     INTEGER
                CHECK (window_shadow IS NULL OR window_shadow IN (0, 1)),
            provenance        TEXT    NOT NULL,
            target_json       TEXT    NOT NULL,
            frame_json        TEXT    NOT NULL,
            -- NULL once the pixels are gone. The document stays either way.
            image_hash        TEXT,
            image_bytes       INTEGER NOT NULL DEFAULT 0,
            image_evicted_at  INTEGER,
            ocr_text          TEXT,
            annotation_count  INTEGER NOT NULL DEFAULT 0,
            -- Case-folded in Rust, because SQL's lower() folds ASCII only.
            search_fold       TEXT    NOT NULL DEFAULT '',
            app_fold          TEXT,
            title_fold        TEXT,
            ocr_fold          TEXT
        ) STRICT;

        -- History is read newest-first far more than any other way.
        CREATE INDEX IF NOT EXISTS captures_by_recency
            ON captures (created_at DESC, id DESC);

        -- Retention's exact question: the oldest unpinned capture that still
        -- has pixels. A partial index keeps it proportional to the answer.
        -- `image_hash` is kept after eviction so a capture remembers what its
        -- pixels were; `image_evicted_at` is what says they are gone.
        CREATE INDEX IF NOT EXISTS captures_evictable
            ON captures (created_at ASC, id ASC)
            WHERE image_hash IS NOT NULL AND image_evicted_at IS NULL AND pinned = 0;

        CREATE INDEX IF NOT EXISTS captures_by_app ON captures (app_fold);
        CREATE INDEX IF NOT EXISTS captures_pinned ON captures (pinned) WHERE pinned = 1;
        CREATE INDEX IF NOT EXISTS captures_by_hash ON captures (image_hash);

        -- One row per distinct blob on disk. Two captures of an unchanged
        -- window share one, so the cap measures disk rather than duplicates.
        CREATE TABLE IF NOT EXISTS blobs (
            hash       TEXT    NOT NULL PRIMARY KEY,
            byte_len   INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE IF NOT EXISTS store_meta (
            key   TEXT NOT NULL PRIMARY KEY,
            value TEXT NOT NULL
        ) STRICT;
    ",
    },
    Migration {
        version: 2,
        name: "recording history",
        sql: r"
        ALTER TABLE captures
            ADD COLUMN media_kind TEXT NOT NULL DEFAULT 'screenshot'
            CHECK (media_kind IN ('screenshot', 'video', 'gif'));
        ALTER TABLE captures ADD COLUMN video_json TEXT;
        CREATE INDEX IF NOT EXISTS captures_by_kind_recency
            ON captures (media_kind, created_at DESC, id DESC);
    ",
    },
    Migration {
        version: 3,
        name: "repair recording history columns",
        sql: r"
        ALTER TABLE captures
            ADD COLUMN media_kind TEXT NOT NULL DEFAULT 'screenshot'
            CHECK (media_kind IN ('screenshot', 'video', 'gif'));
        ALTER TABLE captures ADD COLUMN video_json TEXT;
        CREATE INDEX IF NOT EXISTS captures_by_kind_recency
            ON captures (media_kind, created_at DESC, id DESC);
    ",
    },
    Migration {
        version: 4,
        name: "normalize recording media kinds",
        sql: r"
        CREATE TABLE captures_recording_v4 (
            id                TEXT    NOT NULL PRIMARY KEY,
            created_at        INTEGER NOT NULL,
            stored_at         INTEGER NOT NULL,
            pinned            INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
            app_name          TEXT,
            app_identifier    TEXT,
            window_title      TEXT,
            window_shadow     INTEGER
                CHECK (window_shadow IS NULL OR window_shadow IN (0, 1)),
            provenance        TEXT    NOT NULL,
            target_json       TEXT    NOT NULL,
            frame_json        TEXT    NOT NULL,
            image_hash        TEXT,
            image_bytes       INTEGER NOT NULL DEFAULT 0,
            image_evicted_at  INTEGER,
            ocr_text          TEXT,
            annotation_count  INTEGER NOT NULL DEFAULT 0,
            search_fold       TEXT    NOT NULL DEFAULT '',
            app_fold          TEXT,
            title_fold        TEXT,
            ocr_fold          TEXT,
            media_kind        TEXT    NOT NULL DEFAULT 'screenshot'
                CHECK (media_kind IN ('screenshot', 'video', 'gif')),
            video_json        TEXT
        ) STRICT;

        INSERT INTO captures_recording_v4 (
            id, created_at, stored_at, pinned, app_name, app_identifier,
            window_title, window_shadow, provenance, target_json, frame_json,
            image_hash, image_bytes, image_evicted_at, ocr_text,
            annotation_count, search_fold, app_fold, title_fold, ocr_fold,
            media_kind, video_json
        )
        SELECT
            id, created_at, stored_at, pinned, app_name, app_identifier,
            window_title, window_shadow, provenance, target_json, frame_json,
            image_hash, image_bytes, image_evicted_at, ocr_text,
            annotation_count, search_fold, app_fold, title_fold, ocr_fold,
            CASE media_kind WHEN 'image' THEN 'screenshot' ELSE media_kind END,
            video_json
        FROM captures;

        DROP TABLE captures;
        ALTER TABLE captures_recording_v4 RENAME TO captures;

        CREATE INDEX captures_by_recency
            ON captures (created_at DESC, id DESC);
        CREATE INDEX captures_evictable
            ON captures (created_at ASC, id ASC)
            WHERE image_hash IS NOT NULL AND image_evicted_at IS NULL AND pinned = 0;
        CREATE INDEX captures_by_app ON captures (app_fold);
        CREATE INDEX captures_pinned ON captures (pinned) WHERE pinned = 1;
        CREATE INDEX captures_by_hash ON captures (image_hash);
        CREATE INDEX IF NOT EXISTS captures_by_kind_recency
            ON captures (media_kind, created_at DESC, id DESC);
    ",
    },
    Migration {
        version: 5,
        name: "preserve capture source metadata",
        sql: "SELECT 1;",
    },
    Migration {
        version: 6,
        name: "backfill capture source metadata",
        sql: r"
        INSERT INTO store_meta (key, value)
        VALUES ('source_metadata_backfill_pending', '1')
        ON CONFLICT (key) DO UPDATE SET value = excluded.value;
    ",
    },
];

/// The version a freshly-migrated file ends up at.
#[must_use]
pub fn latest_version(migrations: &[Migration]) -> u32 {
    migrations.last().map_or(0, |m| m.version)
}

/// Reads `PRAGMA user_version`.
///
/// # Errors
///
/// Returns [`Error::Storage`] if the pragma cannot be read, which in practice
/// means the file is not a database.
pub fn schema_version(conn: &Connection) -> Result<u32> {
    conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|e| Error::Storage(format!("cannot read schema version: {e}")))
        .map(|v| u32::try_from(v).unwrap_or(0))
}

/// Climbs `conn` to the top of `migrations`, returning the version reached.
///
/// Already-current files do no work and take no write lock, which matters when
/// the CLI and the GUI both start at once.
///
/// # Errors
///
/// Returns [`Error::Storage`] if the file is newer than this build understands,
/// if `migrations` is not strictly ascending, or if a rung fails. A failed rung
/// is rolled back.
pub fn migrate(conn: &mut Connection, migrations: &[Migration]) -> Result<u32> {
    ensure_ascending(migrations)?;

    let target = latest_version(migrations);
    let current = schema_version(conn)?;

    if current > target {
        return Err(Error::Storage(format!(
            "history index is at schema version {current}, but this build of Scrozz only \
             understands {target}. Refusing to open it rather than risk damaging newer data — \
             update Scrozz, or move the index aside."
        )));
    }
    if current == target {
        return Ok(current);
    }

    for migration in migrations.iter().filter(|m| m.version > current) {
        // IMMEDIATE takes the write lock up front. A deferred transaction that
        // discovers it needs to write partway through cannot be retried by the
        // busy handler, which is exactly how two processes upgrading at once
        // turn into a hard SQLITE_BUSY instead of a short wait.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::Storage(format!("cannot begin migration: {e}")))?;

        // Another process may have won the race to this rung while we waited.
        let now = tx
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .map_err(|e| Error::Storage(format!("cannot read schema version: {e}")))?;
        if u32::try_from(now).unwrap_or(0) >= migration.version {
            tx.rollback()
                .map_err(|e| Error::Storage(format!("cannot end migration: {e}")))?;
            continue;
        }

        apply_migration(&tx, migration).map_err(|e| {
            Error::Storage(format!(
                "migration {} ({}) failed: {e}",
                migration.version, migration.name
            ))
        })?;
        tx.pragma_update(None, "user_version", i64::from(migration.version))
            .map_err(|e| Error::Storage(format!("cannot record schema version: {e}")))?;
        tx.commit()
            .map_err(|e| Error::Storage(format!("cannot commit migration: {e}")))?;

        tracing::info!(
            version = migration.version,
            name = migration.name,
            "migrated history index"
        );
    }

    schema_version(conn)
}

fn apply_migration(tx: &rusqlite::Transaction<'_>, migration: &Migration) -> rusqlite::Result<()> {
    match migration.name {
        "recording history" => {
            ensure_recording_columns(tx)?;
            tx.execute_batch(
                "CREATE INDEX IF NOT EXISTS captures_by_kind_recency
                 ON captures (media_kind, created_at DESC, id DESC);",
            )
        }
        "repair recording history columns" => {
            ensure_recording_columns(tx)?;
            tx.execute_batch(
                "CREATE INDEX IF NOT EXISTS captures_by_kind_recency
                 ON captures (media_kind, created_at DESC, id DESC);",
            )
        }
        "normalize recording media kinds" => {
            ensure_recording_columns(tx)?;
            ensure_source_metadata_columns(tx)?;
            tx.execute_batch(migration.sql)
        }
        "preserve capture source metadata" => ensure_source_metadata_columns(tx),
        _ => tx.execute_batch(migration.sql),
    }
}

fn ensure_recording_columns(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    // SQLite has no portable `ADD COLUMN IF NOT EXISTS`. Inspecting first keeps
    // the rung safe when a filesystem persisted DDL but lost `user_version`.
    if !column_exists(tx, "captures", "media_kind")? {
        tx.execute_batch(
            "ALTER TABLE captures
             ADD COLUMN media_kind TEXT NOT NULL DEFAULT 'screenshot'
             CHECK (media_kind IN ('screenshot', 'video', 'gif'));",
        )?;
    }
    if !column_exists(tx, "captures", "video_json")? {
        tx.execute_batch("ALTER TABLE captures ADD COLUMN video_json TEXT;")?;
    }
    Ok(())
}

fn ensure_source_metadata_columns(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    if !column_exists(tx, "captures", "app_identifier")? {
        tx.execute_batch("ALTER TABLE captures ADD COLUMN app_identifier TEXT;")?;
    }
    if !column_exists(tx, "captures", "window_shadow")? {
        tx.execute_batch(
            "ALTER TABLE captures ADD COLUMN window_shadow INTEGER
             CHECK (window_shadow IS NULL OR window_shadow IN (0, 1));",
        )?;
    }
    Ok(())
}

fn column_exists(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
) -> rusqlite::Result<bool> {
    let mut statement = tx.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_ascending(migrations: &[Migration]) -> Result<()> {
    let mut previous = 0;
    for migration in migrations {
        if migration.version <= previous {
            return Err(Error::Storage(format!(
                "migrations must ascend: {} follows {previous}",
                migration.version
            )));
        }
        previous = migration.version;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Connection {
        Connection::open_in_memory().expect("in-memory database")
    }

    const LADDER: &[Migration] = &[
        Migration {
            version: 1,
            name: "one",
            sql: "CREATE TABLE IF NOT EXISTS a (x INTEGER) STRICT;",
        },
        Migration {
            version: 2,
            name: "two",
            sql: "CREATE TABLE IF NOT EXISTS b (y TEXT) STRICT;",
        },
        Migration {
            version: 3,
            name: "three",
            sql: "ALTER TABLE a ADD COLUMN z TEXT;",
        },
    ];

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("prepare");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect")
    }

    #[test]
    fn a_fresh_file_climbs_to_the_top() {
        let mut conn = memory();
        assert_eq!(schema_version(&conn).expect("version"), 0);
        assert_eq!(migrate(&mut conn, LADDER).expect("migrate"), 3);
        assert_eq!(tables(&conn), vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn an_old_file_climbs_only_the_rungs_it_is_missing() {
        let mut conn = memory();
        migrate(&mut conn, &LADDER[..1]).expect("partial migrate");
        assert_eq!(schema_version(&conn).expect("version"), 1);
        assert!(!tables(&conn).contains(&"b".to_owned()));

        assert_eq!(migrate(&mut conn, LADDER).expect("migrate"), 3);
        assert!(tables(&conn).contains(&"b".to_owned()));
        // Rung 3 is an ALTER, so re-running rung 1 would have failed here.
        conn.execute("INSERT INTO a (x, z) VALUES (1, 'ok')", [])
            .expect("column z must exist exactly once");
    }

    #[test]
    fn migrating_an_up_to_date_file_is_a_no_op() {
        let mut conn = memory();
        migrate(&mut conn, LADDER).expect("first");
        conn.execute("INSERT INTO a (x, z) VALUES (7, 'kept')", [])
            .expect("insert");

        assert_eq!(migrate(&mut conn, LADDER).expect("second"), 3);
        let kept: i64 = conn
            .query_row("SELECT x FROM a", [], |row| row.get(0))
            .expect("row survives");
        assert_eq!(kept, 7);
    }

    #[test]
    fn a_file_from_the_future_is_refused_rather_than_downgraded() {
        let mut conn = memory();
        conn.pragma_update(None, "user_version", 99i64)
            .expect("set version");

        let err = migrate(&mut conn, LADDER).expect_err("must refuse");
        let message = err.to_string();
        assert!(message.contains("99"), "{message}");
        assert!(message.contains("Refusing"), "{message}");
    }

    #[test]
    fn a_failing_rung_rolls_back_and_leaves_the_version_alone() {
        const BROKEN: &[Migration] = &[
            Migration {
                version: 1,
                name: "good",
                sql: "CREATE TABLE IF NOT EXISTS keep (x INTEGER) STRICT;",
            },
            Migration {
                version: 2,
                name: "bad",
                sql: "CREATE TABLE IF NOT EXISTS half (x INTEGER) STRICT; \
                      THIS IS NOT SQL;",
            },
        ];

        let mut conn = memory();
        let err = migrate(&mut conn, BROKEN).expect_err("must fail");
        assert!(err.to_string().contains("migration 2 (bad)"), "{err}");

        assert_eq!(
            schema_version(&conn).expect("version"),
            1,
            "a failed rung must not advance the version"
        );
        assert!(tables(&conn).contains(&"keep".to_owned()));
        assert!(
            !tables(&conn).contains(&"half".to_owned()),
            "the failed rung's earlier statements must be rolled back"
        );
    }

    #[test]
    fn a_non_ascending_ladder_is_rejected_before_it_touches_the_file() {
        const TANGLED: &[Migration] = &[
            Migration {
                version: 2,
                name: "two",
                sql: "SELECT 1;",
            },
            Migration {
                version: 2,
                name: "two again",
                sql: "SELECT 1;",
            },
        ];
        let mut conn = memory();
        assert!(migrate(&mut conn, TANGLED).is_err());
        assert_eq!(schema_version(&conn).expect("version"), 0);
    }

    #[test]
    fn the_real_schema_migrates_and_is_stable_across_reopens() {
        let mut conn = memory();
        let version = migrate(&mut conn, MIGRATIONS).expect("migrate");
        assert_eq!(version, latest_version(MIGRATIONS));

        let names = tables(&conn);
        for expected in ["blobs", "captures", "store_meta"] {
            assert!(names.contains(&expected.to_owned()), "missing {expected}");
        }
        assert_eq!(migrate(&mut conn, MIGRATIONS).expect("again"), version);
    }

    #[test]
    fn recording_columns_are_repaired_when_version_two_was_already_claimed() {
        let mut conn = memory();
        migrate(&mut conn, &MIGRATIONS[..1]).expect("create legacy schema");
        conn.pragma_update(None, "user_version", 2i64)
            .expect("simulate a colliding version two");

        assert_eq!(
            migrate(&mut conn, MIGRATIONS).expect("repair"),
            latest_version(MIGRATIONS)
        );
        conn.execute(
            "INSERT INTO captures (
                id, created_at, stored_at, provenance, target_json, frame_json,
                media_kind, video_json
             ) VALUES ('video', 0, 0, 'display', '{}', 'null', 'video', '{}')",
            [],
        )
        .expect("recording columns exist after repair");
    }

    #[test]
    fn legacy_version_three_media_constraint_is_rebuilt_without_losing_rows() {
        let mut conn = memory();
        migrate(&mut conn, &MIGRATIONS[..1]).expect("create legacy schema");
        conn.execute_batch(
            "ALTER TABLE captures
                 ADD COLUMN media_kind TEXT NOT NULL DEFAULT 'image'
                 CHECK (media_kind IN ('image', 'video'));
             ALTER TABLE captures ADD COLUMN video_json TEXT;
             INSERT INTO captures (
                 id, created_at, stored_at, provenance, target_json, frame_json,
                 media_kind
             ) VALUES ('legacy', 0, 0, 'display', '{}', 'null', 'image');
             PRAGMA user_version = 3;",
        )
        .expect("create old recording schema");

        assert_eq!(
            migrate(&mut conn, MIGRATIONS).expect("repair"),
            latest_version(MIGRATIONS)
        );
        let kind: String = conn
            .query_row(
                "SELECT media_kind FROM captures WHERE id = 'legacy'",
                [],
                |row| row.get(0),
            )
            .expect("legacy row survives");
        assert_eq!(kind, "screenshot");
        conn.execute(
            "INSERT INTO captures (
                id, created_at, stored_at, provenance, target_json, frame_json,
                media_kind
             ) VALUES ('gif', 0, 0, 'display', '{}', 'null', 'gif')",
            [],
        )
        .expect("rebuilt constraint accepts every canonical media kind");
    }

    #[test]
    fn recording_migration_preserves_colliding_source_metadata_columns() {
        let mut conn = memory();
        migrate(&mut conn, &MIGRATIONS[..1]).expect("create legacy schema");
        conn.execute_batch(
            "INSERT INTO captures (
                 id, created_at, stored_at, app_name, app_identifier, window_title,
                 window_shadow, provenance, target_json, frame_json
             ) VALUES (
                 'legacy-source', 0, 0, 'Preview', 'com.apple.Preview', 'Document',
                 0, 'window', '{}', 'null'
             );
             PRAGMA user_version = 2;",
        )
        .expect("create colliding source-metadata schema");

        assert_eq!(
            migrate(&mut conn, MIGRATIONS).expect("repair"),
            latest_version(MIGRATIONS)
        );
        let preserved: (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT app_identifier, window_shadow
                 FROM captures WHERE id = 'legacy-source'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("preserved source metadata");
        assert_eq!(preserved.0.as_deref(), Some("com.apple.Preview"));
        assert_eq!(preserved.1, Some(0));
    }

    #[test]
    fn version_five_repairs_source_columns_dropped_by_the_old_v4_rebuild() {
        let mut conn = memory();
        migrate(&mut conn, MIGRATIONS).expect("create current schema");
        conn.execute_batch(
            "ALTER TABLE captures DROP COLUMN app_identifier;
             ALTER TABLE captures DROP COLUMN window_shadow;
             PRAGMA user_version = 4;",
        )
        .expect("simulate the old version-four rebuild");

        assert_eq!(
            migrate(&mut conn, MIGRATIONS).expect("repair"),
            latest_version(MIGRATIONS)
        );
        let tx = conn.transaction().expect("inspect repaired schema");
        assert!(column_exists(&tx, "captures", "app_identifier").unwrap());
        assert!(column_exists(&tx, "captures", "window_shadow").unwrap());
        tx.rollback().expect("finish schema inspection");
    }

    #[test]
    fn the_schema_rejects_a_pinned_value_that_is_not_a_flag() {
        let mut conn = memory();
        migrate(&mut conn, MIGRATIONS).expect("migrate");
        let bad = conn.execute(
            "INSERT INTO captures (id, created_at, stored_at, pinned, provenance, target_json, frame_json) \
             VALUES ('x', 0, 0, 7, 'window', '{}', '{}')",
            [],
        );
        assert!(bad.is_err(), "CHECK (pinned IN (0,1)) must hold");
    }
}
