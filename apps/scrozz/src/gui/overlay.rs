//! The adapter between this app's cards and `scrozz-ui`'s capture stack.
//!
//! `scrozz-ui` owns everything about how a card looks and moves: the pile at the
//! bottom-left, the swipe-to-dismiss, the dock it collapses into, the hover
//! chrome. What it does not own is *where the pixels came from* or *what
//! happens when you click copy* — that is this binary's job. So the two meet
//! here, and the seam is narrow on purpose.
//!
//! # The two identity spaces
//!
//! Both sides number their cards. [`crate::gui::CardId`] is allocated by the
//! capture pipeline before a screenshot is even taken, so a card can be tracked
//! from shutter to clipboard. [`scrozz_ui::stack::CardId`] is allocated by the
//! stack when the card enters the pile. They are different numbers for the same
//! card and must be translated, because the overlay reports events in *its*
//! numbering and the pipeline only understands *ours*.
//!
//! The overlay assigns its identifier on push and announces it with
//! [`OverlayEvent::Pushed`], in push order. So the translation is a queue: what
//! we pushed and have not yet been told the identifier for, matched up as the
//! announcements arrive.

use std::collections::{HashMap, VecDeque};

use scrozz_core::{ColorSpace, Frame, PhysicalSize, PixelFormat, ScaleFactor};
use scrozz_store::CaptureId;
use scrozz_ui::{
    CaptureRequest, DismissReason, OverlayEvent, OverlayHandle, overlay_app::THUMBNAIL_PX,
};

use crate::gui::card::{
    Card, CardEvent, CardId, CardSurface, PinnedCapture, SurfaceWaker, Thumbnail,
};

/// A [`CardSurface`] backed by a running `scrozz-ui` overlay.
///
/// Holds only an [`OverlayHandle`], which is cheap to clone and safe to hold
/// before the window exists — a capture taken during start-up is waiting in the
/// pile when the overlay opens rather than being lost.
pub struct OverlayCards {
    handle: OverlayHandle,
    /// Pushed, awaiting the overlay's identifier. Front is oldest.
    pending: VecDeque<CardId>,
    /// Overlay identifier to ours, for cards currently in the pile.
    mapped: HashMap<u64, CardId>,
    /// Ours to the overlay's, so a dismissal we initiate can be addressed.
    reverse: HashMap<CardId, u64>,
    /// Durable pin identity to the card that created it.
    pinned: HashMap<String, CardId>,
    /// Translated events beyond the one this poll returned.
    ///
    /// `drain_events` empties the overlay's outbox in one go, so a batch of
    /// five has to be held somewhere; without this, four would be dropped.
    queued: VecDeque<CardEvent>,
}

impl OverlayCards {
    /// Wraps a handle to an overlay.
    #[must_use]
    pub fn new(handle: OverlayHandle) -> Self {
        Self {
            handle,
            pending: VecDeque::new(),
            mapped: HashMap::new(),
            reverse: HashMap::new(),
            pinned: HashMap::new(),
            queued: VecDeque::new(),
        }
    }

    /// A clone of the handle, for the window that draws it.
    #[must_use]
    pub fn handle(&self) -> OverlayHandle {
        self.handle.clone()
    }

    fn forget(&mut self, ours: CardId) {
        if let Some(theirs) = self.reverse.remove(&ours) {
            self.mapped.remove(&theirs);
        }
    }
}

impl CardSurface for OverlayCards {
    fn present(&mut self, card: Card) -> scrozz_core::Result<()> {
        let request = request_for_card(&card);

        self.pending.push_back(card.id);
        self.handle.push(request);
        Ok(())
    }

    fn dismiss(&mut self, id: CardId) {
        if let Some(theirs) = self.reverse.get(&id).copied() {
            self.handle.dismiss(scrozz_ui::stack::CardId(theirs));
        }
        self.forget(id);
    }

    fn restore_pin(&mut self, pin: PinnedCapture) -> scrozz_core::Result<()> {
        let state = pin.state.clone();
        self.handle.restore_pin(pin_restore_request(pin), state);
        Ok(())
    }

