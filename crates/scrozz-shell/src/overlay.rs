//! Floating overlay windows — the only thing Scrozz ever puts on screen.
//!
//! Per decision D27 the app is invisible at rest: no window, no Dock icon, no
//! taskbar entry. Everything that ever appears is a *transient floating*
//! surface — a capture card, the capture dock, the selection overlay, the
//! magnifier — and every one of them is borderless, always-on-top, anchored to
//! the screen rather than owned by a window manager, and must not steal focus.
//!
//! This module is the window layer that makes those surfaces possible. It is
//! split deliberately into two halves:
//!
//! - **Pure geometry** ([`AppKitRect`], [`StackLayout`], the conversion
//!   functions) — no platform calls, no `unsafe`, fully unit-testable
//!   headlessly. Almost every overlay bug in this class of app is arithmetic,
//!   not API misuse, so the arithmetic is kept where a test can reach it.
//! - **Native attachment** ([`NativeOverlay`]) — applies the platform
//!   properties to a window someone else created. eframe/winit creates the
//!   `NSWindow`; we retrofit it, which is the approach the `tauri-nspanel`
//!   plugin takes.
//!
//! # The coordinate flip, which is the thing to get right
//!
//! Scrozz speaks [`LogicalRect`]: **origin at the top-left of the primary
//! display, y increasing downwards**, in points. That matches CoreGraphics,
//! winit, Windows and X11.
//!
//! AppKit does not. `NSRect` in screen coordinates has its **origin at the
//! bottom-left of the primary display, y increasing upwards**, and a rect's
//! `origin` is its *bottom*-left corner, not its top-left. Two things are wrong
//! at once — the axis direction and which corner is named — and fixing only one
//! produces a layout that is subtly off by exactly the height of the window,
//! which looks like a positioning bug rather than a sign error.
//!
//! The bridge is a single number: the **height of the reference display**,
//! which is `NSScreen.screens[0].frame.size.height` — the screen that owns the
//! menu bar and therefore owns AppKit's global origin. Note `frame`, not
//! `visibleFrame`: the global origin sits at the true bottom-left of the
//! primary display, below the Dock, so flipping through the work-area height
//! would shift every overlay by the Dock's height.
//!
//! ```text
//!   AppKit (bottom-left origin)          Scrozz (top-left origin)
//!   ┌───────────────────────┐  y=H       ┌───────────────────────┐  y=0
//!   │                       │            │       ┌─────┐         │
//!   │       ┌─────┐         │            │       │rect │ ← origin│
//!   │       │rect │         │            │       └─────┘         │
//!   │       └─────┘ ← origin│            │                       │
//!   └───────────────────────┘  y=0       └───────────────────────┘  y=H
//! ```
//!
//! So `logical.y = H - (appkit.y + height)`, and the inverse is the same
//! expression — the conversion is an involution, which
//! [`tests/overlay.rs`](../../tests/overlay.rs) checks directly.
//!
//! # Work area, not bounds
//!
//! Overlays anchor to [`scrozz_core::Display::work_area`], never
//! [`scrozz_core::Display::bounds`]. On macOS the work area is
//! `NSScreen.visibleFrame`, which AppKit already computes with the menu bar and
//! the Dock removed — at the Dock's current edge, current size, and honouring
//! auto-hide. Anchoring the capture stack to raw display bounds puts it
//! *behind the Dock*, which is the single most common way a bottom-anchored
//! overlay goes wrong.

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
use std::ffi::c_void;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
use scrozz_core::{Error, Result};
use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize, Opacity};

/// A rectangle in AppKit screen coordinates: **bottom-left origin, y up**.
///
/// Deliberately a plain data type with no platform dependency, so the
/// conversion to and from Scrozz's top-left [`LogicalRect`] can be tested on
/// any machine without a window server. On macOS this maps field-for-field onto
/// `NSRect`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AppKitRect {
    /// Distance from the left edge of the primary display, in points.
    pub x: f64,
    /// Distance from the **bottom** edge of the primary display, in points.
    pub y: f64,
    /// Width in points.
    pub width: f64,
    /// Height in points.
    pub height: f64,
}

