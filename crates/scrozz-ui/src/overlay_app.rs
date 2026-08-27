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

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use egui::{Pos2, Rect, Vec2};
use scrozz_core::{Frame as CaptureFrame, PixelFormat, Provenance};

use crate::card::{self, CardAction, CardContent};
use crate::icons::IconStore;
use crate::motion::Motion;
use crate::paint::Surface;
use crate::stack::{CaptureStack, CardId, CardMetrics, Intent, StackLayout, Timing, dock};
use crate::theme::{Appearance, Theme};

/// How long [`Passthrough::Auto`] waits before dropping click-through for a
/// single frame to re-sample the pointer, when no [`PointerProbe`] is supplied.
pub const RESAMPLE_SECS: f32 = 0.35;

/// Default longest edge, in pixels, of a card thumbnail.
///
/// A 6-card stack of full-resolution 5K captures is well over a gigabyte of
/// texture. Cards are 232 pt wide, so 512 px is already generous at 2×.
pub const THUMBNAIL_PX: u32 = 512;

/// Number of dismissed cards that can be restored during this app session.
pub const RECENTLY_CLOSED_LIMIT: usize = 20;

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

/// Shows or hides the native overlay without changing keyboard focus.
///
/// macOS needs this because winit implements `Visible(true)` with
/// `makeKeyAndOrderFront`, which would divert typing into a non-activating
/// panel. Other platforms can omit the hook and use the viewport command.
pub type VisibilityHook = Arc<dyn Fn(bool) + Send + Sync>;

/// Reports the pointer position in the overlay window's own logical
/// coordinates, whether or not the window is currently accepting mouse events.
///
/// Required on macOS for [`Passthrough::Auto`] to track precisely; see the
/// module documentation.
pub type PointerProbe = Arc<dyn Fn() -> Option<Pos2> + Send + Sync>;

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
    /// Optional focus-preserving native visibility control.
    pub visibility: Option<VisibilityHook>,
    /// Longest thumbnail edge in pixels.
    pub thumbnail_px: u32,
    /// Card size and spacing tokens.
    pub card_metrics: CardMetrics,
    /// Seconds of inactivity before an unpinned card closes.
    ///
    /// `None` keeps cards open until the user acts.
    pub auto_close_secs: Option<f64>,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            geometry: OverlayGeometry::default(),
            appearance: Appearance::Dark,
            passthrough: Passthrough::default(),
            probe: None,
            panel: None,
            visibility: None,
            thumbnail_px: THUMBNAIL_PX,
            card_metrics: CardMetrics::default(),
            auto_close_secs: None,
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
            .field("card_metrics", &self.card_metrics)
            .field("auto_close_secs", &self.auto_close_secs)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// The public seam
// ---------------------------------------------------------------------------

/// A capture handed to the overlay.
#[derive(Clone, Debug)]
pub struct CaptureRequest {
    /// File name shown in the caption.
    pub name: String,
    /// Where the pixels came from. Decides the chrome (D9).
    pub provenance: Provenance,
    /// The capture's own pixel dimensions, shown in the caption.
    pub source_px: (u32, u32),
    /// A pre-scaled thumbnail. `None` shows a holding fill until one arrives.
    pub thumbnail: Option<egui::ColorImage>,
    /// Whether the capture can be rehydrated after its live bytes are released.
    pub restorable: bool,
}

impl CaptureRequest {
    /// A request with no thumbnail yet.
    #[must_use]
    pub fn new(name: impl Into<String>, provenance: Provenance, source_px: (u32, u32)) -> Self {
        Self {
            name: name.into(),
            provenance,
            source_px,
            thumbnail: None,
            restorable: true,
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
            name: name.into(),
            provenance,
            source_px,
            thumbnail: Some(thumbnail),
            restorable: true,
        })
    }

    /// Attach a thumbnail.
    #[must_use]
    pub fn with_thumbnail(mut self, image: egui::ColorImage) -> Self {
        self.thumbnail = Some(image);
        self
    }

    /// Controls whether dismissal adds this capture to recently closed.
    #[must_use]
    pub const fn with_restorable(mut self, restorable: bool) -> Self {
        self.restorable = restorable;
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
    /// The configured inactivity interval elapsed.
    TimedOut,
}

/// Something the user did to the overlay.
#[derive(Clone, Copy, Debug, PartialEq)]
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
        /// The card.
        id: CardId,
        /// The new persistent pin state.
        pinned: bool,
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
        /// Where the gesture crossed the drag-out threshold, in window coordinates.
        at: Pos2,
        /// The card rectangle at the commitment instant.
        rect: Rect,
    },
    /// The pile collapsed into the dock (D20).
    DockCollapsed,
    /// The pile came back out of the dock.
    DockExpanded,
    /// The last card left; the overlay can be hidden.
    Emptied,
    /// A recently closed capture re-entered with a new overlay identity.
    Restored {
        /// The new card identity.
        id: CardId,
    },
    /// Temporary overlay visibility changed.
    VisibilityChanged {
        /// Whether the overlay is intentionally hidden.
        hidden: bool,
    },
}