    fn refresh_pin_texture(
        &mut self,
        capture: &CaptureId,
        texture: Thumbnail,
    ) -> scrozz_core::Result<()> {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [texture.width() as usize, texture.height() as usize],
            texture.pixels(),
        );
        self.handle.refresh_pin_texture(capture.0.clone(), image);
        Ok(())
    }

    fn commit_pin(
        &mut self,
        capture: &CaptureId,
        texture: Option<Thumbnail>,
    ) -> scrozz_core::Result<()> {
        if let Some(texture) = texture {
            self.refresh_pin_texture(capture, texture)?;
        }
        self.handle.commit_pin(capture.0.clone());
        if let Some(card) = self.pinned.remove(&capture.0) {
            self.forget(card);
        }
        Ok(())
    }

    fn fail_pin(&mut self, capture: &CaptureId, reason: String) {
        self.handle.fail_pin(capture.0.clone(), reason);
        self.pinned.remove(&capture.0);
    }

    fn discard_pin(&mut self, capture: &CaptureId) {
        self.handle.discard_pin(capture.0.clone());
        self.pinned.remove(&capture.0);
    }

    fn unlock_pins(&mut self) {
        self.handle.unlock_pins();
    }

    fn poll(&mut self) -> Option<CardEvent> {
        // Anything left from the last batch first: the overlay's ordering is
        // the user's gesture order, and reordering it would let a dismiss
        // overtake the copy that preceded it.
        if let Some(queued) = self.queued.pop_front() {
            return Some(queued);
        }

        // `drain_events` empties the outbox, so the whole batch has to be
        // translated at once even though the caller wants one at a time.
        // Anything not translated here would be silently lost.
        let batch = self.handle.drain_events();
        let mut out = Vec::new();

        for event in batch {
            match event {
                OverlayEvent::Pushed { id } => {
                    // In push order, which is the contract that makes this
                    // matching sound.
                    if let Some(ours) = self.pending.pop_front() {
                        self.mapped.insert(id.0, ours);
                        self.reverse.insert(ours, id.0);
                    } else {
                        tracing::warn!(
                            overlay_card = id.0,
                            "the overlay announced a card this app did not push"
                        );
                    }
                }
                OverlayEvent::Dismissed { id, reason } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        // A drag that left the pile has already delivered the
                        // file to wherever it was dropped. Treating it as a
                        // plain dismissal would be right, but saying which it
                        // was lets the pipeline release the bytes either way.
                        out.push(match reason {
                            DismissReason::DragOut => CardEvent::Drag(ours),
                            _ => CardEvent::Dismiss(ours),
                        });
                        self.forget(ours);
                    }
                }
                OverlayEvent::CopyRequested { id } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        out.push(CardEvent::Copy(ours));
                    }
                }
                OverlayEvent::SaveRequested { id } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        out.push(CardEvent::Save(ours));
                    }
                }
                OverlayEvent::AnnotateRequested { id } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        out.push(CardEvent::Open(ours));
                    }
                }
                OverlayEvent::DragStarted { id, .. } | OverlayEvent::DragOut { id, .. } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        out.push(CardEvent::Drag(ours));
                    }
                }
                OverlayEvent::DockCollapsed => {
                    // Collapsing is about the pile, not a card. The oldest one
                    // stands in for it so the event is not lost entirely.
                    if let Some(ours) = self.mapped.values().copied().min() {
                        out.push(CardEvent::Collapse(ours));
                    }
                }
                // Nothing downstream acts on these yet, and inventing a
                // translation for them would be worse than leaving the gap
                // visible.
                OverlayEvent::UploadRequested { .. }
                | OverlayEvent::DockExpanded
                | OverlayEvent::Emptied => {}
                OverlayEvent::PinRequested { id, pin, state } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        self.pinned.insert(pin.0.clone(), ours);
                        out.push(CardEvent::Pin(ours, CaptureId(pin.0), state));
                    }
                }
                OverlayEvent::PinUpdated { pin, state } => {
                    out.push(CardEvent::PinChanged(CaptureId(pin.0), state));
                }
                OverlayEvent::PinClosed { pin } => {
                    self.pinned.remove(&pin.0);
                    out.push(CardEvent::Unpin(CaptureId(pin.0)));
                }
                OverlayEvent::PinUnavailable { card, reason } => {
                    if let Some(ours) = self.mapped.get(&card.0).copied() {
                        out.push(CardEvent::PinUnavailable { card: ours, reason });
                    }
                }
                OverlayEvent::PinPositioningUnavailable { pin, reason } => {
                    out.push(CardEvent::PinPositioningUnavailable {
                        capture: CaptureId(pin.0),
                        reason,
                    });
                }
                _ => {}
            }
        }

        // `drain_events` took everything, so what is not returned now must be
        // kept. One is returned per call to match the trait; the rest wait.
        let mut iter = out.into_iter();
        let first = iter.next();
        for leftover in iter {
            self.queued.push_back(leftover);
        }
        first.or_else(|| self.queued.pop_front())
    }

    fn len(&self) -> usize {
        self.mapped.len() + self.pending.len()
    }

    fn waker(&self) -> Option<SurfaceWaker> {
        let handle = self.handle.clone();
        Some(std::sync::Arc::new(move || handle.wake()))
    }

    fn describe(&self) -> String {
        if self.handle.is_attached() {
            let panel = self
                .handle
                .panel_report()
                .map_or_else(|| "no panel report".to_owned(), |r| r.detail);
            format!("scrozz-ui overlay ({panel})")
        } else {
            "scrozz-ui overlay (no window yet)".to_owned()
        }
    }
}

