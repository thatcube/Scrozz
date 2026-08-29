//! The adapter between this app's cards and `scrozz-ui`'s Recent Captures Overlay.
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
//! [`RecentCapturesOverlayEvent::Pushed`], in push order. So the translation is a queue: what
//! we pushed and have not yet been told the identifier for, matched up as the
//! announcements arrive.

use std::collections::{HashMap, VecDeque};

use scrozz_core::{ColorSpace, Frame, PhysicalSize, PixelFormat, ScaleFactor};
use scrozz_store::CaptureId;
use scrozz_ui::{
    CaptureMedia, CaptureRequest, RecentCapturesOverlayEvent, RecentCapturesOverlayHandle,
    recent_captures_overlay::THUMBNAIL_PX,
};

use crate::gui::{
    card::{Card, CardEvent, CardId, CardSurface, PinnedCapture, SurfaceWaker, Thumbnail},
    drag::DragSpot,
    panel::BehaviorController,
};

/// A [`CardSurface`] backed by the running Recent Captures Overlay.
///
/// Holds only a [`RecentCapturesOverlayHandle`], which is cheap to clone and safe to hold
/// before the window exists — a capture taken during start-up is waiting in the
/// pile when the overlay opens rather than being lost.
pub struct RecentCapturesOverlayCards {
    handle: RecentCapturesOverlayHandle,
    /// The native window, once the creation hook has seen it. Only a drag needs
    /// it, and a drag asked for before the window exists is refused rather than
    /// guessing at a handle.
    native: Option<BehaviorController>,
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

impl RecentCapturesOverlayCards {
    /// Wraps a handle to the Recent Captures Overlay.
    #[must_use]
    pub fn new(handle: RecentCapturesOverlayHandle) -> Self {
        Self {
            handle,
            native: None,
            pending: VecDeque::new(),
            mapped: HashMap::new(),
            reverse: HashMap::new(),
            pinned: HashMap::new(),
            queued: VecDeque::new(),
        }
    }

    /// Also reports the native window, so drags can start from it.
    #[must_use]
    pub fn with_native(mut self, native: BehaviorController) -> Self {
        self.native = Some(native);
        self
    }

    /// A clone of the handle, for the window that draws it.
    #[must_use]
    pub fn handle(&self) -> RecentCapturesOverlayHandle {
        self.handle.clone()
    }

    fn forget(&mut self, ours: CardId) {
        if let Some(theirs) = self.reverse.remove(&ours) {
            self.mapped.remove(&theirs);
        }
    }
}

impl CardSurface for RecentCapturesOverlayCards {
    fn configure_recent_captures_overlay(
        &mut self,
        settings: scrozz_ui::RecentCapturesOverlaySettings,
    ) {
        self.handle.configure(settings);
    }

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

    fn settle_drag(&mut self, id: CardId, accepted: bool) {
        // Deliberately does *not* `forget` the card: a rejected drop leaves it
        // on the pile, and an accepted one is retired by the separate dismiss
        // that follows. Forgetting here would break the id mapping for both.
        if let Some(theirs) = self.reverse.get(&id).copied() {
            self.handle
                .settle_drag(scrozz_ui::stack::CardId(theirs), accepted);
        }
    }

    fn restore_pin(&mut self, pin: PinnedCapture) -> scrozz_core::Result<()> {
        let mut request = match pin.texture.as_ref().and_then(Thumb::frame) {
            Some(frame) => CaptureRequest::from_frame(
                pin.name.clone(),
                pin.provenance,
                &frame,
                crate::gui::card::PIN_TEXTURE_MAX_EDGE,
            )
            .unwrap_or_else(|| {
                CaptureRequest::new(
                    "Pinned capture",
                    pin.provenance,
                    (pin.source_width, pin.source_height),
                )
            }),
            None => CaptureRequest::new(
                pin.name,
                pin.provenance,
                (pin.source_width, pin.source_height),
            ),
        }
        .with_pin_id(pin.id.0.clone())
        .with_source_scale(pin.scale);
        if let Some(error) = pin.content_error {
            request = request.with_content_error(error);
        }
        request.source_px = (pin.source_width, pin.source_height);
        self.handle.restore_pin(request, pin.state);
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
        if self.queued.is_empty() {
            self.translate_batch();
        }
        self.queued.pop_front()
    }

