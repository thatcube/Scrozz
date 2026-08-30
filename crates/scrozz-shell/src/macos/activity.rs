//! Process-wide counters for native calls that must never become idle polling.

use std::sync::atomic::{AtomicU64, Ordering};

static SCREEN_PREFLIGHTS: AtomicU64 = AtomicU64::new(0);
static SCREEN_REQUESTS: AtomicU64 = AtomicU64::new(0);
static DISPLAY_ENUMERATIONS: AtomicU64 = AtomicU64::new(0);
static POINTER_SAMPLES: AtomicU64 = AtomicU64::new(0);
static ROOT_REDRAWS: AtomicU64 = AtomicU64::new(0);
static AUTOMATIC_TERMINATION_DISABLES: AtomicU64 = AtomicU64::new(0);
static AUTOMATIC_TERMINATION_ENABLES: AtomicU64 = AtomicU64::new(0);

/// Monotonic native activity counters for diagnostics and rate assertions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeActivitySnapshot {
    /// Calls to `CGPreflightScreenCaptureAccess`.
    pub screen_preflights: u64,
    /// Calls to `CGRequestScreenCaptureAccess`.
    pub screen_requests: u64,
    /// Calls that enumerate AppKit display geometry.
    pub display_enumerations: u64,
    /// Raw CoreGraphics pointer samples.
    pub pointer_samples: u64,
    /// Root eframe UI passes that can reach native redraw/present work.
    pub root_redraws: u64,
    /// App-owned automatic-termination inhibitions acquired.
    pub automatic_termination_disables: u64,
    /// App-owned automatic-termination inhibitions released.
    pub automatic_termination_enables: u64,
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
            pointer_samples: self.pointer_samples.saturating_sub(earlier.pointer_samples),
            root_redraws: self.root_redraws.saturating_sub(earlier.root_redraws),
            automatic_termination_disables: self
                .automatic_termination_disables
                .saturating_sub(earlier.automatic_termination_disables),
            automatic_termination_enables: self
                .automatic_termination_enables
                .saturating_sub(earlier.automatic_termination_enables),
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
        pointer_samples: POINTER_SAMPLES.load(Ordering::Relaxed),
        root_redraws: ROOT_REDRAWS.load(Ordering::Relaxed),
        automatic_termination_disables: AUTOMATIC_TERMINATION_DISABLES.load(Ordering::Relaxed),
        automatic_termination_enables: AUTOMATIC_TERMINATION_ENABLES.load(Ordering::Relaxed),
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

pub(crate) fn record_pointer_sample() {
    POINTER_SAMPLES.fetch_add(1, Ordering::Relaxed);
}

/// Records one root UI redraw/present pass.
pub fn record_root_redraw() {
    ROOT_REDRAWS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_automatic_termination_disable() {
    AUTOMATIC_TERMINATION_DISABLES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_automatic_termination_enable() {
    AUTOMATIC_TERMINATION_ENABLES.fetch_add(1, Ordering::Relaxed);
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
        record_pointer_sample();
        record_root_redraw();
        record_automatic_termination_disable();
        record_automatic_termination_enable();

        let delta = snapshot().since(before);
        assert!(delta.screen_preflights >= 1);
        assert!(delta.screen_requests >= 1);
        assert!(delta.display_enumerations >= 1);
        assert!(delta.pointer_samples >= 1);
        assert!(delta.root_redraws >= 1);
        assert!(delta.automatic_termination_disables >= 1);
        assert!(delta.automatic_termination_enables >= 1);
    }
}
