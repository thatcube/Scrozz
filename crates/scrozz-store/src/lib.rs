//! Capture history, annotation documents, and retention.
//!
//! # What is kept, and what is thrown away
//!
//! Decision D23 splits the two deliberately. **Annotation documents are kept
//! forever** — they are kilobytes, and keeping them is the only thing that makes
//! D14's "annotations are never permanent" actually true a year later. **Source
//! images are evicted against a size cap**, oldest first, because they are the
//! entire bulk. **Pinned captures are never evicted.**
//!
//! The alternative, a blunt "delete everything older than a week" slider,
//! discards the edit history along with the pixels, which is the one part worth
//! keeping.
//!
//! # How it is stored
//!
//! ```text
//! <data dir>/Scrozz/
//!   index.sqlite          the query index — a cache, rebuildable at any time
//!   documents/<id>.json   the durable record: metadata + every edit
//!   images/ab/cd/<hash>   source pixels, content-addressed, evictable
//! ```
//!
//! The index is deliberately *not* the source of truth. Every capture writes a
//! self-contained JSON record before its index row exists, so a database lost to
//! corruption, a bad upgrade, or a full disk costs a rebuild rather than a
//! user's history. Source images live on the filesystem rather than in SQLite
//! because eviction should be an `unlink`, and because a 10 GB database is a
//! liability every time it is backed up, vacuumed, or checked.
//!
//! # Concurrency
//!
//! The GUI and the CLI are expected to run at the same time (decision D11), so
//! the index runs in WAL mode with a busy timeout and every write takes an
//! immediate transaction. Readers never block writers and writers never block
//! readers.

#![forbid(unsafe_code)]

pub mod db;
pub mod hash;
pub mod id;
pub mod layout;
pub mod model;
pub mod record;
pub mod schema;
pub mod sqlite_store;

#[doc(hidden)]
pub mod test_support;

pub use layout::StoreLayout;
pub use model::{
    CaptureRecord, FrameHeader, ImageState, MediaKind, Page, RetentionReport, RetentionWindow,
    SearchQuery, Timestamp, VideoCompletion, VideoMetadata, VideoSalvageability,
};
pub use record::StoredRecord;
pub use sqlite_store::{
    DocumentState, EvictedDocument, History, NewCapture, NewRecording, RecoveryReport, SqliteStore,
};

use scrozz_core::Result;
use serde::{Deserialize, Serialize};

/// Identifies a capture in history.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CaptureId(pub String);

/// Retention policy for stored captures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Maximum bytes of source imagery to retain.
    pub max_image_bytes: u64,
    /// Maximum age of unpinned source imagery.
    #[serde(default)]
    pub max_image_age: RetentionWindow,
}

impl RetentionPolicy {
    /// Builds a policy from the byte and day values persisted by settings.
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::InvalidRequest`] when `max_age_days` is
    /// not one of `0`, `1`, `3`, `7`, or `30`.
    pub fn from_limits(max_image_bytes: u64, max_age_days: u32) -> Result<Self> {
        Ok(Self {
            max_image_bytes,
            max_image_age: RetentionWindow::from_days(max_age_days)?,
        })
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        // 10 GB. Generous enough that most users never hit it, bounded enough
        // that the app cannot quietly consume a disk.
        Self {
            max_image_bytes: 10 * 1024 * 1024 * 1024,
            max_image_age: RetentionWindow::Forever,
        }
    }
}

/// Persistent storage for captures and their documents.
pub trait Store {
    /// Lists captures, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::Storage`] if the index is unreadable.
    fn list(&self) -> Result<Vec<CaptureId>>;

    /// Marks a capture exempt from eviction.
    ///
    /// # Errors
    ///
    /// Returns an error if the capture is unknown.
    fn set_pinned(&mut self, id: &CaptureId, pinned: bool) -> Result<()>;

    /// Evicts source images until the policy is satisfied.
    ///
    /// Must never remove an annotation document, and never remove a pinned
    /// capture. A capture whose image has been evicted remains listed, with its
    /// edits intact.
    ///
    /// # Errors
    ///
    /// Returns an error if eviction could not complete.
    fn enforce_retention(&mut self, policy: &RetentionPolicy) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_builds_from_the_exact_persisted_limits() {
        assert_eq!(
            RetentionPolicy::from_limits(42, 7).unwrap(),
            RetentionPolicy {
                max_image_bytes: 42,
                max_image_age: RetentionWindow::OneWeek,
            }
        );
        assert!(RetentionPolicy::from_limits(42, 365).is_err());
    }
}
