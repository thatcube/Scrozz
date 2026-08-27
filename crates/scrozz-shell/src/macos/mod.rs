//! The macOS half of Scrozz's OS integration.
//!
//! Everything here is `cfg(target_os = "macos")` and reached only through the
//! platform-neutral types in [`crate::overlay`] and [`crate::permissions`].
//! Nothing above `scrozz-shell` may name AppKit.
//!
//! # Threading
//!
//! `NSScreen`, `NSWindow` and `NSApplication` are all main-thread-only, which
//! `objc2` enforces with [`MainThreadMarker`]. Every entry point here therefore
//! starts by asking for one and returns [`scrozz_core::Error::Platform`] rather
//! than panicking if it is not on the main thread — a background thread asking
//! for the display list is a caller bug, but crashing the app over it is worse
//! than reporting it.

pub mod clipboard;
pub mod display;
pub mod drag;
pub mod overlay;
pub mod permissions;

use objc2_foundation::MainThreadMarker;
use scrozz_core::{Error, Result};

/// Acquires a main-thread marker, or explains why AppKit cannot be touched.
///
/// # Errors
///
/// Returns [`Error::Platform`] when called off the main thread.
pub(crate) fn main_thread(what: &str) -> Result<MainThreadMarker> {
    MainThreadMarker::new().ok_or_else(|| {
        Error::Platform(format!(
            "{what} requires the main thread; AppKit refuses off-main access to \
             NSScreen, NSWindow and NSApplication"
        ))
    })
}
