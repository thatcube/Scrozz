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

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use egui::{Pos2, Rect, Vec2};
use scrozz_core::{Frame as CaptureFrame, PixelFormat, Provenance};

use crate::card::{self, CardAction, CardContent};
use crate::icons::{Icon, IconStore};
use crate::motion::{Motion, fade};
use crate::paint::{self, Surface};
use crate::stack::{CaptureStack, CardFrame, CardId, CardState, Intent, dock};
use crate::theme::{Appearance, Radius, Theme, corner};

/// How long [`Passthrough::Auto`] waits before dropping click-through for a
/// single frame to re-sample the pointer, when no [`PointerProbe`] is supplied.
pub const RESAMPLE_SECS: f32 = 0.35;

/// Default longest edge, in pixels, of a card thumbnail.
///
/// A 6-card stack of full-resolution 5K captures is well over a gigabyte of
/// texture. Cards are 210 pt wide, so 512 px is already generous at 2×.
pub const THUMBNAIL_PX: u32 = 512;

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
    /// The transparent native viewport. It may extend beyond [`Self::work_area`]
    /// so shadows can fade without moving cards into reserved system UI.
    viewport: Rect,
}

impl OverlayGeometry {
    /// A geometry covering `work_area`.
    #[must_use]
    pub const fn new(work_area: Rect) -> Self {
        Self {
            work_area,
            viewport: work_area,
        }
    }

    /// A geometry whose transparent viewport can bleed beyond the safe area.
    ///
    /// The viewport is expanded to include `work_area` if necessary, so malformed
    /// platform geometry cannot clip the cards themselves.
    #[must_use]
    pub fn with_viewport(work_area: Rect, viewport: Rect) -> Self {
        Self {
            work_area,
            viewport: viewport.union(work_area),
        }
    }

    /// The window's outer position.
    #[must_use]
    pub fn position(self) -> Pos2 {
        self.viewport.min
    }

    /// The window's size.
    #[must_use]
    pub fn size(self) -> Vec2 {
        self.viewport.size()
    }

    /// The complete transparent native viewport in global coordinates.
    #[must_use]
    pub fn viewport(self) -> Rect {
        self.viewport
    }

