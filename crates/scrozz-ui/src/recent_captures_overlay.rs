//! The floating Recent Captures Overlay window that hosts the capture stack.
//!
//! [`stack`](crate::stack) knows where every card goes; [`card`](crate::card)
//! knows how one is painted. This module is the window they live in, and the
//! seam the rest of the application drives them through.
//!
//! # The window
//!
//! Borderless, fully transparent outside its drawn content, no shadow of its
//! own, absent from the Dock and the taskbar, and always on top. Platforms with a
//! safe native adapter also make it non-activating (D27).
//!
//! [`viewport`] builds the [`egui::ViewportBuilder`] that expresses as much of
//! that as egui can express. `scrozz-ui` does not depend on `scrozz-shell` and
//! is `#![forbid(unsafe_code)]`, so additional native configuration is supplied
//! through [`PanelHook`]. The hook reports whether non-activation is genuinely
//! available through [`RecentCapturesOverlayHandle::panel_report`].
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
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use egui::{Pos2, Rect, Vec2};
use scrozz_core::{
    DisplayId, DisplaySet, Frame as CaptureFrame, LockEscape, LogicalSize, PinChromePolicy, PinId,
    PinState, PinnedSurface, PixelFormat, Provenance, ScaleFactor,
};

use crate::card::{self, CardAction, CardContent, CardMedia};
use crate::icons::{Icon, IconStore};
use crate::motion::{Motion, fade};
use crate::paint::{self, Surface};
use crate::pinned;
use crate::scrolling::{ScrollHudAction, ScrollHudState, ScrollingHud};
pub use crate::stack::RecentCapturesPlacement;
use crate::stack::{CaptureStack, CardFrame, CardId, CardMetrics, CardState, Intent, dock};
use crate::theme::{Appearance, Radius, Theme, corner};

/// How long [`Passthrough::Auto`] waits before dropping click-through for a
/// single frame to re-sample the pointer, when no [`PointerProbe`] is supplied.
pub const RESAMPLE_SECS: f32 = 0.35;

/// Default longest edge, in pixels, of a card thumbnail.
///
/// A 6-card stack of full-resolution 5K captures is well over a gigabyte of
/// texture. Cards are 210 pt wide, so 512 px is already generous at 2×.
pub const THUMBNAIL_PX: u32 = 512;
/// Longest edge accepted for a pinned-capture GPU texture.
pub const PIN_TEXTURE_PX: u32 = 2_048;

/// What automatic cleanup does once a card's interval elapses.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RecentCapturesAutoCloseAction {
    /// Hide the card only when its artifact is already retained elsewhere.
    #[default]
    Hide,
    /// Save an unsaved card to the configured Export Location, then hide it.
    SaveThenHide,
}

impl RecentCapturesAutoCloseAction {
    /// Stable persisted setting value.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Hide => "hide",
            Self::SaveThenHide => "save-then-hide",
        }
    }

    /// Parses a persisted setting value.
    #[must_use]
    pub fn from_slug(value: &str) -> Option<Self> {
        match value {
            "hide" => Some(Self::Hide),
            "save-then-hide" => Some(Self::SaveThenHide),
            _ => None,
        }
    }
}

/// Default behavior of the Save button in the Recent Captures Overlay.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RecentCapturesSaveBehavior {
    /// Save directly to the configured Export Location.
    #[default]
    ExportLocation,
    /// Open the native destination chooser.
    ChooseDestination,
}

impl RecentCapturesSaveBehavior {
    /// Stable persisted setting value.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::ExportLocation => "export-location",
            Self::ChooseDestination => "choose-destination",
        }
    }

    /// Parses a persisted setting value.
    #[must_use]
    pub fn from_slug(value: &str) -> Option<Self> {
        match value {
            "export-location" => Some(Self::ExportLocation),
            "choose-destination" => Some(Self::ChooseDestination),
            _ => None,
        }
    }
}

/// Complete behavior contract for the Recent Captures Overlay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecentCapturesOverlaySettings {
    /// Display edge where cards are anchored.
    pub placement: RecentCapturesPlacement,
    /// Follow the active display instead of staying on the current display.
    pub follow_active_display: bool,
    /// Preferred 16:10 card width in logical points.
    pub card_width: f32,
    /// Whether elapsed-time cleanup is enabled.
    pub auto_close_enabled: bool,
    /// What cleanup does when its interval elapses.
    pub auto_close_action: RecentCapturesAutoCloseAction,
    /// Elapsed interval before cleanup, in seconds.
    pub auto_close_seconds: u32,
    /// Hide after an accepted external drag unless Option/Alt is held.
    pub close_after_drag: bool,
    /// Hide only after a confirmed cloud upload succeeds.
    pub close_after_upload: bool,
    /// Default Save-button routing; Option/Alt temporarily inverts it.
    pub save_behavior: RecentCapturesSaveBehavior,
}

impl Default for RecentCapturesOverlaySettings {
    fn default() -> Self {
        Self {
            placement: RecentCapturesPlacement::Left,
            follow_active_display: false,
            card_width: CardMetrics::PREFERRED_WIDTH,
            auto_close_enabled: false,
            auto_close_action: RecentCapturesAutoCloseAction::Hide,
            auto_close_seconds: 30,
            close_after_drag: true,
            close_after_upload: false,
            save_behavior: RecentCapturesSaveBehavior::ExportLocation,
        }
    }
}

impl RecentCapturesOverlaySettings {
    /// Returns a bounded settings value suitable for layout and timers.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.card_width = self
            .card_width
            .clamp(CardMetrics::MIN_WIDTH, CardMetrics::MAX_WIDTH);
        self.auto_close_seconds = self.auto_close_seconds.clamp(5, 3_600);
        self
    }
}

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

    /// A successful native configuration.
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
/// native configuration cannot live here; the application crate, which depends
/// on both, supplies it.
///
/// [`eframe::CreationContext`] implements `HasWindowHandle`, which is where the
/// platform handle comes from. On macOS the whole hook is roughly:
///
/// ```ignore
/// use raw_window_handle::{HasWindowHandle, RawWindowHandle};
/// use scrozz_shell::overlay::{OverlayBehavior, NativeOverlay};
/// use scrozz_ui::recent_captures_overlay::PanelReport;
///
/// let hook = Box::new(|cc: &eframe::CreationContext<'_>| {
///     let Ok(handle) = cc.window_handle() else {
///         return PanelSetup::unsupported("no window handle");
///     };
///     let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
///         return PanelSetup::unsupported("not an AppKit window");
///     };
///     // SAFETY: the view is alive for as long as the `WindowHandle` borrow.
///     let mut overlay = match unsafe {
///         NativeOverlay::from_ns_view(appkit.ns_view.as_ptr())
///     } {
///         Ok(o) => o,
///         Err(e) => return PanelSetup::unsupported(e.to_string()),
///     };
///     let report = match overlay.apply(&OverlayBehavior::capture_card()) {
///         Ok(r) if r.non_activating => PanelReport::converted(r.detail),
///         Ok(r) => PanelReport::unsupported(r.detail),
///         Err(e) => PanelReport::unsupported(e.to_string()),
///     };
///     PanelSetup::new(report)
/// });
/// ```
///
/// Note the entry point differs by platform: `scrozz-shell` exposes
/// `MacOverlay::from_ns_view` / `from_ns_window` on macOS but `adopt` on its
/// stub platforms, so the hook is written per-target anyway.
///
/// Returning [`PanelSetup::unsupported`] is always safe: the overlay still
/// works, it just takes focus when clicked.
pub type PanelHook = Box<dyn FnOnce(&eframe::CreationContext<'_>) -> PanelSetup>;

/// Applies a click-through transition through the native window API and reads
/// the state back.
///
/// Returning `true` certifies that the requested state reached the native
/// window; a queued viewport command alone is not an acknowledgement. Automatic
/// scrolling depends on this distinction: globally addressed scroll input must
/// not be synthesised until the overlay is genuinely transparent to the
/// pointer, or the overlay eats the very wheel events the capture needs.
pub type NativePassthrough = Box<dyn FnMut(bool) -> Result<bool, String>>;

/// Native resources retained after the panel hook configures the window.
pub struct PanelSetup {
    /// Whether native panel conversion succeeded.
    pub report: PanelReport,
    /// Optional synchronous native click-through apply-and-readback.
    pub passthrough: Option<NativePassthrough>,
}

impl PanelSetup {
    /// Creates a setup without a native click-through controller.
    #[must_use]
    pub fn new(report: PanelReport) -> Self {
        Self {
            report,
            passthrough: None,
        }
    }

    /// Attaches a native click-through controller.
    #[must_use]
    pub fn with_passthrough(mut self, passthrough: NativePassthrough) -> Self {
        self.passthrough = Some(passthrough);
        self
    }

    /// Creates a setup for a platform or window kind that cannot be converted.
    #[must_use]
    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::new(PanelReport::unsupported(detail))
    }
}

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
pub struct RecentCapturesOverlayGeometry {
    /// The display's work area: bounds minus the menu bar and the Dock.
    pub work_area: Rect,
    /// The transparent native viewport. It may extend beyond [`Self::work_area`]
    /// so shadows can fade without moving cards into reserved system UI.
    viewport: Rect,
}

