//! The floating overlay window that hosts the capture stack.
//!
//! [`stack`](crate::stack) knows where every card goes; [`card`](crate::card)
//! knows how one is painted. This module is the window they live in, and the
//! seam the rest of the application drives them through.
//!
//! # The window
//!
//! Borderless, fully transparent outside its drawn content, no shadow of its
//! own, absent from the Dock and the taskbar, always on top, and — the property
//! that matters most — **non-activating**: clicking a capture card must not pull
//! focus out of whatever the user is typing in (D27).
//!
//! [`viewport`] builds the [`egui::ViewportBuilder`] that expresses as much of
//! that as egui can express. The rest is native, and on macOS it means
//! converting the `NSWindow` into a non-activating `NSPanel` after it exists.
//! `scrozz-ui` cannot do that itself: it does not depend on `scrozz-shell`, and
//! it is `#![forbid(unsafe_code)]`. So the conversion is a hook —
//! [`PanelHook`] — supplied by whoever owns both crates, and its result is
//! reported back through [`OverlayHandle::panel_report`].
//!
//! # Anchoring
//!
//! The overlay covers the display's **work area**, not its bounds. The
//! difference is the Dock and the menu bar: a window sized to raw bounds puts
//! the bottom slot behind the Dock, where the user cannot click it. The stack's
//! own layout then anchors slot 0 to the bottom-left of that rectangle (D28).
//!
//! # Click-through
//!
//! The overlay is a large, almost entirely empty window with a few opaque cards
//! at the bottom-left. Everything between and around them must stay clickable,
//! or the overlay is worse than no overlay at all.
//!
//! `mouse_passthrough` is per-window and all-or-nothing, so it is toggled every
//! frame from whether the pointer is inside an interactive rectangle —
//! [`passes_through`] is that decision, as a pure function, so it can be
//! asserted without a window.
//!
//! There is a platform trap here worth stating plainly. On macOS,
//! `ignoresMouseEvents` means the window receives *no* mouse events at all, so
//! once passthrough is on, egui can no longer see the pointer and can never
//! learn that it has moved back over a card. The fix is a [`PointerProbe`]: a
//! caller-supplied closure that reports the global cursor position without
//! consuming events (`NSEvent::mouseLocation` on macOS). With a probe, tracking
//! is exact. Without one, [`Passthrough::Auto`] falls back to re-sampling: it
//! drops passthrough for a single frame every [`RESAMPLE_SECS`] so hover can
//! recover. That is a bounded degradation, not a fix, and it is why the probe
//! exists.
//!
//! # Repainting
//!
//! Animations need [`egui::Context::request_repaint`] while they are in flight
//! and must stop asking the moment they are not, or the overlay pins a core
//! forever. [`CaptureStack::activity`] already reports exactly that, and
//! [`Activity::apply`](crate::motion::Activity::apply) turns it into the right
//! call — including a timed wake when something is merely waiting.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::{Pos2, Rect, Vec2};
use scrozz_core::{
    DisplayId, DisplaySet, Frame as CaptureFrame, LockEscape, LogicalSize, PinChromePolicy, PinId,
    PinState, PinnedSurface, PixelFormat, Provenance, ScaleFactor,
};

use crate::card::{self, CardAction, CardContent};
use crate::icons::{Icon, IconStore};
use crate::motion::{Motion, fade};
use crate::paint::{self, Surface};
use crate::pinned;
use crate::stack::{CaptureStack, CardId, Intent, dock};
use crate::theme::{Appearance, Radius, Theme, corner};

/// How long [`Passthrough::Auto`] waits before dropping click-through for a
/// single frame to re-sample the pointer, when no [`PointerProbe`] is supplied.
pub const RESAMPLE_SECS: f32 = 0.35;

/// Default longest edge, in pixels, of a card thumbnail.
///
/// A 6-card stack of full-resolution 5K captures is well over a gigabyte of
/// texture. Cards are 232 pt wide, so 512 px is already generous at 2×.
pub const THUMBNAIL_PX: u32 = 512;
/// Longest edge accepted for a pinned-capture GPU texture.
pub const PIN_TEXTURE_PX: u32 = 2_048;

/// Native window drags can report a new frame on every repaint. Persist only
/// after movement settles so one gesture produces one durable update.
const PIN_GEOMETRY_SETTLE_SECS: f64 = 0.2;

// ---------------------------------------------------------------------------
// Native hook
// ---------------------------------------------------------------------------

/// What the native layer did to the window.
///
/// Mirrors `scrozz_shell::OverlayReport` field for field so the application can
/// translate one into the other without inventing a third vocabulary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PanelReport {
    /// Whether the window ended up non-activating — the property that stops a
    /// click on a card stealing focus.
    pub non_activating: bool,
    /// Human-readable detail, for logs and for the diagnostics surface.
    pub detail: String,
}

impl PanelReport {
    /// A report for a platform with no native layer wired up.
    #[must_use]
    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self {
            non_activating: false,
            detail: detail.into(),
        }
    }

    /// A successful conversion.
    #[must_use]
    pub fn converted(detail: impl Into<String>) -> Self {
        Self {
            non_activating: true,
            detail: detail.into(),
        }
    }
}

/// Converts the freshly created window into a native overlay panel.
///
/// Called once, from inside the `eframe` app creator. This crate is
/// `#![forbid(unsafe_code)]` and does not depend on `scrozz-shell`, so the
/// conversion cannot live here; the application crate, which depends on both,
/// supplies it.
///
/// [`eframe::CreationContext`] implements `HasWindowHandle`, which is where the
/// platform handle comes from. On macOS the whole hook is roughly:
///
/// ```ignore
/// use raw_window_handle::{HasWindowHandle, RawWindowHandle};
/// use scrozz_shell::overlay::{OverlayBehavior, NativeOverlay};
/// use scrozz_ui::overlay_app::PanelReport;
///
/// let hook = Box::new(|cc: &eframe::CreationContext<'_>| {
///     let Ok(handle) = cc.window_handle() else {
///         return PanelReport::unsupported("no window handle");
///     };
///     let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
///         return PanelReport::unsupported("not an AppKit window");
///     };
///     // SAFETY: the view is alive for as long as the `WindowHandle` borrow.
///     let mut overlay = match unsafe {
///         NativeOverlay::from_ns_view(appkit.ns_view.as_ptr())
///     } {
///         Ok(o) => o,
///         Err(e) => return PanelReport::unsupported(e.to_string()),
///     };
///     match overlay.apply(&OverlayBehavior::capture_card()) {
///         Ok(r) if r.non_activating => PanelReport::converted(r.detail),
///         Ok(r) => PanelReport::unsupported(r.detail),
///         Err(e) => PanelReport::unsupported(e.to_string()),
///     }
/// });
/// ```
///
/// Note the entry point differs by platform: `scrozz-shell` exposes
/// `MacOverlay::from_ns_view` / `from_ns_window` on macOS but `adopt` on its
/// stub platforms, so the hook is written per-target anyway.
///
/// Returning [`PanelReport::unsupported`] is always safe: the overlay still
/// works, it just takes focus when clicked.
pub type PanelHook = Box<dyn FnOnce(&eframe::CreationContext<'_>) -> PanelReport>;

/// Reports the pointer position in the overlay window's own logical
/// coordinates, whether or not the window is currently accepting mouse events.
///
/// Required on macOS for [`Passthrough::Auto`] to track precisely; see the
/// module documentation.
pub type PointerProbe = Arc<dyn Fn() -> Option<Pos2> + Send + Sync>;

/// A fresh display/capability snapshot for pinned child windows.
#[derive(Clone, Debug, PartialEq)]
pub struct PinTopology {
    /// Connected displays in global logical coordinates.
    pub displays: DisplaySet,
    /// Display that should receive a newly-created pin.
    pub active_display: Option<DisplayId>,
    /// Native behavior available for this topology.
    pub support: PinSupport,
}

/// Queries the current display topology without caching startup state.
pub type PinTopologyProbe = Arc<dyn Fn() -> Option<PinTopology> + Send + Sync>;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How the overlay handles clicks on its empty space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Passthrough {
    /// Toggle per frame: opaque over cards, transparent everywhere else.
    #[default]
    Auto,
    /// Never pass clicks through. Correct only when the window hugs its content.
    Never,
    /// Always pass clicks through. The overlay becomes purely decorative —
    /// useful for a screenshot or a diagnostic run that must not be clickable.
    Always,
}

/// Where the overlay window sits, in the OS's logical, top-left-origin
/// coordinate space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayGeometry {
    /// The display's work area: bounds minus the menu bar and the Dock.
    pub work_area: Rect,
}

impl OverlayGeometry {
    /// A geometry covering `work_area`.
    #[must_use]
    pub const fn new(work_area: Rect) -> Self {
        Self { work_area }
    }

    /// The window's outer position.
    #[must_use]
    pub fn position(self) -> Pos2 {
        self.work_area.min
    }

    /// The window's size.
    #[must_use]
    pub fn size(self) -> Vec2 {
        self.work_area.size()
    }

    /// The work area expressed in the window's own coordinates, which is what
    /// the stack lays out in.
    #[must_use]
    pub fn local(self) -> Rect {
        Rect::from_min_size(Pos2::ZERO, self.size())
    }
}

impl Default for OverlayGeometry {
    fn default() -> Self {
        Self::new(Rect::from_min_size(Pos2::ZERO, Vec2::new(1440.0, 875.0)))
    }
}

