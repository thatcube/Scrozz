//! Still capture backends.
//!
//! One implementation of [`scrozz_core::CaptureBackend`] per platform, selected
//! at runtime by [`backend`]. Nothing above this crate knows which one it got.
//!
//! # Platform reality
//!
//! - **macOS** — `ScreenCaptureKit` on macOS 12.3+, which is the only path that
//!   still works well and the only one Apple maintains.
//! - **Windows** — `Windows.Graphics.Capture` (WGC) on Windows 10 1903+, with
//!   `BitBlt`/DXGI as the fallback for older builds and odd GPU configurations.
//! - **Linux/X11** — direct `XGetImage`/XShm. Full window enumeration, absolute
//!   positioning, everything works.
//! - **Linux/Wayland** — the `xdg-desktop-portal` `ScreenCast` interface, and
//!   only that. Per decision D8 the gaps are real and documented rather than
//!   papered over: **there is no window enumeration protocol at all**, so
//!   [`scrozz_core::TargetEnumerator::windows`] returns
//!   [`scrozz_core::Error::Unsupported`] and callers fall back to the portal's
//!   own out-of-process picker.

// Platform APIs are reached through objc2 / windows-rs / x11rb, all of which
// require `unsafe`. It is confined to this crate: every crate above it in the
// dependency graph forbids unsafe outright.
#![deny(unsafe_op_in_unsafe_fn)]

use scrozz_core::{CaptureBackend, Result};

/// Constructs the best capture backend for the running system.
///
/// # Errors
///
/// Returns [`scrozz_core::Error::Unsupported`] if no backend can serve this
/// platform or compositor.
pub fn backend() -> Result<Box<dyn CaptureBackend>> {
    todo!("select and construct the platform capture backend")
}