impl AppKitRect {
    /// Creates a rectangle.
    #[must_use]
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Converts an AppKit screen rect to Scrozz's top-left logical space.
///
/// `reference_height` is the height of the display AppKit's global origin sits
/// on — `NSScreen.screens[0].frame.size.height`, the *full* frame and not the
/// visible frame. See the [module docs](self) for why.
#[must_use]
pub fn appkit_to_logical(rect: AppKitRect, reference_height: f64) -> LogicalRect {
    LogicalRect::new(
        LogicalPoint::new(rect.x, reference_height - rect.y - rect.height),
        LogicalSize::new(rect.width, rect.height),
    )
}

/// Converts a Scrozz logical rect to AppKit screen coordinates.
///
/// The exact inverse of [`appkit_to_logical`] for the same
/// `reference_height`; the pair is an involution.
#[must_use]
pub fn logical_to_appkit(rect: LogicalRect, reference_height: f64) -> AppKitRect {
    AppKitRect::new(
        rect.origin.x,
        reference_height - rect.origin.y - rect.size.height,
        rect.size.width,
        rect.size.height,
    )
}

/// How insistently an overlay sits above other windows.
///
/// The numeric values are AppKit's documented `NSWindowLevel` constants.
/// The natural ordering is the stacking order: a greater level is always in
/// front of a lesser one. Derived rather than written out because the variants
/// are already declared bottom-to-top, and keeping the two in sync by hand is
/// the sort of thing that quietly rots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum OverlayLevel {
    /// An ordinary window, `NSNormalWindowLevel`.
    ///
    /// The default, and deliberately so: per D27 any surface that creates a
    /// window starts here and makes always-on-top an explicit opt-in. An
    /// always-on-top window that appears before its dismissal path works makes
    /// the developer's own machine unusable.
    #[default]
    Normal,
    /// `NSFloatingWindowLevel` — roughly winit's `WindowLevel::AlwaysOnTop`.
    ///
    /// Above ordinary windows, below the menu bar. This is where capture cards
    /// and the capture dock live.
    Floating,
    /// `NSStatusWindowLevel` — the level of menu-bar extras. Still under the
    /// menu bar itself.
    Status,
    /// `NSPopUpMenuWindowLevel` — above the menu bar, below the Dock's menus.
    AboveMenuBar,
    /// `NSScreenSaverWindowLevel` — above fullscreen application content and
    /// menu-bar UI while remaining below the operating system's cursor.
    ///
    /// This is the highest level Scrozz uses for interactive content.
    /// `CGShieldingWindowLevel()` is intentionally avoided: Apple does not
    /// recommend positioning UI there and warns that it can become invisible.
    ScreenSaver,
}

/// Native cursor requested while an overlay owns pointer input.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OverlayCursor {
    /// The platform's ordinary pointer.
    #[default]
    Arrow,
    /// The platform's native region-selection crosshair.
    Crosshair,
}