/// Something the application asks the overlay to do.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Command {
    Dismiss(CardId),
    DismissAfterAction(CardId),
    DismissAll,
    Collapse,
    Expand,
    ToggleDock,
    RestoreRecent,
    UploadLatest,
    FinishDrag { id: CardId, accepted: bool },
    SetPinned { id: CardId, pinned: bool },
    SetGeometry(OverlayGeometry),
    SetMetrics(CardMetrics),
    SetAutoClose(Option<f64>),
    SetHidden(bool),
    ToggleHidden,
    Close,
}

#[derive(Default)]
struct Shared {
    inbox: Mutex<Vec<CaptureRequest>>,
    outbox: Mutex<Vec<OverlayEvent>>,
    commands: Mutex<Vec<Command>>,
    ctx: Mutex<Option<egui::Context>>,
    report: Mutex<Option<PanelReport>>,
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

    /// Retire one card after Copy or Save succeeds.
    pub fn dismiss_after_action(&self, id: CardId) {
        self.command(Command::DismissAfterAction(id));
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

    /// Bring back the most recently closed capture.
    pub fn restore_recent(&self) {
        self.command(Command::RestoreRecent);
    }

    /// Upload the newest resident capture.
    pub fn upload_latest(&self) {
        self.command(Command::UploadLatest);
    }

    /// Complete a native drag, restoring rejected drops.
    pub fn finish_drag(&self, id: CardId, accepted: bool) {
        self.command(Command::FinishDrag { id, accepted });
    }

    /// Apply a pin state after the owning history store has persisted it.
    pub fn set_pinned(&self, id: CardId, pinned: bool) {
        self.command(Command::SetPinned { id, pinned });
    }

    /// Move and resize the overlay to another display work area.
    pub fn set_geometry(&self, geometry: OverlayGeometry) {
        self.command(Command::SetGeometry(geometry));
    }

    /// Change card size and stack density.
    pub fn set_card_metrics(&self, metrics: CardMetrics) {
        self.command(Command::SetMetrics(metrics));
    }

    /// Change inactivity-based auto-close.
    pub fn set_auto_close(&self, secs: Option<f64>) {
        self.command(Command::SetAutoClose(secs));
    }

    /// Temporarily hide or show the overlay without dropping captures.
    pub fn set_hidden(&self, hidden: bool) {
        self.command(Command::SetHidden(hidden));
    }

    /// Toggle temporary overlay visibility.
    pub fn toggle_hidden(&self) {
        self.command(Command::ToggleHidden);
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
    if w == 0 || h == 0 {
        return None;
    }
    let bpp = frame.format.bytes_per_pixel();
    let premultiplied = frame.format.is_premultiplied();
    let swap = matches!(
        frame.format,
        PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8
    );

    let mut pixels = Vec::with_capacity(w * h);
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
    let full = color_image(frame)?;
    Some(downscale(&full, max_edge))
}

/// Box-filter an image down so its longest edge is at most `max_edge`.
#[must_use]
pub fn downscale(image: &egui::ColorImage, max_edge: u32) -> egui::ColorImage {
    let (w, h) = (image.size[0], image.size[1]);
    let max = max_edge.max(1) as usize;
    if w == 0 || h == 0 || (w <= max && h <= max) {
        return image.clone();
    }
    let scale = max as f32 / w.max(h) as f32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (nw, nh) = (
        ((w as f32 * scale).round() as usize).max(1),
        ((h as f32 * scale).round() as usize).max(1),
    );

    let mut out = Vec::with_capacity(nw * nh);
    for y in 0..nh {
        let y0 = y * h / nh;
        let y1 = (((y + 1) * h).div_ceil(nh)).min(h).max(y0 + 1);
        for x in 0..nw {
            let x0 = x * w / nw;
            let x1 = (((x + 1) * w).div_ceil(nw)).min(w).max(x0 + 1);
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
    egui::ColorImage::new([nw, nh], out)
}

// ---------------------------------------------------------------------------
// The application
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Entry {
    name: String,
    provenance: Provenance,
    source_px: (u32, u32),
    texture: Option<egui::TextureHandle>,
    pending: Option<egui::ColorImage>,
    pinned: bool,
    restorable: bool,
}

#[derive(Clone)]
struct RecentEntry {
    id: CardId,
    entry: Entry,
}

/// The `eframe` application that hosts the capture stack.
pub struct OverlayApp {
    stack: CaptureStack,
    content: HashMap<CardId, Entry>,
    recent: VecDeque<RecentEntry>,
    restored_aliases: VecDeque<(CardId, CardId)>,
    announced_closed: HashSet<CardId>,
    handle: OverlayHandle,
    theme: Theme,
    icons: IconStore,
    geometry: OverlayGeometry,
    passthrough: Passthrough,
    probe: Option<PointerProbe>,
    visibility: Option<VisibilityHook>,
    thumbnail_px: u32,
    /// The value most recently sent to the window, so the command is sent on
    /// change rather than every frame.
    passthrough_now: bool,
    /// When the pointer was last actually known, for re-sampling.
    last_seen: f64,
    hovered: Option<CardId>,
    dock_collapsed: bool,
    dragging: Option<CardId>,
    hidden: bool,
    hidden_at_motion: Option<f64>,
    refresh_deadlines_after: Option<f64>,
    reported_empty: bool,
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
        let report = options.panel.take().map_or_else(
            || PanelReport::unsupported("no native panel hook supplied"),
            |hook| hook(cc),
        );
        Self::from_context(&cc.egui_ctx, handle, options, report)
    }

    fn from_context(
        ctx: &egui::Context,
        handle: OverlayHandle,
        options: OverlayOptions,
        report: PanelReport,
    ) -> Self {
        let theme = Theme::for_appearance(options.appearance);
        crate::theme::install_fonts(ctx);
        crate::theme::install_style(ctx, &theme);

        if let Ok(mut slot) = handle.shared.ctx.lock() {
            *slot = Some(ctx.clone());
        }

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

        let mut stack = CaptureStack::new(
            StackLayout::new(options.geometry.local(), options.card_metrics),
            Timing::default(),
        );
        stack.set_auto_close(options.auto_close_secs, &Motion::from_context(ctx));

        Self {
            stack,
            content: HashMap::new(),
            recent: VecDeque::with_capacity(RECENTLY_CLOSED_LIMIT),
            restored_aliases: VecDeque::with_capacity(RECENTLY_CLOSED_LIMIT),
            announced_closed: HashSet::new(),
            handle,
            theme,
            icons: IconStore::new(ctx),
            geometry: options.geometry,
            passthrough: options.passthrough,
            probe: options.probe,
            visibility: options.visibility,
            thumbnail_px: options.thumbnail_px.max(1),
            passthrough_now: false,
            last_seen: 0.0,
            hovered: None,
            dock_collapsed: false,
            dragging: None,
            hidden: false,
            hidden_at_motion: None,
            refresh_deadlines_after: None,
            reported_empty: true,
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
        let protected = self.protected_ids();
        let retired = self
            .stack
            .resize_with_protection(geometry.local(), m, |id| protected.contains(&id));
        for id in retired {
            self.announce_closed(id, DismissReason::Overflow);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(geometry.size()));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(geometry.position()));
    }

    fn protected_ids(&self) -> HashSet<CardId> {
        self.content
            .iter()
            .filter_map(|(id, entry)| entry.pinned.then_some(*id))
            .collect()
    }

    fn remember(&mut self, id: CardId) {
        let Some(entry) = self.content.get(&id).cloned() else {
            return;
        };
        if !entry.restorable {
            return;
        }
        self.recent.push_back(RecentEntry { id, entry });
        if self.recent.len() > RECENTLY_CLOSED_LIMIT {
            self.recent.pop_front();
        }
    }

    fn apply_pinned(&mut self, id: CardId, pinned: bool, m: &Motion) {
        let mut resident_id = id;
        for _ in 0..RECENTLY_CLOSED_LIMIT {
            let Some((_, next)) = self
                .restored_aliases
                .iter()
                .rev()
                .find(|(previous, _)| *previous == resident_id)
            else {
                break;
            };
            resident_id = *next;
        }

        let resident = if let Some(entry) = self.content.get_mut(&resident_id) {
            entry.pinned = pinned;
            true
        } else {
            false
        };
        for recent in self
            .recent
            .iter_mut()
            .filter(|recent| recent.id == id || recent.id == resident_id)
        {
            recent.entry.pinned = pinned;
        }

        if !resident {
            return;
        }
        self.stack
            .set_auto_close_paused(resident_id, self.hidden || pinned, m);
        if !pinned {
            let protected = self.protected_ids();
            let retired = self
                .stack
                .enforce_capacity_with_protection(m, |candidate| protected.contains(&candidate));
            for retired in retired {
                self.announce_closed(retired, DismissReason::Overflow);
            }
        }
    }

    fn announce_closed(&mut self, id: CardId, reason: DismissReason) {
        if self.announced_closed.insert(id) {
            self.remember(id);
            self.emit(OverlayEvent::Dismissed { id, reason });
        }
    }

    fn dismiss_all(&mut self, reason: DismissReason, m: &Motion) {
        let ids: Vec<CardId> = self.stack.cards().iter().map(|card| card.id()).collect();
        self.stack.dismiss_all(m);
        for id in ids {
            self.announce_closed(id, reason);
        }
    }

    fn set_hidden(&mut self, hidden: bool, ctx: &egui::Context, m: &Motion) {
        if self.hidden != hidden {
            self.hidden = hidden;
            if hidden {
                self.hidden_at_motion = Some(m.now());
                self.refresh_deadlines_after = None;
                self.reconcile_auto_close_pauses(m);
            } else {
                self.refresh_deadlines_after =
                    Some(self.hidden_at_motion.take().unwrap_or_else(|| m.now()));
            }
            if let Some(visibility) = &self.visibility {
                visibility(!hidden);
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(!hidden));
            }
            self.emit(OverlayEvent::VisibilityChanged { hidden });
        }
    }

    fn reconcile_auto_close_pauses(&mut self, m: &Motion) {
        let paused: Vec<(CardId, bool)> = self
            .stack
            .cards()
            .iter()
            .map(|card| {
                let pinned = self
                    .content
                    .get(&card.id())
                    .is_some_and(|entry| entry.pinned);
                (card.id(), self.hidden || pinned)
            })
            .collect();
        for (id, paused) in paused {
            self.stack.set_auto_close_paused(id, paused, m);
        }
    }

    fn emit(&self, event: OverlayEvent) {
        if let Ok(mut q) = self.handle.shared.outbox.lock() {
            q.push(event);
        }
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

    fn ingest(&mut self, ctx: &egui::Context, m: &Motion) {
        for request in self.take_inbox() {
            self.set_hidden(false, ctx, m);
            let protected = self.protected_ids();
            let pushed = self
                .stack
                .push_with_protection(m, |id| protected.contains(&id));
            let id = pushed.id;
            let thumb = request
                .thumbnail
                .map(|image| downscale(&image, self.thumbnail_px));
            self.content.insert(
                id,
                Entry {
                    name: request.name,
                    provenance: request.provenance,
                    source_px: request.source_px,
                    texture: None,
                    pending: thumb,
                    pinned: false,
                    restorable: request.restorable,
                },
            );
            self.emit(OverlayEvent::Pushed { id });
            for retired in pushed.retired {
                self.announce_closed(retired, DismissReason::Overflow);
            }
            if !pushed.resident {
                self.announce_closed(id, DismissReason::Overflow);
            }
        }
    }

    fn run_commands(&mut self, ctx: &egui::Context, m: &Motion) {
        for cmd in self.take_commands() {
            match cmd {
                Command::Dismiss(id) => {
                    if self.stack.dismiss(id, m) {
                        self.announce_closed(id, DismissReason::Programmatic);
                    }
                }
                Command::DismissAfterAction(id) => {
                    if self.stack.dismiss(id, m) {
                        self.announce_closed(id, DismissReason::Acted);
                    }
                }
                Command::DismissAll => self.dismiss_all(DismissReason::Programmatic, m),
                Command::Collapse => self.stack.collapse(m),
                Command::Expand => self.stack.expand(m),
                Command::ToggleDock => self.stack.toggle_dock(m),
                Command::RestoreRecent => {
                    if let Some(recent) = self.recent.pop_back() {
                        let protected = self.protected_ids();
                        let pushed = self
                            .stack
                            .push_with_protection(m, |id| protected.contains(&id));
                        if pushed.resident {
                            let id = pushed.id;
                            self.restored_aliases.push_back((recent.id, id));
                            if self.restored_aliases.len() > RECENTLY_CLOSED_LIMIT {
                                self.restored_aliases.pop_front();
                            }
                            let pinned = recent.entry.pinned;
                            self.content.insert(id, recent.entry);
                            if pinned {
                                self.stack.set_auto_close_paused(id, true, m);
                            }
                            self.emit(OverlayEvent::Restored { id });
                            for retired in pushed.retired {
                                self.announce_closed(retired, DismissReason::Overflow);
                            }
                            self.set_hidden(false, ctx, m);
                        } else {
                            self.recent.push_back(recent);
                        }
                    }
                }
                Command::UploadLatest => {
                    if let Some(id) = self.stack.cards().last().map(|card| card.id()) {
                        self.emit(OverlayEvent::UploadRequested { id });
                    }
                }
                Command::FinishDrag { id, accepted } => {
                    if accepted {
                        if self.stack.finalize_drag_out(id) {
                            self.announce_closed(id, DismissReason::DragOut);
                        }
                    } else {
                        let protected = self.protected_ids();
                        let restored = self
                            .stack
                            .restore_drag_out(id, m, |candidate| protected.contains(&candidate));
                        for retired in restored.retired {
                            self.announce_closed(retired, DismissReason::Overflow);
                        }
                        if restored.restored
                            && self.content.get(&id).is_some_and(|entry| entry.pinned)
                        {
                            self.stack.set_auto_close_paused(id, true, m);
                        }
                    }
                }
                Command::SetPinned { id, pinned } => self.apply_pinned(id, pinned, m),
                Command::SetGeometry(geometry) => self.set_geometry(geometry, ctx, m),
                Command::SetMetrics(metrics) => {
                    let protected = self.protected_ids();
                    let retired = self
                        .stack
                        .set_metrics_with_protection(metrics, m, |id| protected.contains(&id));
                    for id in retired {
                        self.announce_closed(id, DismissReason::Overflow);
                    }
                }
                Command::SetAutoClose(secs) => {
                    self.stack.set_auto_close(secs, m);
                    self.reconcile_auto_close_pauses(m);
                }
                Command::SetHidden(hidden) => self.set_hidden(hidden, ctx, m),
                Command::ToggleHidden => self.set_hidden(!self.hidden, ctx, m),
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
            self.announce_closed(id, DismissReason::Overflow);
            self.content.remove(&id);
            self.announced_closed.remove(&id);
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

    fn finish_drag_gesture(&mut self, m: &Motion) {
        if let Some(release) = self.stack.release_drag(m) {
            match release.intent {
                Intent::Dismiss => {
                    self.announce_closed(release.id, DismissReason::Swipe);
                }
                Intent::DragOut => {
                    self.emit(OverlayEvent::DragOut {
                        id: release.id,
                        at: release.pointer,
                        rect: release.rect,
                    });
                }
                Intent::Collapse | Intent::SpringBack => {}
            }
        }
        self.dragging = None;
    }

    fn handle_action(&mut self, id: CardId, action: CardAction, m: &Motion) {
        if action == CardAction::Pin {
            let Some(entry) = self.content.get(&id) else {
                return;
            };
            let pinned = !entry.pinned;
            self.emit(OverlayEvent::PinRequested { id, pinned });
            return;
        }
        let (event, dismiss) = match action {
            CardAction::Copy => (Some(OverlayEvent::CopyRequested { id }), false),
            CardAction::Save => (Some(OverlayEvent::SaveRequested { id }), false),
            CardAction::Annotate => (Some(OverlayEvent::AnnotateRequested { id }), false),
            CardAction::Upload => (Some(OverlayEvent::UploadRequested { id }), false),
            CardAction::Pin => unreachable!("pin is handled before the action table"),
            CardAction::Close => (None, true),
        };
        if let Some(event) = event {
            self.emit(event);
        }
        if dismiss && self.stack.dismiss(id, m) {
            self.announce_closed(
                id,
                if action == CardAction::Close {
                    DismissReason::Closed
                } else {
                    DismissReason::Acted
                },
            );
        }
    }

    /// Draw the dock, and return its hit rectangle plus whether it was clicked.
    fn draw_dock(
        &self,
        ui: &mut egui::Ui,
        surface: &Surface<'_>,
        m: &Motion,
    ) -> (Option<Rect>, bool) {
        if self.stack.is_empty() && self.stack.departing().is_empty() {
            return (None, false);
        }
        dock::draw(ui, surface, self.stack.dock(), m)
    }

    fn update_state(&mut self, ctx: &egui::Context, m: &Motion, painted: bool) {
        self.run_commands(ctx, m);
        self.ingest(ctx, m);
        if painted && let Some(hidden_clock) = self.refresh_deadlines_after {
            self.reconcile_auto_close_pauses(m);
            if m.now() > hidden_clock {
                self.refresh_deadlines_after = None;
            }
        }
        self.reconcile(ctx);
        let protected = self.protected_ids();
        let expired = self
            .stack
            .advance_with_protection(m, |id| protected.contains(&id));
        for id in expired {
            self.announce_closed(id, DismissReason::TimedOut);
        }

        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.dismiss_all(DismissReason::Programmatic, m);
        }
    }

    fn announce_stack_state(&mut self) {
        let dock_now = self.stack.dock().is_collapsed();
        if dock_now != self.dock_collapsed {
            self.emit(if dock_now {
                OverlayEvent::DockCollapsed
            } else {
                OverlayEvent::DockExpanded
            });
        }
        self.dock_collapsed = dock_now;

        let empty_now = self.stack.is_empty() && self.stack.departing().is_empty();
        if empty_now && !self.reported_empty {
            self.emit(OverlayEvent::Emptied);
        }
        self.reported_empty = empty_now;
    }

    /// Services model commands while the native panel is ordered out.
    ///
    /// Eframe continues calling its logic phase for hidden windows but skips
    /// painting. Processing here is what lets a tray action, restoration, or new
    /// capture make an ordered-out panel visible again.
    pub fn service_hidden(&mut self, ctx: &egui::Context) {
        if !self.hidden {
            return;
        }

        let m = Motion::from_context(ctx);
        self.update_state(ctx, &m, false);
        self.announce_stack_state();
        if self.hidden {
            self.apply_passthrough(ctx, &[], None);
            let protected = self.protected_ids();
            self.stack
                .activity_with_protection(&m, |id| protected.contains(&id))
                .apply(ctx);
        } else {
            ctx.request_repaint();
        }
    }
}

impl OverlayApp {
    fn draw_frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let m = Motion::from_context(&ctx);

        self.update_state(&ctx, &m, true);

        if self.hidden {
            self.apply_passthrough(&ctx, &[], None);
            let protected = self.protected_ids();
            self.stack
                .activity_with_protection(&m, |id| protected.contains(&id))
                .apply(&ctx);
            return;
        }

        let surface = Surface::new(&self.theme, &self.icons, m);
        let frames = if self.stack.dock().is_collapsed() {
            Vec::new()
        } else {
            self.stack.frame(&m)
        };

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
            let mut content = CardContent::new(&entry.name, entry.source_px, entry.provenance)
                .with_pinned(entry.pinned);
            if let Some(tex) = &entry.texture {
                content.texture = Some(tex.id());
            }
            let response = card::draw_card(ui, &surface, f, &content);
            hits.push(response.hit);

            if response.body.hovered() {
                hovered = Some(f.id);
            }
            if let Some(a) = response.action {
                action = Some((f.id, a));
            } else if response.body.clicked() {
                action = Some((f.id, CardAction::Annotate));
            }
            if response.body.drag_started() {
                drag_start = response
                    .body
                    .interact_pointer_pos()
                    .map(|p| (f.id, p - response.body.drag_delta()));
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
            if self.stack.active_drag_intent(&m) == Some(Intent::DragOut) {
                self.finish_drag_gesture(&m);
            }
        }
        if drag_end && self.dragging.is_some() {
            self.finish_drag_gesture(&m);
        }
        if let Some((id, a)) = action {
            self.handle_action(id, a, &m);
        }

        self.announce_stack_state();

        let pointer = self.pointer(&ctx);
        self.apply_passthrough(&ctx, &hits, pointer);

        // The single place repainting is requested: idle costs nothing, an
        // animation gets a continuous repaint, and a pending wake gets a timer.
        let protected = self.protected_ids();
        self.stack
            .activity_with_protection(&m, |id| protected.contains(&id))
            .apply(&ctx);
        if self.refresh_deadlines_after.is_some() {
            ctx.request_repaint();
        }
    }
}

impl eframe::App for OverlayApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.service_hidden(ctx);
    }

    /// Fully transparent. eframe's default is a dark translucent wash, which on
    /// an overlay is a grey sheet over the entire work area.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw_frame(ui);
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

    fn test_overlay(
        geometry: OverlayGeometry,
        visibility: Option<VisibilityHook>,
    ) -> (egui::Context, OverlayApp, OverlayHandle) {
        let ctx = egui::Context::default();
        let handle = OverlayHandle::new();
        let options = OverlayOptions {
            geometry,
            visibility,
            ..OverlayOptions::default()
        };
        let app = OverlayApp::from_context(
            &ctx,
            handle.clone(),
            options,
            PanelReport::converted("test panel"),
        );
        (ctx, app, handle)
    }

    fn run_frame(
        ctx: &egui::Context,
        app: &mut OverlayApp,
        time: f64,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let mut output = ctx.run_ui(
            egui::RawInput {
                time: Some(time),
                predicted_dt: 1.0 / 60.0,
                screen_rect: Some(app.geometry.local()),
                events,
                ..Default::default()
            },
            |ui| app.draw_frame(ui),
        );
        output.textures_delta.clear();
        output
    }

    fn push_and_settle(
        ctx: &egui::Context,
        app: &mut OverlayApp,
        handle: &OverlayHandle,
    ) -> CardId {
        handle.push(CaptureRequest::new(
            "Shot.png",
            Provenance::Display,
            (1920, 1080),
        ));
        run_frame(ctx, app, 0.0, Vec::new());
        let id = app.stack.cards()[0].id();
        run_frame(ctx, app, 4.0, Vec::new());
        let _ = handle.drain_events();
        id
    }

    fn pointer_button(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
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
    fn an_empty_collapsed_stack_does_not_leave_a_dock_hit_target() {
        let geometry = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 480.0));
        let (ctx, mut app, handle) = test_overlay(geometry, None);
        handle.collapse();

        run_frame(&ctx, &mut app, 0.0, Vec::new());

        assert!(app.stack.is_empty());
        assert!(
            app.passthrough_now,
            "an empty dock must leave the desktop clickable"
        );
    }

    #[test]
    fn clicking_the_card_body_requests_annotation() {
        let geometry = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 480.0));
        let (ctx, mut app, handle) = test_overlay(geometry, None);
        let id = push_and_settle(&ctx, &mut app, &handle);
        let card = app.stack.frame_of(id, &Motion::at_ms(4_000)).unwrap();
        let pointer = Pos2::new(card.rect.left() + 24.0, card.rect.center().y);

