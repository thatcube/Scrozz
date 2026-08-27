//! A compiler for the Linux overlay code, run from a machine that is not Linux.
//!
//! This crate contains no code of its own. It `#[path]`-includes the real
//! sources from `crates/scrozz-shell/src/` — the same files, not copies — with a
//! dependency set narrow enough to cross-compile: `scrozz-shell` itself cannot,
//! because `tray-icon` drags in GTK 3, whose `*-sys` build scripts need a Linux
//! sysroot for pkg-config.
//!
//! What that buys is the thing hardest to get right by reading: every x11rb and
//! `wayland-client` call in `linux/x11.rs` and `linux/wayland.rs` is checked
//! against the real bindings, on every platform, by anyone running
//! `tools/check-all-platforms.sh`.
//!
//! What it does **not** buy, and must not be mistaken for: this proves the code
//! compiles, not that a compositor accepts it. Anchors, exclusive zones and
//! input regions are only tested by running against a real X server or a real
//! Wayland session — see `tools/linux-smoke/`.

// The included files carry their own documentation; requiring it again here
// would mean documenting a re-export of a re-export.
#![allow(missing_docs, dead_code, unused_imports)]

use scrozz_core::{LogicalRect, Result};

#[path = "../../../crates/scrozz-shell/src/hotkey.rs"]
pub mod hotkey;

#[path = "../../../crates/scrozz-shell/src/linux/mod.rs"]
pub mod linux;

#[path = "../../../crates/scrozz-shell/src/overlay.rs"]
pub mod overlay;

/// A copy of `scrozz_shell::OverlayWindow`, because the included modules
/// implement it.
///
/// The trait is three lines and has no logic to drift; duplicating it is
/// cheaper than pulling in the crate that cannot be cross-compiled, which is the
/// entire reason this shim exists.
pub trait OverlayWindow {
    /// Anchors this overlay within a display's work area.
    ///
    /// # Errors
    ///
    /// Returns an error where the compositor forbids client positioning.
    fn set_frame(&mut self, frame: LogicalRect) -> Result<()>;

    /// Sets whether clicks pass through to whatever is beneath.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform refused.
    fn set_click_through(&mut self, passthrough: bool) -> Result<()>;
}

/// A key combination bound globally. Required by the included `hotkey` module.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hotkey {
    /// Platform-independent accelerator description.
    pub accelerator: String,
}

/// Registers system-wide hotkeys. Required by the included `hotkey` module.
pub trait HotkeyManager {
    /// Binds a hotkey to an action name.
    ///
    /// # Errors
    ///
    /// Returns an error where the compositor offers no mechanism.
    fn register(&mut self, hotkey: &Hotkey, action: &str) -> Result<()>;

    /// Releases a previously bound hotkey.
    ///
    /// # Errors
    ///
    /// Returns an error if the hotkey was never registered.
    fn unregister(&mut self, hotkey: &Hotkey) -> Result<()>;
}