/// The complete set of native properties a floating overlay needs.
///
/// [`Default`] is the conservative one — an ordinary, opaque, focus-taking
/// window — so that forgetting to configure a surface produces something
/// harmless rather than something stuck on top of the user's screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayBehavior {
    /// Stacking level.
    pub level: OverlayLevel,
    /// Whether clicks fall through to whatever is beneath.
    pub click_through: bool,
    /// Whether the overlay is visible on every Space rather than just the one
    /// it was created on (`NSWindowCollectionBehaviorCanJoinAllSpaces`).
    pub join_all_spaces: bool,
    /// Whether the overlay may appear over a fullscreen app
    /// (`NSWindowCollectionBehaviorFullScreenAuxiliary`).
    pub over_fullscreen: bool,
    /// Whether Mission Control leaves the overlay in place rather than sliding
    /// it around with the desktop (`NSWindowCollectionBehaviorStationary`).
    pub stationary: bool,
    /// Whether the overlay is skipped by Cmd-` window cycling
    /// (`NSWindowCollectionBehaviorIgnoresCycle`).
    pub ignore_cycle: bool,
    /// Whether clicking the overlay moves keyboard focus to it.
    ///
    /// This is the distinction the whole design turns on, and it has three
    /// levels rather than two:
    ///
    /// - **App activation** — Scrozz becomes the frontmost app. Refused
    ///   outright by a non-activating panel; never wanted.
    /// - **Main-window status** — the window becomes the active app's document
    ///   window. Also refused; also never wanted.
    /// - **Key status** — the window receives keystrokes. *Sometimes* wanted:
    ///   the selection overlay must read Escape and the arrow keys.
    ///
    /// `false` means the overlay takes key status only when the user clicks a
    /// control that genuinely needs typing — an `NSPanel`'s
    /// `becomesKeyOnlyIfNeeded`. A capture card wants this: clicking it to drag
    /// or annotate must not pull the insertion point out of whatever the user
    /// was writing in. `true` means the surface owns the keyboard while it is
    /// up, which is right for the selection overlay and wrong for everything
    /// else.
    pub accepts_key: bool,
    /// Whether macOS should make edge-triggered system UI unavailable.
    ///
    /// Region selection owns every screen pixel, including the Dock edge. A
    /// topmost window intercepts clicks there, but macOS still reveals an
    /// auto-hidden Dock from global pointer motion unless the active app uses
    /// `NSApplicationPresentationHideDock`.
    pub suppress_system_ui: bool,
    /// Whether the overlay hides when the app is deactivated.
    ///
    /// `false` for every Scrozz overlay: they exist precisely while the user is
    /// working in some other app.
    pub hides_on_deactivate: bool,
    /// Whether the overlay is opaque and draws its own background.
    ///
    /// `false` for every Scrozz overlay: capture cards are rounded thumbnails
    /// on empty space, and the selection overlay composites only its active
    /// selection affordances.
    pub opaque: bool,
    /// Whether the OS draws a drop shadow behind the window.
    pub has_shadow: bool,
    /// Native opacity of the complete window.
    pub opacity: Opacity,
    /// Whether other processes may capture or share this utility surface.
    ///
    /// This is true for Scrozz's card and selector chrome, never for ordinary
    /// Settings/editor windows. It supplements real visibility management; an
    /// excluded window must still be ordered out while idle.
    pub capture_excluded: bool,
    /// Whether the user can drag the window.
    ///
    /// `false` per D27: capture cards live in fixed slots because the slot *is*
    /// their meaning — position encodes recency, and letting the user scatter
    /// them destroys the ordering that makes the pile readable.
    pub movable: bool,
}

impl Default for OverlayBehavior {
    fn default() -> Self {
        Self {
            level: OverlayLevel::Normal,
            click_through: false,
            join_all_spaces: false,
            over_fullscreen: false,
            stationary: false,
            ignore_cycle: false,
            accepts_key: true,
            suppress_system_ui: false,
            hides_on_deactivate: false,
            opaque: true,
            has_shadow: true,
            opacity: Opacity::OPAQUE,
            capture_excluded: false,
            movable: true,
        }
    }
}

impl OverlayBehavior {
    /// The behaviour of a capture card or the capture dock.
    ///
    /// Floating above ordinary windows but below the menu bar, present on every
    /// Space, transparent, shadowless, and pinned to its slot.
    ///
    /// `accepts_key` is `false`: a card is clicked to drag, copy or open, and
    /// none of those need the keyboard. Taking key status would stop the user's
    /// keystrokes reaching the editor they were typing in, which is the exact
    /// interruption this whole layer exists to avoid.
    #[must_use]
    pub const fn capture_card() -> Self {
        Self {
            level: OverlayLevel::Floating,
            click_through: false,
            join_all_spaces: true,
            over_fullscreen: true,
            stationary: true,
            ignore_cycle: true,
            accepts_key: false,
            suppress_system_ui: false,
            hides_on_deactivate: false,
            opaque: false,
            has_shadow: false,
            opacity: Opacity::OPAQUE,
            capture_excluded: true,
            movable: false,
        }
    }