        run_frame(
            &ctx,
            &mut app,
            4.1,
            vec![
                egui::Event::PointerMoved(pointer),
                pointer_button(pointer, true),
            ],
        );
        run_frame(&ctx, &mut app, 4.2, vec![pointer_button(pointer, false)]);

        assert!(
            handle
                .drain_events()
                .contains(&OverlayEvent::AnnotateRequested { id })
        );
    }

    #[test]
    fn drag_out_is_emitted_while_the_mouse_is_still_held() {
        let geometry = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 480.0));
        let (ctx, mut app, handle) = test_overlay(geometry, None);
        let id = push_and_settle(&ctx, &mut app, &handle);
        let card = app.stack.frame_of(id, &Motion::at_ms(4_000)).unwrap();
        let origin = Pos2::new(card.rect.left() + 24.0, card.rect.center().y);
        let committed = origin + Vec2::new(240.0, 0.0);

        run_frame(
            &ctx,
            &mut app,
            4.1,
            vec![
                egui::Event::PointerMoved(origin),
                pointer_button(origin, true),
            ],
        );
        run_frame(
            &ctx,
            &mut app,
            4.2,
            vec![egui::Event::PointerMoved(committed)],
        );

        let events = handle.drain_events();
        assert!(events.iter().any(
            |event| matches!(event, OverlayEvent::DragStarted { id: found, .. } if *found == id)
        ));
        assert!(
            events.iter().any(
                |event| matches!(event, OverlayEvent::DragOut { id: found, .. } if *found == id)
            ),
            "native drag commitment must precede the pointer-up event: {events:?}"
        );
    }

    #[test]
    fn escape_dismisses_every_resident_card() {
        let geometry = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 480.0));
        let (ctx, mut app, handle) = test_overlay(geometry, None);
        let id = push_and_settle(&ctx, &mut app, &handle);

        run_frame(
            &ctx,
            &mut app,
            4.1,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: Some(egui::Key::Escape),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );

        assert!(app.stack.is_empty());
        assert!(handle.drain_events().contains(&OverlayEvent::Dismissed {
            id,
            reason: DismissReason::Programmatic,
        }));
    }

    #[test]
    fn successful_action_dismissal_reports_acted() {
        let geometry = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 480.0));
        let (ctx, mut app, handle) = test_overlay(geometry, None);
        let id = push_and_settle(&ctx, &mut app, &handle);

        handle.dismiss_after_action(id);
        run_frame(&ctx, &mut app, 4.1, Vec::new());

        assert!(handle.drain_events().contains(&OverlayEvent::Dismissed {
            id,
            reason: DismissReason::Acted,
        }));
    }

    #[test]
    fn unpinning_reconciles_over_capacity_only_after_persistence_acknowledges_it() {
        let roomy = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 1_000.0));
        let (ctx, mut app, handle) = test_overlay(roomy, None);
        handle.push(CaptureRequest::new(
            "One.png",
            Provenance::Display,
            (100, 100),
        ));
        handle.push(CaptureRequest::new(
            "Two.png",
            Provenance::Display,
            (100, 100),
        ));
        run_frame(&ctx, &mut app, 0.0, Vec::new());
        let ids: Vec<_> = app.stack.cards().iter().map(|card| card.id()).collect();
        for id in &ids {
            app.content.get_mut(id).unwrap().pinned = true;
            app.stack
                .set_auto_close_paused(*id, true, &Motion::at_ms(0));
        }

        app.set_geometry(
            OverlayGeometry::new(rect(0.0, 0.0, 640.0, 300.0)),
            &ctx,
            &Motion::at_ms(4_000),
        );
        assert_eq!(app.stack.capacity(), 1);
        assert_eq!(app.stack.len(), 2);
        let _ = handle.drain_events();

        app.handle_action(ids[1], CardAction::Pin, &Motion::at_ms(4_100));

        assert_eq!(app.stack.len(), 2);
        assert!(app.content.get(&ids[1]).unwrap().pinned);
        let events = handle.drain_events();
        assert!(events.contains(&OverlayEvent::PinRequested {
            id: ids[1],
            pinned: false,
        }));
        assert!(!events.iter().any(|event| matches!(
            event,
            OverlayEvent::Dismissed {
                reason: DismissReason::Overflow,
                ..
            }
        )));

        handle.set_pinned(ids[1], false);
        run_frame(&ctx, &mut app, 4.2, Vec::new());

        assert_eq!(app.stack.len(), 1);
        assert_eq!(app.stack.slot_of(ids[0]), Some(0));
        let events = handle.drain_events();
        assert!(events.contains(&OverlayEvent::Dismissed {
            id: ids[1],
            reason: DismissReason::Overflow,
        }));
    }

    #[test]
    fn pin_acknowledgement_updates_a_recent_card_before_restore() {
        let geometry = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 480.0));
        let (ctx, mut app, handle) = test_overlay(geometry, None);
        let id = push_and_settle(&ctx, &mut app, &handle);

        handle.dismiss(id);
        run_frame(&ctx, &mut app, 4.1, Vec::new());
        let _ = handle.drain_events();

        handle.restore_recent();
        handle.set_pinned(id, true);
        run_frame(&ctx, &mut app, 4.2, Vec::new());

        let restored = handle
            .drain_events()
            .into_iter()
            .find_map(|event| match event {
                OverlayEvent::Restored { id } => Some(id),
                _ => None,
            })
            .expect("the recent capture should restore");
        assert!(app.content.get(&restored).unwrap().pinned);
    }

    #[test]
    fn temporary_hide_preserves_cards_and_uses_the_visibility_hook() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let visibility: VisibilityHook = {
            let calls = Arc::clone(&calls);
            Arc::new(move |visible| calls.lock().unwrap().push(visible))
        };
        let geometry = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 480.0));
        let (ctx, mut app, handle) = test_overlay(geometry, Some(visibility));
        let id = push_and_settle(&ctx, &mut app, &handle);

        handle.set_hidden(true);
        run_frame(&ctx, &mut app, 4.1, Vec::new());
        assert_eq!(app.stack.slot_of(id), Some(0));
        handle.set_hidden(false);
        run_frame(&ctx, &mut app, 4.2, Vec::new());

        assert_eq!(*calls.lock().unwrap(), vec![false, true]);
        assert_eq!(app.stack.slot_of(id), Some(0));
    }

    #[test]
    fn recently_closed_capture_restores_with_a_fresh_overlay_identity() {
        let geometry = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 480.0));
        let (ctx, mut app, handle) = test_overlay(geometry, None);
        let original = push_and_settle(&ctx, &mut app, &handle);

        handle.dismiss(original);
        run_frame(&ctx, &mut app, 4.1, Vec::new());
        assert!(app.stack.is_empty());
        assert_eq!(app.recent.len(), 1);
        let _ = handle.drain_events();

        handle.restore_recent();
        run_frame(&ctx, &mut app, 4.2, Vec::new());

        let restored = app.stack.cards()[0].id();
        assert_ne!(restored, original);
        assert_eq!(app.content[&restored].name, "Shot.png");
        assert!(
            handle
                .drain_events()
                .contains(&OverlayEvent::Restored { id: restored })
        );
    }

    #[test]
    fn a_historyless_capture_is_not_offered_for_restoration_after_cache_release() {
        let geometry = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 480.0));
        let (ctx, mut app, handle) = test_overlay(geometry, None);
        handle.push(
            CaptureRequest::new("Ephemeral.png", Provenance::Display, (100, 100))
                .with_restorable(false),
        );
        run_frame(&ctx, &mut app, 0.0, Vec::new());
        let id = app.stack.cards()[0].id();
        run_frame(&ctx, &mut app, 4.0, Vec::new());

        handle.dismiss(id);
        run_frame(&ctx, &mut app, 4.1, Vec::new());
        assert!(app.recent.is_empty());

        handle.restore_recent();
        run_frame(&ctx, &mut app, 4.2, Vec::new());
        assert!(app.stack.is_empty());
    }

    #[test]
    fn temporary_hide_pauses_auto_close_until_the_stack_is_shown_again() {
        let geometry = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 480.0));
        let (ctx, mut app, handle) = test_overlay(geometry, None);
        handle.set_auto_close(Some(1.0));
        handle.push(CaptureRequest::new(
            "Shot.png",
            Provenance::Display,
            (1920, 1080),
        ));
        run_frame(&ctx, &mut app, 0.0, Vec::new());
        let id = app.stack.cards()[0].id();
        run_frame(&ctx, &mut app, 0.4, Vec::new());

        handle.set_hidden(true);
        run_frame(&ctx, &mut app, 0.5, Vec::new());
        assert_eq!(app.stack.slot_of(id), Some(0));

        handle.set_hidden(false);
        app.service_hidden(&ctx);
        assert!(!app.hidden, "the logic-only path must show the panel");
        run_frame(&ctx, &mut app, 20.0, Vec::new());
        assert_eq!(
            app.stack.slot_of(id),
            Some(0),
            "a stale hidden clock must not expire the card on first paint"
        );
        run_frame(&ctx, &mut app, 20.999, Vec::new());
        assert_eq!(app.stack.slot_of(id), Some(0));
        run_frame(&ctx, &mut app, 21.0, Vec::new());
        assert!(app.stack.is_empty());
    }

    #[test]
    fn geometry_resizes_before_repositioning_the_window() {
        let geometry = OverlayGeometry::new(rect(0.0, 0.0, 640.0, 480.0));
        let (ctx, mut app, handle) = test_overlay(geometry, None);
        handle.set_geometry(OverlayGeometry::new(rect(100.0, 50.0, 800.0, 600.0)));

        let output = run_frame(&ctx, &mut app, 0.0, Vec::new());
        let commands = &output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport")
            .commands;
        let resize = commands
            .iter()
            .position(|command| matches!(command, egui::ViewportCommand::InnerSize(_)))
            .expect("resize command");
        let reposition = commands
            .iter()
            .position(|command| matches!(command, egui::ViewportCommand::OuterPosition(_)))
            .expect("position command");

        assert!(resize < reposition);
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
        let small = downscale(&big, 100);
        assert_eq!(small.size, [100, 50]);
        assert!(small.pixels.iter().all(|p| *p == egui::Color32::RED));
    }

    #[test]
    fn downscale_leaves_small_images_alone() {
        let small = egui::ColorImage::new([32, 16], vec![egui::Color32::BLUE; 32 * 16]);
        assert_eq!(downscale(&small, 512).size, [32, 16]);
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
}