    /// The work area expressed in the window's own coordinates, which is what
    /// the stack lays out in.
    #[must_use]
    pub fn local(self) -> Rect {
        self.work_area.translate(-self.viewport.min.to_vec2())
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
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            geometry: OverlayGeometry::default(),
            appearance: Appearance::Dark,
            passthrough: Passthrough::default(),
            probe: None,
            panel: None,
            thumbnail_px: THUMBNAIL_PX,
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
        })
    }

    /// Attach a thumbnail.
    #[must_use]
    pub fn with_thumbnail(mut self, image: egui::ColorImage) -> Self {
        self.thumbnail = Some(image);
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
    },
    /// A drag began on a card. The host starts the platform drag here.
    DragStarted {
        /// The card.
        id: CardId,
        /// Where the pointer was, in window coordinates.
        at: Pos2,
    },
    /// A drag committed to leaving the pile *while the button is still down*.
    ///
    /// This is the one the host acts on: it is the last moment at which a
    /// native drag session can still be started. The overlay makes no further
    /// claim about the card — it will spring back on release — and the platform
    /// drag becomes the sole authority on what happens next. The host removes
    /// the card only once the drop is accepted.
    DragOutArmed {
        /// The card.
        id: CardId,
        /// Where the card is on screen right now, in window coordinates. The
        /// platform uses this as the drag image's starting frame.
        card: Rect,
        /// Where the pointer is right now, in window coordinates.
        pointer: Pos2,
    },
    /// A drag committed to leaving the pile, observed at release.
    ///
    /// Emitted only when no host took over via [`OverlayEvent::DragOutArmed`],
    /// so a platform without a native drag source still sees the gesture.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Dismiss(CardId),
    SettleDrag { id: CardId, accepted: bool },
    DismissAll,
    Collapse,
    Expand,
    ToggleDock,
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

    /// Places an event in the outbox as though the overlay had raised it.
    ///
    /// The renderer does not use this — it emits directly. This is for a host
    /// that notices something natively which the renderer cannot see, and for
    /// tests that need to drive a host's event translation without a window.
    pub fn report(&self, event: OverlayEvent) {
        if let Ok(mut q) = self.shared.outbox.lock() {
            q.push(event);
        }
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

    /// Reports that the native drag for `id` has finished.
    ///
    /// # Why the host has to tell the overlay this
    ///
    /// Once a drag-out is armed the platform owns the gesture, and every
    /// platform runs it as a *modal* loop: AppKit's dragging session and
    /// Windows' `DoDragDrop` both pump events themselves until the drop is
    /// over. The mouse-up that ended the drag is consumed in there and never
    /// reaches egui, so the surface's own `drag_stopped` may simply never fire.
    ///
    /// Left alone, the card stays held and displaced under a pointer that is no
    /// longer pressing it, and — because a gesture is only armed once — it can
    /// never be dragged again. The window looks broken and nothing has logged a
    /// thing.
    ///
    /// So the outcome comes back the way it went out: explicitly. Call this for
    /// **every** ending, not just the happy one. `accepted` distinguishes a
    /// drop something took from one that was cancelled, refused, or failed; all
    /// four release the gesture, and only the first is followed by the host
    /// retiring the card.
    pub fn settle_drag(&self, id: CardId, accepted: bool) {
        self.command(Command::SettleDrag { id, accepted });
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
        // Windows otherwise registers this full-work-area, always-on-top
        // source window as an inbound OLE drop target. It can then accept its
        // own outgoing CF_HDROP before the application underneath ever sees it.
        .with_drag_and_drop(false)
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
// Card hover
// ---------------------------------------------------------------------------

/// Which card, if any, sits under the pointer — hit-tested from the same
/// authoritative pointer source click-through already trusts, rather than
/// read back from whether egui has *observed* a pointer event over that card.
///
/// # Why `Response::hovered()` is not enough on its own
///
/// egui only updates a widget's hover state when it actually receives a
/// pointer-moved (or similar) event at that screen position. A retained card
/// window can expand underneath a pointer that has not moved — the ordinary
/// shape of "reveal a card that just grew into place" — and winit has nothing
/// to deliver in that case, so `hovered()` stays false for every card until
/// the next click manufactures an event. [`OverlayApp::pointer`] already
/// tracks the pointer independently of that, through the macOS
/// [`PointerProbe`] when one is installed, for exactly the reason that
/// click-through cannot trust egui's pointer state either (see
/// [`passes_through`]); this reuses the same source for hover.
///
/// `hits` must already be in back-to-front paint order, as produced by
/// [`sort_card_frames_for_painting`] — later entries are on top and win where
/// two cards' rectangles overlap.
#[must_use]
pub fn hovered_card(pointer: Option<Pos2>, hits: &[(CardId, Rect)]) -> Option<CardId> {
    let p = pointer?;
    hits.iter()
        .rev()
        .find(|(_, rect)| rect.contains(p))
        .map(|(id, _)| *id)
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

struct Entry {
    name: String,
    provenance: Provenance,
    source_px: (u32, u32),
    texture: Option<egui::TextureHandle>,
    pending: Option<egui::ColorImage>,
}

/// The `eframe` application that hosts the capture stack.
pub struct OverlayApp {
    stack: CaptureStack,
    content: HashMap<CardId, Entry>,
    handle: OverlayHandle,
    theme: Theme,
    icons: IconStore,
    geometry: OverlayGeometry,
    passthrough: Passthrough,
    probe: Option<PointerProbe>,
    thumbnail_px: u32,
    /// The value most recently sent to the window, so the command is sent on
    /// change rather than every frame. `None` means native code changed the
    /// window behind this renderer and the next frame must reassert its choice.
    passthrough_now: Option<bool>,
    /// When the pointer was last actually known, for re-sampling.
    last_seen: f64,
    hovered: Option<CardId>,
    dock_collapsed: bool,
    dragging: Option<CardId>,
    /// The card whose drag-out a host has already been handed, if any.
    ///
    /// Set the instant the live gesture commits, so the release path knows the
    /// platform now owns this drag and must not be told about it a second time.
    armed: Option<CardId>,
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
            handle,
            theme,
            icons: IconStore::new(ctx),
            geometry: options.geometry,
            passthrough: options.passthrough,
            probe: options.probe,
            thumbnail_px: options.thumbnail_px.max(1),
            passthrough_now: None,
            last_seen: 0.0,
            hovered: None,
            dock_collapsed: false,
            dragging: None,
            armed: None,
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

    /// Forces the next card frame to reassert native mouse passthrough.
    ///
    /// Selection temporarily takes exclusive input through the same native
    /// window. Its behavior changes bypass this renderer, so the cached value is
    /// no longer authoritative when the cards return.
    pub fn invalidate_passthrough_cache(&mut self) {
        self.passthrough_now = None;
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

    fn ingest(&mut self, m: &Motion) {
        for request in self.take_inbox() {
            let id = self.stack.push(m);
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
                Command::SettleDrag { id, accepted } => {
                    // Uniform on purpose: cancelled, refused and failed are
                    // three ways of saying the card did not go anywhere, and
                    // the card's answer to all three is to sit back down. Only
                    // the arming state is per-card, so only that is guarded.
                    let stuck = self.stack.settle_drag(id, m);
                    if self.armed == Some(id) {
                        self.armed = None;
                    }
                    if self.dragging == Some(id) {
                        self.dragging = None;
                    }
                    tracing::debug!(card = id.0, accepted, stuck, "overlay: native drag settled");
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
            self.emit(OverlayEvent::Dismissed {
                id,
                reason: DismissReason::Overflow,
            });
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
                    self.passthrough_now.unwrap_or(false)
                }
            }
        };

        if self.passthrough == Passthrough::Auto && self.probe.is_none() && desired && !empty {
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(RESAMPLE_SECS));
        }
        if self.passthrough_now != Some(desired) {
            self.passthrough_now = Some(desired);
            ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(desired));
        }
    }

    fn handle_action(&mut self, id: CardId, action: CardAction, m: &Motion) {
        let (event, dismiss) = match action {
            CardAction::Copy => (Some(OverlayEvent::CopyRequested { id }), true),
            CardAction::Save => (Some(OverlayEvent::SaveRequested { id }), true),
            CardAction::Annotate => (Some(OverlayEvent::AnnotateRequested { id }), false),
            CardAction::Upload => (Some(OverlayEvent::UploadRequested { id }), false),
            CardAction::Pin => (Some(OverlayEvent::PinRequested { id }), false),
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

/// Paints settled residents from top to bottom. The card below consequently
/// covers the downward shadow of the card above instead of that shadow muddying
/// its thumbnail. Transient cards stay above the settled pile.
fn sort_card_frames_for_painting(frames: &mut [CardFrame]) {
    frames.sort_by_key(|frame| {
        let layer = match frame.state {
            CardState::Entering | CardState::Resting | CardState::Falling => 0,
            CardState::Departing => 1,
            CardState::Returning => 2,
            CardState::Dragging => 3,
        };
        (layer, Reverse(frame.slot))
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

        self.run_commands(&ctx, &m);
        self.ingest(&m);
        self.reconcile(&ctx);
        self.stack.advance(&m);

        let was_empty = self.stack.is_empty();
        let dock_was = self.stack.dock().is_collapsed();

        let surface = Surface::new(&self.theme, &self.icons, m);
        let mut frames = self.stack.frame(&m);
        sort_card_frames_for_painting(&mut frames);

        // Computed up front, not just before `apply_passthrough` as before: the
        // same authoritative position now also drives card hover below, since
        // it is exactly what stays correct when winit has not delivered a
        // fresh pointer event over a card that just moved or grew under a
        // stationary pointer.
        let pointer = self.pointer(&ctx);

        let mut hits: Vec<Rect> = Vec::with_capacity(frames.len() + 1);
        let mut card_hits: Vec<(CardId, Rect)> = Vec::with_capacity(frames.len());
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
            hits.push(response.hit);
            card_hits.push((f.id, response.hit));

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

        // The geometric read wins whenever it has an answer: it is the one
        // that still works when egui's own hover state is stale. It only
        // falls back to egui's `hovered()` result when the pointer itself is
        // unknown (no probe and no recent egui pointer event either).
        hovered = hovered_card(pointer, &card_hits).or(hovered);

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
            self.armed = None;
            self.emit(OverlayEvent::DragStarted { id, at: pointer });
        }
        if let Some(p) = drag_to {
            self.stack.drag_to(p, &m);
        }
        // Hand a committed drag-out over *before* the button comes up. AppKit
        // will not start a dragging session from a released mouse, so waiting
        // for `drag_stopped()` is waiting until it is too late — that is
        // precisely the bug where the card animates away and nothing ever
        // drops.
        if self.armed.is_none()
            && let Some(live) = self.stack.live_gesture(&m)
            && live.intent == Intent::DragOut
        {
            self.armed = Some(live.id);
            self.emit(OverlayEvent::DragOutArmed {
                id: live.id,
                card: live.rect,
                pointer: live.pointer,
            });
        }
        if drag_end {
            if self.armed.is_some() {
                // The platform owns this gesture now. The card springs back to
                // its slot and stays there; it leaves the pile only if the drop
                // is accepted, which the host reports separately. Re-emitting
                // the drag-out here would race the native session and retire a
                // capture that was never delivered (D14).
                self.stack.cancel_drag(&m);
            } else if let Some(release) = self.stack.release_drag(&m) {
                let at = release.rect.center();
                match release.intent {
                    Intent::Dismiss => self.emit(OverlayEvent::Dismissed {
                        id: release.id,
                        reason: DismissReason::Swipe,
                    }),
                    Intent::DragOut => {
                        self.emit(OverlayEvent::DragOut { id: release.id, at });
                    }
                    Intent::Collapse | Intent::SpringBack => {}
                }
            }
            self.dragging = None;
            self.armed = None;
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
    fn geometry_keeps_shadow_bleed_outside_the_card_safe_area() {
        let work_area = rect(80.0, 25.0, 1360.0, 800.0);
        let viewport = rect(0.0, 25.0, 1440.0, 848.0);
        let g = OverlayGeometry::with_viewport(work_area, viewport);

        assert_eq!(g.position(), viewport.min);
        assert_eq!(g.size(), viewport.size());
        assert_eq!(g.viewport(), viewport);
        assert_eq!(g.local(), rect(80.0, 0.0, 1360.0, 800.0));
    }

    #[test]
    fn lower_residents_paint_above_upper_card_shadows() {
        let frame = |id, slot, state| CardFrame {
            id: CardId(id),
            slot,
            rect: rect(40.0, slot as f32 * 158.0, 210.0, 150.0),
            alpha: 1.0,
            reveal: 0.0,
            lift: 0.0,
            angle: 0.0,
            state,
        };
        let mut frames = vec![
            frame(1, 0, CardState::Resting),
            frame(2, 1, CardState::Resting),
            frame(3, 2, CardState::Resting),
            frame(4, 0, CardState::Departing),
            frame(5, 2, CardState::Returning),
            frame(6, 1, CardState::Dragging),
        ];

        sort_card_frames_for_painting(&mut frames);

        assert_eq!(
            frames.iter().map(|frame| frame.id.0).collect::<Vec<_>>(),
            vec![3, 2, 1, 4, 5, 6],
            "residents paint top-to-bottom, then transient cards paint above the pile"
        );
    }

    #[test]
    fn passthrough_is_off_over_a_card_and_on_beside_it() {
        let card = rect(40.0, 700.0, 210.0, 150.0);
        assert!(!passes_through(Some(Pos2::new(100.0, 760.0)), &[card]));
        assert!(passes_through(Some(Pos2::new(600.0, 400.0)), &[card]));
    }

    #[test]
    fn unknown_pointer_never_passes_through() {
        // With something to hit, an unknown pointer keeps its clicks: the
        // alternative is an overlay that can never be touched again.
        let card = rect(40.0, 700.0, 210.0, 150.0);
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
    fn hover_is_found_from_the_authoritative_pointer_alone() {
        // No `Response::hovered()` in sight: a stationary pointer over a card
        // that only just grew into place must still resolve to that card, the
        // exact case where egui's own hover state has nothing to go on yet.
        let card = rect(40.0, 700.0, 210.0, 150.0);
        assert_eq!(
            hovered_card(Some(Pos2::new(100.0, 760.0)), &[(CardId(1), card)]),
            Some(CardId(1))
        );
    }

    #[test]
    fn hover_is_none_beside_every_card() {
        let card = rect(40.0, 700.0, 210.0, 150.0);
        assert_eq!(
            hovered_card(Some(Pos2::new(600.0, 400.0)), &[(CardId(1), card)]),
            None
        );
    }

    #[test]
    fn hover_is_none_without_a_known_pointer() {
        let card = rect(40.0, 700.0, 210.0, 150.0);
        assert_eq!(hovered_card(None, &[(CardId(1), card)]), None);
    }

    #[test]
    fn hover_is_none_with_no_cards_at_all() {
        assert_eq!(hovered_card(Some(Pos2::new(1.0, 1.0)), &[]), None);
    }

    #[test]
    fn overlapping_cards_resolve_to_the_topmost_one() {
        // `hits` mirrors paint order: later entries are on top, matching
        // `sort_card_frames_for_painting`'s back-to-front convention.
        let overlap = rect(40.0, 700.0, 210.0, 150.0);
        let hits = [(CardId(1), overlap), (CardId(2), overlap)];
        assert_eq!(
            hovered_card(Some(Pos2::new(100.0, 760.0)), &hits),
            Some(CardId(2)),
            "the last entry paints on top and must win the tie"
        );
    }

    #[test]
    fn hover_survives_no_egui_response_over_a_freshly_expanded_card() {
        // Regression test for the actual bug report: a retained card window
        // expands under a pointer that never moved, so egui's own
        // `Response::hovered()` would still say false. The geometric read
        // must not depend on that at all.
        let expanded = rect(40.0, 500.0, 210.0, 360.0);
        assert_eq!(
            hovered_card(Some(Pos2::new(120.0, 520.0)), &[(CardId(7), expanded)]),
            Some(CardId(7))
        );
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