    /// The behavior of an overlay window while none of its UI is visible.
    ///
    /// Selection preparation and capture can leave the long-lived native window
    /// alive for several frames. Hidden empty space must fail open: no invisible
    /// Scrozz surface may intercept pointer input.
    #[must_use]
    pub const fn hidden_surface() -> Self {
        let mut behavior = Self::capture_card();
        behavior.click_through = true;
        behavior
    }

    /// The behaviour of the fullscreen selection overlay.
    ///
    /// Above the menu bar, because a selection must be able to cover it, and
    /// never click-through, because the whole surface is the click target. It
    /// is the sole Scrozz surface that covers a meaningful part of the screen,
    /// and is correspondingly momentary: it exists between the hotkey and the
    /// click, and Escape always cancels it.
    #[must_use]
    pub const fn selection_overlay() -> Self {
        Self {
            level: OverlayLevel::ScreenSaver,
            click_through: false,
            join_all_spaces: true,
            over_fullscreen: true,
            stationary: true,
            ignore_cycle: true,
            accepts_key: true,
            suppress_system_ui: true,
            hides_on_deactivate: false,
            opaque: false,
            has_shadow: false,
            opacity: Opacity::OPAQUE,
            capture_excluded: true,
            movable: false,
        }
    }

    /// The behaviour of one pinned capture window.
    ///
    /// Each pin owns a native window because click-through is a whole-window
    /// property. Durable lock state controls keyboard focus independently from
    /// transient pointer passthrough, allowing Lock and Close control islands
    /// to remain reachable over a click-through image.
    #[must_use]
    pub const fn pinned_capture(locked: bool, passthrough: bool) -> Self {
        Self {
            level: OverlayLevel::Floating,
            click_through: passthrough,
            join_all_spaces: true,
            over_fullscreen: true,
            stationary: true,
            ignore_cycle: true,
            accepts_key: !locked,
            suppress_system_ui: false,
            hides_on_deactivate: false,
            opaque: false,
            has_shadow: true,
            opacity: Opacity::OPAQUE,
            capture_excluded: true,
            movable: true,
        }
    }
}

/// Geometry of the bottom-anchored capture stack described by decision D28.
///
/// The pile is anchored to the **bottom-left of the work area** and grows
/// **upward**. Slot 0 is the bottom slot and holds the oldest card; it is also
/// the first to leave. The invariant to check an implementation against is that
/// **a card never moves upward**.
///
/// This type owns only the arithmetic. Animation, ordering and dismissal live
/// in the UI crate; what belongs here is the part that has to agree with the
/// window server about where a slot actually is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StackLayout {
    /// Size of one capture card, in points.
    pub card: LogicalSize,
    /// Gap between the stack and the top/bottom of the work area, in points.
    pub margin: f64,
    /// Gap between the stack and the left edge of the work area, in points.
    pub left_margin: f64,
    /// Vertical gap between adjacent cards, in points.
    pub gap: f64,
}

impl Default for StackLayout {
    fn default() -> Self {
        Self {
            card: LogicalSize::new(288.0, 180.0),
            margin: 2.0,
            left_margin: 40.0,
            gap: 8.0,
        }
    }
}

impl StackLayout {
    /// How many slots fit in this work area.
    ///
    /// Derived from available height rather than hard-coded, per D28, and
    /// Always at least one: a capture that produced no visible card at all would
    /// read as the app having failed, which is worse than a card that crowds the
    /// margin on a very short display.
    #[must_use]
    pub fn capacity(&self, work_area: LogicalRect) -> usize {
        let usable = work_area.size.height - 2.0 * self.margin;
        scrozz_core::layout::vertical_capacity(usable, self.card.height, self.gap)
    }