/// Everything the overlay needs that is not the stack itself.
pub struct OverlayOptions {
    /// Where the window goes.
    pub geometry: OverlayGeometry,
    /// Light or dark.
    pub appearance: Appearance,
    /// Click-through policy.
    pub passthrough: Passthrough,
    /// Optional exact pointer source; see [`PointerProbe`].
    pub probe: Option<PointerProbe>,
    /// Optional native conversion; see [`PanelHook`].
    pub panel: Option<PanelHook>,
    /// Longest thumbnail edge in pixels.
    pub thumbnail_px: u32,
    /// Connected displays used for pin placement and restoration.
    pub displays: DisplaySet,
    /// Display that receives newly-created pins.
    pub active_display: Option<DisplayId>,
    /// Truthful native-window capabilities for pinned captures.
    pub pin_support: PinSupport,
    /// Routes registered outside a click-through pin window.
    pub pin_lock_escapes: Vec<LockEscape>,
    /// Optional live display query for pin creation and hot-plug reconciliation.
    pub pin_topology_probe: Option<PinTopologyProbe>,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        let geometry = OverlayGeometry::default();
        Self {
            geometry,
            appearance: Appearance::Dark,
            passthrough: Passthrough::default(),
            probe: None,
            panel: None,
            thumbnail_px: THUMBNAIL_PX,
            displays: DisplaySet::new(Vec::new()),
            active_display: None,
            pin_support: PinSupport::unavailable(
                "native display metrics and pin-window capabilities were not supplied",
            ),
            pin_lock_escapes: Vec::new(),
            pin_topology_probe: None,
        }
    }
}

impl std::fmt::Debug for OverlayOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayOptions")
            .field("geometry", &self.geometry)
            .field("appearance", &self.appearance)
            .field("passthrough", &self.passthrough)
            .field("probe", &self.probe.is_some())
            .field("panel", &self.panel.is_some())
            .field("thumbnail_px", &self.thumbnail_px)
            .field("displays", &self.displays)
            .field("active_display", &self.active_display)
            .field("pin_support", &self.pin_support)
            .field("pin_lock_escapes", &self.pin_lock_escapes)
            .field("pin_topology_probe", &self.pin_topology_probe.is_some())
            .finish()
    }
}

/// Capabilities of the platform adapter that hosts pinned child windows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinSupport {
    /// Whether a native child window can be created at all.
    pub windows: bool,
    /// Whether global logical positions can be read and set.
    pub positioning: bool,
    /// Whether the adapter can keep a pin above ordinary application windows.
    pub always_on_top: bool,
    /// Whether opacity is applied to the whole native window.
    pub native_opacity: bool,
    /// Whether locked pins can pass pointer input through.
    pub click_through: bool,
    /// Whether clicks avoid activating the application.
    pub non_activating: bool,
    /// Whether this session has a native child-window adoption adapter.
    pub native_adoption: bool,
    /// Whether the native child should advertise a managed X11 dock type.
    pub x11_managed_dock: bool,
    /// Human-readable capability detail for feedback and diagnostics.
    pub detail: String,
}

impl PinSupport {
    /// Explicitly unavailable pin host used by safe defaults.
    #[must_use]
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            windows: false,
            positioning: false,
            always_on_top: false,
            native_opacity: false,
            click_through: false,
            non_activating: false,
            native_adoption: false,
            x11_managed_dock: false,
            detail: detail.into(),
        }
    }

    /// Portable egui/winit behavior used by tests and non-specialized hosts.
    #[must_use]
    pub fn portable() -> Self {
        Self {
            windows: true,
            positioning: true,
            always_on_top: true,
            native_opacity: false,
            click_through: true,
            non_activating: false,
            native_adoption: false,
            x11_managed_dock: false,
            detail: "portable child viewport; native adoption not reported".into(),
        }
    }

    fn limitation_notice(&self) -> Option<String> {
        (!self.positioning || !self.always_on_top || !self.non_activating)
            .then(|| self.detail.clone())
    }
}

// ---------------------------------------------------------------------------
// The public seam
// ---------------------------------------------------------------------------

/// A capture handed to the overlay.
#[derive(Clone, Debug)]
pub struct CaptureRequest {
    /// Durable capture identity used by persistence when this becomes a pin.
    pub pin_id: Option<PinId>,
    /// File name shown in the caption.
    pub name: String,
    /// Where the pixels came from. Decides the chrome (D9).
    pub provenance: Provenance,
    /// The capture's own pixel dimensions, shown in the caption.
    pub source_px: (u32, u32),
    /// Source pixels per logical point.
    pub source_scale: f64,
    /// A pre-scaled thumbnail. `None` shows a holding fill until one arrives.
    pub thumbnail: Option<egui::ColorImage>,
    /// Explicit content failure for a durable pin whose state remains recoverable.
    pub content_error: Option<String>,
}

impl CaptureRequest {
    /// A request with no thumbnail yet.
    #[must_use]
    pub fn new(name: impl Into<String>, provenance: Provenance, source_px: (u32, u32)) -> Self {
        Self {
            pin_id: None,
            name: name.into(),
            provenance,
            source_px,
            source_scale: 1.0,
            thumbnail: None,
            content_error: None,
        }
    }

    /// Build a request from a captured frame, scaling a thumbnail from it.
    ///
    /// Returns `None` when the frame is malformed, rather than panicking deep
    /// inside a texture upload.
    #[must_use]
    pub fn from_frame(
        name: impl Into<String>,
        provenance: Provenance,
        frame: &CaptureFrame,
        max_edge: u32,
    ) -> Option<Self> {
        let source_px = (frame.width(), frame.height());
        let thumbnail = thumbnail(frame, max_edge)?;
        Some(Self {
            pin_id: None,
            name: name.into(),
            provenance,
            source_px,
            source_scale: frame.scale.get(),
            thumbnail: Some(thumbnail),
            content_error: None,
        })
    }

    /// Attach a thumbnail.
    #[must_use]
    pub fn with_thumbnail(mut self, image: egui::ColorImage) -> Self {
        self.thumbnail = downscale(&image, PIN_TEXTURE_PX);
        self
    }

    /// Assign the authoritative capture identity used by pin persistence.
    #[must_use]
    pub fn with_pin_id(mut self, id: impl Into<PinId>) -> Self {
        self.pin_id = Some(id.into());
        self
    }

    /// Override source pixels per logical point.
    #[must_use]
    pub fn with_source_scale(mut self, scale: f64) -> Self {
        self.source_scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        self
    }

    /// Mark this request as a recoverable pin whose source pixels are unavailable.
    #[must_use]
    pub fn with_content_error(mut self, error: impl Into<String>) -> Self {
        self.content_error = Some(error.into());
        self
    }
}

/// Why a card left the pile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DismissReason {
    /// Swiped left.
    Swipe,
    /// Dragged out onto another application.
    DragOut,
    /// The close button.
    Closed,
    /// Copy or Save, both of which retire the card (D12).
    Acted,
    /// The pile was full and the oldest retired to make room (D28).
    Overflow,
    /// The application asked.
    Programmatic,
}

/// Something the user did to the overlay.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum OverlayEvent {
    /// A capture entered the pile.
    Pushed {
        /// The new card.
        id: CardId,
    },
    /// A card left the pile.
    Dismissed {
        /// The card that left.
        id: CardId,
        /// Why.
        reason: DismissReason,
    },
    /// Copy this capture to the clipboard.
    CopyRequested {
        /// The card.
        id: CardId,
    },
    /// Save this capture to disk.
    SaveRequested {
        /// The card.
        id: CardId,
    },
    /// Open this capture in the annotation editor.
    AnnotateRequested {
        /// The card.
        id: CardId,
    },
    /// Upload this capture and produce a link.
    UploadRequested {
        /// The card.
        id: CardId,
    },
    /// Pin this capture so overflow will not retire it.
    PinRequested {
        /// Source card.
        id: CardId,
        /// Durable capture identity.
        pin: PinId,
        /// Initial durable geometry and presentation state.
        state: PinState,
    },
    /// A pin's durable geometry or presentation changed.
    PinUpdated {
        /// Durable capture identity.
        pin: PinId,
        /// Current durable state.
        state: PinState,
    },
    /// A pin was closed and should no longer restore.
    PinClosed {
        /// Durable capture identity.
        pin: PinId,
    },
    /// A requested pin could not be created.
    PinUnavailable {
        /// Source card identity.
        card: CardId,
        /// Truthful platform reason.
        reason: String,
    },
    /// The compositor cannot honor or report explicit pin positioning.
    PinPositioningUnavailable {
        /// Durable capture identity.
        pin: PinId,
        /// Truthful platform reason.
        reason: String,
    },
    /// A drag began on a card. The host starts the platform drag here.
    DragStarted {
        /// The card.
        id: CardId,
        /// Where the pointer was, in window coordinates.
        at: Pos2,
    },
    /// A drag committed to leaving the pile.
    DragOut {
        /// The card.
        id: CardId,
        /// Where it was released, in window coordinates.
        at: Pos2,
    },
    /// The pile collapsed into the dock (D20).
    DockCollapsed,
    /// The pile came back out of the dock.
    DockExpanded,
    /// The last card left; the overlay can be hidden.
    Emptied,
}

/// Something the application asks the overlay to do.
#[derive(Clone, Debug)]
enum Command {
    Dismiss(CardId),
    DismissAll,
    Collapse,
    Expand,
    ToggleDock,
    RestorePin {
        request: CaptureRequest,
        state: PinState,
    },
    RefreshPinTexture {
        pin: PinId,
        image: egui::ColorImage,
    },
    CommitPin(PinId),
    FailPin {
        pin: PinId,
        reason: String,
    },
    NativePinFailure {
        pin: PinId,
        reason: String,
    },
    DiscardPin(PinId),
    ClosePin(PinId),
    UnlockPins,
    Close,
}

/// Native state the host must apply to one child viewport.
#[derive(Clone, Debug, PartialEq)]
pub struct NativePinRequest {
    /// Stable pin identity for native failure feedback.
    pub pin: PinId,
    /// Exact unique title used to discover the native child window.
    pub title: String,
    /// Durable state to apply.
    pub state: PinState,
    /// Whether native code may set global geometry in this session.
    pub positioning: bool,
    /// Authoritative destination-display pixels per logical point.
    pub display_scale: ScaleFactor,
    /// Whether this pin may carry a native shadow.
    pub shadow: bool,
}

#[derive(Default)]
struct Shared {
    inbox: Mutex<Vec<CaptureRequest>>,
    outbox: Mutex<Vec<OverlayEvent>>,
    commands: Mutex<Vec<Command>>,
    ctx: Mutex<Option<egui::Context>>,
    report: Mutex<Option<PanelReport>>,
    native_pins: Mutex<Vec<NativePinRequest>>,
}

