//! Opening the index, and deciding when it is beyond opening.
//!
//! # Two processes, one database
//!
//! Decision D11 makes the CLI a first-class peer of the GUI rather than a
//! convenience wrapper, and on wlroots the *only* way to bind a hotkey is a
//! compositor keybinding that runs the CLI. So a capture taken from the command
//! line lands in the same history the running GUI is displaying, at the same
//! moment. Concurrent access is the design, not an edge case.
//!
//! Three settings make that safe:
//!
//! - **WAL** lets readers keep reading while a writer commits. Without it the
//!   GUI's history list blocks every CLI capture and vice versa.
//! - **`busy_timeout`** turns the remaining single-writer contention into a
//!   short wait instead of an immediate `SQLITE_BUSY` the caller must retry.
//! - **`BEGIN IMMEDIATE`** for anything that writes. A deferred transaction
//!   that upgrades to a write mid-way cannot be retried by the busy handler —
//!   SQLite has to fail it, because rolling back would discard reads the
//!   statement already returned. Taking the write lock at the start is what
//!   makes the timeout actually apply.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use scrozz_core::{Error, Result};

/// How long a writer waits for another process before giving up.
///
/// Generous on purpose: the competing writer is another Scrozz process
/// inserting one screenshot, so the wait is milliseconds in practice, and the
/// cost of being wrong is a lost capture.
pub const BUSY_TIMEOUT_MS: u32 = 10_000;

/// Opens the index at `path`, applying the settings above.
///
/// # Errors
///
/// Returns [`Error::Storage`] if the file cannot be opened or configured.
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| Error::Storage(format!("cannot open history index: {e}")))?;

    configure(&conn)?;
    Ok(conn)
}

/// Opens an index that lives only in memory, for tests and dry runs.
///
/// # Errors
///
/// Returns [`Error::Storage`] if the connection cannot be configured.
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()
        .map_err(|e| Error::Storage(format!("cannot open in-memory index: {e}")))?;
    // WAL is meaningless for a private in-memory database; the rest still is.
    conn.busy_timeout(std::time::Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))
        .map_err(|e| Error::Storage(format!("cannot set busy timeout: {e}")))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| Error::Storage(format!("cannot enable foreign keys: {e}")))?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))
        .map_err(|e| Error::Storage(format!("cannot set busy timeout: {e}")))?;

    // `journal_mode` answers with the mode it settled on, so it must be read as
    // a query. Setting it is persistent — it survives in the file header — but
    // asserting it every open catches a database copied out of a mode we do not
    // support.
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|e| Error::Storage(format!("cannot enable write-ahead logging: {e}")))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(
            mode = %mode,
            "history index is not in WAL mode; concurrent CLI and GUI access will contend"
        );
    }

    // FULL rather than NORMAL. Under WAL, NORMAL can lose the most recent
    // commits to a power cut. Screenshots are written a handful of times a
    // minute, so the extra fsync is invisible, and "the capture I just took is
    // missing" is the single worst thing this crate could do.
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|e| Error::Storage(format!("cannot set synchronous mode: {e}")))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| Error::Storage(format!("cannot enable foreign keys: {e}")))?;

    Ok(())
}

/// Runs SQLite's own structural check.
///
/// `quick_check` rather than `integrity_check`: it catches the damage that
/// actually happens — truncation, a torn page, a bad B-tree link — without the
/// full index cross-verification, which on a large history would add seconds to
/// every launch for a class of fault the recovery path handles anyway.
///
/// # Errors
///
/// Never returns `Err` for a *corrupt* database; corruption is the `false`
/// case. `Err` means the check could not be run at all.
pub fn is_healthy(conn: &Connection) -> Result<bool> {
    let verdict: std::result::Result<String, _> =
        conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0));

    match verdict {
        Ok(answer) => Ok(answer.eq_ignore_ascii_case("ok")),
        Err(err) if looks_like_corruption(&err) => Ok(false),
        Err(err) => Err(Error::Storage(format!("cannot check history index: {err}"))),
    }
}

/// Whether a rusqlite failure means the file is damaged rather than busy.
///
/// The distinction decides between quarantining a database and simply retrying,
/// so it is deliberately narrow: only codes that mean the bytes are wrong.
#[must_use]
pub fn looks_like_corruption(err: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode;

    if let rusqlite::Error::SqliteFailure(inner, _) = err {
        return matches!(
            inner.code,
            ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
        );
    }
    false
}

/// Whether an error raised while opening or migrating means the file is damaged.
#[must_use]
pub fn is_corruption_error(err: &Error) -> bool {
    // rusqlite errors are flattened into `Error::Storage` at the boundary, so
    // the classification has to survive as text. Matching on the message is
    // unlovely, but the alternative is a second error enum threaded through
    // every call site to carry one bit.
    match err {
        Error::Storage(message) => {
            let lower = message.to_ascii_lowercase();
            lower.contains("database disk image is malformed")
                || lower.contains("file is not a database")
                || lower.contains("file is encrypted")
                || lower.contains("database corrupt")
                || lower.contains("malformed database schema")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_support::scratch_dir;

    #[test]
    fn a_new_index_opens_in_wal_mode() {
        let dir = scratch_dir("db-wal");
        let conn = open(&dir.path().join("index.sqlite")).expect("open");

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read mode");
        assert!(mode.eq_ignore_ascii_case("wal"), "got {mode}");
    }

    #[test]
    fn a_new_index_is_healthy() {
        let dir = scratch_dir("db-health");
        let conn = open(&dir.path().join("index.sqlite")).expect("open");
        assert!(is_healthy(&conn).expect("check"));
    }

    #[test]
    fn a_file_of_noise_is_not_a_database() {
        let dir = scratch_dir("db-noise");
        let path = dir.path().join("index.sqlite");
        fs::write(&path, b"this is not a database, it is a haiku").expect("write");

        // Either the open or the check must reject it; both count as detection.
        let detected = match open(&path) {
            Ok(conn) => !is_healthy(&conn).unwrap_or(false),
            Err(err) => is_corruption_error(&err),
        };
        assert!(detected, "a noise file must not pass as a healthy index");
    }

    #[test]
    fn corruption_is_told_apart_from_ordinary_failure() {
        assert!(is_corruption_error(&Error::Storage(
            "cannot open history index: file is not a database".into()
        )));
        assert!(is_corruption_error(&Error::Storage(
            "database disk image is malformed".into()
        )));
        assert!(!is_corruption_error(&Error::Storage(
            "database is locked".into()
        )));
        assert!(!is_corruption_error(&Error::Cancelled));
    }

    #[test]
    fn two_connections_to_one_file_both_work() {
        let dir = scratch_dir("db-two");
        let path = dir.path().join("index.sqlite");

        let first = open(&path).expect("first");
        first
            .execute("CREATE TABLE t (x INTEGER) STRICT", [])
            .expect("create");
        first.execute("INSERT INTO t VALUES (1)", []).expect("write");

        let second = open(&path).expect("second");
        let seen: i64 = second
            .query_row("SELECT x FROM t", [], |row| row.get(0))
            .expect("read");
        assert_eq!(seen, 1);
    }
}