    /// The frame of one slot, in Scrozz's top-left logical space.
    ///
    /// `slot` counts upward from 0 at the bottom. Slots beyond
    /// [`Self::capacity`] are still computable — the caller decides whether a
    /// card is allowed there — so this never fails.
    #[must_use]
    pub fn slot_frame(&self, work_area: LogicalRect, slot: usize) -> LogicalRect {
        // In top-left space the bottom edge of the work area is at
        // origin.y + height, and moving *up* a slot means *subtracting* from y.
        let bottom = work_area.origin.y + work_area.size.height - self.margin;
        let pitch = self.card.height + self.gap;
        // Realistic slot indices remain exactly representable in f64.
        #[allow(clippy::cast_precision_loss)]
        let offset = slot as f64 * pitch;
        LogicalRect::new(
            LogicalPoint::new(
                work_area.origin.x + self.left_margin,
                bottom - self.card.height - offset,
            ),
            self.card,
        )
    }
}

/// Anchors a surface to the bottom-left corner of a work area.
///
/// The primitive behind [`StackLayout::slot_frame`], exposed separately because
/// the capture dock and the magnifier want the same anchor without the pile.
///
/// Callers pass [`scrozz_core::Display::work_area`], never
/// [`scrozz_core::Display::bounds`] — see the [module docs](self).
#[must_use]
pub fn anchor_bottom_left(work_area: LogicalRect, size: LogicalSize, margin: f64) -> LogicalRect {
    LogicalRect::new(
        LogicalPoint::new(
            work_area.origin.x + margin,
            work_area.origin.y + work_area.size.height - margin - size.height,
        ),
        size,
    )
}

/// What happened when an existing window was converted into a native overlay.
///
/// Returned rather than logged because the non-activating conversion is the one
/// property the design genuinely depends on, and a silent failure would look
/// like "the app steals focus sometimes" much later and much further away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayReport {
    /// Whether the window is now a non-activating panel.
    ///
    /// `false` means clicking the overlay will pull focus to Scrozz. Everything
    /// else may still have been applied.
    pub non_activating: bool,
    /// Human-readable account of what was done, or why it was refused.
    pub detail: String,
}

#[cfg(target_os = "linux")]
pub use crate::linux::x11::overlay::X11Overlay as NativeOverlay;
#[cfg(target_os = "macos")]
pub use crate::macos::overlay::MacOverlay as NativeOverlay;
#[cfg(target_os = "windows")]
pub use crate::windows::overlay::WindowsOverlay as NativeOverlay;

/// A native overlay window on a platform Scrozz does not yet retrofit.
///
/// Present so that call sites compile everywhere; every method reports
/// [`Error::Unsupported`] rather than silently doing nothing.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
#[derive(Debug)]
pub struct NativeOverlay {
    _private: (),
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl NativeOverlay {
    /// Adopts a native window handle.
    ///
    /// # Errors
    ///
    /// Always [`Error::Unsupported`] on this platform.
    ///
    /// # Safety
    ///
    /// `handle` must be a live native window or view pointer.
    pub unsafe fn adopt(handle: *mut c_void) -> Result<Self> {
        let _ = handle;
        Err(unsupported())
    }

    /// Applies overlay properties.
    ///
    /// # Errors
    ///
    /// Always [`Error::Unsupported`] on this platform.
    pub fn apply(&mut self, behavior: &OverlayBehavior) -> Result<OverlayReport> {
        let _ = behavior;
        Err(unsupported())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl crate::OverlayWindow for NativeOverlay {
    fn set_frame(&mut self, frame: LogicalRect) -> Result<()> {
        let _ = frame;
        Err(unsupported())
    }

    fn set_click_through(&mut self, passthrough: bool) -> Result<()> {
        let _ = passthrough;
        Err(unsupported())
    }
}

/// The error every not-yet-implemented platform returns.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn unsupported() -> Error {
    Error::Unsupported {
        what: "native overlay window".into(),
        why: "only the macOS overlay backend is implemented so far".into(),
    }
}
