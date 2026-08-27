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

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod scroll_units;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
pub use macos::ScreenCaptureKitBackend;

use scrozz_core::{CaptureBackend, Result, ScrollDriver};

/// Constructs the best capture backend for the running system.
///
/// # Errors
///
/// Returns [`scrozz_core::Error::Unsupported`] if no backend can serve this
/// platform or compositor.
pub fn backend() -> Result<Box<dyn CaptureBackend>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::ScreenCaptureKitBackend::new()))
    }

    #[cfg(target_os = "linux")]
    {
        linux::backend()
    }

    #[cfg(target_os = "windows")]
    {
        windows::backend()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        todo!("select and construct the platform capture backend")
    }
}

/// Constructs the scroll-input driver for the running desktop.
///
/// Linux selection is runtime-sensitive: a native X11 session uses XTEST,
/// GNOME/KDE Wayland uses the RemoteDesktop portal when it can be reached, and
/// desktops without a safe synthesis route return a manual driver carrying the
/// reason. Constructing the driver never opens a permission prompt; grants are
/// acquired by [`ScrollDriver::prepare`].
///
/// # Errors
///
/// Returns [`scrozz_core::Error::Platform`] when the selected native input
/// service cannot be reached, or [`scrozz_core::Error::Unsupported`] on an
/// unknown operating system.
pub fn scroll_driver() -> Result<Box<dyn ScrollDriver>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::scroll::CgEventScrollDriver::new()))
    }

    #[cfg(target_os = "linux")]
    {
        linux::scroll_driver()
    }

    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::scroll::SendInputScrollDriver::new()))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(scrozz_core::Error::Unsupported {
            what: "automatic scroll input".into(),
            why: "Scrozz has no scroll-input driver for this operating system".into(),
        })
    }
}

#[cfg(test)]
mod scroll_factory_tests {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn native_factory_reports_its_static_capabilities_without_preparing() {
        let driver = super::scroll_driver().expect("native construction does not acquire a grant");
        let capabilities = driver.capabilities();
        assert!(capabilities.is_automatic());
        assert_eq!(capabilities.requires_permission, cfg!(target_os = "macos"));
    }
}