/// The chrome a capture kind should get.
///
/// Per D9 a window capture keeps the shape and shadow the OS gave it, and a
/// region capture must never gain a synthetic one, so this is not cosmetic.
fn request_for_card(card: &Card) -> CaptureRequest {
    let name = card.file_name();
    let provenance = card.provenance;

    // The pixels handed to the overlay are the thumbnail, not the capture.
    // Preserve the capture's full dimensions separately: pin geometry is based
    // on the source, while egui only needs a bounded texture.
    let mut request = match card.thumbnail.as_ref().and_then(Thumb::frame) {
        Some(frame) => CaptureRequest::from_frame(
            name.clone(),
            provenance,
            &frame,
            // Already at or below this; passing it keeps the overlay's
            // guarantee about texture size rather than assuming ours.
            THUMBNAIL_PX,
        )
        .unwrap_or_else(|| CaptureRequest::new(name.clone(), provenance, card.source_px())),
        // No pixels yet: the card still appears, with a holding fill. A capture
        // that happened should be visible even if thumbnailing failed.
        None => CaptureRequest::new(name, provenance, card.source_px()),
    }
    .with_source_scale(card.scale);
    request.source_px = card.source_px();
    if let Some(capture) = &card.capture_id {
        request = request.with_pin_id(capture.0.clone());
    }
    request
}

/// Build the overlay request for a persisted pin: pixels, or the explanation.
///
/// A restore has two ways to end up without pixels — the worker could not load
/// them, or the loaded texture could not be turned into one this window can
/// upload — and neither may reach the screen as an untextured window with
/// nothing to say. A pin that opens blank looks like a bug in the pin; a pin
/// that opens carrying its reason is a capture whose pixels are gone, which is
/// a different and recoverable thing. So every path that loses the image also
/// produces the sentence that explains it, and the pin keeps its own name
/// either way, so the user can still tell which capture they are looking at.
fn pin_restore_request(pin: PinnedCapture) -> CaptureRequest {
    let source_px = (pin.source_width, pin.source_height);
    let mut conversion_error = None;
    let mut request = match pin.texture.as_ref() {
        Some(texture) => match Thumb::frame(texture) {
            Some(frame) => CaptureRequest::from_frame(
                pin.name.clone(),
                pin.provenance,
                &frame,
                crate::gui::card::PIN_TEXTURE_MAX_EDGE,
            )
            .unwrap_or_else(|| {
                conversion_error = Some(format!(
                    "the stored {}x{} preview exceeded the safe texture limits of this window",
                    texture.width(),
                    texture.height()
                ));
                CaptureRequest::new(pin.name.clone(), pin.provenance, source_px)
            }),
            None => {
                conversion_error =
                    Some("the stored preview reported no pixels to upload".to_owned());
                CaptureRequest::new(pin.name.clone(), pin.provenance, source_px)
            }
        },
        None => CaptureRequest::new(pin.name.clone(), pin.provenance, source_px),
    }
    .with_pin_id(pin.id.0.clone())
    .with_source_scale(pin.scale);

    // Both causes are worth saying when both happened: "the pixels could
    // not be read" and "and the preview could not be uploaded either" are
    // different repairs.
    if let Some(error) = [pin.content_error, conversion_error]
        .into_iter()
        .flatten()
        .reduce(|left, right| format!("{left}; {right}"))
    {
        request = request.with_content_error(error);
    }
    request.source_px = source_px;
    request
}

/// Reading a [`crate::gui::Thumbnail`] as a frame the UI can scale.
trait Thumb {
    /// Wraps the thumbnail's pixels as a frame, without copying more than once.
    fn frame(&self) -> Option<Frame>;
}

