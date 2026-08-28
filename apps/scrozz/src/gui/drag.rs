//! Starting, and then outliving, a drag out of the capture stack.
//!
//! # The timing problem this exists to solve
//!
//! A native drag is not something an application *reports*; it is something an
//! application *starts*, and every platform insists it be started while the
//! mouse button is still held down. AppKit is the strictest: `beginDragging
//! Session:` called after the button came up starts nothing and returns a
//! session that immediately ends. So the capture stack cannot tell us "that
//! card was dragged away" once the gesture finishes — by then it is too late,
//! and the visible symptom is a card that flies off the screen while the chat
//! window it was aimed at receives nothing at all.
//!
//! `scrozz-ui` therefore raises [`OverlayEvent::DragOutArmed`][armed] the
//! moment the pointer has travelled far enough to mean it, *mid*-gesture, and
//! this module turns that into a real [`DragSource::begin`]. From that instant
//! the operating system owns the gesture: it draws the image, it tracks the
//! destination, and it tells us what happened.
//!
//! [armed]: scrozz_ui::OverlayEvent::DragOutArmed
//!
//! # Why the card does not leave on its own
//!
//! Because the drop may be refused. A card that vanishes the moment the pointer
//! crosses a threshold has thrown away the capture whether or not anything
//! caught it, and there is no way back. So the stack springs the card home, and
//! the card is dismissed here — only once [`DragOutcome::Accepted`] says a
//! destination really took it.
//!
//! # Why the file outlives the gesture
//!
//! A drop target is entitled to read the file it was handed *after* the drag
//! ends: Finder copies on its own schedule, and an Electron app may not touch
//! the path until its renderer process gets around to it. Deleting on drop
//! would make Scrozz work in Finder and fail in Slack for reasons no log would
//! explain. So [`DragArtifact`] keeps the file for a retention window and this
//! host sweeps it afterwards, every tick, until it is gone.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};
use scrozz_shell::{
    DragOrigin, DragOutcome, DragPayload, DragPreview, DragSession, DragSource, NativeDragSource,
    NativeSurface, byte_source, drag::artifact,
};

use crate::gui::{card::CardId, pipeline::CaptureBytes};

/// Where a drag-out gesture was when it committed.
///
/// Window-local logical points, top-left origin — the coordinate space
/// `scrozz-ui` reports and [`DragOrigin`] expects. Kept as plain numbers so the
/// card layer never has to name a platform type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DragSpot {
    /// The card's rectangle: x, y, width, height.
    pub card: [f32; 4],
    /// The pointer position: x, y.
    pub pointer: [f32; 2],
}

impl DragSpot {
    /// Converts to the shell's geometry, anchored at `surface`.
    fn origin(self, surface: NativeSurface) -> DragOrigin {
        let [x, y, w, h] = self.card;
        DragOrigin::new(
            surface,
            LogicalRect::new(
                LogicalPoint::new(f64::from(x), f64::from(y)),
                LogicalSize::new(f64::from(w), f64::from(h)),
            ),
            LogicalPoint::new(f64::from(self.pointer[0]), f64::from(self.pointer[1])),
        )
    }
}

/// What a drag that has been started is still waiting to find out.
struct InFlight {
    session: DragSession,
    /// Whether the card has already been taken off the stack, so an outcome
    /// arriving twice cannot dismiss twice.
    dismissed: bool,
}

/// Owns the platform drag source and every drag still in flight.
///
/// # Threading
///
/// Main thread only, like everything else in [`crate::gui::App`]. The platform
/// source is built lazily on first use for exactly that reason: on macOS it
/// touches AppKit, which must not happen on the capture worker.
pub struct DragHost {
    source: Option<NativeDragSource>,
    /// `None` until the window exists. A drag asked for before then is refused
    /// with an explanation rather than silently doing nothing.
    surface: Option<NativeSurface>,
    live: HashMap<CardId, InFlight>,
    root: PathBuf,
    /// Set once the platform source has been tried and found wanting, so a
    /// hopeless platform is explained once rather than on every gesture.
    unavailable: Option<String>,
}

impl Default for DragHost {
    fn default() -> Self {
        Self::new()
    }
}

impl DragHost {
    /// A host with no window yet and nothing in flight.
    #[must_use]
    pub fn new() -> Self {
        Self {
            source: None,
            surface: None,
            live: HashMap::new(),
            root: artifact::artifact_root(),
            unavailable: None,
        }
    }

    /// Deletes drag files left behind by a previous run.
    ///
    /// A drag that was in flight when the process died leaves its file on disk,
    /// because the whole point of the retention window is that nothing deletes
    /// it early. Nothing will ever come back for it either, so it is swept at
    /// start-up. Returns how many were removed, for the log.
    pub fn sweep_orphans(&self) -> usize {
        artifact::sweep_orphans(
            &self.root,
            std::time::SystemTime::now(),
            artifact::ORPHAN_MAX_AGE,
        )
    }

