//! The capture card, and the seam where it becomes pixels.
//!
//! # Why there is a trait here
//!
//! Per D12 the capture stack is the primary interface of the whole app: a
//! bottom-left pile of cards, newest on top, growing upward. `scrozz-ui` owns
//! how one is drawn — the geometry, the motion, the gestures — and `scrozz-ui`
//! is where eframe lives.
//!
//! `apps/scrozz` deliberately has no windowing dependency. It is the binary a
//! compositor keybinding invokes on a headless machine, and linking winit into
//! it would make `scrozz capture --stdout` depend on a display server. So this
//! module defines the narrowest possible contract — show this, hide that, tell
//! me what was clicked — and [`crate::platform::card_surface`] decides which
//! implementation satisfies it.
//!
//! # What crosses the seam
//!
//! A [`Card`] carries *pixels the surface can draw immediately*, not a handle
//! it must go and resolve. Two reasons. The thumbnail is produced on the
//! pipeline worker, where a 3456×2234 downscale costs nothing; doing it on the
//! main thread would drop frames at exactly the moment a card is animating in.
//! And a card must still render when its capture has been image-evicted (D23),
//! which a lazily-resolved handle could not promise.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use scrozz_core::{ColorSpace, Frame, PinState, Provenance, Transform as ColorTransform};
use scrozz_store::CaptureId;
use scrozz_ui::card::CardMedia;
use scrozz_ui::recent_captures_overlay::RecentCapturesAutoCloseAction;
use scrozz_ui::{ScrollHudAction, ScrollHudState};

use crate::gui::action::CaptureKind;

/// Thread-safe request for the window host to process newly queued work.
pub type SurfaceWaker = Arc<dyn Fn() + Send + Sync>;

/// Identifies one card within a session.
///
/// Distinct from [`CaptureId`], and deliberately so: a card exists from the
/// moment the shutter fires, before the store has written anything, and it must
/// be addressable in that window — that is where "dismiss it before it finishes
/// saving" lives. A card whose capture failed never gets a `CaptureId` at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CardId(pub u64);

impl std::fmt::Display for CardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "card:{}", self.0)
    }
}

/// Process-local generation of one durable pin identity.
///
/// A capture can be pinned, closed, and pinned again while worker results from
/// the first attempt are still in flight. Pairing identity with this generation
/// keeps those old settlements from mutating the new pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PinGeneration(pub u64);

/// The longest edge of a card thumbnail, in pixels.
///
/// The largest density token is 320 pt wide and normally drawn at up to 2×, so
/// 640 stays sharp on a Retina panel while keeping a 5K downscale bounded.
pub const THUMBNAIL_MAX_EDGE: u32 = 640;

/// Longest texture edge retained for a native pinned window.
pub const PIN_TEXTURE_MAX_EDGE: u32 = scrozz_ui::recent_captures_overlay::PIN_TEXTURE_PX;
const MAX_TEXTURE_PIXELS: u64 = (PIN_TEXTURE_MAX_EDGE as u64) * (PIN_TEXTURE_MAX_EDGE as u64);

/// Straight-alpha RGBA8 pixels, ready to upload as a texture.
#[derive(Clone, PartialEq, Eq)]
pub struct Thumbnail {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl std::fmt::Debug for Thumbnail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Thumbnail")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.pixels.len())
            .finish()
    }
}

impl Thumbnail {
    /// Wraps raw RGBA8.
    ///
    /// Returns `None` if the buffer does not match the geometry, rather than
    /// letting a mismatched buffer reach a texture upload — which is a crash on
    /// some drivers and a garbage card on others.
    #[must_use]
    pub fn from_rgba(width: u32, height: u32, pixels: Vec<u8>) -> Option<Self> {
        let expected = Self::checked_texture_len(width, height)?;
        (expected == pixels.len() && width > 0 && height > 0).then_some(Self {
            width,
            height,
            pixels,
        })
    }

    /// Downscales a captured frame to card size.
    ///
    /// A box filter, not a resample: every source pixel contributes to exactly
    /// one destination pixel, which is both the cheapest correct answer for a
    /// pure minification and the one that does not need a filter kernel
    /// dependency this crate does not have.
    ///
    /// # Errors
    ///
    /// Returns whatever [`scrozz_export::to_straight_rgba8`] returns for a
    /// frame whose pixel format cannot be straightened.
    pub fn from_frame(frame: &Frame, max_edge: u32) -> scrozz_core::Result<Self> {
        let source = scrozz_export::to_straight_rgba8(frame)?;
        let mut thumbnail = Self::downscale(source.width, source.height, &source.data, max_edge)
            .ok_or_else(|| {
                scrozz_core::Error::InvalidRequest(format!(
                    "cannot create a bounded texture from {}x{} RGBA pixels",
                    source.width, source.height
                ))
            })?;
        let into_srgb = ColorTransform::new(frame.color_space, ColorSpace::Srgb);
        for pixel in thumbnail.pixels.as_chunks_mut::<4>().0 {
            let converted = into_srgb.convert_u8([pixel[0], pixel[1], pixel[2]]);
            pixel[..3].copy_from_slice(&converted);
        }
        Ok(thumbnail)
    }