impl RecentCapturesOverlayGeometry {
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

    /// A content-bounded viewport over a larger global work area.
    ///
    /// Capture cards still lay out against the complete work area, but their
    /// native host needs to cover only the occupied stack column. The viewport
    /// may therefore be smaller than `work_area`; [`Self::local`] preserves the
    /// offset that keeps the same global card positions.
    #[must_use]
    pub fn with_content_viewport(work_area: Rect, viewport: Rect) -> Self {
        Self {
            work_area,
            viewport,
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

impl Default for RecentCapturesOverlayGeometry {
    fn default() -> Self {
        Self::new(Rect::from_min_size(Pos2::ZERO, Vec2::new(1440.0, 875.0)))
    }
}

/// Everything the overlay needs that is not the stack itself.
pub struct RecentCapturesOverlayOptions {
    /// Where the window goes.
    pub geometry: RecentCapturesOverlayGeometry,
    /// Light or dark.
    pub appearance: Appearance,
    /// Click-through policy.
    pub passthrough: Passthrough,
    /// Optional exact pointer source; see [`PointerProbe`].
    pub probe: Option<PointerProbe>,
    /// Optional safe native configuration; see [`PanelHook`].
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
    /// User-configurable Recent Captures behavior.
    pub settings: RecentCapturesOverlaySettings,
}

impl Default for RecentCapturesOverlayOptions {
    fn default() -> Self {
        let geometry = RecentCapturesOverlayGeometry::default();
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
            settings: RecentCapturesOverlaySettings::default(),
        }
    }
}

impl std::fmt::Debug for RecentCapturesOverlayOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecentCapturesOverlayOptions")
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
            .field("settings", &self.settings)
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

/// What kind of finished media a request carries.
///
/// Deliberately a value rather than a flag: a recording card needs its duration
/// and whether it has audio, and the overlay must never have to go and ask for
/// them while painting.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum CaptureMedia {
    /// A still image capture.
    #[default]
    Image,
    /// A finalized recording whose durable file is owned outside the overlay.
    Video {
        /// Native container duration.
        duration: std::time::Duration,
        /// Whether the durable source carries audio.
        has_audio: bool,
    },
}

impl CaptureMedia {
    const fn card_media(self) -> CardMedia {
        match self {
            Self::Image => CardMedia::Image,
            Self::Video {
                duration,
                has_audio,
            } => CardMedia::Video {
                duration,
                has_audio,
            },
        }
    }