    /// Tells the host which native surface drags start from.
    ///
    /// Called once the window exists. Until then [`Self::begin`] refuses.
    pub fn attach(&mut self, surface: NativeSurface) {
        self.surface = Some(surface);
    }

    /// Whether a window has been attached.
    #[must_use]
    pub const fn is_attached(&self) -> bool {
        self.surface.is_some()
    }

    /// How many drags are still waiting for an outcome or a sweep.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.live.len()
    }

    /// Starts a native drag carrying `card`'s capture.
    ///
    /// # Errors
    ///
    /// Returns a human-readable explanation if there is no window yet, no
    /// capture bytes for the card, or the platform refused to start a session.
    /// Every one of those leaves the card where it was.
    pub fn begin(
        &mut self,
        card: CardId,
        spot: DragSpot,
        bytes: &CaptureBytes,
    ) -> Result<(), String> {
        if let Some(why) = &self.unavailable {
            return Err(why.clone());
        }
        let surface = self
            .surface
            .ok_or_else(|| "the overlay window is not on screen yet".to_owned())?;

        // Re-dragging a card whose previous drag is still settling is fine, but
        // the old session's file must not be adopted by the new one.
        self.live.remove(&card);

        let payload = payload_for(card, bytes);
        let source = self.source()?;
        match source.begin(payload, spot.origin(surface)) {
            Ok(session) => {
                self.live.insert(
                    card,
                    InFlight {
                        session,
                        dismissed: false,
                    },
                );
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }

    /// Services every drag in flight. Never blocks.
    ///
    /// Returns the cards whose drop was **accepted**, which is the only signal
    /// that should take a card off the stack. Cancelled and failed drags are
    /// logged by the caller and leave the card alone.
    pub fn poll(&mut self) -> Vec<CardId> {
        let mut accepted = Vec::new();

        self.live.retain(|card, flight| {
            if let Some(DragOutcome::Accepted { .. }) = flight.session.outcome()
                && !flight.dismissed
            {
                flight.dismissed = true;
                accepted.push(*card);
            }

            // Keep servicing after the outcome: the file is deliberately still
            // on disk, and `sweep` is what eventually removes it.
            flight.session.sweep();
            !flight.session.is_settled()
        });

        accepted
    }

    /// What happened to `card`'s drag, if it has finished.
    #[must_use]
    pub fn outcome(&self, card: CardId) -> Option<DragOutcome> {
        self.live.get(&card).and_then(|f| f.session.outcome())
    }

    fn source(&mut self) -> Result<&NativeDragSource, String> {
        if self.source.is_none() {
            match scrozz_shell::native_drag_source() {
                Ok(source) => self.source = Some(source),
                Err(err) => {
                    let why = err.to_string();
                    self.unavailable = Some(why.clone());
                    return Err(why);
                }
            }
        }
        Ok(self
            .source
            .as_ref()
            .expect("the source was just constructed"))
    }
}

/// The size the drag image is drawn at, in logical points.
///
/// Matched to the card so the picture the user is already looking at is the
/// picture that follows the cursor — a drag image that suddenly changes size at
/// the moment it detaches reads as a glitch.
const PREVIEW_EDGE: f64 = 168.0;

/// Builds the payload for one card.
fn payload_for(card: CardId, bytes: &CaptureBytes) -> DragPayload {
    let full = Arc::clone(&bytes.full);
    let png = byte_source(move || Ok(full.as_ref().clone()));
    let mut payload = DragPayload::png_capture(&stem_for(card), png);

    if let Some(preview) = &bytes.preview
        && let Some(size) = preview_size(preview)
        && let Ok(preview) = DragPreview::from_png(preview.as_ref().clone(), size)
    {
        payload = payload.with_preview(preview);
    }
    payload
}

/// The filename a drop target will see, without its extension.
///
/// Deliberately not the card number: a file called `card-3.png` in a chat is
/// meaningless a week later, and every screenshot tool in the world has settled
/// on a timestamp for the same reason.
fn stem_for(card: CardId) -> String {
    let _ = card;
    scrozz_export::NameTemplate::parse("Scrozz {date} at {time}").map_or_else(
        |_| "Scrozz capture".to_owned(),
        |template| {
            template.render(
                &scrozz_export::NamingContext::now(),
                &scrozz_export::NamePolicy::default(),
            )
        },
    )
}

/// Reads a PNG's dimensions and scales them to fit [`PREVIEW_EDGE`].
///
/// Reads the IHDR directly rather than decoding: the drag image is wanted in
/// the few milliseconds before the session starts, and decoding a thumbnail
/// just to learn its aspect ratio would be paid on every gesture.
fn preview_size(png: &[u8]) -> Option<LogicalSize> {
    // 8-byte signature, 4-byte length, "IHDR", then width and height.
    let width = u32::from_be_bytes(png.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(png.get(20..24)?.try_into().ok()?);
    if width == 0 || height == 0 {
        return None;
    }

    let longest = f64::from(width.max(height));
    let scale = PREVIEW_EDGE / longest;
    let size = LogicalSize::new(f64::from(width) * scale, f64::from(height) * scale);
    (!size.is_empty()).then_some(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 24];
        bytes[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        bytes[12..16].copy_from_slice(b"IHDR");
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        bytes
    }

    fn capture() -> CaptureBytes {
        CaptureBytes {
            full: Arc::new(b"full-resolution-png".to_vec()),
            preview: Some(Arc::new(png(400, 200))),
        }
    }

    #[test]
    fn a_wide_preview_keeps_its_shape() {
        let size = preview_size(&png(400, 200)).expect("a 400x200 PNG has a size");
        assert!((size.width - PREVIEW_EDGE).abs() < 0.001);
        assert!((size.height - PREVIEW_EDGE / 2.0).abs() < 0.001);
    }

    #[test]
    fn a_tall_preview_is_bounded_by_its_height() {
        let size = preview_size(&png(200, 400)).expect("a 200x400 PNG has a size");
        assert!((size.height - PREVIEW_EDGE).abs() < 0.001);
        assert!((size.width - PREVIEW_EDGE / 2.0).abs() < 0.001);
    }

    #[test]
    fn a_truncated_png_has_no_preview_size() {
        assert!(preview_size(&[0x89, b'P', b'N', b'G']).is_none());
        assert!(preview_size(&png(0, 100)).is_none());
    }

    #[test]
    fn the_payload_offers_the_full_resolution_bytes_as_a_png_file() {
        let bytes = capture();
        let payload = payload_for(CardId(7), &bytes);

        assert!(
            payload.file().file_name().ends_with(".png"),
            "a drop target decides what it is from the extension"
        );
        let produced = payload.file().produce().expect("the bytes are in hand");
        assert_eq!(produced, *bytes.full, "the file must be the full capture");
    }

    #[test]
    fn the_payload_offers_the_same_bytes_as_image_data() {
        let bytes = capture();
        let payload = payload_for(CardId(1), &bytes);

        let image = payload.image().expect("an image flavour is offered");
        assert_eq!(
            image().expect("the image bytes are in hand"),
            *bytes.full,
            "an app that pastes rather than saves must get the same picture"
        );
    }

    #[test]
    fn the_drag_image_is_the_thumbnail_not_the_capture() {
        let bytes = capture();
        let payload = payload_for(CardId(1), &bytes);

        let preview = payload.preview_png().expect("a preview is offered");
        assert_eq!(
            preview,
            bytes
                .preview
                .as_deref()
                .map(Vec::as_slice)
                .expect("a thumbnail"),
            "the picture that follows the cursor is the small one"
        );
    }

    #[test]
    fn a_capture_without_a_thumbnail_still_drags() {
        let bytes = CaptureBytes {
            full: Arc::new(b"png".to_vec()),
            preview: None,
        };
        let payload = payload_for(CardId(1), &bytes);

        assert!(payload.preview().is_none());
        assert!(
            payload.image().is_some(),
            "a missing picture costs the drag its looks, not its payload"
        );
    }

    #[test]
    fn a_drag_asked_for_before_the_window_exists_is_refused_not_ignored() {
        let mut host = DragHost::new();
        let spot = DragSpot {
            card: [0.0, 0.0, 100.0, 60.0],
            pointer: [10.0, 10.0],
        };

        let refusal = host
            .begin(CardId(1), spot, &capture())
            .expect_err("there is no window");
        assert!(
            refusal.contains("window"),
            "the refusal must say why: {refusal}"
        );
        assert_eq!(host.in_flight(), 0);
    }

    #[test]
    fn a_spot_becomes_window_local_geometry() {
        let spot = DragSpot {
            card: [12.0, 34.0, 56.0, 78.0],
            pointer: [20.0, 40.0],
        };
        // Safety: never dereferenced — `DragOrigin` only carries the handle.
        let surface = unsafe { NativeSurface::from_raw(std::ptr::null_mut()) };
        let origin = spot.origin(surface);

        assert!((origin.card().origin.x - 12.0).abs() < f64::EPSILON);
        assert!((origin.card().size.height - 78.0).abs() < f64::EPSILON);
        assert!((origin.pointer().y - 40.0).abs() < f64::EPSILON);
    }
}