/// The application's grip on a running overlay.
///
/// Cheap to clone, safe to hold on another thread, and usable *before* the
/// window exists: a hotkey handler can be wired to a handle at start-up and the
/// first capture pushed through it will be waiting when the window opens.
#[derive(Clone, Default)]
pub struct OverlayHandle {
    shared: Arc<Shared>,
}

impl OverlayHandle {
    /// A handle not yet bound to a window.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Show a capture. Returns immediately; the card appears on the next frame.
    pub fn push(&self, request: CaptureRequest) {
        if let Ok(mut q) = self.shared.inbox.lock() {
            q.push(request);
        }
        self.wake();
    }

    /// Take everything that has happened since the last call.
    #[must_use]
    pub fn drain_events(&self) -> Vec<OverlayEvent> {
        self.shared
            .outbox
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }

    /// Retire one card.
    pub fn dismiss(&self, id: CardId) {
        self.command(Command::Dismiss(id));
    }

    /// Retire every card.
    pub fn dismiss_all(&self) {
        self.command(Command::DismissAll);
    }

    /// Collapse the pile into the dock (D20).
    pub fn collapse(&self) {
        self.command(Command::Collapse);
    }

    /// Bring the pile back out of the dock.
    pub fn expand(&self) {
        self.command(Command::Expand);
    }

    /// Collapse or expand, whichever is not current.
    pub fn toggle_dock(&self) {
        self.command(Command::ToggleDock);
    }

    /// Restore a persisted pin into its own native child viewport.
    pub fn restore_pin(&self, request: CaptureRequest, state: PinState) {
        self.command(Command::RestorePin { request, state });
    }

    /// Replace a live pin's pixels without touching its newer UI-owned state.
    pub fn refresh_pin_texture(&self, pin: impl Into<PinId>, image: egui::ColorImage) {
        self.command(Command::RefreshPinTexture {
            pin: pin.into(),
            image,
        });
    }

    /// Commit a provisional pin after durable persistence succeeds.
    pub fn commit_pin(&self, id: impl Into<PinId>) {
        self.command(Command::CommitPin(id.into()));
    }

    /// Roll a provisional pin back to its source card and explain why.
    pub fn fail_pin(&self, id: impl Into<PinId>, reason: impl Into<String>) {
        self.command(Command::FailPin {
            pin: id.into(),
            reason: reason.into(),
        });
    }

    /// Surface a platform adapter failure inside the affected pin.
    pub fn native_pin_failure(&self, id: impl Into<PinId>, reason: impl Into<String>) {
        self.command(Command::NativePinFailure {
            pin: id.into(),
            reason: reason.into(),
        });
    }

    /// Roll back a new pin without emitting a second persistence mutation.
    pub fn discard_pin(&self, id: impl Into<PinId>) {
        self.command(Command::DiscardPin(id.into()));
    }

    /// Close one pin and clear its durable pinned state.
    pub fn close_pin(&self, id: impl Into<PinId>) {
        self.command(Command::ClosePin(id.into()));
    }

    /// Unlock every click-through pin through the required external escape.
    pub fn unlock_pins(&self) {
        self.command(Command::UnlockPins);
    }

    /// Current child-window descriptors for native host reconciliation.
    #[must_use]
    pub fn native_pin_requests(&self) -> Vec<NativePinRequest> {
        self.shared
            .native_pins
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }

    /// Ask the overlay window to close.
    pub fn close(&self) {
        self.command(Command::Close);
    }

    /// What the native layer reported, once the window exists.
    #[must_use]
    pub fn panel_report(&self) -> Option<PanelReport> {
        self.shared.report.lock().ok().and_then(|r| r.clone())
    }

    /// Whether a window has bound itself to this handle yet.
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.shared.ctx.lock().map(|c| c.is_some()).unwrap_or(false)
    }

    /// Ask the overlay to draw a frame, if it is running.
    pub fn wake(&self) {
        if let Ok(ctx) = self.shared.ctx.lock()
            && let Some(ctx) = ctx.as_ref()
        {
            ctx.request_repaint();
        }
    }

    fn command(&self, cmd: Command) {
        if let Ok(mut q) = self.shared.commands.lock() {
            q.push(cmd);
        }
        self.wake();
    }
}

impl std::fmt::Debug for OverlayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayHandle")
            .field("attached", &self.is_attached())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Window construction
// ---------------------------------------------------------------------------

/// The viewport for a capture overlay.
///
/// Every property here is one the overlay depends on, and every one of them is
/// a hint the window manager is free to ignore — which is why the macOS path
/// also converts the window natively, and why [`PanelReport`] exists to say
/// whether that worked.
#[must_use]
pub fn viewport(geometry: OverlayGeometry) -> egui::ViewportBuilder {
    let builder = egui::ViewportBuilder::default()
        .with_title("Scrozz Overlay")
        .with_app_id("com.scrozz.overlay")
        .with_position(geometry.position())
        .with_inner_size(geometry.size())
        .with_decorations(false)
        .with_transparent(true)
        .with_has_shadow(false)
        .with_taskbar(false)
        .with_resizable(false)
        .with_drag_and_drop(true)
        .with_always_on_top()
        // Do not take focus when the window opens. On macOS the real guarantee
        // is the `NSPanel` conversion; this is the portable half of it.
        .with_active(false);

    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
    let builder = builder
        // X11 hints. `Dock` keeps it off the pager and above normal windows;
        // override-redirect takes it out of the window manager's hands
        // altogether, which is what stops a tiling WM from reparenting it.
        .with_window_type(egui::X11WindowType::Dock)
        .with_override_redirect(true);

    builder
}

/// [`eframe::NativeOptions`] for a capture overlay.
#[must_use]
pub fn native_options(geometry: OverlayGeometry) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: viewport(geometry),
        // The overlay is transient: nothing about it is worth restoring, and a
        // restored position would fight the work-area anchor.
        persist_window: false,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Click-through
// ---------------------------------------------------------------------------

/// Whether the window should pass clicks through, given where the pointer is.
///
/// Two rules, and the asymmetry between them is deliberate.
///
/// **An empty overlay always passes through.** There is nothing to click, and
/// the entire desktop is underneath. This holds even when the pointer is
/// unknown, because it is trivially recoverable: the moment a card exists,
/// `hits` is non-empty and the second rule takes over.
///
/// **With something to hit, an unknown pointer never passes through.** Guessing
/// "yes" makes the overlay permanently invisible to the mouse on a platform
/// that stops delivering pointer events under passthrough — macOS sets
/// `ignoresMouseEvents`, so egui never learns the pointer came back — and there
/// is then no way out of it. Eating clicks for one frame is recoverable;
/// becoming untouchable is not.
#[must_use]
pub fn passes_through(pointer: Option<Pos2>, hits: &[Rect]) -> bool {
    if hits.is_empty() {
        return true;
    }
    pointer.is_some_and(|p| !hits.iter().any(|r| r.contains(p)))
}

// ---------------------------------------------------------------------------
// Thumbnails
// ---------------------------------------------------------------------------

/// Convert a captured frame into an egui image, honouring stride and
/// premultiplication.
///
/// Both matter. A padded stride read as tightly packed shears the image; a
/// premultiplied buffer treated as straight alpha puts a black halo around
/// exactly the rounded window corners D9 is about.
#[must_use]
pub fn color_image(frame: &CaptureFrame) -> Option<egui::ColorImage> {
    if !frame.is_well_formed() {
        return None;
    }
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    if w == 0
        || h == 0
        || w > PIN_TEXTURE_PX as usize
        || h > PIN_TEXTURE_PX as usize
        || w.checked_mul(h)? > (PIN_TEXTURE_PX as usize).checked_pow(2)?
    {
        return None;
    }
    let bpp = frame.format.bytes_per_pixel();
    let premultiplied = frame.format.is_premultiplied();
    let swap = matches!(
        frame.format,
        PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8
    );

    let mut pixels = Vec::with_capacity(w.checked_mul(h)?);
    for y in 0..h {
        let row = &frame.data[y * frame.stride..y * frame.stride + w * bpp];
        for px in row.chunks_exact(bpp) {
            let (r, g, b, a) = if swap {
                (px[2], px[1], px[0], px[3])
            } else {
                (px[0], px[1], px[2], px[3])
            };
            pixels.push(if premultiplied {
                egui::Color32::from_rgba_premultiplied(r, g, b, a)
            } else {
                egui::Color32::from_rgba_unmultiplied(r, g, b, a)
            });
        }
    }
    Some(egui::ColorImage::new([w, h], pixels))
}