    fn downscale(width: u32, height: u32, rgba: &[u8], max_edge: u32) -> Option<Self> {
        let source_len = Self::checked_rgba_len(width, height)?;
        if rgba.len() != source_len || max_edge == 0 || max_edge > PIN_TEXTURE_MAX_EDGE {
            return None;
        }
        let longest = width.max(height).max(1);
        if longest <= max_edge {
            return Some(Self {
                width,
                height,
                pixels: rgba.to_vec(),
            });
        }

        let rounded_scale = |value: u32| {
            u64::from(value)
                .checked_mul(u64::from(max_edge))
                .map(|scaled| ((scaled + u64::from(longest) / 2) / u64::from(longest)).max(1))
                .and_then(|scaled| u32::try_from(scaled).ok())
        };
        let out_w = rounded_scale(width)?;
        let out_h = rounded_scale(height)?;

        let mut pixels = vec![0u8; Self::checked_texture_len(out_w, out_h)?];
        for y in 0..out_h {
            let (y0, y1) = span(y, out_h, height);
            for x in 0..out_w {
                let (x0, x1) = span(x, out_w, width);
                let mut sums = [0u64; 4];
                let mut count = 0u64;
                for sy in y0..y1 {
                    let row = (sy as usize) * (width as usize) * 4;
                    for sx in x0..x1 {
                        let i = row + (sx as usize) * 4;
                        // A short final row is a corrupt buffer, not a reason to
                        // panic on the main pipeline.
                        let Some(px) = rgba.get(i..i + 4) else {
                            continue;
                        };
                        for (sum, channel) in sums.iter_mut().zip(px) {
                            *sum += u64::from(*channel);
                        }
                        count += 1;
                    }
                }
                let out = ((y as usize) * (out_w as usize) + x as usize) * 4;
                if count == 0 {
                    continue;
                }
                for (channel, sum) in sums.iter().enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        pixels[out + channel] = (sum / count) as u8;
                    }
                }
            }
        }

        Some(Self {
            width: out_w,
            height: out_h,
            pixels,
        })
    }

    fn checked_rgba_len(width: u32, height: u32) -> Option<usize> {
        if width == 0 || height == 0 {
            return None;
        }
        let pixels = u64::from(width).checked_mul(u64::from(height))?;
        usize::try_from(pixels.checked_mul(4)?).ok()
    }

    fn checked_texture_len(width: u32, height: u32) -> Option<usize> {
        if width > PIN_TEXTURE_MAX_EDGE || height > PIN_TEXTURE_MAX_EDGE {
            return None;
        }
        let pixels = u64::from(width).checked_mul(u64::from(height))?;
        (pixels <= MAX_TEXTURE_PIXELS)
            .then(|| usize::try_from(pixels.checked_mul(4)?).ok())
            .flatten()
    }

    /// Width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The RGBA8 buffer.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

/// Which half of the source span pixel `index` covers.
fn span(index: u32, out: u32, source: u32) -> (u32, u32) {
    let start = (u64::from(index) * u64::from(source) / u64::from(out)) as u32;
    let end = ((u64::from(index) + 1) * u64::from(source) / u64::from(out)) as u32;
    (start.min(source), end.clamp(start + 1, source))
}

/// One capture, as the stack shows it.
#[derive(Debug, Clone)]
pub struct Card {
    /// Session-local identifier.
    pub id: CardId,
    /// Whether this card shows a still or a finished recording.
    ///
    /// A recording carries its own durable media file, which the card only
    /// points at: dismissing the card, or deleting the history row, never
    /// removes it.
    pub media: CardMedia,
    /// Where it lives in history, once the store has it.
    ///
    /// `None` means the capture happened but was not persisted — a store that
    /// would not open, or a capture the user asked to send straight to the
    /// clipboard. The card still works; only "reveal in history" does not.
    pub capture_id: Option<CaptureId>,
    /// What kind of capture produced it.
    pub kind: CaptureKind,
    /// Authoritative capture provenance used by pin chrome and fidelity rules.
    pub provenance: Provenance,
    /// Source width in pixels, before thumbnailing.
    pub source_width: u32,
    /// Source height in pixels.
    pub source_height: u32,
    /// Pixels per point of the display it came from.
    pub scale: f64,
    /// What to draw, if the pixels are available.
    pub thumbnail: Option<Thumbnail>,
    /// Where the capture was written, if anywhere.
    pub written: Vec<String>,
    /// When the shutter fired.
    pub taken_at: SystemTime,
    /// Whether the Upload action is currently usable.
    pub upload_available: bool,
    /// Explanation for a disabled Upload action.
    pub upload_unavailable_reason: Option<String>,
}

