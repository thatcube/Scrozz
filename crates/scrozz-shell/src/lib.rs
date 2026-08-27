//! OS integration: hotkeys, overlay windows, tray, permissions, drag source.
//!
//! Everything here is a place where the three platforms genuinely disagree, and
//! where Wayland may refuse outright. Nothing above this crate should contain a
//! `cfg(target_os)`.

// Platform APIs are reached through objc2 / windows-rs / x11rb, all of which
// require `unsafe`. It is confined to this crate: every crate above it in the
// dependency graph forbids unsafe outright.
#![deny(unsafe_op_in_unsafe_fn)]

pub mod drag;
pub mod hotkey;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod overlay;
pub mod permissions;
pub mod tray;

pub use drag::{
    ByteSource, DragCapability, DragFormat, DragOperation, DragOrigin, DragOutcome, DragPayload,
    DragPreview, DragSession, DragSource, NativeDragSource, NativeSurface, PromisedFile,
    byte_source, native_drag_source,
};
pub use hotkey::{
    Accelerator, Compositor, Conflict, DisplayServer, GlobalHotkeys, HotkeyEvent, KeyState,
    ReservedShortcut, Session,
};
pub use overlay::{
    AppKitRect, NativeOverlay, OverlayBehavior, OverlayLevel, OverlayReport, StackLayout,
    anchor_bottom_left, appkit_to_logical, logical_to_appkit,
};
pub use permissions::SystemPermissions;
pub use tray::{Tray, TrayAction, TrayEntry};

use scrozz_core::{LogicalRect, Result};

/// A floating, chrome-less window that lives over the desktop.
///
/// Scrozz's capture stack, capture dock and selection overlay are not
/// application windows — they are anchored to the *screen*, borderless, mostly
/// transparent, always on top, and must not steal focus from whatever the user
/// is typing in.
///
/// # Platform reality
///
/// - **macOS** — a plain `NSWindow` activates its app on click, so the capture
///   stack would yank focus out of the user's editor. The correct construct is
///   an `NSPanel` with `NSWindowStyleMaskNonactivatingPanel`. The selection
///   overlay additionally needs a level above the menu bar, which is higher than
///   winit's `WindowLevel::AlwaysOnTop` reaches.
/// - **Windows** — `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED` with
///   `HWND_TOPMOST`.
/// - **Linux/X11** — override-redirect plus `_NET_WM_WINDOW_TYPE_DOCK`.
/// - **Linux/Wayland** — clients **cannot set absolute window position**;
///   `xdg_shell` omits it deliberately. Overlays require the `wlr-layer-shell`
///   protocol, which KDE implements and GNOME/Mutter does not. Per decision D8
///   this degrades explicitly rather than silently misplacing the overlay.
pub trait OverlayWindow {
    /// Anchors this overlay within a display's work area.
    ///
    /// Callers pass [`scrozz_core::Display::work_area`], never
    /// [`scrozz_core::Display::bounds`]: anchoring
    /// to raw display bounds puts the overlay behind the Dock or taskbar.
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::Unsupported`] where the compositor forbids
    /// client positioning.
    fn set_frame(&mut self, frame: LogicalRect) -> Result<()>;

    /// Sets whether clicks pass through to whatever is beneath.
    ///
    /// Per-window and all-or-nothing on every platform, while the capture stack
    /// is mostly empty space around a few opaque cards. Implementations toggle
    /// this per frame from the pointer position, or give each card its own
    /// window.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform refused.
    fn set_click_through(&mut self, passthrough: bool) -> Result<()>;
}

/// A key combination bound globally.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hotkey {
    /// Platform-independent accelerator description, e.g. `"Cmd+Shift+4"`.
    pub accelerator: String,
}

/// Registers system-wide hotkeys.
pub trait HotkeyManager {
    /// Binds a hotkey to an action name.
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::Unsupported`] on wlroots compositors, which
    /// implement no global-shortcut portal. That is not a defect to work around:
    /// per decision D11 the remedy is that the user binds a compositor keybinding
    /// to the Scrozz CLI, and onboarding generates that config line for them.
    /// This is the reason the CLI is a platform requirement rather than a
    /// convenience.
    fn register(&mut self, hotkey: &Hotkey, action: &str) -> Result<()>;

    /// Releases a binding.
    ///
    /// # Errors
    ///
    /// Returns an error if the hotkey was not registered.
    fn unregister(&mut self, hotkey: &Hotkey) -> Result<()>;
}

/// An OS capability that must be granted before a feature works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Reading screen contents.
    ScreenRecording,
    /// Capturing microphone audio.
    Microphone,
    /// Synthesising input and reading window metadata.
    Accessibility,
}

/// Queries and requests OS permissions.
///
/// Per decision D15 Scrozz attempts everything and asks at the moment a feature
/// is first used, never during onboarding. A permission wall before the user has
/// seen the app do anything is the single most common way these tools lose
/// people.
pub trait Permissions {
    /// Whether a capability is currently granted.
    fn is_granted(&self, capability: Capability) -> bool;

    /// Prompts for a capability, or opens the relevant settings pane.
    ///
    /// # Errors
    ///
    /// Returns an error if the request could not be presented.
    fn request(&self, capability: Capability) -> Result<()>;
}