/// A downscaled thumbnail of a captured frame, longest edge at most `max_edge`.
///
/// Box-filtered rather than nearest: a nearest-sampled 5K screenshot at card
/// size is a field of aliasing artefacts and reads as a broken thumbnail.
#[must_use]
pub fn thumbnail(frame: &CaptureFrame, max_edge: u32) -> Option<egui::ColorImage> {
    if !frame.is_well_formed() || max_edge == 0 || max_edge > PIN_TEXTURE_PX {
        return None;
    }
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let max = max_edge as usize;
    let longest = w.max(h);
    let scaled = |value: usize| {
        value
            .checked_mul(max)
            .and_then(|value| value.checked_add(longest / 2))
            .map(|value| (value / longest).max(1))
    };
    let (nw, nh) = if longest <= max {
        (w, h)
    } else {
        (scaled(w)?, scaled(h)?)
    };
    let output_len = nw.checked_mul(nh)?;
    if output_len > (PIN_TEXTURE_PX as usize).checked_pow(2)? {
        return None;
    }

    let bpp = frame.format.bytes_per_pixel();
    let premultiplied = frame.format.is_premultiplied();
    let swap = matches!(
        frame.format,
        PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8
    );
    let mut output = Vec::with_capacity(output_len);
    for y in 0..nh {
        let (y0, y1) = box_span(y, nh, h)?;
        for x in 0..nw {
            let (x0, x1) = box_span(x, nw, w)?;
            let mut colour = [0u128; 3];
            let mut alpha = 0u128;
            let mut count = 0u128;
            for sy in y0..y1 {
                let row_start = sy.checked_mul(frame.stride)?;
                for sx in x0..x1 {
                    let start = row_start.checked_add(sx.checked_mul(bpp)?)?;
                    let px = frame.data.get(start..start.checked_add(bpp)?)?;
                    let channels = if swap {
                        [px[2], px[1], px[0], px[3]]
                    } else {
                        [px[0], px[1], px[2], px[3]]
                    };
                    let a = u128::from(channels[3]);
                    for (sum, channel) in colour.iter_mut().zip(&channels[..3]) {
                        // Premultiplied samples are already alpha-weighted;
                        // straight ones have to be weighted here, or a
                        // transparent pixel's black drags a fringe into the
                        // window edge D9 exists to protect.
                        *sum += u128::from(*channel) * if premultiplied { 1 } else { a };
                    }
                    alpha += a;
                    count += 1;
                }
            }
            let divisor = if premultiplied { count } else { alpha };
            let channels = colour.map(|sum| {
                match (sum + divisor / 2).checked_div(divisor) {
                    // No coverage at all: there is no colour to report, and
                    // inventing one is how a dark halo gets into the image.
                    None => 0,
                    Some(mean) => u8::try_from(mean).unwrap_or(u8::MAX),
                }
            });
            let mean_alpha = u8::try_from((alpha + count / 2) / count).unwrap_or(u8::MAX);
            output.push(if premultiplied {
                egui::Color32::from_rgba_premultiplied(
                    channels[0],
                    channels[1],
                    channels[2],
                    mean_alpha,
                )
            } else {
                egui::Color32::from_rgba_unmultiplied(
                    channels[0],
                    channels[1],
                    channels[2],
                    mean_alpha,
                )
            });
        }
    }
    Some(egui::ColorImage::new([nw, nh], output))
}

/// The half-open source span one destination pixel covers.
///
/// Both ends floor, so consecutive spans tile the source exactly: every source
/// pixel lands in exactly one destination pixel, and the final span always ends
/// on `source`. A ceiling end would overlap neighbours and leave the outermost
/// row and column supported differently from every other one — visible on a
/// window capture as a softened, mis-weighted edge, which is the one part of a
/// window capture D9 says must survive intact.
fn box_span(index: usize, out: usize, source: usize) -> Option<(usize, usize)> {
    let start = index.checked_mul(source)? / out;
    let end = ((index + 1).checked_mul(source)? / out).min(source);
    Some((start, end.max(start + 1)))
}

/// Box-filter an image down so its longest edge is at most `max_edge`.
#[must_use]
pub fn downscale(image: &egui::ColorImage, max_edge: u32) -> Option<egui::ColorImage> {
    let (w, h) = (image.size[0], image.size[1]);
    if w == 0
        || h == 0
        || max_edge == 0
        || max_edge > PIN_TEXTURE_PX
        || image.pixels.len() != w.checked_mul(h)?
    {
        return None;
    }
    let max = max_edge as usize;
    if w <= max && h <= max {
        return Some(image.clone());
    }
    let scale = max as f32 / w.max(h) as f32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (nw, nh) = (
        ((w as f32 * scale).round() as usize).max(1),
        ((h as f32 * scale).round() as usize).max(1),
    );

    let mut out = Vec::with_capacity(nw.checked_mul(nh)?);
    for y in 0..nh {
        let (y0, y1) = box_span(y, nh, h)?;
        for x in 0..nw {
            let (x0, x1) = box_span(x, nw, w)?;
            // Average in premultiplied space, which is where `Color32` already
            // lives — averaging straight alpha would darken translucent edges.
            let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = image.pixels[sy * w + sx];
                    r += u32::from(p.r());
                    g += u32::from(p.g());
                    b += u32::from(p.b());
                    a += u32::from(p.a());
                    n += 1;
                }
            }
            let n = n.max(1);
            #[allow(clippy::cast_possible_truncation)]
            out.push(egui::Color32::from_rgba_premultiplied(
                (r / n) as u8,
                (g / n) as u8,
                (b / n) as u8,
                (a / n) as u8,
            ));
        }
    }
    Some(egui::ColorImage::new([nw, nh], out))
}

// ---------------------------------------------------------------------------
// The application
// ---------------------------------------------------------------------------

struct Entry {
    name: String,
    provenance: Provenance,
    source_px: (u32, u32),
    pin_id: Option<PinId>,
    source_scale: f64,
    texture: Option<egui::TextureHandle>,
    pending: Option<egui::ColorImage>,
    pin_notice: Option<String>,
}

/// On-screen ownership of pin identities.
///
/// Pin lifecycle commands are asynchronous, and two of them race. A restore can
/// be issued by the host while this overlay is closing the same pin, because
/// the host is told about a close only after the window has already gone. If
/// whichever command is applied last decided the outcome, one ordering would
/// put back a pin the user dismissed and whose durable state is being cleared —
/// a pin that reappears and cannot be explained.
///
/// The overlay owns the on-screen lifetime, so it settles the race rather than
/// leaving it to arrival order: an identity it has retired stays retired until
/// a deliberate new pin revives it. Kept pure so every ordering is testable
/// without a window server.
///
/// The retired set grows with the number of pins closed in one session and is
/// never evicted, which is deliberate: dropping a retirement is precisely what
/// would let a stale restore land, and the cost of keeping one is a capture id.
/// A session would have to close tens of thousands of pins before that were
/// measurable, and it ends with the process.
#[derive(Debug, Default)]
struct PinLifetimes {
    retired: HashSet<PinId>,
}

/// Why a queued pin restore no longer describes reality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaleRestore {
    /// This overlay closed the pin after the restore was issued.
    Retired,
    /// A live pin already owns the identity, and its geometry is newer.
    AlreadyOnScreen,
}

impl StaleRestore {
    const fn reason(self) -> &'static str {
        match self {
            Self::Retired => "this overlay closed the same pin after the restore was issued",
            Self::AlreadyOnScreen => "the pin is already on screen with newer geometry",
        }
    }
}

impl PinLifetimes {
    /// A deliberate new pin starts a fresh lifetime for an identity.
    fn opened(&mut self, id: &PinId) {
        self.retired.remove(id);
    }

    /// The pin left the screen, so later restores of it are stale.
    fn retired(&mut self, id: &PinId) {
        self.retired.insert(id.clone());
    }

    /// Whether a queued restore may still open a window.
    fn admits_restore(&self, id: &PinId, on_screen: bool) -> Result<(), StaleRestore> {
        if self.retired.contains(id) {
            Err(StaleRestore::Retired)
        } else if on_screen {
            Err(StaleRestore::AlreadyOnScreen)
        } else {
            Ok(())
        }
    }
}

struct PinnedEntry {
    name: String,
    surface: PinnedSurface,
    texture: Option<egui::TextureHandle>,
    pending: Option<egui::ColorImage>,
    content_error: Option<String>,
    positioning_notice: Option<String>,
    native_frame_changed_at: Option<f64>,
}

impl PinnedEntry {
    fn from_request(
        request: CaptureRequest,
        surface: PinnedSurface,
        limitation_notice: Option<String>,
    ) -> Self {
        Self {
            name: request.name,
            surface,
            texture: None,
            pending: request.thumbnail,
            content_error: request.content_error,
            positioning_notice: limitation_notice,
            native_frame_changed_at: None,
        }
    }
}

/// The `eframe` application that hosts the capture stack.
pub struct OverlayApp {
    stack: CaptureStack,
    content: HashMap<CardId, Entry>,
    pins: HashMap<PinId, PinnedEntry>,
    pin_lifetimes: PinLifetimes,
    pending_pin_cards: HashMap<PinId, CardId>,
    pinned_cards: HashSet<CardId>,
    handle: OverlayHandle,
    theme: Theme,
    icons: IconStore,
    geometry: OverlayGeometry,
    passthrough: Passthrough,
    probe: Option<PointerProbe>,
    thumbnail_px: u32,
    displays: DisplaySet,
    active_display: Option<DisplayId>,
    pin_support: PinSupport,
    pin_lock_escapes: Vec<LockEscape>,
    pin_topology_probe: Option<PinTopologyProbe>,
    last_pin_topology_refresh: f64,
    /// The value most recently sent to the window, so the command is sent on
    /// change rather than every frame.
    passthrough_now: bool,
    /// When the pointer was last actually known, for re-sampling.
    last_seen: f64,
    hovered: Option<CardId>,
    dock_collapsed: bool,
    dragging: Option<CardId>,
}