/// A persisted capture ready to restore into a native pinned window.
#[derive(Debug, Clone)]
pub struct PinnedCapture {
    /// Durable history identity.
    pub id: CaptureId,
    /// Human-readable label.
    pub name: String,
    /// Source provenance, which controls synthetic chrome (D9).
    pub provenance: Provenance,
    /// Source width in physical pixels.
    pub source_width: u32,
    /// Source height in physical pixels.
    pub source_height: u32,
    /// Source pixels per logical point.
    pub scale: f64,
    /// Durable window geometry and presentation.
    pub state: PinState,
    /// Display texture, bounded independently from card thumbnails.
    pub texture: Option<Thumbnail>,
    /// Explicit reason pixels could not be loaded, if the durable state survives.
    pub content_error: Option<String>,
}

impl Card {
    /// A card with nothing but an identity, for tests and for the failure path.
    #[must_use]
    pub fn placeholder(id: CardId, kind: CaptureKind) -> Self {
        Self {
            id,
            media: CardMedia::Image,
            capture_id: None,
            kind,
            provenance: match kind {
                CaptureKind::AllInOne | CaptureKind::Region => Provenance::Region,
                CaptureKind::Window => Provenance::Window,
                CaptureKind::Fullscreen | CaptureKind::AllDisplays => Provenance::Display,
                // A stitched page is assembled from one window's viewport, so
                // it carries the window's provenance and D9's no-compositing
                // rule with it.
                CaptureKind::Scrolling => Provenance::Window,
            },
            source_width: 0,
            source_height: 0,
            scale: 1.0,
            thumbnail: None,
            written: Vec::new(),
            taken_at: SystemTime::now(),
            upload_available: false,
            upload_unavailable_reason: Some("Sharing is not configured.".to_owned()),
        }
    }

    /// A card for a finished recording, built from its durable handoff.
    ///
    /// The poster is already bounded by the handoff, so nothing is decoded or
    /// resampled here. The durable path becomes the card's written location,
    /// which is what makes drag-out and reveal point at the real video rather
    /// than at a copy this card would own.
    #[must_use]
    pub fn from_finalized_media(
        id: CardId,
        capture_id: Option<CaptureId>,
        handoff: &scrozz_record::handoff::FinalizedMediaHandoff,
    ) -> Self {
        let poster = &handoff.poster;
        Self {
            id,
            media: CardMedia::video(handoff.duration, handoff.audio_present),
            capture_id,
            kind: CaptureKind::Fullscreen,
            provenance: Provenance::Display,
            source_width: handoff.dimensions.0,
            source_height: handoff.dimensions.1,
            scale: 1.0,
            thumbnail: Thumbnail::from_rgba(poster.width, poster.height, poster.bytes.clone()),
            written: vec![handoff.path.to_string_lossy().into_owned()],
            taken_at: SystemTime::now(),
            upload_available: false,
            upload_unavailable_reason: None,
        }
    }

    /// A one-line description, for logs and for accessibility labels.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut text = match self.media {
            CardMedia::Image => format!(
                "{} capture {}×{} at {}×",
                self.kind.label(),
                self.source_width,
                self.source_height,
                self.scale
            ),
            CardMedia::Video { duration, .. } => format!(
                "video recording {}×{}, {:.1}s",
                self.source_width,
                self.source_height,
                duration.as_secs_f64()
            ),
        };
        if !self.written.is_empty() {
            text.push_str(" → ");
            text.push_str(&self.written.join(", "));
        }
        text
    }

    /// The capture's own pixel dimensions.
    #[must_use]
    pub const fn source_px(&self) -> (u32, u32) {
        (self.source_width, self.source_height)
    }

    /// The name shown in the card's caption.
    ///
    /// The file name when the capture reached disk, because that is the name
    /// the user will look for; otherwise a description, because a card that
    /// only lives in memory has no file name and inventing one would send
    /// someone hunting for a file that is not there.
    #[must_use]
    pub fn file_name(&self) -> String {
        self.written
            .first()
            .and_then(|path| {
                std::path::Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| self.kind.label().to_owned())
    }
}

/// Something the user did to a card.
///
/// Per D21 direction carries meaning, and the gesture-to-intent mapping belongs
/// to `scrozz-ui`, which knows the thresholds and the velocities. What crosses
/// this seam is the *decision* — never a raw drag delta, because then two
/// crates would be deciding what a swipe means.
#[derive(Debug, Clone, PartialEq)]
pub enum CardEvent {
    /// Put this capture on the clipboard.
    Copy(CardId),
    /// Save it, optionally asking for a destination first.
    Save {
        /// Card to save.
        card: CardId,
        /// Whether the platform-native destination chooser should open.
        choose_destination: bool,
    },
    /// Upload it to configured private storage and copy the link.
    Upload(CardId),
    /// A card's configured elapsed cleanup interval expired.
    AutoClose(CardId, RecentCapturesAutoCloseAction),
    /// Swiped left: throw it away.
    Dismiss(CardId),
    /// The display could no longer fit this card.
    Overflow(CardId),
    /// Dragged right or up far enough to mean it, **while the button is still
    /// down**.
    ///
    /// This is the one card event that is not a report of something finished.
    /// It is a request to start a native drag *now*, because every platform
    /// refuses to start one after the mouse comes up. The geometry travels with
    /// it because the drag image has to be placed where the card already is.
    Drag {
        /// Which card.
        card: CardId,
        /// Where the gesture was when it committed.
        at: crate::gui::drag::DragSpot,
    },
    /// Swiped down: collapse into the capture dock (D20).
    Collapse(CardId),
    /// Clicked: open it for editing.
    Open(CardId),
    /// A card became a durable native pin.
    Pin(CardId, CaptureId, PinState),
    /// A pin's geometry or presentation changed.
    PinChanged(CaptureId, PinState),
    /// A pin closed and must not restore.
    Unpin(CaptureId),
    /// A native pin request was truthfully refused.
    PinUnavailable {
        /// Card that was not pinned.
        card: CardId,
        /// Platform reason and remedy.
        reason: String,
    },
    /// A compositor refused explicit positioning.
    PinPositioningUnavailable {
        /// Durable capture identity.
        capture: CaptureId,
        /// Platform reason and remedy.
        reason: String,
    },
}

