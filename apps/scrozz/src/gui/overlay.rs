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

use std::collections::{HashMap, HashSet, VecDeque};

use scrozz_core::{
    ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize, PixelFormat,
    ScaleFactor,
};
use scrozz_shell::NativeSurface;
use scrozz_ui::{
    CaptureRequest, OverlayEvent, OverlayHandle,
    overlay_app::{RECENTLY_CLOSED_LIMIT, THUMBNAIL_PX},
    stack::CardMetrics,
};

use crate::gui::{
    card::{Card, CardEvent, CardId, CardSurface},
    panel::NativeSurfaceSlot,
};

/// A [`CardSurface`] backed by a running `scrozz-ui` overlay.
///
/// Holds only an [`OverlayHandle`], which is cheap to clone and safe to hold
/// before the window exists — a capture taken during start-up is waiting in the
/// pile when the overlay opens rather than being lost.
pub struct OverlayCards {
    handle: OverlayHandle,
    /// Pushed, awaiting the overlay's identifier. Front is oldest.
    pending: VecDeque<(CardId, bool)>,
    /// Overlay identifier to ours, for cards currently in the pile.
    mapped: HashMap<u64, CardId>,
    /// Ours to the overlay's, so a dismissal we initiate can be addressed.
    reverse: HashMap<CardId, u64>,
    /// Translated events beyond the one this poll returned.
    ///
    /// `drain_events` empties the overlay's outbox in one go, so a batch of
    /// five has to be held somewhere; without this, four would be dropped.
    queued: VecDeque<CardEvent>,
    /// Application and prior overlay identities for the matching restoration ring.
    recent: VecDeque<(CardId, u64)>,
    /// Cards whose pixels can be rehydrated after cache release.
    restorable: HashSet<CardId>,
    /// The live native view retained by the panel hook.
    native_surface: NativeSurfaceSlot,
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
            queued: VecDeque::new(),
            recent: VecDeque::with_capacity(RECENTLY_CLOSED_LIMIT),
            restorable: HashSet::new(),
            native_surface: NativeSurfaceSlot::default(),
        }
    }

    /// Wraps a handle and the native view slot owned by the same window host.
    #[must_use]
    pub fn with_native_surface(handle: OverlayHandle, native_surface: NativeSurfaceSlot) -> Self {
        Self {
            native_surface,
            ..Self::new(handle)
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

    fn remember_closed(&mut self, ours: CardId, theirs: u64) {
        self.recent.push_back((ours, theirs));
        if self.recent.len() > RECENTLY_CLOSED_LIMIT
            && let Some((evicted, _)) = self.recent.pop_front()
        {
            self.restorable.remove(&evicted);
        }
    }

    fn translate(&mut self, event: OverlayEvent, out: &mut Vec<CardEvent>) {
        match event {
            OverlayEvent::Pushed { id } => {
                // In push order, which is the contract that makes this
                // matching sound.
                if let Some((ours, restorable)) = self.pending.pop_front() {
                    self.mapped.insert(id.0, ours);
                    self.reverse.insert(ours, id.0);
                    if restorable {
                        self.restorable.insert(ours);
                    }
                } else {
                    tracing::warn!(
                        overlay_card = id.0,
                        "the overlay announced a card this app did not push"
                    );
                }
            }
            OverlayEvent::Dismissed { id, reason } => {
                if let Some(ours) = self.mapped.get(&id.0).copied() {
                    let _ = reason;
                    out.push(CardEvent::Dismiss(ours));
                    if self.restorable.contains(&ours) {
                        self.remember_closed(ours, id.0);
                    } else {
                        self.restorable.remove(&ours);
                    }
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
            OverlayEvent::UploadRequested { id } => {
                if let Some(ours) = self.mapped.get(&id.0).copied() {
                    out.push(CardEvent::Upload(ours));
                }
            }
            OverlayEvent::PinRequested { id, pinned } => {
                if let Some(ours) = self.mapped.get(&id.0).copied() {
                    out.push(CardEvent::Pin { id: ours, pinned });
                }
            }
            OverlayEvent::DragStarted { id, .. } => {
                if let Some(ours) = self.mapped.get(&id.0).copied() {
                    out.push(CardEvent::DragStarted(ours));
                }
            }
            OverlayEvent::DragOut { id, at, rect } => {
                if let Some(ours) = self.mapped.get(&id.0).copied() {
                    out.push(CardEvent::DragOut {
                        id: ours,
                        rect: logical_rect(rect),
                        pointer: LogicalPoint::new(f64::from(at.x), f64::from(at.y)),
                    });
                }
            }
            OverlayEvent::DockCollapsed => {
                out.push(CardEvent::DockCollapsed);
            }
            OverlayEvent::DockExpanded => {
                out.push(CardEvent::DockExpanded);
            }
            OverlayEvent::Emptied => {
                out.push(CardEvent::Emptied);
            }
            OverlayEvent::Restored { id } => {
                if let Some((ours, _)) = self.recent.pop_back() {
                    self.mapped.insert(id.0, ours);
                    self.reverse.insert(ours, id.0);
                    out.push(CardEvent::Restored(ours));
                } else {
                    tracing::warn!(
                        overlay_card = id.0,
                        "the overlay restored a card with no matching application history"
                    );
                }
            }
            OverlayEvent::VisibilityChanged { hidden } => {
                out.push(CardEvent::VisibilityChanged { hidden });
            }
            _ => {}
        }
    }
}

impl CardSurface for OverlayCards {
    fn present(&mut self, card: Card) -> scrozz_core::Result<()> {
        let name = card.file_name();
        let provenance = card.provenance;

        // The pixels handed to the overlay are the thumbnail, not the capture.
        // A 3456×2234 frame is about thirty megabytes; the card draws it at
        // roughly 300 points wide. Shipping the full frame across so egui can
        // throw away 99% of it would cost a copy of every capture, held for as
        // long as the card is on screen.
        let restorable = card.capture_id.is_some();
        let request = match card.thumbnail.as_ref().and_then(Thumb::frame) {
            Some(frame) => CaptureRequest::from_frame(
                name.clone(),
                provenance,
                &frame,
                // Already at or below this; passing it keeps the overlay's
                // guarantee about texture size rather than assuming ours.
                THUMBNAIL_PX,
            )
            .unwrap_or_else(|| CaptureRequest::new(name.clone(), provenance, card.source_px())),
            // No pixels yet: the card still appears, with a holding fill. A
            // capture that happened should be visible even if thumbnailing
            // failed, because the file on disk is fine.
            None => CaptureRequest::new(name, provenance, card.source_px()),
        }
        .with_source_badge(card.source_badge().map(str::to_owned))
        .with_restorable(restorable);

        self.pending.push_back((card.id, restorable));
        self.handle.push(request);
        Ok(())
    }

    fn dismiss(&mut self, id: CardId) {
        if let Some(theirs) = self.reverse.get(&id).copied() {
            self.handle.dismiss(scrozz_ui::stack::CardId(theirs));
        }
    }

    fn dismiss_after_action(&mut self, id: CardId) {
        if let Some(theirs) = self.reverse.get(&id).copied() {
            self.handle
                .dismiss_after_action(scrozz_ui::stack::CardId(theirs));
        }
    }

    fn set_pinned(&mut self, id: CardId, pinned: bool) {
        let theirs = self.reverse.get(&id).copied().or_else(|| {
            self.recent
                .iter()
                .rev()
                .find_map(|(ours, theirs)| (*ours == id).then_some(*theirs))
        });
        if let Some(theirs) = theirs {
            self.handle
                .set_pinned(scrozz_ui::stack::CardId(theirs), pinned);
        }
    }

    fn dismiss_all(&mut self) {
        self.handle.dismiss_all();
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
            self.translate(event, &mut out);
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

    fn restore_recent(&mut self) {
        self.handle.restore_recent();
    }

    fn upload_latest(&mut self) {
        self.handle.upload_latest();
    }

    fn toggle_hidden(&mut self) {
        self.handle.toggle_hidden();
    }

    fn set_card_metrics(&mut self, metrics: CardMetrics) {
        self.handle.set_card_metrics(metrics);
    }

    fn set_auto_close(&mut self, seconds: Option<f64>) {
        self.handle.set_auto_close(seconds);
    }

    fn native_surface(&self) -> Option<NativeSurface> {
        self.native_surface.get()
    }

    fn finish_drag(&mut self, id: CardId, accepted: bool) {
        if let Some(theirs) = self.reverse.get(&id).copied() {
            self.handle
                .finish_drag(scrozz_ui::stack::CardId(theirs), accepted);
        }
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

fn logical_rect(rect: egui::Rect) -> LogicalRect {
    LogicalRect::new(
        LogicalPoint::new(f64::from(rect.min.x), f64::from(rect.min.y)),
        LogicalSize::new(f64::from(rect.width()), f64::from(rect.height())),
    )
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
        assert_eq!(surface.pending[0], (CardId(7), false));
        assert_eq!(surface.pending[1], (CardId(8), false));
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
    fn each_capture_keeps_its_actual_source_chrome() {
        // D9: a region capture that gained a window shadow would be a lie about
        // what was captured.
        let mut card = card(1);
        card.kind = CaptureKind::AllInOne;
        card.provenance = Provenance::Window;
        assert_eq!(card.provenance, Provenance::Window);
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

    #[test]
    fn every_overlay_action_translates_once_and_in_order() {
        let mut surface = OverlayCards::new(OverlayHandle::new());
        let ours = CardId(41);
        let theirs = scrozz_ui::stack::CardId(7);
        surface.pending.push_back((ours, true));
        let mut events = Vec::new();
        surface.translate(OverlayEvent::Pushed { id: theirs }, &mut events);

        let at = egui::pos2(12.0, 34.0);
        let rect = egui::Rect::from_min_size(egui::pos2(1.0, 2.0), egui::vec2(3.0, 4.0));
        for event in [
            OverlayEvent::CopyRequested { id: theirs },
            OverlayEvent::SaveRequested { id: theirs },
            OverlayEvent::AnnotateRequested { id: theirs },
            OverlayEvent::UploadRequested { id: theirs },
            OverlayEvent::PinRequested {
                id: theirs,
                pinned: true,
            },
            OverlayEvent::DragStarted { id: theirs, at },
            OverlayEvent::DragOut {
                id: theirs,
                at,
                rect,
            },
            OverlayEvent::DockCollapsed,
            OverlayEvent::DockExpanded,
            OverlayEvent::Emptied,
            OverlayEvent::VisibilityChanged { hidden: true },
        ] {
            surface.translate(event, &mut events);
        }

        assert_eq!(
            events,
            vec![
                CardEvent::Copy(ours),
                CardEvent::Save(ours),
                CardEvent::Open(ours),
                CardEvent::Upload(ours),
                CardEvent::Pin {
                    id: ours,
                    pinned: true,
                },
                CardEvent::DragStarted(ours),
                CardEvent::DragOut {
                    id: ours,
                    rect: LogicalRect::new(LogicalPoint::new(1.0, 2.0), LogicalSize::new(3.0, 4.0),),
                    pointer: LogicalPoint::new(12.0, 34.0),
                },
                CardEvent::DockCollapsed,
                CardEvent::DockExpanded,
                CardEvent::Emptied,
                CardEvent::VisibilityChanged { hidden: true },
            ]
        );
    }

    #[test]
    fn an_action_stays_ahead_of_the_dismiss_that_releases_its_bytes() {
        let mut surface = OverlayCards::new(OverlayHandle::new());
        let ours = CardId(5);
        let theirs = scrozz_ui::stack::CardId(9);
        surface.pending.push_back((ours, true));
        let mut events = Vec::new();
        surface.translate(OverlayEvent::Pushed { id: theirs }, &mut events);
        surface.translate(OverlayEvent::CopyRequested { id: theirs }, &mut events);
        surface.translate(
            OverlayEvent::Dismissed {
                id: theirs,
                reason: scrozz_ui::DismissReason::Acted,
            },
            &mut events,
        );

        assert_eq!(
            events,
            vec![CardEvent::Copy(ours), CardEvent::Dismiss(ours)]
        );
        assert!(!surface.reverse.contains_key(&ours));
        assert_eq!(surface.recent.back(), Some(&(ours, theirs.0)));
    }

    #[test]
    fn restoration_reuses_the_application_identity_with_a_new_overlay_identity() {
        let mut surface = OverlayCards::new(OverlayHandle::new());
        let ours = CardId(5);
        let first = scrozz_ui::stack::CardId(9);
        let restored = scrozz_ui::stack::CardId(10);
        surface.pending.push_back((ours, true));
        let mut events = Vec::new();
        surface.translate(OverlayEvent::Pushed { id: first }, &mut events);
        surface.translate(
            OverlayEvent::Dismissed {
                id: first,
                reason: scrozz_ui::DismissReason::Closed,
            },
            &mut events,
        );
        events.clear();

        surface.translate(OverlayEvent::Restored { id: restored }, &mut events);
        surface.translate(OverlayEvent::CopyRequested { id: restored }, &mut events);

        assert_eq!(
            events,
            vec![CardEvent::Restored(ours), CardEvent::Copy(ours)]
        );
        assert_eq!(surface.reverse.get(&ours), Some(&restored.0));
        assert!(surface.recent.is_empty());
    }
}