impl OverlayApp {
    /// Build the overlay.
    ///
    /// Installs fonts and the style, uploads the icon set, binds `handle` to
    /// this window, and runs the native [`PanelHook`] if one was supplied.
    #[must_use]
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        handle: OverlayHandle,
        mut options: OverlayOptions,
    ) -> Self {
        let ctx = &cc.egui_ctx;
        let theme = Theme::for_appearance(options.appearance);
        crate::theme::install_fonts(ctx);
        crate::theme::install_style(ctx, &theme);

        if let Ok(mut slot) = handle.shared.ctx.lock() {
            *slot = Some(ctx.clone());
        }

        let report = options.panel.take().map_or_else(
            || PanelReport::unsupported("no native panel hook supplied"),
            |hook| hook(cc),
        );
        if !report.non_activating {
            tracing::warn!(detail = %report.detail, "overlay window is not non-activating");
        }
        if let Ok(mut slot) = handle.shared.report.lock() {
            *slot = Some(report);
        }

        if options.passthrough == Passthrough::Auto && options.probe.is_none() {
            tracing::warn!(
                "no pointer probe: click-through will re-sample every {RESAMPLE_SECS}s, \
                 which is imprecise on platforms that stop delivering pointer events \
                 to a click-through window"
            );
        }

        Self {
            stack: CaptureStack::for_work_area(options.geometry.local()),
            content: HashMap::new(),
            pins: HashMap::new(),
            pin_lifetimes: PinLifetimes::default(),
            pending_pin_cards: HashMap::new(),
            pinned_cards: HashSet::new(),
            handle,
            theme,
            icons: IconStore::new(ctx),
            geometry: options.geometry,
            passthrough: options.passthrough,
            probe: options.probe,
            thumbnail_px: options.thumbnail_px.max(1),
            displays: options.displays,
            active_display: options.active_display,
            pin_support: options.pin_support,
            pin_lock_escapes: options.pin_lock_escapes,
            pin_topology_probe: options.pin_topology_probe,
            last_pin_topology_refresh: f64::NEG_INFINITY,
            passthrough_now: false,
            last_seen: 0.0,
            hovered: None,
            dock_collapsed: false,
            dragging: None,
        }
    }

    /// The stack this overlay is showing, for tests and diagnostics.
    #[must_use]
    pub fn stack(&self) -> &CaptureStack {
        &self.stack
    }

    /// The theme in use.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Where the window is.
    #[must_use]
    pub fn geometry(&self) -> OverlayGeometry {
        self.geometry
    }

    /// Move the overlay to a new work area, e.g. after a display change.
    pub fn set_geometry(&mut self, geometry: OverlayGeometry, ctx: &egui::Context, m: &Motion) {
        if geometry == self.geometry {
            return;
        }
        self.geometry = geometry;
        self.stack.resize(geometry.local(), m);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(geometry.position()));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(geometry.size()));
    }

    /// Emits authoritative pin state immediately before the host shuts down.
    ///
    /// This bypasses the movement-settle debounce and observes close requests
    /// already present in the current raw input, so neither the latest native
    /// position nor a close gesture is lost when the worker stops this frame.
    pub fn flush_pin_states(&mut self, ctx: &egui::Context) {
        let zoom_factor = ctx.zoom_factor();
        let mut ids: Vec<PinId> = self.pins.keys().cloned().collect();
        ids.sort_by(|left, right| left.0.cmp(&right.0));
        let mut changed = Vec::new();
        let mut closed = Vec::new();
        for id in ids {
            let viewport =
                ctx.input(|input| input.raw.viewports.get(&pin_viewport_id(&id)).cloned());
            if viewport
                .as_ref()
                .is_some_and(egui::ViewportInfo::close_requested)
            {
                closed.push(id);
                continue;
            }
            let Some(entry) = self.pins.get_mut(&id) else {
                continue;
            };
            if self.pin_support.positioning
                && let Some(rect) = viewport.and_then(|info| info.outer_rect)
            {
                entry.surface.sync_native_frame(
                    pinned::native_logical_rect(rect, zoom_factor),
                    &self.displays,
                );
            }
            entry.native_frame_changed_at = None;
            changed.push((id, entry.surface.state().clone()));
        }
        for (pin, state) in changed {
            self.emit(OverlayEvent::PinUpdated { pin, state });
        }
        for pin in closed {
            self.close_pin(ctx, &pin);
        }
    }

    fn refresh_pin_topology(&mut self) {
        let topology = self.pin_topology_probe.as_ref().and_then(|probe| probe());
        let Some(topology) = topology else {
            return;
        };
        if topology.displays.displays().is_empty() {
            return;
        }
        let limitation_notice = topology.support.limitation_notice();
        self.displays = topology.displays;
        self.active_display = topology.active_display;
        self.pin_support = topology.support;

        let mut changed = Vec::new();
        for (id, entry) in &mut self.pins {
            let before = entry.surface.state().clone();
            let _ = entry.surface.reconcile(&self.displays);
            entry.positioning_notice = limitation_notice.clone();
            if entry.surface.state() != &before {
                entry.native_frame_changed_at = None;
                changed.push((id.clone(), entry.surface.state().clone()));
            }
        }
        for (pin, state) in changed {
            self.emit(OverlayEvent::PinUpdated { pin, state });
        }
    }

    fn refresh_pin_topology_if_due(&mut self, now: f64) {
        const REFRESH_SECONDS: f64 = 1.0;
        if now - self.last_pin_topology_refresh < REFRESH_SECONDS {
            return;
        }
        self.last_pin_topology_refresh = now;
        self.refresh_pin_topology();
    }

    fn emit(&self, event: OverlayEvent) {
        if let Ok(mut q) = self.handle.shared.outbox.lock() {
            q.push(event);
        }
        self.handle.wake();
    }

    fn take_inbox(&self) -> Vec<CaptureRequest> {
        self.handle
            .shared
            .inbox
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }

    fn take_commands(&self) -> Vec<Command> {
        self.handle
            .shared
            .commands
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }

    fn ingest(&mut self, m: &Motion) {
        for request in self.take_inbox() {
            let id = self.stack.push(m);
            let thumb = request
                .thumbnail
                .and_then(|image| downscale(&image, self.thumbnail_px));
            self.content.insert(
                id,
                Entry {
                    name: request.name,
                    provenance: request.provenance,
                    source_px: request.source_px,
                    pin_id: request.pin_id,
                    source_scale: request.source_scale,
                    texture: None,
                    pending: thumb,
                    pin_notice: request.content_error,
                },
            );
            self.emit(OverlayEvent::Pushed { id });
        }
    }

    fn run_commands(&mut self, ctx: &egui::Context, m: &Motion) {
        for cmd in self.take_commands() {
            match cmd {
                Command::Dismiss(id) => {
                    if self.stack.dismiss(id, m) {
                        self.emit(OverlayEvent::Dismissed {
                            id,
                            reason: DismissReason::Programmatic,
                        });
                    }
                }
                Command::DismissAll => {
                    let ids: Vec<CardId> = self.stack.cards().iter().map(|c| c.id()).collect();
                    self.stack.dismiss_all(m);
                    for id in ids {
                        self.emit(OverlayEvent::Dismissed {
                            id,
                            reason: DismissReason::Programmatic,
                        });
                    }
                }
                Command::Collapse => self.stack.collapse(m),
                Command::Expand => self.stack.expand(m),
                Command::ToggleDock => self.stack.toggle_dock(m),
                Command::RestorePin { request, state } => {
                    self.restore_pin(request, state);
                }
                Command::RefreshPinTexture { pin, image } => {
                    if let Some(entry) = self.pins.get_mut(&pin) {
                        match downscale(&image, PIN_TEXTURE_PX) {
                            Some(image) => {
                                entry.pending = Some(image);
                                // Pixels arrived, so whatever explained their
                                // absence has stopped being true.
                                entry.content_error = None;
                            }
                            // A pin already showing a bounded preview keeps it:
                            // a smaller correct image beats an error page, and
                            // the host reports the refresh failure as a note.
                            // A pin with nothing to draw has to say so instead.
                            None if entry.texture.is_none() && entry.pending.is_none() => {
                                entry.content_error = Some(
                                    "The refreshed pin texture exceeded safe GPU limits.".into(),
                                );
                            }
                            None => {}
                        }
                    }
                }
                Command::CommitPin(id) => self.commit_pin(&id, m),
                Command::FailPin { pin, reason } => self.fail_pin(ctx, &pin, reason),
                Command::NativePinFailure { pin, reason } => {
                    if let Some(entry) = self.pins.get_mut(&pin) {
                        entry.positioning_notice = Some(reason);
                    }
                }
                Command::DiscardPin(id) => self.discard_pin(ctx, &id),
                Command::ClosePin(id) => self.close_pin(ctx, &id),
                Command::UnlockPins => self.unlock_pins(ctx),
                Command::Close => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            }
        }
    }

    /// Upload any thumbnails that arrived, and drop textures for cards that are
    /// entirely gone.
    fn reconcile(&mut self, ctx: &egui::Context) {
        let live: std::collections::HashSet<CardId> = self
            .stack
            .cards()
            .iter()
            .map(|c| c.id())
            .chain(self.stack.departing().iter().map(|d| d.id()))
            .collect();

        let mut gone: Vec<CardId> = self
            .content
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        gone.sort_unstable_by_key(|id| id.0);
        for id in gone {
            self.content.remove(&id);
            if !self.pinned_cards.remove(&id) {
                self.emit(OverlayEvent::Dismissed {
                    id,
                    reason: DismissReason::Overflow,
                });
            }
        }

        for (id, entry) in &mut self.content {
            if let Some(image) = entry.pending.take() {
                entry.texture = Some(ctx.load_texture(
                    format!("scrozz.card.{}", id.0),
                    image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }

        for (id, entry) in &mut self.pins {
            if let Some(image) = entry.pending.take() {
                entry.texture = Some(ctx.load_texture(
                    format!("scrozz.pin.{}", id.0),
                    image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
    }

    fn begin_pin(&mut self, card: CardId, _m: &Motion) {
        self.refresh_pin_topology();
        if !self.pin_support.windows {
            self.reject_pin(card, self.pin_support.detail.clone());
            return;
        }
        let Some(source) = self.content.get(&card) else {
            return;
        };
        if source.texture.is_none() && source.pending.is_none() {
            self.reject_pin(
                card,
                "Capture pixels are unavailable, so Pin to Screen kept the source card.",
            );
            return;
        }
        let Some(pin) = source.pin_id.clone() else {
            self.reject_pin(
                card,
                "This capture is not in durable history, so it cannot restore as a pin.",
            );
            return;
        };
        if source.source_px.0 == 0
            || source.source_px.1 == 0
            || !source.source_scale.is_finite()
            || source.source_scale <= 0.0
        {
            self.reject_pin(
                card,
                "Pin to Screen rejected invalid source dimensions before creating a window.",
            );
            return;
        }
        let name = source.name.clone();
        let provenance = source.provenance;
        let source_px = source.source_px;
        let source_scale = source.source_scale;
        let texture = source.texture.clone();
        let pending = source.pending.clone();
        if let Some(source) = self.content.get_mut(&card) {
            source.pin_notice = None;
        }
        let Some(display) = self
            .active_display
            .as_ref()
            .and_then(|id| self.displays.get(id))
            .or_else(|| self.displays.displays().iter().find(|d| d.is_primary))
            .or_else(|| self.displays.displays().first())
        else {
            self.reject_pin(
                card,
                "Native display metrics are unavailable, so Pin to Screen was not opened.",
            );
            return;
        };
        let natural = LogicalSize::new(
            f64::from(source_px.0) / source_scale,
            f64::from(source_px.1) / source_scale,
        );
        let policy = chrome_policy(provenance);
        let surface = match PinnedSurface::on_display(
            pin.clone(),
            natural,
            display,
            policy,
            self.pin_lock_escapes.clone(),
        ) {
            Ok(surface) => surface,
            Err(error) => {
                let reason = format!("Pin to Screen rejected unsafe source dimensions: {error}");
                self.reject_pin(card, reason);
                return;
            }
        };
        let state = surface.state().clone();
        let entry = PinnedEntry {
            name,
            surface,
            texture,
            pending,
            content_error: None,
            positioning_notice: self.pin_support.limitation_notice(),
            native_frame_changed_at: None,
        };
        self.pin_lifetimes.opened(&pin);
        self.pins.insert(pin.clone(), entry);
        self.pending_pin_cards.insert(pin.clone(), card);
        self.emit(OverlayEvent::PinRequested {
            id: card,
            pin,
            state,
        });
    }

    fn reject_pin(&mut self, card: CardId, reason: impl Into<String>) {
        let reason = reason.into();
        if let Some(source) = self.content.get_mut(&card) {
            source.pin_notice = Some(reason.clone());
        }
        self.emit(OverlayEvent::PinUnavailable { card, reason });
    }

    fn restore_pin(&mut self, request: CaptureRequest, state: PinState) {
        if !self.pin_support.windows {
            tracing::warn!(
                detail = %self.pin_support.detail,
                "persisted pin was not restored because native pin windows are unavailable"
            );
            return;
        }
        let Some(id) = request.pin_id.clone() else {
            tracing::warn!("persisted pin restore omitted its durable capture identity");
            return;
        };
        if let Err(stale) = self
            .pin_lifetimes
            .admits_restore(&id, self.pins.contains_key(&id))
        {
            tracing::debug!(pin = %id, "ignoring a stale pin restore: {}", stale.reason());
            return;
        }
        let requested_locked = state.locked;
        let policy = chrome_policy(request.provenance);
        let scale = request.source_scale;
        let natural = LogicalSize::new(
            f64::from(request.source_px.0) / scale,
            f64::from(request.source_px.1) / scale,
        );
        let restored = PinnedSurface::restore_with_natural_size(
            id.clone(),
            natural,
            state,
            policy,
            self.pin_lock_escapes.clone(),
            &self.displays,
        );
        let Some(mut surface) = (match restored {
            Ok(surface) => surface,
            Err(error) => {
                tracing::warn!(pin = %id, "persisted pin dimensions were rejected: {error}");
                return;
            }
        }) else {
            tracing::warn!(pin = %id, "persisted pin was not restored because no display exists");
            return;
        };
        let unlocked_for_platform = surface.state().locked && !self.pin_support.click_through;
        if unlocked_for_platform {
            let _ = surface.set_locked(false);
        }
        let normalized = surface.state().clone();
        self.pins.insert(
            id.clone(),
            PinnedEntry::from_request(request, surface, self.pin_support.limitation_notice()),
        );
        if requested_locked && !normalized.locked {
            self.emit(OverlayEvent::PinUpdated {
                pin: id,
                state: normalized,
            });
        }
    }

    fn close_pin(&mut self, ctx: &egui::Context, id: &PinId) {
        if self.pins.remove(id).is_none() {
            return;
        }
        self.pin_lifetimes.retired(id);
        ctx.send_viewport_cmd_to(pin_viewport_id(id), egui::ViewportCommand::Close);
        self.pending_pin_cards.remove(id);
        self.emit(OverlayEvent::PinClosed { pin: id.clone() });
    }

    fn commit_pin(&mut self, id: &PinId, m: &Motion) {
        let Some(card) = self.pending_pin_cards.remove(id) else {
            return;
        };
        self.pinned_cards.insert(card);
        let _ = self.stack.dismiss(card, m);
    }

    fn fail_pin(&mut self, ctx: &egui::Context, id: &PinId, reason: String) {
        let card = self.pending_pin_cards.remove(id);
        self.discard_pin(ctx, id);
        if let Some(card) = card
            && let Some(source) = self.content.get_mut(&card)
        {
            source.pin_notice = Some(reason);
        }
    }

    fn discard_pin(&mut self, ctx: &egui::Context, id: &PinId) {
        self.pending_pin_cards.remove(id);
        self.pin_lifetimes.retired(id);
        if self.pins.remove(id).is_some() {
            ctx.send_viewport_cmd_to(pin_viewport_id(id), egui::ViewportCommand::Close);
        }
    }

    fn unlock_pins(&mut self, ctx: &egui::Context) {
        let mut changed = Vec::new();
        for (id, entry) in &mut self.pins {
            if entry.surface.state().locked {
                let _ = entry.surface.set_locked(false);
                ctx.send_viewport_cmd_to(
                    pin_viewport_id(id),
                    egui::ViewportCommand::MousePassthrough(false),
                );
                changed.push((id.clone(), entry.surface.state().clone()));
            }
        }
        for (pin, state) in changed {
            self.emit(OverlayEvent::PinUpdated { pin, state });
        }
    }

    fn draw_pins(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|input| input.time);
        let mut ids: Vec<PinId> = self.pins.keys().cloned().collect();
        ids.sort_by(|left, right| left.0.cmp(&right.0));
        let mut changed = Vec::new();
        let mut closed = Vec::new();
        let mut unavailable = Vec::new();
        let mut native = Vec::with_capacity(ids.len());

        for id in ids {
            let title = pin_window_title(&id);
            let Some(entry) = self.pins.get_mut(&id) else {
                continue;
            };
            let zoom_factor = ctx.zoom_factor();
            let builder = pin_viewport(
                &title,
                entry.surface.state(),
                &self.pin_support,
                zoom_factor,
            );
            let positioning = self.pin_support.positioning;
            let native_opacity = self.pin_support.native_opacity;
            let displays = &self.displays;
            let theme = &self.theme;
            let (result, native_frame_changed) =
                ctx.show_viewport_immediate(pin_viewport_id(&id), builder, |ui, _class| {
                    let native_frame_changed = positioning
                        && ui
                            .input(|input| input.viewport().outer_rect)
                            .is_some_and(|rect| {
                                entry.surface.sync_native_frame(
                                    pinned::native_logical_rect(rect, zoom_factor),
                                    displays,
                                )
                            });
                    let mut response = pinned::draw(
                        ui,
                        pinned::PinFrame {
                            name: &entry.name,
                            texture: entry.texture.as_ref().map(egui::TextureHandle::id),
                            content_error: entry.content_error.as_deref(),
                            surface: &mut entry.surface,
                            displays,
                            positioning,
                            native_opacity,
                            click_through: self.pin_support.click_through,
                            theme,
                            chrome_visibility: pinned::ChromeVisibility::Auto,
                        },
                    );
                    if ui.input(|input| input.viewport().close_requested()) {
                        response.close = true;
                    }
                    if let Some(notice) = &entry.positioning_notice {
                        draw_pin_notice(ui, notice, theme);
                    }
                    (response, native_frame_changed)
                });

            if result.positioning_unavailable {
                let reason = self.pin_support.detail.clone();
                entry.positioning_notice = Some(reason.clone());
                unavailable.push((id.clone(), reason));
            }
            if result.changed {
                entry.native_frame_changed_at = None;
                changed.push((id.clone(), entry.surface.state().clone()));
            } else if native_frame_changed {
                entry.native_frame_changed_at = Some(now);
                ctx.request_repaint_after(Duration::from_secs_f64(PIN_GEOMETRY_SETTLE_SECS));
            } else if let Some(changed_at) = entry.native_frame_changed_at {
                let remaining = PIN_GEOMETRY_SETTLE_SECS - (now - changed_at);
                if remaining <= 0.0 {
                    entry.native_frame_changed_at = None;
                    changed.push((id.clone(), entry.surface.state().clone()));
                } else {
                    ctx.request_repaint_after(Duration::from_secs_f64(remaining));
                }
            }
            if result.close {
                closed.push(id.clone());
            } else if self.pin_support.native_adoption {
                let display_scale = entry
                    .surface
                    .state()
                    .display
                    .as_ref()
                    .and_then(|display| displays.get(display))
                    .map_or(ScaleFactor::IDENTITY, |display| display.scale);
                native.push(NativePinRequest {
                    pin: id.clone(),
                    title,
                    state: entry.surface.state().clone(),
                    positioning,
                    display_scale,
                    shadow: entry.surface.state().chrome.shadow,
                });
            }
        }

        for (pin, state) in changed {
            self.emit(OverlayEvent::PinUpdated { pin, state });
        }
        for (pin, reason) in unavailable {
            self.emit(OverlayEvent::PinPositioningUnavailable { pin, reason });
        }
        for pin in closed {
            self.close_pin(ctx, &pin);
        }
        if let Ok(mut requests) = self.handle.shared.native_pins.lock() {
            *requests = native;
        }
    }

    fn pointer(&self, ctx: &egui::Context) -> Option<Pos2> {
        self.probe.as_ref().map_or_else(
            || ctx.input(|i| i.pointer.latest_pos()),
            |probe| probe().or_else(|| ctx.input(|i| i.pointer.latest_pos())),
        )
    }

    fn apply_passthrough(&mut self, ctx: &egui::Context, hits: &[Rect], pointer: Option<Pos2>) {
        let now = ctx.input(|i| i.time);
        let empty = hits.is_empty();
        let desired = match self.passthrough {
            Passthrough::Never => false,
            Passthrough::Always => true,
            // Nothing to click. Pass everything through unconditionally and do
            // not re-sample: an empty overlay that blinked its click-through
            // off once a second would eat a desktop click for no reason, and
            // an idle overlay has no business scheduling repaints.
            Passthrough::Auto if empty => true,
            Passthrough::Auto => {
                if pointer.is_some() {
                    self.last_seen = now;
                    passes_through(pointer, hits)
                } else if self.probe.is_some() {
                    // The probe answered "nowhere"; there is genuinely no
                    // pointer on this display.
                    true
                } else if now - self.last_seen > f64::from(RESAMPLE_SECS) {
                    // Drop click-through for one frame so the pointer becomes
                    // visible again, then decide properly next frame.
                    self.last_seen = now;
                    false
                } else {
                    self.passthrough_now
                }
            }
        };

        if self.passthrough == Passthrough::Auto && self.probe.is_none() && desired && !empty {
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(RESAMPLE_SECS));
        }
        if desired != self.passthrough_now {
            self.passthrough_now = desired;
            ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(desired));
        }
    }

    fn handle_action(&mut self, id: CardId, action: CardAction, m: &Motion) {
        if action == CardAction::Pin {
            self.begin_pin(id, m);
            return;
        }
        let (event, dismiss) = match action {
            CardAction::Copy => (Some(OverlayEvent::CopyRequested { id }), true),
            CardAction::Save => (Some(OverlayEvent::SaveRequested { id }), true),
            CardAction::Annotate => (Some(OverlayEvent::AnnotateRequested { id }), false),
            CardAction::Upload => (Some(OverlayEvent::UploadRequested { id }), false),
            CardAction::Pin => unreachable!("pin actions return above"),
            CardAction::Close => (None, true),
        };
        if let Some(event) = event {
            self.emit(event);
        }
        if dismiss && self.stack.dismiss(id, m) {
            self.emit(OverlayEvent::Dismissed {
                id,
                reason: if action == CardAction::Close {
                    DismissReason::Closed
                } else {
                    DismissReason::Acted
                },
            });
        }
    }

    /// Draw the dock, and return its hit rectangle plus whether it was clicked.
    fn draw_dock(
        &self,
        ui: &mut egui::Ui,
        surface: &Surface<'_>,
        m: &Motion,
    ) -> (Option<Rect>, bool) {
        let d = self.stack.dock();
        if !d.is_visible(m) {
            return (None, false);
        }
        let rect = d.rect();
        let alpha = d.alpha(m);
        if alpha <= 0.004 || rect.width() <= 0.0 {
            return (None, false);
        }
        let palette = surface.palette();
        let painter = ui.painter();
        paint::soft_shadow(painter, rect, Radius::BAR, palette, 0.6 * alpha);
        painter.rect_filled(
            rect,
            corner(Radius::BAR),
            fade(palette.card_fill_raised, alpha),
        );
        surface.icons.draw_faded(
            painter,
            Icon::ChevronRight,
            d.chevron_rect().center(),
            crate::icons::SIZE,
            palette.text_muted,
            alpha,
        );

        let response = ui.interact(rect, egui::Id::new("scrozz.dock"), egui::Sense::click());
        (Some(rect), response.clicked())
    }
}

fn chrome_policy(provenance: Provenance) -> PinChromePolicy {
    if provenance.forbids_compositing() {
        PinChromePolicy::Forbidden
    } else {
        PinChromePolicy::Allowed
    }
}

fn pin_window_title(id: &PinId) -> String {
    format!("Scrozz Pinned Capture [{}]", id.0)
}

fn pin_viewport_id(id: &PinId) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("scrozz-pinned-capture", &id.0))
}

fn pin_viewport(
    title: &str,
    state: &PinState,
    support: &PinSupport,
    zoom_factor: f32,
) -> egui::ViewportBuilder {
    let frame = pinned::viewport_rect(state.frame, zoom_factor);
    let mut builder = egui::ViewportBuilder::default()
        .with_title(title)
        .with_app_id("com.scrozz.pinned-capture")
        .with_inner_size(frame.size())
        .with_resizable(false)
        .with_decorations(false)
        .with_transparent(true)
        .with_active(false)
        .with_taskbar(false)
        .with_close_button(false)
        .with_minimize_button(false)
        .with_maximize_button(false)
        .with_has_shadow(state.chrome.shadow)
        .with_movable_by_background(false)
        .with_drag_and_drop(false)
        .with_clamp_size_to_monitor_size(true)
        .with_mouse_passthrough(state.locked && support.click_through);
    if support.positioning {
        builder = builder.with_position(frame.min);
    }
    if support.always_on_top {
        builder = builder.with_always_on_top();
    }
    if support.x11_managed_dock {
        builder = builder.with_window_type(egui::X11WindowType::Dock);
    }
    builder
}

fn draw_pin_notice(ui: &egui::Ui, text: &str, theme: &Theme) {
    let max = ui.max_rect();
    let rect = Rect::from_center_size(
        Pos2::new(max.center().x, max.min.y + 68.0),
        Vec2::new((max.width() - 16.0).clamp(0.0, 390.0), 34.0),
    );
    if rect.width() <= 0.0 {
        return;
    }

    ui.painter()
        .rect_filled(rect, corner(Radius::BUTTON), theme.palette.card_fill_raised);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        theme.font(crate::theme::Text::Caption),
        theme.palette.text,
    );
    ui.interact(
        rect,
        ui.id().with("pin-positioning-notice"),
        egui::Sense::hover(),
    )
    .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
}

