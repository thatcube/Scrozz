//! Process-wide counters for native calls that must never become idle polling.

use std::sync::atomic::{AtomicU64, Ordering};

static SCREEN_PREFLIGHTS: AtomicU64 = AtomicU64::new(0);
static SCREEN_REQUESTS: AtomicU64 = AtomicU64::new(0);
static DISPLAY_ENUMERATIONS: AtomicU64 = AtomicU64::new(0);

/// Monotonic native activity counters for diagnostics and rate assertions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeActivitySnapshot {
    /// Calls to `CGPreflightScreenCaptureAccess`.
    pub screen_preflights: u64,
    /// Calls to `CGRequestScreenCaptureAccess`.
    pub screen_requests: u64,
    /// Calls that enumerate AppKit display geometry.
    pub display_enumerations: u64,
}

impl NativeActivitySnapshot {
    /// Activity since an earlier snapshot.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Self {
        Self {
            screen_preflights: self
                .screen_preflights
                .saturating_sub(earlier.screen_preflights),
            screen_requests: self.screen_requests.saturating_sub(earlier.screen_requests),
            display_enumerations: self
                .display_enumerations
                .saturating_sub(earlier.display_enumerations),
        }
    }
}

/// Reads the process-wide counters without resetting them.
#[must_use]
pub fn snapshot() -> NativeActivitySnapshot {
    NativeActivitySnapshot {
        screen_preflights: SCREEN_PREFLIGHTS.load(Ordering::Relaxed),
        screen_requests: SCREEN_REQUESTS.load(Ordering::Relaxed),
        display_enumerations: DISPLAY_ENUMERATIONS.load(Ordering::Relaxed),
    }
}

pub(crate) fn record_screen_preflight() {
    SCREEN_PREFLIGHTS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_screen_request() {
    SCREEN_REQUESTS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_display_enumeration() {
    DISPLAY_ENUMERATIONS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_report_only_activity_after_the_baseline() {
        let before = snapshot();
        record_screen_preflight();
        record_screen_request();
        record_display_enumeration();

        let delta = snapshot().since(before);
        assert!(delta.screen_preflights >= 1);
        assert!(delta.screen_requests >= 1);
        assert!(delta.display_enumerations >= 1);
    }
}