impl CardEvent {
    /// Which card this happened to.
    #[must_use]
    pub const fn card(&self) -> Option<CardId> {
        match self {
            Self::Copy(id)
            | Self::Upload(id)
            | Self::AutoClose(id, _)
            | Self::Dismiss(id)
            | Self::Overflow(id)
            | Self::Collapse(id)
            | Self::Open(id)
            | Self::Pin(id, _, _)
            | Self::PinUnavailable { card: id, .. } => Some(*id),
            Self::Save { card, .. } | Self::Drag { card, .. } => Some(*card),
            Self::PinChanged(..) | Self::Unpin(..) | Self::PinPositioningUnavailable { .. } => None,
        }
    }
}

/// Where cards are drawn.
///
/// # Threading
///
/// Every method is called on the main thread, from [`crate::gui::App::tick`].
/// Implementations may therefore be `!Send`, which they have to be: an eframe
/// context is.
pub trait CardSurface {
    /// Applies Recent Captures preferences to current and future cards.
    fn configure_recent_captures_overlay(
        &mut self,
        settings: scrozz_ui::RecentCapturesOverlaySettings,
    ) {
        let _ = settings;
    }

    /// Shows a new card at the top of the stack.
    ///
    /// # Errors
    ///
    /// Returns an error only if the card could not be shown at all. A surface
    /// that dropped the *oldest* card to make room has succeeded — that is the
    /// stack working as designed, not a failure.
    fn present(&mut self, card: Card) -> scrozz_core::Result<()>;

    /// Removes a card, animating it out if the surface animates.
    fn dismiss(&mut self, id: CardId);

    /// Reports that `id`'s native drag has finished, however it finished.
    ///
    /// The surface armed the gesture and then handed it to the platform; this
    /// is how it learns the platform is done. It must be called for **every**
    /// outcome, because a native drag loop can consume the mouse-up that the
    /// surface would otherwise have used to notice — leaving a card held under
    /// a pointer that has long since let go, and unable to be dragged again.
    ///
    /// `accepted` says whether something took the drop. Every outcome releases
    /// the gesture; only an accepted one is followed by the card being retired.
    fn settle_drag(&mut self, id: CardId, accepted: bool) {
        let _ = (id, accepted);
    }

    /// Restore or refresh a durable pinned capture.
    ///
    /// # Errors
    ///
    /// Returns an explicit unsupported error when this surface has no native
    /// window host, or a rendering error when the request cannot be accepted.
    fn restore_pin(&mut self, pin: PinnedCapture) -> scrozz_core::Result<()>;

    /// Refresh a live pin's pixels without replacing its UI-owned state.
    fn refresh_pin_texture(
        &mut self,
        capture: &CaptureId,
        texture: Thumbnail,
    ) -> scrozz_core::Result<()>;

    /// Commit a successfully persisted provisional pin and retire its source card.
    fn commit_pin(
        &mut self,
        capture: &CaptureId,
        texture: Option<Thumbnail>,
    ) -> scrozz_core::Result<()>;

    /// Roll a provisional pin back to its still-recoverable source card.
    fn fail_pin(&mut self, capture: &CaptureId, reason: String);

    /// Remove a live pin without emitting another persistence mutation.
    fn discard_pin(&mut self, capture: &CaptureId);

    /// Unlock every pointer-transparent pin through an external escape.
    fn unlock_pins(&mut self);

    /// Shows or clears action status on a card.
    fn set_status(&mut self, _id: CardId, _status: Option<String>) {}

    /// Shows or updates the scrolling-capture HUD.
    ///
    /// Defaulted so a surface with no HUD is silently correct rather than
    /// forced to carry an unimplemented method; the app treats a surface that
    /// never reports [`Self::scroll_passthrough_ready`] as manual-only.
    fn show_scroll_hud(&mut self, state: ScrollHudState) {
        let _ = state;
    }

    /// Hides the scrolling-capture HUD.
    fn hide_scroll_hud(&mut self) {}