fn draw_card_notice(ui: &mut egui::Ui, card: Rect, text: &str, theme: &Theme) {
    let rect = Rect::from_min_size(
        card.min + Vec2::splat(8.0),
        Vec2::new(
            (card.width() - 16.0).max(0.0),
            52.0_f32.min((card.height() - 16.0).max(0.0)),
        ),
    );
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    ui.painter()
        .rect_filled(rect, corner(Radius::BUTTON), theme.palette.card_fill_raised);
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink(6.0)), |ui| {
        ui.centered_and_justified(|ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text)
                        .size(11.0)
                        .color(theme.palette.text),
                )
                .wrap(),
            );
        });
    });
}

impl eframe::App for OverlayApp {
    /// Fully transparent. eframe's default is a dark translucent wash, which on
    /// an overlay is a grey sheet over the entire work area.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let m = Motion::from_context(&ctx);

        self.refresh_pin_topology_if_due(ctx.input(|input| input.time));
        self.run_commands(&ctx, &m);
        self.ingest(&m);
        self.reconcile(&ctx);
        self.draw_pins(&ctx);
        self.stack.advance(&m);

        let was_empty = self.stack.is_empty();
        let dock_was = self.stack.dock().is_collapsed();

        let surface = Surface::new(&self.theme, &self.icons, m);
        let frames = self.stack.frame(&m);

