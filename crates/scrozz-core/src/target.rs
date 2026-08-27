//! What can be captured: displays, windows, and regions of them.

use serde::{Deserialize, Serialize};

use crate::geometry::{LogicalRect, ScaleFactor};

/// Stable-for-a-session handle to a display.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DisplayId(pub String);

/// Stable-for-a-session handle to a window.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId(pub String);

/// A connected display.
#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    /// Handle for capture requests.
    pub id: DisplayId,
    /// Human-readable name, e.g. "Built-in Retina Display".
    pub name: String,
    /// Full bounds in the global logical desktop.
    pub bounds: LogicalRect,
    /// Bounds excluding OS furniture — menu bar, Dock, taskbar, panels.
    ///
    /// This, not [`Self::bounds`], is what floating overlays anchor to. Anchoring
    /// the capture stack to the raw bottom-left of a Mac display puts it behind
    /// the Dock.
    pub work_area: LogicalRect,
    /// Pixels per point for this display.
    ///
    /// Per-display, never global: a desktop may mix a 2× laptop panel with a 1×
    /// external monitor, and a single app-wide scale is wrong on one of them.
    pub scale: ScaleFactor,
    /// Whether this is the primary display.
    pub is_primary: bool,
}

/// An on-screen window belonging to some application.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// Handle for capture requests.
    pub id: WindowId,
    /// Window title, if the OS exposes one.
    pub title: Option<String>,
    /// Owning application's display name.
    pub application: Option<String>,
    /// Frame in the global logical desktop.
    pub bounds: LogicalRect,
    /// The display this window is predominantly on.
    pub display: DisplayId,
    /// Whether the window is on screen and not minimised.
    pub is_visible: bool,
}

/// What a capture is aimed at.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureTarget {
    /// One entire display.
    Display(DisplayId),
    /// A single window, excluding anything overlapping it.
    Window(WindowId),
    /// A user-chosen rectangle in the global logical desktop.
    Region(LogicalRect),
    /// Every display, composited into one image.
    AllDisplays,
}

impl CaptureTarget {
    /// Whether this target is a window.
    ///
    /// Load-bearing rather than cosmetic: decision D9 forbids compositing
    /// anything onto a window capture — no synthetic corner rounding, no shadow,
    /// no background padding — because the OS already provides the window's true
    /// shape and shadow, and re-adding them produces a subtly wrong image. Call
    /// sites use this to disable those controls entirely.
    #[must_use]
    pub const fn is_window(&self) -> bool {
        matches!(self, Self::Window(_))
    }
}

/// Enumeration of what is currently capturable.
///
/// Implementations must treat this as a snapshot: windows close and displays are
/// unplugged between enumeration and capture, which surfaces as
/// [`crate::Error::TargetGone`].
pub trait TargetEnumerator {
    /// Lists connected displays.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform refuses access.
    fn displays(&self) -> crate::Result<Vec<Display>>;

    /// Lists capturable windows, front-most first.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Unsupported`] on Wayland, which has no window
    /// enumeration protocol; callers fall back to the portal's own picker, which
    /// performs the selection out-of-process. Returns
    /// [`crate::Error::PermissionDenied`] where titles require a grant.
    fn windows(&self) -> crate::Result<Vec<Window>>;

    /// The display containing the pointer, used to decide where overlays appear.
    ///
    /// # Errors
    ///
    /// Returns an error if pointer position is unavailable.
    fn active_display(&self) -> crate::Result<Display>;
}