    /// Whether this request presents playable video.
    #[must_use]
    pub const fn is_video(self) -> bool {
        matches!(self, Self::Video { .. })
    }
}

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
    /// Whether this card is a still or a recording.
    pub media: CaptureMedia,
    /// A pre-scaled thumbnail. `None` shows a holding fill until one arrives.
    pub thumbnail: Option<egui::ColorImage>,
    /// Explicit content failure for a durable pin whose state remains recoverable.
    pub content_error: Option<String>,
    /// Whether Upload can act on this capture at all.
    pub upload_available: bool,
    /// Why Upload is unavailable, shown on the disabled control.
    pub upload_unavailable_reason: Option<String>,
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
            media: CaptureMedia::Image,
            thumbnail: None,
            content_error: None,
            // Off until the app says otherwise: offering Upload before the
            // provider has been checked would put a control on the card that
            // can only fail.
            upload_available: false,
            upload_unavailable_reason: None,
        }
    }

    /// Records what Upload can do for this capture.
    #[must_use]
    pub fn with_upload_availability(mut self, available: bool, reason: Option<String>) -> Self {
        self.upload_available = available;
        self.upload_unavailable_reason = reason;
        self
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
            media: CaptureMedia::Image,
            thumbnail: Some(thumbnail),
            content_error: None,
            upload_available: false,
            upload_unavailable_reason: None,
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

    /// Present this request as a recording rather than a still.
    #[must_use]
    pub const fn with_media(mut self, media: CaptureMedia) -> Self {
        self.media = media;
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
pub enum RecentCapturesOverlayEvent {
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
        /// Whether the native destination chooser should be used.
        choose_destination: bool,
    },
    /// Open this capture in the annotation editor.
    AnnotateRequested {
        /// The card.
        id: CardId,
    },
    /// Open this recording in the video editor.
    ///
    /// Distinct from [`RecentCapturesOverlayEvent::AnnotateRequested`] because the two open
    /// different editors over different documents; collapsing them would make a
    /// video silently open an annotation surface it has no raster for.
    EditRequested {
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
        /// Option/Alt was held when the native hand-off committed.
        keep_after_accept: bool,
    },
    /// A card's elapsed cleanup interval expired.
    AutoCloseRequested {
        /// The card.
        id: CardId,
        /// Safe cleanup action selected in Settings.
        action: RecentCapturesAutoCloseAction,
    },
    /// A drag committed to leaving the pile, observed at release.
    ///
    /// Emitted only when no host took over via [`RecentCapturesOverlayEvent::DragOutArmed`],
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
    /// A decision from the scrolling-capture HUD.
    Scrolling(ScrollHudAction),
}

/// Something the application asks the overlay to do.
#[derive(Clone, Debug)]
enum Command {
    Dismiss(CardId),
    SetStatus {
        id: CardId,
        status: Option<String>,
    },
    SetUploadAvailability {
        id: CardId,
        available: bool,
        reason: Option<String>,
    },
    SetEditing {
        id: CardId,
        editing: bool,
    },
    SetCardImage {
        id: CardId,
        image: egui::ColorImage,
    },
    SettleDrag {
        id: CardId,
        accepted: bool,
    },
    DismissAll,
    Collapse,
    Expand,
    ToggleDock,
    Configure(RecentCapturesOverlaySettings),
    // Boxed: a restore carries a complete capture request, which is an order
    // of magnitude larger than every other command, and the queue holds one
    // enum-sized slot per entry.
    RestorePin {
        request: Box<CaptureRequest>,
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
    outbox: Mutex<Vec<RecentCapturesOverlayEvent>>,
    commands: Mutex<Vec<Command>>,
    scroll_hud: Mutex<Option<ScrollHudState>>,
    ctx: Mutex<Option<egui::Context>>,
    report: Mutex<Option<PanelReport>>,
    native_pins: Mutex<Vec<NativePinRequest>>,
    visible_content: AtomicBool,
    visible_card_count: AtomicUsize,
    geometry_locked: AtomicBool,
    close_requested: AtomicBool,
    scroll_passthrough_requested: AtomicBool,
    passthrough_applied: AtomicBool,
    applied_settings: Mutex<RecentCapturesOverlaySettings>,
}

/// The application's grip on a running overlay.
///
/// Cheap to clone, safe to hold on another thread, and usable *before* the
/// window exists: a hotkey handler can be wired to a handle at start-up and the
/// first capture pushed through it will be waiting when the window opens.
#[derive(Clone, Default)]
pub struct RecentCapturesOverlayHandle {
    shared: Arc<Shared>,
}

impl RecentCapturesOverlayHandle {
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
    pub fn report(&self, event: RecentCapturesOverlayEvent) {
        if let Ok(mut q) = self.shared.outbox.lock() {
            q.push(event);
        }
    }

    /// Show or update the scrolling-capture HUD.
    pub fn show_scroll_hud(&self, state: ScrollHudState) {
        if let Ok(mut slot) = self.shared.scroll_hud.lock() {
            *slot = Some(state);
        }
        self.wake();
    }

    /// Hide the scrolling-capture HUD.
    pub fn hide_scroll_hud(&self) {
        if let Ok(mut slot) = self.shared.scroll_hud.lock() {
            *slot = None;
        }
        self.wake();
    }

    /// Whether the scrolling-capture HUD is currently shown.
    #[must_use]
    pub fn scroll_hud_visible(&self) -> bool {
        self.shared
            .scroll_hud
            .lock()
            .is_ok_and(|slot| slot.is_some())
    }

    /// Keep the native overlay mouse-transparent while automatic scrolling is
    /// delivering globally addressed input.
    pub fn request_scroll_passthrough(&self, requested: bool) {
        self.shared
            .scroll_passthrough_requested
            .store(requested, Ordering::Release);
        self.wake();
    }

    /// Whether the overlay has applied mouse transparency to its native window.
    #[must_use]
    pub fn scroll_passthrough_ready(&self) -> bool {
        self.shared.passthrough_applied.load(Ordering::Acquire)
    }

    /// Take everything that has happened since the last call.
    #[must_use]
    pub fn drain_events(&self) -> Vec<RecentCapturesOverlayEvent> {
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

    /// Shows or clears the action status line on one card.
    pub fn set_status(&self, id: CardId, status: Option<String>) {
        self.command(Command::SetStatus { id, status });
    }

    /// Updates what Upload can do for one card.
    ///
    /// Sent for every visible card whenever provider settings change, so a
    /// control never claims a capability the app has since lost.
    pub fn set_upload_availability(&self, id: CardId, available: bool, reason: Option<String>) {
        self.command(Command::SetUploadAvailability {
            id,
            available,
            reason,
        });
    }

    /// Tells the overlay whether a card's editor is open.
    ///
    /// Drives the card's morphing Editing/Continue pill and pauses its
    /// auto-close timer for as long as `editing` is `true`; the timer resumes
    /// with its full configured duration once it goes back to `false`.
    pub fn set_editing(&self, id: CardId, editing: bool) {
        self.command(Command::SetEditing { id, editing });
    }

    /// Replaces a card's visible thumbnail with a freshly rendered image.
    ///
    /// The card keeps its pre-edit thumbnail for the entire editing session —
    /// this is only ever called once, when the editor commits (Done), so the
    /// card never shows an ambiguous intermediate revision while a document
    /// is still being annotated. Downscaled the same way an incoming capture
    /// is, so a committed edit cannot suddenly demand a larger GPU texture
    /// than the card ever needed before.
    pub fn set_card_image(&self, id: CardId, image: egui::ColorImage) {
        self.command(Command::SetCardImage { id, image });
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

    /// Applies Recent Captures settings to current and future cards.
    pub fn configure(&self, settings: RecentCapturesOverlaySettings) {
        self.command(Command::Configure(settings.normalized()));
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
        self.command(Command::RestorePin {
            request: Box::new(request),
            state,
        });
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
        self.shared.close_requested.store(true, Ordering::Release);
        self.wake();
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

    /// Whether a card, departure animation, or dock is currently drawable.
    #[must_use]
    pub fn has_visible_content(&self) -> bool {
        self.shared.visible_content.load(Ordering::Acquire)
    }

    /// Number of card frames still drawable, including departure animations.
    #[must_use]
    pub fn visible_card_count(&self) -> usize {
        self.shared.visible_card_count.load(Ordering::Acquire)
    }

    /// Settings the renderer has safely applied to current card geometry.
    #[must_use]
    pub fn applied_settings(&self) -> RecentCapturesOverlaySettings {
        self.shared.applied_settings.lock().map_or_else(
            |_| RecentCapturesOverlaySettings::default(),
            |settings| *settings,
        )
    }

    /// Whether pointer state is currently expressed in this viewport's local
    /// coordinates and the viewport origin must not move.
    #[must_use]
    pub fn geometry_locked(&self) -> bool {
        self.shared.geometry_locked.load(Ordering::Acquire)
    }

    /// Whether a newly submitted card still waits for the overlay's next pass.
    #[must_use]
    pub fn has_pending_content(&self) -> bool {
        self.shared
            .inbox
            .lock()
            .is_ok_and(|inbox| !inbox.is_empty())
    }

    /// Whether the native card surface needs to be ordered in.
    #[must_use]
    pub fn needs_visible_surface(&self) -> bool {
        self.has_pending_content() || self.has_visible_content()
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

    fn take_close_request(&self) -> bool {
        self.shared.close_requested.swap(false, Ordering::AcqRel)
    }
}

impl std::fmt::Debug for RecentCapturesOverlayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecentCapturesOverlayHandle")
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
/// also applies any safe native properties the platform exposes, and why
/// [`PanelReport`] exists to report whether non-activation is genuinely available.
#[must_use]
pub fn viewport(geometry: RecentCapturesOverlayGeometry) -> egui::ViewportBuilder {
    let builder = egui::ViewportBuilder::default()
        .with_title("Scrozz Recent Captures Overlay")
        .with_app_id("com.scrozz.recent-captures-overlay")
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
        // Do not request focus when the window opens. Native adapters may add
        // stronger guarantees where the platform exposes them safely.
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
pub fn native_options(geometry: RecentCapturesOverlayGeometry) -> eframe::NativeOptions {
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
/// the next click manufactures an event. [`RecentCapturesOverlayApp::pointer`] already
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
        let y0 = y.checked_mul(h)? / nh;
        let y1 = (y + 1).checked_mul(h)?.div_ceil(nh).min(h).max(y0 + 1);
        for x in 0..nw {
            let x0 = x.checked_mul(w)? / nw;
            let x1 = (x + 1).checked_mul(w)?.div_ceil(nw).min(w).max(x0 + 1);
            let mut sums = [0u128; 4];
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
                    for (sum, channel) in sums.iter_mut().zip(channels) {
                        *sum += u128::from(channel);
                    }
                    count += 1;
                }
            }
            let channels = sums.map(|sum| u8::try_from(sum / count).unwrap_or(u8::MAX));
            output.push(if premultiplied {
                egui::Color32::from_rgba_premultiplied(
                    channels[0],
                    channels[1],
                    channels[2],
                    channels[3],
                )
            } else {
                egui::Color32::from_rgba_unmultiplied(
                    channels[0],
                    channels[1],
                    channels[2],
                    channels[3],
                )
            });
        }
    }
    Some(egui::ColorImage::new([nw, nh], output))
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
    media: CaptureMedia,
    texture: Option<egui::TextureHandle>,
    pending: Option<egui::ColorImage>,
    pin_notice: Option<String>,
    auto_close_started_at: f64,
    /// Whether this card's editor is open. Freezes the auto-close timer at
    /// its current elapsed time while `true`; going back to `false` restarts
    /// it with the full configured duration, so an edit never eats into the
    /// window the user gets to look at the result afterward.
    editing: bool,
    upload_available: bool,
    upload_unavailable_reason: Option<String>,
    status: Option<String>,
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
pub struct RecentCapturesOverlayApp {
    stack: CaptureStack,
    content: HashMap<CardId, Entry>,
    pins: HashMap<PinId, PinnedEntry>,
    pending_pin_cards: HashMap<PinId, CardId>,
    pinned_cards: HashSet<CardId>,
    handle: RecentCapturesOverlayHandle,
    theme: Theme,
    icons: IconStore,
    geometry: RecentCapturesOverlayGeometry,
    passthrough: Passthrough,
    probe: Option<PointerProbe>,
    native_passthrough: Option<NativePassthrough>,
    thumbnail_px: u32,
    displays: DisplaySet,
    active_display: Option<DisplayId>,
    pin_support: PinSupport,
    pin_lock_escapes: Vec<LockEscape>,
    pin_topology_probe: Option<PinTopologyProbe>,
    #[cfg(not(target_os = "macos"))]
    last_pin_topology_refresh: f64,
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
    settings: RecentCapturesOverlaySettings,
    pending_settings: Option<RecentCapturesOverlaySettings>,
}

/// Keeps screen-anchored overlay geometry in native logical points.
///
/// Egui zoom scales viewport commands as well as widgets. A capture overlay's
/// coordinates already come from the operating system, so applying an
/// additional user zoom would move/resize the native window and make a
/// 210-point card cease to match its window-server frame.
pub fn install_native_point_scale(ctx: &egui::Context) {
    ctx.options_mut(|options| options.zoom_with_keyboard = false);
    ctx.set_zoom_factor(1.0);
}

impl RecentCapturesOverlayApp {
    /// Build the overlay.
    ///
    /// Installs fonts and the style, uploads the icon set, binds `handle` to
    /// this window, and runs the native [`PanelHook`] if one was supplied.
    #[must_use]
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        handle: RecentCapturesOverlayHandle,
        mut options: RecentCapturesOverlayOptions,
    ) -> Self {
        let ctx = &cc.egui_ctx;
        install_native_point_scale(ctx);
        let theme = Theme::for_appearance(options.appearance);
        crate::theme::install_fonts(ctx);
        crate::theme::install_style(ctx, &theme);

        if let Ok(mut slot) = handle.shared.ctx.lock() {
            *slot = Some(ctx.clone());
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::ContentProtected(true));

        let setup = options.panel.take().map_or_else(
            || PanelSetup::unsupported("no native panel hook supplied"),
            |hook| hook(cc),
        );
        let PanelSetup {
            report,
            passthrough: native_passthrough,
        } = setup;
        if !report.non_activating {
            tracing::warn!(
                detail = %report.detail,
                "Recent Captures Overlay window is not non-activating"
            );
        }

        if let Ok(mut slot) = handle.shared.report.lock() {
            *slot = Some(report);
        }
        if let Ok(mut slot) = handle.shared.applied_settings.lock() {
            *slot = options.settings;
        }

        if options.passthrough == Passthrough::Auto && options.probe.is_none() {
            tracing::warn!(
                "no pointer probe: click-through will re-sample every {RESAMPLE_SECS}s, \
                 which is imprecise on platforms that stop delivering pointer events \
                 to a click-through window"
            );
        }

        Self {
            stack: CaptureStack::configured(
                options.geometry.local(),
                options.settings.placement,
                options.settings.card_width,
            ),
            content: HashMap::new(),
            pins: HashMap::new(),
            pending_pin_cards: HashMap::new(),
            pinned_cards: HashSet::new(),
            handle,
            theme,
            icons: IconStore::new(ctx),
            geometry: options.geometry,
            passthrough: options.passthrough,
            probe: options.probe,
            native_passthrough,
            thumbnail_px: options.thumbnail_px.max(1),
            displays: options.displays,
            active_display: options.active_display,
            pin_support: options.pin_support,
            pin_lock_escapes: options.pin_lock_escapes,
            pin_topology_probe: options.pin_topology_probe,
            #[cfg(not(target_os = "macos"))]
            last_pin_topology_refresh: f64::NEG_INFINITY,
            passthrough_now: None,
            last_seen: 0.0,
            hovered: None,
            dock_collapsed: false,
            dragging: None,
            armed: None,
            settings: options.settings.normalized(),
            pending_settings: None,
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
    pub fn geometry(&self) -> RecentCapturesOverlayGeometry {
        self.geometry
    }

    /// Move the overlay to a new work area, e.g. after a display change.
    pub fn set_geometry(
        &mut self,
        geometry: RecentCapturesOverlayGeometry,
        ctx: &egui::Context,
        m: &Motion,
    ) {
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

    /// Restores pointer input before the host begins a potentially blocking
    /// shutdown.
    ///
    /// A scrolling capture that is still running holds the overlay
    /// mouse-transparent. This applies the reversal through the native
    /// controller immediately rather than waiting for another frame that may
    /// never be drawn, so a quit during a scroll never leaves a click-through
    /// window behind.
    pub fn prepare_shutdown(&mut self, ctx: &egui::Context) {
        self.passthrough = Passthrough::Never;
        self.handle.request_scroll_passthrough(false);
        self.handle.hide_scroll_hud();
        self.passthrough_now = Some(false);
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
        self.acknowledge_passthrough(false, false);
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
            self.emit(RecentCapturesOverlayEvent::PinUpdated { pin, state });
        }
        for pin in closed {
            self.close_pin(ctx, &pin);
        }
    }

    /// Refreshes display-dependent pin state after a native display-change event
    /// or immediately before creating a pin.
    pub fn refresh_pin_topology(&mut self) {
        let topology = self.pin_topology_probe.as_ref().and_then(|probe| probe());
        let Some(topology) = topology else {
            return;
        };
        self.apply_pin_topology(topology);
    }

    /// Applies an already-queried topology to pin state.
    ///
    /// Hosts with native display-change notifications use this to update the
    /// root and pinned children from the same event snapshot.
    pub fn apply_pin_topology(&mut self, topology: PinTopology) {
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
            self.emit(RecentCapturesOverlayEvent::PinUpdated { pin, state });
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn refresh_live_pin_topology_if_due(&mut self, now: f64) {
        const REFRESH_SECONDS: f64 = 1.0;
        if self.pins.is_empty() || now - self.last_pin_topology_refresh < REFRESH_SECONDS {
            return;
        }
        self.last_pin_topology_refresh = now;
        self.refresh_pin_topology();
    }

    /// Processes commands that must work while the native root is ordered out.
    ///
    /// Eframe calls app logic for a hidden root but skips its UI pass. Close is
    /// therefore kept out of the ordinary card-command queue.
    pub fn logic(&self, ctx: &egui::Context) {
        if self.handle.take_close_request() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn emit(&self, event: RecentCapturesOverlayEvent) {
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
                    media: request.media,
                    texture: None,
                    pending: thumb,
                    pin_notice: request.content_error,
                    auto_close_started_at: m.now(),
                    editing: false,
                    upload_available: request.upload_available,
                    upload_unavailable_reason: request.upload_unavailable_reason,
                    status: None,
                },
            );
            self.emit(RecentCapturesOverlayEvent::Pushed { id });
        }
    }

    fn run_commands(&mut self, ctx: &egui::Context, m: &Motion) {
        for cmd in self.take_commands() {
            match cmd {
                Command::Dismiss(id) => {
                    if self.stack.dismiss(id, m) {
                        self.emit(RecentCapturesOverlayEvent::Dismissed {
                            id,
                            reason: DismissReason::Programmatic,
                        });
                    }
                }
                Command::SetStatus { id, status } => {
                    if let Some(entry) = self.content.get_mut(&id) {
                        entry.status = status;
                    }
                }
                Command::SetUploadAvailability {
                    id,
                    available,
                    reason,
                } => {
                    if let Some(entry) = self.content.get_mut(&id) {
                        entry.upload_available = available;
                        entry.upload_unavailable_reason = reason;
                    }
                }
                Command::SetEditing { id, editing } => {
                    if let Some(entry) = self.content.get_mut(&id)
                        && entry.editing != editing
                    {
                        entry.auto_close_started_at = auto_close_restart_on_edit_end(
                            entry.editing,
                            editing,
                            entry.auto_close_started_at,
                            m.now(),
                        );
                        entry.editing = editing;
                    }
                }
                Command::SetCardImage { id, image } => {
                    if let Some(entry) = self.content.get_mut(&id) {
                        entry.pending = downscale(&image, self.thumbnail_px);
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
                    tracing::debug!(
                        card = id.0,
                        accepted,
                        stuck,
                        "Recent Captures Overlay native drag settled"
                    );
                }
                Command::DismissAll => {
                    let ids: Vec<CardId> = self.stack.cards().iter().map(|c| c.id()).collect();
                    self.stack.dismiss_all(m);
                    for id in ids {
                        self.emit(RecentCapturesOverlayEvent::Dismissed {
                            id,
                            reason: DismissReason::Programmatic,
                        });
                    }
                }
                Command::Collapse => self.stack.collapse(m),
                Command::Expand => self.stack.expand(m),
                Command::ToggleDock => self.stack.toggle_dock(m),
                Command::Configure(settings) => {
                    self.pending_settings = Some(settings.normalized());
                }
                Command::RestorePin { request, state } => self.restore_pin(*request, state),
                Command::RefreshPinTexture { pin, image } => {
                    if let Some(entry) = self.pins.get_mut(&pin) {
                        entry.pending = downscale(&image, PIN_TEXTURE_PX);
                        entry.content_error = entry
                            .pending
                            .is_none()
                            .then(|| "The refreshed pin texture exceeded safe GPU limits.".into());
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
        if self.dragging.is_none()
            && self.armed.is_none()
            && let Some(requested) = self.pending_settings
        {
            let width_is_safe = self
                .stack
                .configuration_preserves_residents(requested.placement, requested.card_width);
            let mut applied = requested;
            if !width_is_safe {
                applied.card_width = self.settings.card_width;
            }
            if applied != self.settings {
                self.settings = applied;
                self.stack
                    .configure(self.settings.placement, self.settings.card_width, m);
                if let Ok(mut acknowledged) = self.handle.shared.applied_settings.lock() {
                    *acknowledged = applied;
                }
            }
            if width_is_safe {
                self.pending_settings = None;
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
                self.emit(RecentCapturesOverlayEvent::Dismissed {
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
        self.pins.insert(pin.clone(), entry);
        self.pending_pin_cards.insert(pin.clone(), card);
        self.emit(RecentCapturesOverlayEvent::PinRequested {
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
        self.emit(RecentCapturesOverlayEvent::PinUnavailable { card, reason });
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
            self.emit(RecentCapturesOverlayEvent::PinUpdated {
                pin: id,
                state: normalized,
            });
        }
    }

    fn close_pin(&mut self, ctx: &egui::Context, id: &PinId) {
        if self.pins.remove(id).is_none() {
            return;
        }
        ctx.send_viewport_cmd_to(pin_viewport_id(id), egui::ViewportCommand::Close);
        self.pending_pin_cards.remove(id);
        self.emit(RecentCapturesOverlayEvent::PinClosed { pin: id.clone() });
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
            self.emit(RecentCapturesOverlayEvent::PinUpdated { pin, state });
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
            self.emit(RecentCapturesOverlayEvent::PinUpdated { pin, state });
        }
        for (pin, reason) in unavailable {
            self.emit(RecentCapturesOverlayEvent::PinPositioningUnavailable { pin, reason });
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
        // Automatic scrolling delivers globally addressed wheel input, which the
        // overlay would otherwise swallow. The request outranks the pointer
        // heuristic for as long as it is held.
        let forced = self
            .handle
            .shared
            .scroll_passthrough_requested
            .load(Ordering::Acquire);
        let desired = if forced {
            true
        } else {
            match self.passthrough {
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
            }
        };

        if !forced
            && self.passthrough == Passthrough::Auto
            && self.probe.is_none()
            && desired
            && !empty
        {
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(RESAMPLE_SECS));
        }
        if self.passthrough_now != Some(desired) {
            self.passthrough_now = Some(desired);
            ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(desired));
        }
        self.acknowledge_passthrough(desired, true);
    }

    /// Applies the click-through transition natively and records what the
    /// window actually reported back.
    ///
    /// The queued viewport command is asynchronous, so it is not evidence. Only
    /// a native readback may set the flag automatic scrolling waits on.
    fn acknowledge_passthrough(&mut self, desired: bool, warn: bool) {
        let applied = self
            .handle
            .shared
            .passthrough_applied
            .load(Ordering::Acquire);
        let acknowledged =
            apply_native_passthrough(self.native_passthrough.as_mut(), desired, applied)
                .unwrap_or_else(|err| {
                    if warn {
                        tracing::warn!(
                            requested = desired,
                            error = %err,
                            "native overlay click-through transition failed"
                        );
                    }
                    applied
                });
        self.handle
            .shared
            .passthrough_applied
            .store(acknowledged, Ordering::Release);
    }

    fn handle_action(&mut self, id: CardId, action: CardAction, alt_held: bool, m: &Motion) {
        if action == CardAction::Pin {
            // The card never draws Pin for a recording, and a caller that
            // reaches here anyway is told why rather than opening a native
            // window over a video it cannot show.
            if self
                .content
                .get(&id)
                .is_some_and(|entry| entry.media.is_video())
            {
                self.emit(RecentCapturesOverlayEvent::PinUnavailable {
                    card: id,
                    reason: "Pin to Screen holds a still image and does not apply to a recording."
                        .to_owned(),
                });
                return;
            }
            self.begin_pin(id, m);
            return;
        }
        let (event, dismiss) = match action {
            CardAction::Copy => (
                Some(RecentCapturesOverlayEvent::CopyRequested { id }),
                false,
            ),
            CardAction::Save => (
                Some(RecentCapturesOverlayEvent::SaveRequested {
                    id,
                    choose_destination: save_chooses_destination(
                        self.settings.save_behavior,
                        alt_held,
                    ),
                }),
                false,
            ),
            CardAction::Annotate => (
                Some(RecentCapturesOverlayEvent::AnnotateRequested { id }),
                false,
            ),
            CardAction::Edit => (
                Some(RecentCapturesOverlayEvent::EditRequested { id }),
                false,
            ),
            CardAction::Continue => {
                // Not a new action: it asks for exactly what Annotate/Edit
                // already ask for — focus the editor for this capture — so it
                // reuses the identical event and, with it, the app's existing
                // dedupe/focus routing. A card only ever offers Continue once
                // an editor already exists, so there is nothing new to open.
                let video = self
                    .content
                    .get(&id)
                    .is_some_and(|entry| entry.media.is_video());
                (Some(continue_event(id, video)), false)
            }
            CardAction::Upload => (
                Some(RecentCapturesOverlayEvent::UploadRequested { id }),
                false,
            ),
            CardAction::Pin => unreachable!("pin actions return above"),
            CardAction::Close => (None, true),
        };
        if let Some(event) = event {
            self.emit(event);
        }
        if dismiss && self.stack.dismiss(id, m) {
            self.emit(RecentCapturesOverlayEvent::Dismissed {
                id,
                reason: if action == CardAction::Close {
                    DismissReason::Closed
                } else {
                    DismissReason::Acted
                },
            });
        }
    }

    fn emit_due_auto_close(&mut self, ctx: &egui::Context, now: f64) {
        if !self.settings.auto_close_enabled || self.dragging.is_some() || self.armed.is_some() {
            return;
        }

        let seconds = f64::from(self.settings.auto_close_seconds);
        let (due, next) = auto_close_due(
            self.content
                .iter()
                .map(|(&id, entry)| (id, entry.editing, entry.auto_close_started_at)),
            now,
            seconds,
        );
        for id in due {
            if let Some(entry) = self.content.get_mut(&id) {
                entry.auto_close_started_at = f64::INFINITY;
            }
            self.emit(RecentCapturesOverlayEvent::AutoCloseRequested {
                id,
                action: self.settings.auto_close_action,
            });
        }
        if next.is_finite() {
            ctx.request_repaint_after(Duration::from_secs_f64(next.max(0.01)));
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

impl eframe::App for RecentCapturesOverlayApp {
    /// Fully transparent. eframe's default is a dark translucent wash, which on
    /// an overlay is a grey sheet over the entire work area.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        Self::logic(self, ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let m = Motion::from_context(&ctx);

        #[cfg(not(target_os = "macos"))]
        self.refresh_live_pin_topology_if_due(ctx.input(|input| input.time));
        self.run_commands(&ctx, &m);
        self.ingest(&m);
        self.reconcile(&ctx);
        self.draw_pins(&ctx);
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

        let mut hits: Vec<Rect> = Vec::with_capacity(frames.len() + 2);
        let mut card_hits: Vec<(CardId, Rect)> = Vec::with_capacity(frames.len());
        let mut hovered = None;
        let mut action = None;
        let mut drag_start = None;
        let mut drag_to = None;
        let mut drag_end = false;

        // Drawn before the cards so a long scrolling session never hides its own
        // Stop control behind a capture that arrived while it was running.
        let scroll_hud = self
            .handle
            .shared
            .scroll_hud
            .lock()
            .ok()
            .and_then(|state| state.clone());
        if let Some(state) = scroll_hud {
            let response = ScrollingHud::draw(ui, &self.theme, &state, true);
            hits.push(response.rect);
            if let Some(action) = response.action {
                self.emit(RecentCapturesOverlayEvent::Scrolling(action));
            }
        }

        for f in &frames {
            let Some(entry) = self.content.get(&f.id) else {
                continue;
            };
            let mut content = CardContent::new(&entry.name, entry.source_px, entry.provenance)
                .with_media(entry.media.card_media());
            content.upload_enabled = entry.upload_available;
            content.upload_unavailable_reason = entry.upload_unavailable_reason.as_deref();
            content.status = entry.status.as_deref();
            content.editing = entry.editing;
            if let Some(tex) = &entry.texture {
                content.texture = Some(tex.id());
            }
            let response = card::draw_card(ui, &surface, f, &content);
            if let Some(notice) = entry.pin_notice.as_deref() {
                draw_card_notice(ui, f.rect, notice, &self.theme);
            }
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
            self.emit(RecentCapturesOverlayEvent::DragStarted { id, at: pointer });
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
            self.emit(RecentCapturesOverlayEvent::DragOutArmed {
                id: live.id,
                card: live.rect,
                pointer: live.pointer,
                keep_after_accept: keep_after_accepted_drag(
                    self.settings.close_after_drag,
                    ctx.input(|input| input.modifiers.alt),
                ),
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
                    Intent::Dismiss => self.emit(RecentCapturesOverlayEvent::Dismissed {
                        id: release.id,
                        reason: DismissReason::Swipe,
                    }),
                    Intent::DragOut => {
                        self.emit(RecentCapturesOverlayEvent::DragOut { id: release.id, at });
                    }
                    Intent::Collapse | Intent::SpringBack => {}
                }
            }
            self.dragging = None;
            self.armed = None;
        }
        if let Some((id, a)) = action {
            self.handle_action(id, a, ctx.input(|input| input.modifiers.alt), &m);
        }

        // Emitted after this frame's own card interactions (drag settle and
        // `handle_action`) so a same-frame open/annotate/edit/continue click
        // is queued to the app *before* a same-frame expiry. `entry.editing`
        // cannot yet reflect a click made this very frame (it only flips on
        // the async `Command::SetEditing` round trip), so ordering, not
        // state, is what keeps auto-close from racing and dismissing a card
        // the same frame its editor is opening.
        self.emit_due_auto_close(&ctx, ctx.input(|input| input.time));

        let dock_now = self.stack.dock().is_collapsed();
        if dock_now != dock_was {
            self.emit(if dock_now {
                RecentCapturesOverlayEvent::DockCollapsed
            } else {
                RecentCapturesOverlayEvent::DockExpanded
            });
        }
        self.dock_collapsed = dock_now;
        if !was_empty && self.stack.is_empty() && self.stack.departing().is_empty() {
            self.emit(RecentCapturesOverlayEvent::Emptied);
        }

        self.apply_passthrough(&ctx, &hits, pointer);
        self.handle
            .shared
            .visible_content
            .store(!frames.is_empty() || dock_hit.is_some(), Ordering::Release);
        let previous_card_count = self
            .handle
            .shared
            .visible_card_count
            .swap(frames.len(), Ordering::AcqRel);
        if previous_card_count == 0
            && let Some(first) = frames.first()
        {
            tracing::debug!(
                card = first.id.0,
                card_rect = ?first.rect,
                viewport = ?self.geometry.size(),
                pixels_per_point = ctx.pixels_per_point(),
                "painted first card in settled native viewport"
            );
        }
        self.handle
            .shared
            .geometry_locked
            .store(self.dragging.is_some(), Ordering::Release);
        // The single place repainting is requested: idle costs nothing, an
        // animation gets a continuous repaint, and a pending wake gets a timer.
        self.stack.activity(&m).apply(&ctx);
    }
}

fn save_chooses_destination(behavior: RecentCapturesSaveBehavior, alt_held: bool) -> bool {
    (behavior == RecentCapturesSaveBehavior::ChooseDestination) ^ alt_held
}

fn keep_after_accepted_drag(close_after_drag: bool, alt_held: bool) -> bool {
    !close_after_drag || alt_held
}

/// The event `CardAction::Continue` resolves to for a given card.
///
/// Identical to what a fresh Annotate/Edit click on the same card would have
/// produced, so Continue can only ever ask to focus the editor a card
/// already has — the app's existing dedupe/focus routing on that event is
/// what makes "never a duplicate editor" true, and this function is the one
/// place that guarantee depends on staying wired up.
fn continue_event(id: CardId, is_video: bool) -> RecentCapturesOverlayEvent {
    if is_video {
        RecentCapturesOverlayEvent::EditRequested { id }
    } else {
        RecentCapturesOverlayEvent::AnnotateRequested { id }
    }
}

/// Pure decision core of the auto-close timer: given each card's editing
/// state, its countdown start time, `now`, and the configured window,
/// decides which cards are due to auto-close and how soon the next deadline
/// is. Kept free of `egui::Context`/[`RecentCapturesOverlayApp`] so the
/// pause-while-editing math is directly testable.
///
/// A card with `editing == true` contributes nothing here: it is neither due
/// nor does it push `next` earlier, which is exactly "pause" — its countdown
/// stays wherever it left off and asks for no repaint on its own behalf.
fn auto_close_due(
    entries: impl Iterator<Item = (CardId, bool, f64)>,
    now: f64,
    seconds: f64,
) -> (Vec<CardId>, f64) {
    let mut due = Vec::new();
    let mut next = seconds;
    for (id, editing, started_at) in entries {
        if editing {
            continue;
        }
        let elapsed = now - started_at;
        if elapsed >= seconds {
            due.push(id);
        } else {
            next = next.min(seconds - elapsed);
        }
    }
    (due, next)
}

/// Pure core of the `Command::SetEditing` transition: the new
/// `auto_close_started_at` for a card whose editing flag just changed.
///
/// Ending an edit (`was_editing && !editing`) always restarts the clock at
/// `now`, so time spent editing never counts against the window the user
/// gets to look at the result afterward — a full, fresh duration every time,
/// never a partial one. Starting or continuing to edit leaves the stored
/// start time untouched; the timer resumes it verbatim once editing ends.
fn auto_close_restart_on_edit_end(
    was_editing: bool,
    editing: bool,
    started_at: f64,
    now: f64,
) -> f64 {
    if was_editing && !editing {
        now
    } else {
        started_at
    }
}

/// The rectangle the dock occupies for a given work area, without building a
/// stack — for a host that needs to know where to put a click target before the
/// overlay exists.
#[must_use]
pub fn dock_rect(geometry: RecentCapturesOverlayGeometry) -> Rect {
    let layout =
        crate::stack::StackLayout::new(geometry.local(), crate::stack::CardMetrics::default());
    dock::rect_for_slot0(layout.slot_rect(0))
}

/// Drives one click-through transition through the native window and reports
/// whether the window acknowledged it.
///
/// A missing controller can never invent an acknowledgement: the caller keeps
/// the previously applied value, so automatic scrolling stays blocked on a
/// platform that has no native passthrough rather than scrolling into an
/// overlay that is still eating the wheel.
fn apply_native_passthrough(
    controller: Option<&mut NativePassthrough>,
    desired: bool,
    applied: bool,
) -> Result<bool, String> {
    if desired == applied {
        return Ok(applied);
    }
    let Some(controller) = controller else {
        return Ok(applied);
    };
    if controller(desired)? {
        Ok(desired)
    } else {
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
    }

    #[test]
    fn geometry_is_work_area_relative() {
        let g = RecentCapturesOverlayGeometry::new(rect(0.0, 25.0, 1440.0, 850.0));
        assert_eq!(g.position(), Pos2::new(0.0, 25.0));
        assert_eq!(g.size(), Vec2::new(1440.0, 850.0));
        assert_eq!(g.local().min, Pos2::ZERO);
        assert_eq!(g.local().size(), g.size());
    }

    #[test]
    fn screen_anchored_overlays_refuse_application_zoom() {
        let ctx = egui::Context::default();
        ctx.set_zoom_factor(1.75);
        let mut zoomed = ctx.run_ui(egui::RawInput::default(), |_| {});
        zoomed.textures_delta.clear();
        assert_eq!(ctx.zoom_factor(), 1.75);

        install_native_point_scale(&ctx);
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();

        assert_eq!(ctx.zoom_factor(), 1.0);
        assert!(!ctx.options(|options| options.zoom_with_keyboard));
    }

    #[test]
    fn option_or_alt_inverts_save_routing_for_one_action() {
        assert!(!save_chooses_destination(
            RecentCapturesSaveBehavior::ExportLocation,
            false
        ));
        assert!(save_chooses_destination(
            RecentCapturesSaveBehavior::ExportLocation,
            true
        ));
        assert!(save_chooses_destination(
            RecentCapturesSaveBehavior::ChooseDestination,
            false
        ));
        assert!(!save_chooses_destination(
            RecentCapturesSaveBehavior::ChooseDestination,
            true
        ));
    }

    #[test]
    fn option_or_alt_keeps_an_accepted_external_drag() {
        assert!(!keep_after_accepted_drag(true, false));
        assert!(keep_after_accepted_drag(true, true));
        assert!(keep_after_accepted_drag(false, false));
        assert!(keep_after_accepted_drag(false, true));
    }

    #[test]
    fn auto_close_timer_pauses_completely_while_editing() {
        let a = CardId(1);
        let b = CardId(2);
        // `a` is mid-countdown but editing; `b` is mid-countdown and idle.
        // Only `b` should ever contribute to `due` or to the next deadline.
        let (due, next) = auto_close_due([(a, true, 0.0), (b, false, 0.0)].into_iter(), 3.0, 5.0);
        assert!(due.is_empty(), "nothing is due yet");
        assert_eq!(next, 2.0, "only the idle card's remaining time counts");

        // Even an editing card whose raw elapsed time has blown past the
        // window must not fire while editing: pause means pause.
        let (due, next) = auto_close_due([(a, true, 999.0)].into_iter(), 1000.0, 5.0);
        assert!(due.is_empty(), "an editing card is never due");
        assert_eq!(
            next, 5.0,
            "an editing card asks for nothing on its own behalf"
        );
    }

    #[test]
    fn auto_close_timer_fires_only_for_idle_cards_past_the_window() {
        let a = CardId(1);
        let b = CardId(2);
        let (due, _) = auto_close_due([(a, false, 0.0), (b, false, 4.0)].into_iter(), 5.0, 5.0);
        assert_eq!(
            due,
            vec![a],
            "only the card whose window has fully elapsed fires"
        );
    }

    #[test]
    fn ending_an_edit_restarts_the_full_configured_duration() {
        // Editing -> not editing: always a fresh full window from `now`,
        // regardless of how long the countdown had already run before the
        // edit began.
        assert_eq!(auto_close_restart_on_edit_end(true, false, 0.0, 42.0), 42.0);
        assert_eq!(
            auto_close_restart_on_edit_end(true, false, 999.0, 42.0),
            42.0
        );
    }

    #[test]
    fn starting_or_continuing_an_edit_never_touches_the_stored_start_time() {
        // Not editing -> editing: leave the stored start time exactly where
        // it was, so it resumes with whatever time remained once the edit
        // (eventually) ends.
        assert_eq!(auto_close_restart_on_edit_end(false, true, 7.0, 100.0), 7.0);
        // A no-op transition (same value in and out) must never reset the
        // timer either; the caller already guards this with an equality
        // check, but the pure function itself must agree.
        assert_eq!(auto_close_restart_on_edit_end(true, true, 7.0, 100.0), 7.0);
        assert_eq!(
            auto_close_restart_on_edit_end(false, false, 7.0, 100.0),
            7.0
        );
    }

    #[test]
    fn continue_resolves_to_the_same_event_a_fresh_open_click_would() {
        let id = CardId(9);
        assert_eq!(
            continue_event(id, false),
            RecentCapturesOverlayEvent::AnnotateRequested { id },
            "a still capture's Continue is identical to a fresh Annotate click"
        );
        assert_eq!(
            continue_event(id, true),
            RecentCapturesOverlayEvent::EditRequested { id },
            "a recording's Continue is identical to a fresh Edit click"
        );
    }

    #[test]
    fn settings_normalize_accessibility_and_timer_bounds() {
        let compact = RecentCapturesOverlaySettings {
            card_width: 1.0,
            auto_close_seconds: 1,
            ..RecentCapturesOverlaySettings::default()
        }
        .normalized();
        assert_eq!(compact.card_width, CardMetrics::MIN_WIDTH);
        assert_eq!(compact.auto_close_seconds, 5);

        let large = RecentCapturesOverlaySettings {
            card_width: 10_000.0,
            auto_close_seconds: u32::MAX,
            ..RecentCapturesOverlaySettings::default()
        }
        .normalized();
        assert_eq!(large.card_width, CardMetrics::MAX_WIDTH);
        assert_eq!(large.auto_close_seconds, 3_600);
    }

    #[test]
    fn geometry_keeps_shadow_bleed_outside_the_card_safe_area() {
        let work_area = rect(80.0, 25.0, 1360.0, 800.0);
        let viewport = rect(0.0, 25.0, 1440.0, 848.0);
        let g = RecentCapturesOverlayGeometry::with_viewport(work_area, viewport);

        assert_eq!(g.position(), viewport.min);
        assert_eq!(g.size(), viewport.size());
        assert_eq!(g.viewport(), viewport);
        assert_eq!(g.local(), rect(80.0, 0.0, 1360.0, 800.0));
    }

    #[test]
    fn content_viewport_keeps_global_work_area_offsets() {
        let work_area = rect(0.0, 25.0, 1440.0, 850.0);
        let viewport = rect(24.0, 650.0, 282.0, 210.0);
        let geometry = RecentCapturesOverlayGeometry::with_content_viewport(work_area, viewport);

        assert_eq!(geometry.viewport(), viewport);
        assert_eq!(
            geometry.local(),
            rect(-24.0, -625.0, 1440.0, 850.0),
            "card layout remains anchored to the original global work area"
        );
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
        let h = RecentCapturesOverlayHandle::new();
        assert!(!h.is_attached());
        assert!(!h.needs_visible_surface());
        h.push(CaptureRequest::new(
            "Shot.png",
            Provenance::Display,
            (100, 50),
        ));
        assert_eq!(h.shared.inbox.lock().unwrap().len(), 1);
        assert!(
            h.needs_visible_surface(),
            "pending card content must wake and reveal the ordered-out root"
        );
        assert!(h.drain_events().is_empty());
        assert!(h.panel_report().is_none());
    }

    #[test]
    fn close_is_a_hidden_root_command_not_a_ui_command() {
        let h = RecentCapturesOverlayHandle::new();
        h.close();
        assert!(h.take_close_request());
        assert!(!h.take_close_request(), "close is consumed exactly once");
        assert!(
            h.shared.commands.lock().unwrap().is_empty(),
            "an idle root has no UI pass available to drain card commands"
        );
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
        let v = viewport(RecentCapturesOverlayGeometry::new(rect(
            0.0, 25.0, 1440.0, 850.0,
        )));
        assert_eq!(v.decorations, Some(false));
        assert_eq!(v.transparent, Some(true));
        assert_eq!(v.has_shadow, Some(false));
        assert_eq!(v.taskbar, Some(false));
        assert_eq!(v.resizable, Some(false));
        assert_eq!(v.active, Some(false));
        assert_eq!(v.visible, None);
        assert_eq!(v.window_level, Some(egui::WindowLevel::AlwaysOnTop));
        assert_eq!(v.position, Some(Pos2::new(0.0, 25.0)));
        assert_eq!(v.inner_size, Some(Vec2::new(1440.0, 850.0)));
    }

    #[test]
    fn panel_report_defaults_to_not_converted() {
        assert!(!PanelReport::default().non_activating);
        assert!(!PanelReport::unsupported("none").non_activating);
        assert!(PanelReport::converted("safe native adapter").non_activating);
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

    #[test]
    fn scrolling_passthrough_request_is_explicit_and_acknowledged_separately() {
        let handle = RecentCapturesOverlayHandle::new();
        handle.request_scroll_passthrough(true);

        assert!(
            handle
                .shared
                .scroll_passthrough_requested
                .load(Ordering::Acquire)
        );
        assert!(
            !handle.scroll_passthrough_ready(),
            "the request is not an acknowledgement from the native viewport"
        );
    }

    #[test]
    fn native_passthrough_readback_controls_the_acknowledgement() {
        let mut refused: NativePassthrough = Box::new(|_| Ok(false));
        assert!(!apply_native_passthrough(Some(&mut refused), true, false).unwrap());

        let mut accepted: NativePassthrough = Box::new(|_| Ok(true));
        assert!(apply_native_passthrough(Some(&mut accepted), true, false).unwrap());
        assert!(!apply_native_passthrough(Some(&mut accepted), false, true).unwrap());
    }

    #[test]
    fn missing_native_passthrough_never_invents_an_acknowledgement() {
        assert!(!apply_native_passthrough(None, true, false).unwrap());
    }

    #[test]
    fn scroll_hud_state_round_trips_through_the_handle() {
        let handle = RecentCapturesOverlayHandle::new();
        assert!(!handle.scroll_hud_visible());

        handle.show_scroll_hud(ScrollHudState::choosing(scrozz_core::ScrollAxis::Vertical));
        assert!(handle.scroll_hud_visible());

        handle.hide_scroll_hud();
        assert!(!handle.scroll_hud_visible());
    }
}