        let mut hits: Vec<Rect> = Vec::with_capacity(frames.len() + 1);
        let mut hovered = None;
        let mut action = None;
        let mut drag_start = None;
        let mut drag_to = None;
        let mut drag_end = false;

        for f in &frames {
            let Some(entry) = self.content.get(&f.id) else {
                continue;
            };
            let mut content = CardContent::new(&entry.name, entry.source_px, entry.provenance);
            if let Some(tex) = &entry.texture {
                content.texture = Some(tex.id());
            }
            let response = card::draw_card(ui, &surface, f, &content);
            if let Some(notice) = entry.pin_notice.as_deref() {
                draw_card_notice(ui, f.rect, notice, &self.theme);
            }
            hits.push(response.hit);

            if response.body.hovered() {
                hovered = Some(f.id);
            }
            if let Some(a) = response.action {
                action = Some((f.id, a));
            }
            if response.body.drag_started() {
                drag_start = response.body.interact_pointer_pos().map(|p| (f.id, p));
            }
            if response.body.dragged() {
                drag_to = response.body.interact_pointer_pos();
            }
            if response.body.drag_stopped() {
                drag_end = true;
            }
        }

        let (dock_hit, dock_clicked) = self.draw_dock(ui, &surface, &m);
        if let Some(rect) = dock_hit {
            hits.push(rect);
        }

        // Gestures, applied after drawing so this frame's responses drive the
        // next frame's layout rather than tearing the current one.
        if dock_clicked {
            self.stack.expand(&m);
        }
        if hovered != self.hovered {
            self.hovered = hovered;
            self.stack.set_hover(hovered, &m);
        }
        if let Some((id, pointer)) = drag_start
            && self.stack.begin_drag(id, pointer, &m)
        {
            self.dragging = Some(id);
            self.emit(OverlayEvent::DragStarted { id, at: pointer });
        }
        if let Some(p) = drag_to {
            self.stack.drag_to(p, &m);
        }
        if drag_end {
            if let Some(release) = self.stack.release_drag(&m) {
                let at = release.rect.center();
                match release.intent {
                    Intent::Dismiss => self.emit(OverlayEvent::Dismissed {
                        id: release.id,
                        reason: DismissReason::Swipe,
                    }),
                    Intent::DragOut => {
                        self.emit(OverlayEvent::DragOut { id: release.id, at });
                        self.emit(OverlayEvent::Dismissed {
                            id: release.id,
                            reason: DismissReason::DragOut,
                        });
                    }
                    Intent::Collapse | Intent::SpringBack => {}
                }
            }
            self.dragging = None;
        }
        if let Some((id, a)) = action {
            self.handle_action(id, a, &m);
        }

        let dock_now = self.stack.dock().is_collapsed();
        if dock_now != dock_was {
            self.emit(if dock_now {
                OverlayEvent::DockCollapsed
            } else {
                OverlayEvent::DockExpanded
            });
        }
        self.dock_collapsed = dock_now;
        if !was_empty && self.stack.is_empty() && self.stack.departing().is_empty() {
            self.emit(OverlayEvent::Emptied);
        }

        let pointer = self.pointer(&ctx);
        self.apply_passthrough(&ctx, &hits, pointer);

        // The single place repainting is requested: idle costs nothing, an
        // animation gets a continuous repaint, and a pending wake gets a timer.
        self.stack.activity(&m).apply(&ctx);
    }
}