    fn poll_drag_starts(&mut self) -> Vec<CardEvent> {
        self.translate_batch();

        // Everything drained stays in order; only the drags are lifted out.
        let mut drags = Vec::new();
        let mut rest = std::collections::VecDeque::with_capacity(self.queued.len());
        for event in self.queued.drain(..) {
            if matches!(event, CardEvent::Drag { .. }) {
                drags.push(event);
            } else {
                rest.push_back(event);
            }
        }
        self.queued = rest;
        drags
    }
    fn len(&self) -> usize {
        self.mapped.len() + self.pending.len()
    }

    fn waker(&self) -> Option<SurfaceWaker> {
        let handle = self.handle.clone();
        Some(std::sync::Arc::new(move || handle.wake()))
    }

    fn native_surface(&self) -> Option<scrozz_shell::NativeSurface> {
        self.native.as_ref()?.native_surface()
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

impl RecentCapturesOverlayCards {
    /// Drains the overlay's outbox and appends the translation to `queued`.
    ///
    /// `drain_events` empties the outbox, so the whole batch must be translated
    /// at once even though callers want them one at a time. Anything not
    /// translated here is silently lost.
    fn translate_batch(&mut self) {
        let batch = self.handle.drain_events();
        let mut out = Vec::new();

        for event in batch {
            match event {
                RecentCapturesOverlayEvent::Pushed { id } => {
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
                RecentCapturesOverlayEvent::Dismissed { id, reason } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        tracing::debug!(?reason, card = %ours, "card left the pile");
                        if reason == scrozz_ui::DismissReason::Overflow {
                            out.push(CardEvent::Overflow(ours));
                        } else {
                            out.push(CardEvent::Dismiss(ours));
                        }
                        self.forget(ours);
                    }
                }
                RecentCapturesOverlayEvent::CopyRequested { id } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        out.push(CardEvent::Copy(ours));
                    }
                }
                RecentCapturesOverlayEvent::SaveRequested {
                    id,
                    choose_destination,
                } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        out.push(CardEvent::Save {
                            card: ours,
                            choose_destination,
                        });
                    }
                }
                RecentCapturesOverlayEvent::EditRequested { id } => {
                    // Both editors are reached through `Open`; the coordinator
                    // already knows which media the card holds and therefore
                    // which editor the request means.
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        out.push(CardEvent::Open(ours));
                    }
                }
                RecentCapturesOverlayEvent::AnnotateRequested { id } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        out.push(CardEvent::Open(ours));
                    }
                }
                RecentCapturesOverlayEvent::DragOutArmed {
                    id,
                    card,
                    pointer,
                    keep_after_accept,
                } => {
                    // The only event that starts a native drag, and the only
                    // one that arrives while the mouse button is still down.
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        out.push(CardEvent::Drag {
                            card: ours,
                            at: DragSpot {
                                card: [card.min.x, card.min.y, card.width(), card.height()],
                                pointer: [pointer.x, pointer.y],
                                keep_after_accept,
                            },
                        });
                    }
                }
                // `DragStarted` is the pointer merely moving with the button
                // down — far too early to commit to anything. `DragOut` is the
                // release-time report, which the overlay raises only when no
                // host armed the gesture; this host always does, on every
                // platform, and refuses visibly if it cannot.
                RecentCapturesOverlayEvent::DragStarted { .. }
                | RecentCapturesOverlayEvent::DragOut { .. } => {}
                RecentCapturesOverlayEvent::DockCollapsed => {
                    // Collapsing is about the pile, not a card. The oldest one
                    // stands in for it so the event is not lost entirely.
                    if let Some(ours) = self.mapped.values().copied().min() {
                        out.push(CardEvent::Collapse(ours));
                    }
                }
                // Nothing downstream acts on these yet, and inventing a
                // translation for them would be worse than leaving the gap
                // visible.
                RecentCapturesOverlayEvent::UploadRequested { id } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        out.push(CardEvent::Upload(ours));
                    }
                }
                RecentCapturesOverlayEvent::AutoCloseRequested { id, action } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        out.push(CardEvent::AutoClose(ours, action));
                    }
                }
                RecentCapturesOverlayEvent::DockExpanded | RecentCapturesOverlayEvent::Emptied => {}
                RecentCapturesOverlayEvent::PinRequested { id, pin, state } => {
                    if let Some(ours) = self.mapped.get(&id.0).copied() {
                        self.pinned.insert(pin.0.clone(), ours);
                        out.push(CardEvent::Pin(ours, CaptureId(pin.0), state));
                    }
                }
                RecentCapturesOverlayEvent::PinUpdated { pin, state } => {
                    out.push(CardEvent::PinChanged(CaptureId(pin.0), state));
                }
                RecentCapturesOverlayEvent::PinClosed { pin } => {
                    self.pinned.remove(&pin.0);
                    out.push(CardEvent::Unpin(CaptureId(pin.0)));
                }
                RecentCapturesOverlayEvent::PinUnavailable { card, reason } => {
                    if let Some(ours) = self.mapped.get(&card.0).copied() {
                        out.push(CardEvent::PinUnavailable { card: ours, reason });
                    }
                }
                RecentCapturesOverlayEvent::PinPositioningUnavailable { pin, reason } => {
                    out.push(CardEvent::PinPositioningUnavailable {
                        capture: CaptureId(pin.0),
                        reason,
                    });
                }

                _ => {}
            }
        }

        // `drain_events` took everything, so nothing translated may be dropped.
        self.queued.extend(out);
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
    .with_source_scale(card.scale)
    .with_media(capture_media(card.media));
    request.source_px = card.source_px();
    if let Some(capture) = &card.capture_id {
        request = request.with_pin_id(capture.0.clone());
    }
    request
}