    /// Takes one pending scrolling-HUD decision, if there is one.
    fn poll_scroll_hud(&mut self) -> Option<ScrollHudAction> {
        None
    }

    /// Asks the surface to hold its window mouse-transparent.
    ///
    /// Automatic scrolling posts globally addressed wheel input, which the
    /// overlay would otherwise consume.
    fn request_scroll_passthrough(&mut self, requested: bool) {
        let _ = requested;
    }

    /// Whether the native window has *confirmed* mouse transparency.
    ///
    /// A queued viewport command is not an acknowledgement, so the default is
    /// `false`: a surface that cannot prove transparency keeps the capture
    /// manual instead of scrolling itself.
    fn scroll_passthrough_ready(&self) -> bool {
        false
    }

    /// Updates one card's Upload capability.
    fn set_upload_availability(&mut self, _id: CardId, _enabled: bool, _reason: Option<String>) {}

    /// Tells the surface whether a card's editor is open.
    ///
    /// Drives the card's morphing Editing/Continue pill and pauses its
    /// auto-close timer for as long as `editing` is `true`; the timer resumes
    /// with its full configured duration once `editing` goes back to `false`.
    /// A no-op default so surfaces without a Recent Captures stack (such as
    /// tests that only exercise output actions) never need to implement it.
    fn set_editing(&mut self, _id: CardId, _editing: bool) {}

    /// Replaces a card's visible thumbnail with a freshly rendered document.
    ///
    /// Called only when an editor commits (Done) a dirty session, never while
    /// editing is in progress: the card must show its pre-edit thumbnail
    /// unchanged for the whole session, with no ambiguous intermediate
    /// revision. A no-op default so surfaces without a Recent Captures stack
    /// never need to implement it.
    fn refresh_card_image(&mut self, _id: CardId, _frame: &Frame) {}

    /// Takes one pending interaction, if there is one. Never blocks.
    ///
    /// Polled rather than delivered through a callback, for the same reason
    /// `scrozz-shell` polls `muda`: a registered handler is a global the first
    /// caller wins.
    fn poll(&mut self) -> Option<CardEvent>;

    /// Takes only the drag-outs, leaving every other event for [`Self::poll`].
    ///
    /// # Why this exists separately
    ///
    /// Every other card event can wait a frame. A drag-out cannot. AppKit's
    /// `beginDraggingSessionWithItems:` and OLE's `DoDragDrop` both take over
    /// the mouse from wherever it *currently* is, so they have to be called
    /// while the button is still physically down — within the same input frame
    /// that produced the gesture. One frame late is a drag that never starts,
    /// or worse, one that starts under a released button and follows the cursor
    /// until the user clicks something.
    ///
    /// The ordinary [`Self::poll`] is drained from the host's logic pass, which
    /// runs *before* the UI pass that generates the gesture — so an event born
    /// in frame N is not seen until frame N+1. This method is called straight
    /// after the surface has drawn, inside the same frame, and returns only the
    /// events that cannot survive that wait. Everything else it drains is
    /// buffered and comes back out of `poll` in order.
    ///
    /// Returning drags ahead of buffered non-drag events is deliberate: the
    /// alternative is holding a drag behind a dismiss for a frame, which is the
    /// exact failure this exists to prevent.
    fn poll_drag_starts(&mut self) -> Vec<CardEvent> {
        Vec::new()
    }

    /// How many cards are showing.
    fn len(&self) -> usize;

    /// Whether the stack is empty — which, per D27, is Scrozz's resting state.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Event-loop wake hook for worker and IPC producers.
    fn waker(&self) -> Option<SurfaceWaker> {
        None
    }

    /// A human-readable name for this surface, for diagnostics.
    fn describe(&self) -> String {
        "card surface".to_owned()
    }

    /// The native window this surface draws into, once there is one.
    ///
    /// Only a drag needs this, and only because every platform's drag API is
    /// begun *from a window* rather than from an application. `None` is the
    /// honest answer for a surface that has no window — the recording surface,
    /// or a real one before it has opened — and callers refuse the drag rather
    /// than guessing.
    fn native_surface(&self) -> Option<scrozz_shell::NativeSurface> {
        None
    }
}

/// A surface that records instead of drawing.
///
/// This is what makes the pipeline testable, and it is not only a test double:
/// it is what `scrozz gui` runs on a machine with no windowing host, so the
/// capture path, the store writes and the IPC forwarding can all be exercised
/// end to end without anything appearing on screen.
#[derive(Debug, Clone)]
pub struct Recording {
    log: Arc<Mutex<Vec<Card>>>,
    injected: Arc<Mutex<Vec<CardEvent>>>,
    scroll_hud: Arc<Mutex<Option<ScrollHudState>>>,
    scroll_actions: Arc<Mutex<Vec<ScrollHudAction>>>,
    scroll_passthrough_requested: Arc<AtomicBool>,
    scroll_passthrough_ready: Arc<AtomicBool>,
    /// Events armed mid-gesture, answered by
    /// [`CardSurface::poll_drag_starts`] rather than by `poll`.
    armed: Arc<Mutex<Vec<CardEvent>>>,
    /// Every surface call, in the order it happened.
    ///
    /// Ordering is the whole subject of the drag hand-off: producing the right
    /// events in the wrong frame is exactly the bug, and a test that only reads
    /// the events cannot see it.
    trace: Arc<Mutex<Vec<SurfaceCall>>>,
}