/// The rectangle the dock occupies for a given work area, without building a
/// stack — for a host that needs to know where to put a click target before the
/// overlay exists.
#[must_use]
pub fn dock_rect(geometry: OverlayGeometry) -> Rect {
    let layout =
        crate::stack::StackLayout::new(geometry.local(), crate::stack::CardMetrics::default());
    dock::rect_for_slot0(layout.slot_rect(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
    }

    #[test]
    fn geometry_is_work_area_relative() {
        let g = OverlayGeometry::new(rect(0.0, 25.0, 1440.0, 850.0));
        assert_eq!(g.position(), Pos2::new(0.0, 25.0));
        assert_eq!(g.size(), Vec2::new(1440.0, 850.0));
        assert_eq!(g.local().min, Pos2::ZERO);
        assert_eq!(g.local().size(), g.size());
    }

    #[test]
    fn passthrough_is_off_over_a_card_and_on_beside_it() {
        let card = rect(16.0, 700.0, 232.0, 145.0);
        assert!(!passes_through(Some(Pos2::new(100.0, 760.0)), &[card]));
        assert!(passes_through(Some(Pos2::new(600.0, 400.0)), &[card]));
    }

    #[test]
    fn unknown_pointer_never_passes_through() {
        // With something to hit, an unknown pointer keeps its clicks: the
        // alternative is an overlay that can never be touched again.
        let card = rect(16.0, 700.0, 232.0, 145.0);
        assert!(!passes_through(None, &[card]));
    }

    #[test]
    fn empty_overlay_is_click_through_wherever_the_pointer_is() {
        assert!(passes_through(Some(Pos2::new(1.0, 1.0)), &[]));
        // Including when it is nowhere: there is nothing to click, so this is
        // trivially recoverable the moment a card appears.
        assert!(passes_through(None, &[]));
    }

    #[test]
    fn handle_queues_before_a_window_exists() {
        let h = OverlayHandle::new();
        assert!(!h.is_attached());
        h.push(CaptureRequest::new(
            "Shot.png",
            Provenance::Display,
            (100, 50),
        ));
        assert_eq!(h.shared.inbox.lock().unwrap().len(), 1);
        assert!(h.drain_events().is_empty());
        assert!(h.panel_report().is_none());
    }

    #[test]
    fn downscale_caps_the_longest_edge() {
        let big = egui::ColorImage::new([400, 200], vec![egui::Color32::RED; 400 * 200]);
        let small = downscale(&big, 100).expect("bounded image");
        assert_eq!(small.size, [100, 50]);
        assert!(small.pixels.iter().all(|p| *p == egui::Color32::RED));
    }

    #[test]
    fn downscale_leaves_small_images_alone() {
        let small = egui::ColorImage::new([32, 16], vec![egui::Color32::BLUE; 32 * 16]);
        assert_eq!(
            downscale(&small, 512).expect("bounded image").size,
            [32, 16]
        );
    }

    #[test]
    fn texture_helpers_reject_oversized_or_malformed_inputs_before_upload() {
        let oversized = egui::ColorImage::new(
            [PIN_TEXTURE_PX as usize + 1, 1],
            vec![egui::Color32::BLACK; PIN_TEXTURE_PX as usize + 1],
        );
        assert!(downscale(&oversized, PIN_TEXTURE_PX + 1).is_none());

        let malformed = egui::ColorImage {
            size: [2, 2],
            pixels: vec![egui::Color32::BLACK; 3],
            source_size: egui::Vec2::new(2.0, 2.0),
        };
        assert!(downscale(&malformed, 2).is_none());
    }

    #[test]
    fn a_restore_issued_before_a_close_cannot_resurrect_the_pin() {
        let pin = PinId::from("capture-1");
        let mut lifetimes = PinLifetimes::default();

        // The ordinary case: nothing owns the identity, so a persisted pin opens.
        assert_eq!(lifetimes.admits_restore(&pin, false), Ok(()));

        // The user closes it. The host is told only afterwards, so a restore it
        // had already queued arrives next — and must not be honoured.
        lifetimes.retired(&pin);
        assert_eq!(
            lifetimes.admits_restore(&pin, false),
            Err(StaleRestore::Retired)
        );
        // Repeating the stale restore never wears the refusal down.
        assert_eq!(
            lifetimes.admits_restore(&pin, false),
            Err(StaleRestore::Retired)
        );

        // Pinning the same capture again is a deliberate new lifetime, so the
        // identity becomes restorable once more rather than being poisoned.
        lifetimes.opened(&pin);
        assert_eq!(lifetimes.admits_restore(&pin, false), Ok(()));

        // Retirement is per identity, never global.
        let other = PinId::from("capture-2");
        lifetimes.retired(&other);
        assert_eq!(lifetimes.admits_restore(&pin, false), Ok(()));
        assert_eq!(
            lifetimes.admits_restore(&other, false),
            Err(StaleRestore::Retired)
        );
    }

    #[test]
    fn a_late_restore_never_teleports_a_pin_the_user_is_looking_at() {
        // The live surface carries the user's own drags; the restore carries
        // state as it was persisted. Replacing one with the other would move a
        // pin under the pointer for no reason the user can see.
        let pin = PinId::from("capture-1");
        let lifetimes = PinLifetimes::default();
        assert_eq!(
            lifetimes.admits_restore(&pin, true),
            Err(StaleRestore::AlreadyOnScreen)
        );
    }

    #[test]
    fn box_spans_tile_a_source_exactly_including_its_outermost_pixel() {
        for source in 1usize..=48 {
            for out in 1..=source {
                let mut previous_end = 0usize;
                for index in 0..out {
                    let (start, end) = box_span(index, out, source).expect("finite span");
                    assert_eq!(start, previous_end, "spans must not gap or overlap");
                    assert!(end > start, "every destination pixel needs a sample");
                    previous_end = end;
                }
                assert_eq!(
                    previous_end, source,
                    "the final span must reach the outermost source pixel"
                );
            }
        }
    }

    #[test]
    fn thumbnailing_a_straight_alpha_edge_keeps_its_colour() {
        // One opaque white pixel beside three transparent ones: the corner of an
        // antialiased window. A straight average would report quarter-grey and
        // fringe the window; the alpha-weighted one reports white at quarter
        // coverage, which is what D9 asks the pin to preserve.
        let frame = CaptureFrame {
            data: vec![
                255, 255, 255, 255, 0, 0, 0, 0, // row 0
                0, 0, 0, 0, 0, 0, 0, 0, // row 1
            ],
            size: scrozz_core::PhysicalSize::new(2.0, 2.0),
            stride: 8,
            format: PixelFormat::Rgba8,
            color_space: scrozz_core::ColorSpace::Srgb,
            scale: scrozz_core::ScaleFactor::IDENTITY,
        };
        let img = thumbnail(&frame, 1).expect("bounded thumbnail");
        assert_eq!(img.size, [1, 1]);
        assert_eq!(img.pixels[0].to_srgba_unmultiplied(), [255, 255, 255, 64]);
    }

    #[test]
    fn thumbnailing_preserves_a_solid_opaque_colour_exactly() {
        let frame = CaptureFrame {
            data: [10u8, 200, 30, 255].repeat(16),
            size: scrozz_core::PhysicalSize::new(4.0, 4.0),
            stride: 16,
            format: PixelFormat::Rgba8,
            color_space: scrozz_core::ColorSpace::Srgb,
            scale: scrozz_core::ScaleFactor::IDENTITY,
        };
        let img = thumbnail(&frame, 2).expect("bounded thumbnail");
        assert_eq!(img.size, [2, 2]);
        for pixel in &img.pixels {
            assert_eq!(pixel.to_srgba_unmultiplied(), [10, 200, 30, 255]);
        }
    }

    #[test]
    fn color_image_honours_stride_and_channel_order() {
        // Two 2x1 rows with 4 bytes of padding each.
        let frame = CaptureFrame {
            data: vec![
                1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 0, // row 0 + padding
                7, 8, 9, 255, 10, 11, 12, 255, 0, 0, 0, 0, // row 1 + padding
            ],
            size: scrozz_core::PhysicalSize::new(2.0, 2.0),
            stride: 12,
            format: PixelFormat::Bgra8,
            color_space: scrozz_core::ColorSpace::Srgb,
            scale: scrozz_core::ScaleFactor::IDENTITY,
        };
        let img = color_image(&frame).expect("well formed");
        assert_eq!(img.size, [2, 2]);
        // BGRA 1,2,3 becomes RGB 3,2,1.
        assert_eq!(img.pixels[0].to_srgba_unmultiplied(), [3, 2, 1, 255]);
        assert_eq!(img.pixels[2].to_srgba_unmultiplied(), [9, 8, 7, 255]);
    }

    #[test]
    fn viewport_declares_the_overlay_properties() {
        let v = viewport(OverlayGeometry::new(rect(0.0, 25.0, 1440.0, 850.0)));
        assert_eq!(v.decorations, Some(false));
        assert_eq!(v.transparent, Some(true));
        assert_eq!(v.has_shadow, Some(false));
        assert_eq!(v.taskbar, Some(false));
        assert_eq!(v.resizable, Some(false));
        assert_eq!(v.active, Some(false));
        assert_eq!(v.window_level, Some(egui::WindowLevel::AlwaysOnTop));
        assert_eq!(v.position, Some(Pos2::new(0.0, 25.0)));
        assert_eq!(v.inner_size, Some(Vec2::new(1440.0, 850.0)));
    }

    #[test]
    fn panel_report_defaults_to_not_converted() {
        assert!(!PanelReport::default().non_activating);
        assert!(!PanelReport::unsupported("none").non_activating);
        assert!(PanelReport::converted("NSPanel").non_activating);
    }

    #[test]
    fn positioning_stacking_or_activation_gaps_are_visible_immediately() {
        let mut support = PinSupport::portable();
        support.positioning = false;
        support.always_on_top = false;
        support.detail = "the compositor controls placement and stacking".into();
        assert_eq!(
            support.limitation_notice().as_deref(),
            Some("the compositor controls placement and stacking")
        );

        support.positioning = true;
        support.always_on_top = true;
        assert!(support.limitation_notice().is_some());
        support.non_activating = true;
        assert!(support.limitation_notice().is_none());
    }
}
