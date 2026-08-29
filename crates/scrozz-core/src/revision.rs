//! Immutable content revision identities.
//!
//! A revision is not a timestamp or a hash of pixels. It is a monotonically
//! increasing identity owned by the content model. Keeping it opaque prevents a
//! caller from mistaking "same dimensions" or "same file name" for "same
//! pixels", which is the race-sensitive distinction export and analysis need.

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SOURCE: AtomicU64 = AtomicU64::new(1);

/// The revision of editable image content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ContentRevision {
    source: u64,
    generation: u64,
}

impl ContentRevision {
    /// The revision of untouched content.
    pub const INITIAL: Self = Self {
        source: 0,
        generation: 0,
    };

    /// Creates a revision supplied by an external immutable source.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self {
            source: 0,
            generation: value,
        }
    }

    /// Creates an initial revision unique to one in-process content source.
    #[must_use]
    pub fn fresh() -> Self {
        let source = NEXT_SOURCE.fetch_add(1, Ordering::Relaxed);
        assert!(
            source != 0 && source != u64::MAX,
            "content source identity space exhausted"
        );
        Self {
            source,
            generation: 0,
        }
    }

    /// Stable source/generation parts, suitable for diagnostic cache keys.
    #[must_use]
    pub const fn parts(self) -> (u64, u64) {
        (self.source, self.generation)
    }

    /// A new globally unique revision after this one.
    ///
    /// # Panics
    ///
    /// Panics if every in-process source identity has been exhausted. Failing
    /// closed after 18 quintillion edits is safer than letting two divergent
    /// document clones reuse the same generation.
    #[must_use]
    pub fn next(self) -> Self {
        debug_assert!(self.source != u64::MAX);
        Self::fresh()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_never_wrap_to_an_old_identity() {
        let first = ContentRevision::fresh();
        let next = first.next();
        assert_ne!(first, next);
    }

    #[test]
    fn fresh_sources_cannot_share_a_cache_identity() {
        let first = ContentRevision::fresh();
        let second = ContentRevision::fresh();
        assert_ne!(first, second);
        assert_ne!(first.next(), second.next());
    }

    #[test]
    fn divergent_clones_receive_different_next_revisions() {
        let shared = ContentRevision::fresh();
        assert_ne!(shared.next(), shared.next());
    }
}