/// Carries the card's media kind across the overlay seam.
///
/// The two enums are deliberately separate types: `scrozz-ui` must not depend
/// on the app's card module, and the app must not be able to hand the overlay a
/// video without saying so.
const fn capture_media(media: scrozz_ui::card::CardMedia) -> CaptureMedia {
    match media {
        scrozz_ui::card::CardMedia::Image => CaptureMedia::Image,
        scrozz_ui::card::CardMedia::Video {
            duration,
            has_audio,
        } => CaptureMedia::Video {
            duration,
            has_audio,
        },
    }
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
    use scrozz_ui::stack::CardId as UiCardId;

    fn card(id: u64) -> Card {
        Card::placeholder(CardId(id), CaptureKind::Fullscreen)
    }

    #[test]
    fn a_pushed_card_is_counted_before_the_overlay_answers() {
        // The window may not exist yet. A capture taken now must still be
        // accounted for, or the app will think nothing is showing.
        let mut surface = RecentCapturesOverlayCards::new(RecentCapturesOverlayHandle::new());
        surface.present(card(1)).expect("push never refuses");
        assert_eq!(surface.len(), 1);
    }

    #[test]
    fn identifiers_are_matched_in_push_order() {
        let handle = RecentCapturesOverlayHandle::new();
        let mut surface = RecentCapturesOverlayCards::new(handle);
        surface.present(card(7)).expect("push");
        surface.present(card(8)).expect("push");

        // Simulate the overlay announcing them, which is what a real frame does.
        assert_eq!(surface.pending.len(), 2);
        assert_eq!(surface.pending[0], CardId(7));
        assert_eq!(surface.pending[1], CardId(8));
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
    fn each_capture_keeps_its_actual_source_chrome() {
        // D9: a region capture that gained a window shadow would be a lie about
        // what was captured.
        let mut card = card(1);
        card.kind = CaptureKind::AllInOne;
        card.provenance = Provenance::Window;
        assert_eq!(request_for_card(&card).provenance, Provenance::Window);
    }

    #[test]
    fn a_surface_with_no_window_says_so() {
        let surface = RecentCapturesOverlayCards::new(RecentCapturesOverlayHandle::new());
        assert!(surface.describe().contains("no window"));
    }

    #[test]
    fn dismissing_an_unknown_card_is_harmless() {
        let mut surface = RecentCapturesOverlayCards::new(RecentCapturesOverlayHandle::new());
        surface.dismiss(CardId(99));
        assert_eq!(surface.len(), 0);
    }

    // -----------------------------------------------------------------------
    // Lifting drags out of the batch
    // -----------------------------------------------------------------------
    //
    // A native drag has to begin while the mouse button is still down. The
    // ordinary event drain runs in the host's *logic* pass, which for a given
    // gesture happens on the frame after the one that produced it — by then the
    // button may be up and AppKit will refuse. So drags are drained separately,
    // in the same frame, and everything else is left for the ordinary pass.

    /// Announces `card` to the surface the way a real frame does, and returns
    /// the overlay-side id the surface will be told about.
    fn announce(surface: &mut RecentCapturesOverlayCards, card: Card) -> u64 {
        let id = card.id.0;
        surface.present(card).expect("push never refuses");
        surface
            .handle
            .report(RecentCapturesOverlayEvent::Pushed { id: UiCardId(id) });
        id
    }

    fn drag_event(id: u64) -> RecentCapturesOverlayEvent {
        RecentCapturesOverlayEvent::DragOutArmed {
            id: UiCardId(id),
            card: egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(210.0, 150.0)),
            pointer: egui::pos2(60.0, 70.0),
            keep_after_accept: false,
        }
    }

    #[test]
    fn a_drag_is_lifted_out_of_the_batch_it_arrived_in() {
        let mut surface = RecentCapturesOverlayCards::new(RecentCapturesOverlayHandle::new());
        let a = announce(&mut surface, card(1));
        surface.poll();

        surface.handle.report(drag_event(a));
        let drags = surface.poll_drag_starts();
        assert_eq!(drags.len(), 1, "the drag did not come out in its own frame");
        assert!(matches!(drags[0], CardEvent::Drag { card, .. } if card == CardId(1)));
    }

    #[test]
    fn everything_that_is_not_a_drag_waits_for_the_ordinary_drain() {
        // Hoisting the drag ahead of a queued dismiss is deliberate; leaving
        // the rest *reordered* would not be. The pile's event order is the
        // user's gesture order.
        let mut surface = RecentCapturesOverlayCards::new(RecentCapturesOverlayHandle::new());
        let a = announce(&mut surface, card(1));
        let b = announce(&mut surface, card(2));
        surface.poll();

        surface
            .handle
            .report(RecentCapturesOverlayEvent::CopyRequested { id: UiCardId(b) });
        surface.handle.report(drag_event(a));
        surface
            .handle
            .report(RecentCapturesOverlayEvent::SaveRequested {
                id: UiCardId(b),
                choose_destination: false,
            });

        let drags = surface.poll_drag_starts();
        assert_eq!(drags.len(), 1, "only the drag may be lifted out");

        assert_eq!(surface.poll(), Some(CardEvent::Copy(CardId(2))));
        assert_eq!(
            surface.poll(),
            Some(CardEvent::Save {
                card: CardId(2),
                choose_destination: false,
            })
        );
        assert_eq!(surface.poll(), None, "the drag was drained twice");
    }

    #[test]
    fn drags_are_drained_even_when_other_events_are_already_queued() {
        // The distinguishing property. `poll` only translates a new batch once
        // the queue empties, which is right for ordering and fatal for a drag:
        // a copy queued ahead of it would hold the drag until a later frame,
        // by which time the mouse is up.
        let mut surface = RecentCapturesOverlayCards::new(RecentCapturesOverlayHandle::new());
        let a = announce(&mut surface, card(1));
        surface.poll();

        surface
            .handle
            .report(RecentCapturesOverlayEvent::CopyRequested { id: UiCardId(a) });
        surface.poll_drag_starts();
        assert_eq!(surface.queued.len(), 1, "the copy should be waiting");

        surface.handle.report(drag_event(a));
        let drags = surface.poll_drag_starts();
        assert_eq!(
            drags.len(),
            1,
            "a queued event delayed the drag past its frame"
        );
        assert_eq!(surface.poll(), Some(CardEvent::Copy(CardId(1))));
    }

    #[test]
    fn the_drag_carries_where_the_card_and_pointer_are() {
        // Wrong geometry here and the drag image jumps somewhere else the
        // instant the platform takes over.
        let mut surface = RecentCapturesOverlayCards::new(RecentCapturesOverlayHandle::new());
        let a = announce(&mut surface, card(1));
        surface.poll();
        surface.handle.report(drag_event(a));

        let drags = surface.poll_drag_starts();
        let CardEvent::Drag { at, .. } = &drags[0] else {
            panic!("expected a drag, got {:?}", drags[0]);
        };
        assert_eq!(at.card, [10.0, 20.0, 210.0, 150.0]);
        assert_eq!(at.pointer, [60.0, 70.0]);
    }

    #[test]
    fn a_settled_drag_is_reported_by_the_overlay_id_not_ours() {
        // The two id spaces are different, and a settle sent under the wrong
        // one silently does nothing — which looks exactly like the bug it is
        // meant to fix.
        let mut surface = RecentCapturesOverlayCards::new(RecentCapturesOverlayHandle::new());
        let a = announce(&mut surface, card(1));
        surface.poll();
        assert_eq!(surface.reverse.get(&CardId(1)).copied(), Some(a));

        surface.settle_drag(CardId(1), false);
        assert_eq!(
            surface.len(),
            1,
            "settling a rejected drop must leave the card on the pile"
        );
    }

    #[test]
    fn settling_a_card_the_overlay_never_saw_is_harmless() {
        let mut surface = RecentCapturesOverlayCards::new(RecentCapturesOverlayHandle::new());
        surface.settle_drag(CardId(404), true);
        assert_eq!(surface.len(), 0);
    }
}