/// One call into a [`Recording`], for tests that care when things happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceCall {
    /// [`CardSurface::poll`] was drained.
    Poll,
    /// [`CardSurface::poll_drag_starts`] was drained.
    PollDragStarts,
    /// A card was dismissed.
    Dismiss(CardId),
    /// A native drag reported back.
    Settle {
        /// Which card's drag ended.
        id: CardId,
        /// Whether something took the drop.
        accepted: bool,
    },
    /// [`CardSurface::set_editing`] toggled a card's pill/timer state.
    SetEditing {
        /// Which card's editing state changed.
        id: CardId,
        /// The new editing state.
        editing: bool,
    },
    /// [`CardSurface::refresh_card_image`] replaced a card's thumbnail.
    RefreshCardImage(CardId),
}

impl Default for Recording {
    fn default() -> Self {
        Self {
            log: Arc::default(),
            injected: Arc::default(),
            armed: Arc::default(),
            trace: Arc::default(),
            scroll_hud: Arc::default(),
            scroll_actions: Arc::default(),
            scroll_passthrough_requested: Arc::default(),
            // A recording surface has no native window to intercept input, so
            // its acknowledgement is true by construction rather than by luck.
            scroll_passthrough_ready: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl Recording {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every card presented so far, oldest first.
    ///
    /// # Panics
    ///
    /// If a previous caller panicked while holding the lock.
    #[must_use]
    pub fn presented(&self) -> Vec<Card> {
        self.log.lock().expect("card log is poisoned").clone()
    }

    /// Queues an event for the next [`CardSurface::poll`].
    ///
    /// The only way to drive the card-action half of the pipeline in a test.
    ///
    /// # Panics
    ///
    /// If a previous caller panicked while holding the lock.
    pub fn inject(&self, event: CardEvent) {
        self.injected
            .lock()
            .expect("card events are poisoned")
            .push(event);
    }

    /// Queues a scrolling-HUD decision.
    ///
    /// # Panics
    ///
    /// If a previous caller panicked while holding the lock.
    pub fn inject_scroll_action(&self, action: ScrollHudAction) {
        self.scroll_actions
            .lock()
            .expect("scroll HUD events are poisoned")
            .push(action);
    }

    /// The scrolling-HUD state most recently presented.
    ///
    /// # Panics
    ///
    /// If a previous caller panicked while holding the lock.
    #[must_use]
    pub fn scrolling_hud(&self) -> Option<ScrollHudState> {
        self.scroll_hud
            .lock()
            .expect("scroll HUD state is poisoned")
            .clone()
    }

    /// Controls the native-passthrough acknowledgement exposed to coordinator
    /// tests.
    pub fn set_scroll_passthrough_ready(&self, ready: bool) {
        self.scroll_passthrough_ready
            .store(ready, Ordering::Release);
    }

    /// Whether the coordinator most recently requested click-through.
    #[must_use]
    pub fn scroll_passthrough_requested(&self) -> bool {
        self.scroll_passthrough_requested.load(Ordering::Acquire)
    }

    /// Arms an event for the next [`CardSurface::poll_drag_starts`].
    ///
    /// Separate from [`Self::inject`] on purpose: the two are drained by
    /// different passes of the frame, and a test that conflates them cannot
    /// tell a drag started in the gesture frame from one started a frame late.
    ///
    /// # Panics
    ///
    /// If a previous caller panicked while holding the lock.
    pub fn arm(&self, event: CardEvent) {
        self.armed
            .lock()
            .expect("armed events are poisoned")
            .push(event);
    }

    /// Every surface call so far, in order.
    ///
    /// # Panics
    ///
    /// If a previous caller panicked while holding the lock.
    #[must_use]
    pub fn trace(&self) -> Vec<SurfaceCall> {
        self.trace
            .lock()
            .expect("surface trace is poisoned")
            .clone()
    }

    /// Forgets the call trace, so a test can time one phase in isolation.
    ///
    /// # Panics
    ///
    /// If a previous caller panicked while holding the lock.
    pub fn clear_trace(&self) {
        self.trace
            .lock()
            .expect("surface trace is poisoned")
            .clear();
    }

    fn record(&self, call: SurfaceCall) {
        self.trace
            .lock()
            .expect("surface trace is poisoned")
            .push(call);
    }

    /// A handle sharing this recorder's state.
    #[must_use]
    pub fn handle(&self) -> Self {
        self.clone()
    }
}

impl CardSurface for Recording {
    fn present(&mut self, card: Card) -> scrozz_core::Result<()> {
        tracing::info!(card = %card.id, "{}", card.summary());
        self.log.lock().expect("card log is poisoned").push(card);
        Ok(())
    }

    fn dismiss(&mut self, id: CardId) {
        self.record(SurfaceCall::Dismiss(id));
        self.log
            .lock()
            .expect("card log is poisoned")
            .retain(|card| card.id != id);
    }

    fn settle_drag(&mut self, id: CardId, accepted: bool) {
        self.record(SurfaceCall::Settle { id, accepted });
    }

    fn set_editing(&mut self, id: CardId, editing: bool) {
        self.record(SurfaceCall::SetEditing { id, editing });
    }

    fn refresh_card_image(&mut self, id: CardId, _frame: &Frame) {
        self.record(SurfaceCall::RefreshCardImage(id));
    }

    fn poll_drag_starts(&mut self) -> Vec<CardEvent> {
        self.record(SurfaceCall::PollDragStarts);
        std::mem::take(&mut *self.armed.lock().expect("armed events are poisoned"))
    }

    fn restore_pin(&mut self, _pin: PinnedCapture) -> scrozz_core::Result<()> {
        Err(scrozz_core::Error::Unsupported {
            what: "native pinned capture windows".into(),
            why: "the recording card surface has no window host".into(),
        })
    }

    fn refresh_pin_texture(
        &mut self,
        _capture: &CaptureId,
        _texture: Thumbnail,
    ) -> scrozz_core::Result<()> {
        Err(scrozz_core::Error::Unsupported {
            what: "native pinned capture windows".into(),
            why: "the recording card surface has no window host".into(),
        })
    }

    fn commit_pin(
        &mut self,
        _capture: &CaptureId,
        _texture: Option<Thumbnail>,
    ) -> scrozz_core::Result<()> {
        Err(scrozz_core::Error::Unsupported {
            what: "native pinned capture windows".into(),
            why: "the recording card surface has no window host".into(),
        })
    }

    fn fail_pin(&mut self, _capture: &CaptureId, _reason: String) {}

    fn discard_pin(&mut self, _capture: &CaptureId) {}
    fn unlock_pins(&mut self) {}

    fn poll(&mut self) -> Option<CardEvent> {
        self.record(SurfaceCall::Poll);
        let mut queue = self.injected.lock().expect("card events are poisoned");
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    fn show_scroll_hud(&mut self, state: ScrollHudState) {
        *self
            .scroll_hud
            .lock()
            .expect("scroll HUD state is poisoned") = Some(state);
    }

    fn hide_scroll_hud(&mut self) {
        *self
            .scroll_hud
            .lock()
            .expect("scroll HUD state is poisoned") = None;
    }

    fn poll_scroll_hud(&mut self) -> Option<ScrollHudAction> {
        let mut queue = self
            .scroll_actions
            .lock()
            .expect("scroll HUD events are poisoned");
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    fn request_scroll_passthrough(&mut self, requested: bool) {
        self.scroll_passthrough_requested
            .store(requested, Ordering::Release);
    }

    fn scroll_passthrough_ready(&self) -> bool {
        self.scroll_passthrough_ready.load(Ordering::Acquire)
    }

    fn len(&self) -> usize {
        self.log.lock().expect("card log is poisoned").len()
    }

    fn describe(&self) -> String {
        "recording surface (no window; cards are reported, not drawn)".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, colour: [u8; 4]) -> Vec<u8> {
        colour
            .iter()
            .copied()
            .cycle()
            .take((width as usize) * (height as usize) * 4)
            .collect()
    }

    #[test]
    fn a_thumbnail_rejects_a_buffer_that_does_not_match_its_geometry() {
        assert!(Thumbnail::from_rgba(2, 2, vec![0; 16]).is_some());
        assert!(Thumbnail::from_rgba(2, 2, vec![0; 15]).is_none());
        assert!(Thumbnail::from_rgba(0, 2, vec![]).is_none());
    }

    #[test]
    fn downscaling_preserves_a_solid_colour() {
        // A box filter over one colour must return that colour, exactly. If it
        // does not, the averaging is wrong somewhere.
        let source = solid(64, 64, [10, 200, 30, 255]);
        let thumb = Thumbnail::downscale(64, 64, &source, 8).expect("bounded thumbnail");
        assert_eq!(thumb.width(), 8);
        assert_eq!(thumb.height(), 8);
        for px in thumb.pixels().as_chunks::<4>().0 {
            assert_eq!(*px, [10, 200, 30, 255]);
        }
    }

    #[test]
    fn downscaling_keeps_the_aspect_ratio() {
        // The real case: a 3456×2234 Retina capture on this machine.
        let source = solid(3456, 2234, [1, 2, 3, 255]);
        let thumb = Thumbnail::downscale(3456, 2234, &source, THUMBNAIL_MAX_EDGE)
            .expect("bounded thumbnail");
        assert_eq!(thumb.width(), THUMBNAIL_MAX_EDGE);
        assert_eq!(thumb.height(), 414);
        assert_eq!(
            thumb.pixels().len(),
            (thumb.width() as usize) * (thumb.height() as usize) * 4
        );
    }

    #[test]
    fn an_image_smaller_than_the_target_is_left_alone() {
        let source = solid(10, 4, [9, 9, 9, 255]);
        let thumb = Thumbnail::downscale(10, 4, &source, 512).expect("bounded thumbnail");
        assert_eq!((thumb.width(), thumb.height()), (10, 4));
        assert_eq!(thumb.pixels(), source.as_slice());
    }

    #[test]
    fn a_wide_gamut_thumbnail_is_converted_to_srgb() {
        let source = [180, 100, 40, 200];
        let frame = Frame {
            data: source.to_vec(),
            size: scrozz_core::PhysicalSize::new(1.0, 1.0),
            stride: 4,
            format: scrozz_core::PixelFormat::Rgba8,
            color_space: ColorSpace::DisplayP3,
            scale: scrozz_core::ScaleFactor::IDENTITY,
        };
        let thumbnail = Thumbnail::from_frame(&frame, 512).unwrap();
        let expected = ColorTransform::new(ColorSpace::DisplayP3, ColorSpace::Srgb)
            .convert_u8(source[..3].try_into().unwrap());

        assert_eq!(&thumbnail.pixels()[..3], &expected);
        assert_eq!(thumbnail.pixels()[3], source[3]);
        assert_ne!(&thumbnail.pixels()[..3], &source[..3]);
    }

    #[test]
    fn downscaling_averages_across_a_boundary() {
        // Left half black, right half white, halved horizontally: each output
        // column must be the mean of the two it covers.
        let mut source = vec![0u8; 16];
        for x in 0..4 {
            let v = if x < 2 { 0 } else { 255 };
            source[x * 4..x * 4 + 4].copy_from_slice(&[v, v, v, 255]);
        }
        let thumb = Thumbnail::downscale(4, 1, &source, 2).expect("bounded thumbnail");
        assert_eq!(thumb.width(), 2);
        assert_eq!(&thumb.pixels()[0..4], &[0, 0, 0, 255]);
        assert_eq!(&thumb.pixels()[4..8], &[255, 255, 255, 255]);
    }

    #[test]
    fn a_truncated_buffer_is_rejected_instead_of_becoming_a_blank_texture() {
        let short = vec![0u8; 10];
        assert!(Thumbnail::downscale(64, 64, &short, 4).is_none());
    }

    #[test]
    fn texture_dimensions_are_rejected_before_upload_when_they_exceed_the_cap() {
        assert!(Thumbnail::from_rgba(PIN_TEXTURE_MAX_EDGE + 1, 1, vec![]).is_none());
        assert!(Thumbnail::downscale(1, 1, &[0; 4], PIN_TEXTURE_MAX_EDGE + 1).is_none());
    }

    #[test]
    fn a_recording_surface_keeps_what_it_was_shown() {
        let mut surface = Recording::new();
        assert!(surface.is_empty());

        surface
            .present(Card::placeholder(CardId(1), CaptureKind::Region))
            .expect("recording never refuses");
        surface
            .present(Card::placeholder(CardId(2), CaptureKind::Fullscreen))
            .expect("recording never refuses");
        assert_eq!(surface.len(), 2);

        surface.dismiss(CardId(1));
        assert_eq!(surface.len(), 1);
        assert_eq!(surface.presented()[0].id, CardId(2));
    }

    #[test]
    fn injected_events_come_back_in_order() {
        let mut surface = Recording::new();
        surface.inject(CardEvent::Copy(CardId(7)));
        surface.inject(CardEvent::Dismiss(CardId(7)));

        assert_eq!(surface.poll(), Some(CardEvent::Copy(CardId(7))));
        assert_eq!(surface.poll(), Some(CardEvent::Dismiss(CardId(7))));
        assert_eq!(surface.poll(), None);
    }

    #[test]
    fn every_event_names_its_card() {
        let id = CardId(3);
        for event in [
            CardEvent::Copy(id),
            CardEvent::Save {
                card: id,
                choose_destination: false,
            },
            CardEvent::Upload(id),
            CardEvent::Dismiss(id),
            CardEvent::Drag {
                card: id,
                at: crate::gui::drag::DragSpot {
                    card: [0.0, 0.0, 10.0, 10.0],
                    pointer: [5.0, 5.0],
                    keep_after_accept: false,
                },
            },
            CardEvent::Collapse(id),
            CardEvent::Open(id),
        ] {
            assert_eq!(event.card(), Some(id));
        }
    }

    #[test]
    fn a_card_summarises_itself() {
        let mut card = Card::placeholder(CardId(1), CaptureKind::Fullscreen);
        card.source_width = 3456;
        card.source_height = 2234;
        card.scale = 2.0;
        card.written = vec!["/tmp/a.png".to_owned()];

        let summary = card.summary();
        assert!(summary.contains("3456×2234"), "{summary}");
        assert!(summary.contains("/tmp/a.png"), "{summary}");
    }
}