impl Thumb for crate::gui::card::Thumbnail {
    fn frame(&self) -> Option<Frame> {
        let (width, height) = (self.width(), self.height());
        if width == 0 || height == 0 {
            return None;
        }
        Some(Frame {
            data: self.pixels().to_vec(),
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride: (width as usize) * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            // The thumbnail is already in pixels the card draws one-for-one.
            scale: ScaleFactor::IDENTITY,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::action::CaptureKind;
    use crate::gui::card::Thumbnail;
    use scrozz_core::Provenance;

    fn card(id: u64) -> Card {
        Card::placeholder(CardId(id), CaptureKind::Fullscreen)
    }

    #[test]
    fn a_pushed_card_is_counted_before_the_overlay_answers() {
        // The window may not exist yet. A capture taken now must still be
        // accounted for, or the app will think nothing is showing.
        let mut surface = OverlayCards::new(OverlayHandle::new());
        surface.present(card(1)).expect("push never refuses");
        assert_eq!(surface.len(), 1);
    }

    #[test]
    fn identifiers_are_matched_in_push_order() {
        let handle = OverlayHandle::new();
        let mut surface = OverlayCards::new(handle);
        surface.present(card(7)).expect("push");
        surface.present(card(8)).expect("push");

        // Simulate the overlay announcing them, which is what a real frame does.
        assert_eq!(surface.pending.len(), 2);
        assert_eq!(surface.pending[0], CardId(7));
        assert_eq!(surface.pending[1], CardId(8));
    }

    fn pinned(texture: Option<Thumbnail>, content_error: Option<String>) -> PinnedCapture {
        PinnedCapture {
            id: CaptureId("capture-1".into()),
            name: "Mail — Inbox".into(),
            provenance: Provenance::Window,
            source_width: 1_280,
            source_height: 800,
            scale: 2.0,
            state: scrozz_core::PinState::new(
                scrozz_core::LogicalRect::new(
                    scrozz_core::LogicalPoint::new(10.0, 20.0),
                    scrozz_core::LogicalSize::new(320.0, 200.0),
                ),
                scrozz_core::PinScale::ORIGINAL,
                None,
            ),
            texture,
            content_error,
        }
    }

    #[test]
    fn a_pin_with_pixels_restores_with_no_failure_attached() {
        let request = pin_restore_request(pinned(Thumbnail::from_rgba(2, 2, vec![255; 16]), None));
        assert!(request.thumbnail.is_some());
        assert_eq!(request.content_error, None);
        assert_eq!(request.name, "Mail — Inbox");
        assert_eq!(request.source_px, (1_280, 800));
        assert_eq!(request.source_scale, 2.0);
    }

    #[test]
    fn a_pin_without_pixels_carries_its_reason_rather_than_opening_blank() {
        // The worker could not read the source. A window with no texture and no
        // sentence is indistinguishable from a broken pin.
        let request = pin_restore_request(pinned(
            None,
            Some("source pixels are unavailable".to_owned()),
        ));
        assert!(request.thumbnail.is_none());
        assert_eq!(
            request.content_error.as_deref(),
            Some("source pixels are unavailable")
        );
        assert_eq!(
            request.name, "Mail — Inbox",
            "a pin that lost its pixels still has to say which capture it is"
        );
        assert_eq!(request.source_px, (1_280, 800));
    }

    #[test]
    fn a_thumbnail_becomes_a_frame_of_the_same_shape() {
        let thumb = Thumbnail::from_rgba(2, 2, vec![255; 16]).expect("well-formed");
        let frame = thumb.frame().expect("a frame");
        assert_eq!(frame.width(), 2);
        assert_eq!(frame.height(), 2);
        assert_eq!(frame.stride, 8);
        assert_eq!(frame.format, PixelFormat::Rgba8);
    }

    #[test]
    fn thumbnail_requests_keep_authoritative_capture_dimensions() {
        let mut card = card(4);
        card.source_width = 3_456;
        card.source_height = 2_234;
        card.scale = 2.0;
        card.thumbnail = Thumbnail::from_rgba(2, 2, vec![255; 16]);

        let request = request_for_card(&card);

        assert_eq!(request.source_px, (3_456, 2_234));
        assert_eq!(request.source_scale, 2.0);
        assert_eq!(
            request.thumbnail.as_ref().map(|image| image.size),
            Some([2, 2])
        );
    }

    #[test]
    fn each_capture_kind_keeps_its_own_chrome() {
        // D9: a region capture that gained a window shadow would be a lie about
        // what was captured.
        assert_eq!(card(1).provenance, Provenance::Display);
        let mut window = card(2);
        window.kind = CaptureKind::Window;
        window.provenance = Provenance::Window;
        assert_eq!(request_for_card(&window).provenance, Provenance::Window);
    }

    #[test]
    fn a_surface_with_no_window_says_so() {
        let surface = OverlayCards::new(OverlayHandle::new());
        assert!(surface.describe().contains("no window"));
    }

    #[test]
    fn dismissing_an_unknown_card_is_harmless() {
        let mut surface = OverlayCards::new(OverlayHandle::new());
        surface.dismiss(CardId(99));
        assert_eq!(surface.len(), 0);
    }
}
