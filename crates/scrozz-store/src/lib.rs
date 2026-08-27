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

#![forbid(unsafe_code)]

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
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        // 10 GB. Generous enough that most users never hit it, bounded enough
        // that the app cannot quietly consume a disk.
        Self {
            max_image_bytes: 10 * 1024 * 1024 * 1024,
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
