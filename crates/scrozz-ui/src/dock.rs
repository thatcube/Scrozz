//! The **capture dock** — decision D20.
//!
//! Swiping the pile down collapses it into a short, wide box at the bottom-left
//! of the work area: the same width as a capture card, roughly one sixth its
//! height, carrying an upward chevron. Clicking it or swiping up brings the
//! captures back.
//!
//! **Collapsing never dismisses.** Nothing is lost — the captures are still
//! there, just out of the way. This is the one gesture that clears the whole
//! overlay in a single motion and costs nothing to undo, and it is a Scrozz
//! original: comparable tools offer "temporarily hide overlays" as a settings
//! toggle, not a spatial, reversible, one-gesture affordance.
//!
//! # Motion
//!
//! Collapse and expand are headline animations (D20): the cards must visibly
//! travel *into* the dock and *out* of it, so the spatial relationship stays
//! legible. A fade would be wrong. [`Dock::collapse_progress`] is the single
//! scalar [`crate::stack::CaptureStack`] interpolates every card along, and like
//! everything else in this surface it is a pure function of a
//! [`Motion`](crate::motion::Motion) instant — nothing here reads a clock.
//!
//! # Module placement
//!
//! This file lives at `src/dock.rs` but is declared by [`crate::stack`] rather
//! than by `lib.rs`, so the whole capture surface is reachable through a single
//! `pub mod stack;`. `lib.rs` must **not** also declare `pub mod dock;` — that
//! would compile this file twice into two unrelated type identities.

use crate::motion::{Activity, Duration, Ease, Motion, Timeline};
use egui::{Rect, Vec2, pos2, vec2};

/// Dock height as a fraction of card height.
///
/// D20 specifies "roughly one-sixth the height". One sixth of a 145 pt card is
/// about 24 pt: a comfortable chevron target, and unmistakably not a card.
pub const HEIGHT_RATIO: f32 = 1.0 / 6.0;

/// The smallest dock that is still a reliable click target.
///
/// On a very small display the derived height can fall below what a pointer can
/// comfortably hit. The dock is the *escape hatch* from the overlay, and D27
/// makes cheap escape a hard requirement, so it is the last thing allowed to
/// shrink.
pub const MIN_HEIGHT: f32 = 18.0;

/// Where the dock is in its collapse cycle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DockPhase {
    /// The pile is showing; the dock is not on screen.
    Open,
    /// Cards are travelling down into the dock.
    Collapsing(Timeline),
    /// The pile is stowed; only the dock is on screen.
    Collapsed,
    /// Cards are travelling back up out of the dock.
    Expanding(Timeline),
}

/// The capture dock: geometry plus the collapse/expand state machine.
///
/// The dock owns no cards. It publishes one number —
/// [`Dock::collapse_progress`] — and the stack interpolates every card between
/// its slot and [`Dock::rect`] along it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dock {
    rect: Rect,
    phase: DockPhase,
    collapse: Duration,
    expand: Duration,
    ease: Ease,
}

impl Dock {
    /// Creates an open dock occupying `rect`.
    #[must_use]
    pub fn new(rect: Rect, collapse: Duration, expand: Duration) -> Self {
        Self {
            rect,
            phase: DockPhase::Open,
            collapse,
            expand,
            // Symmetric easing: the collapse moves on its own, with no user at
            // either end of it.
            ease: Ease::InOutCubic,
        }
    }

    /// The dock's on-screen rectangle: card width, ~1/6 card height, sitting on
    /// the bottom edge of slot 0's footprint.
    #[must_use]
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// Moves the dock, which happens when the work area changes — a display
    /// change, or the Dock/taskbar being shown or hidden.
    pub fn set_rect(&mut self, rect: Rect) {
        self.rect = rect;
    }

    /// Replaces the collapse and expand duration tokens.
    pub fn set_durations(&mut self, collapse: Duration, expand: Duration) {
        self.collapse = collapse;
        self.expand = expand;
    }

    /// The hit target for the upward chevron.
    ///
    /// Centred and square, so a pointer aimed at the glyph hits it. The whole
    /// dock is clickable too — this exists for hit-testing precision and for the
    /// painter, not to narrow the target.
    #[must_use]
    pub fn chevron_rect(&self) -> Rect {
        let side = (self.rect.height() * 0.62).max(8.0);
        Rect::from_center_size(self.rect.center(), vec2(side, side))
    }

    /// The current phase.
    #[must_use]
    pub fn phase(&self) -> DockPhase {
        self.phase
    }

    /// Whether the pile is stowed, or on its way to being stowed.
    ///
    /// This is the predicate for "the user asked for the overlay to be out of
    /// the way", so it is true the instant the gesture commits rather than when
    /// the animation finishes.
    #[must_use]
    pub fn is_stowing(&self) -> bool {
        matches!(self.phase, DockPhase::Collapsed | DockPhase::Collapsing(_))
    }

    /// Whether the collapse has finished.
    #[must_use]
    pub fn is_collapsed(&self) -> bool {
        self.phase == DockPhase::Collapsed
    }

    /// How far the pile has travelled into the dock.
    ///
    /// `0.0` is fully open (cards in their slots), `1.0` is fully collapsed
    /// (cards absorbed into the dock). Pure: the same instant always yields the
    /// same number, and an instant far past the end of an animation is safe to
    /// query without having stepped through the frames in between.
    #[must_use]
    pub fn collapse_progress(&self, m: &Motion) -> f32 {
        match self.phase {
            DockPhase::Open => 0.0,
            DockPhase::Collapsed => 1.0,
            DockPhase::Collapsing(t) => t.value(m).clamp(0.0, 1.0),
            DockPhase::Expanding(t) => 1.0 - t.value(m).clamp(0.0, 1.0),
        }
    }

    /// Whether the dock itself should be painted.
    #[must_use]
    pub fn is_visible(&self, m: &Motion) -> bool {
        self.collapse_progress(m) > 0.0
    }

    /// The dock's own opacity.
    ///
    /// Trails the cards, so the box does not appear before there is anything in
    /// it to hold.
    #[must_use]
    pub fn alpha(&self, m: &Motion) -> f32 {
        ((self.collapse_progress(m) - 0.25) / 0.75).clamp(0.0, 1.0)
    }

    /// What the dock contributes to the frame schedule.
    ///
    /// egui repaints only on input, so a caller must apply this or the collapse
    /// silently does nothing (D19).
    #[must_use]
    pub fn activity(&self, m: &Motion) -> Activity {
        match self.phase {
            DockPhase::Open | DockPhase::Collapsed => Activity::IDLE,
            DockPhase::Collapsing(t) | DockPhase::Expanding(t) => t.activity(m),
        }
    }

    /// Begins collapsing the pile into the dock.
    ///
    /// Reversing mid-flight is continuous: an expand interrupted halfway
    /// collapses from where it actually is, not from fully open. Back-dating the
    /// timeline's start is what buys that, and it keeps the whole thing a pure
    /// function of the instant rather than a value that has to be stepped.
    pub fn collapse(&mut self, m: &Motion) {
        if self.is_stowing() {
            return;
        }
        let done = self.collapse_progress(m);
        self.phase = DockPhase::Collapsing(back_dated(m, done, self.collapse, self.ease));
        self.advance(m);
    }

    /// Brings the captures back out of the dock.
    pub fn expand(&mut self, m: &Motion) {
        if !self.is_stowing() {
            return;
        }
        let done = 1.0 - self.collapse_progress(m);
        self.phase = DockPhase::Expanding(back_dated(m, done, self.expand, self.ease));
        self.advance(m);
    }

    /// Collapses if open, expands if stowed. This is what the chevron does.
    pub fn toggle(&mut self, m: &Motion) {
        if self.is_stowing() {
            self.expand(m);
        } else {
            self.collapse(m);
        }
    }

    /// Promotes a finished animation to its terminal phase.
    pub fn advance(&mut self, m: &Motion) {
        match self.phase {
            DockPhase::Collapsing(t) if !t.is_active(m) => self.phase = DockPhase::Collapsed,
            DockPhase::Expanding(t) if !t.is_active(m) => self.phase = DockPhase::Open,
            _ => {}
        }
    }

    /// Where a card resting at `slot_rect` should be drawn.
    ///
    /// Cards do not merely fade into the dock — they *travel* into it and shrink
    /// to its size, which is the property D20 calls out as load-bearing.
    #[must_use]
    pub fn absorb(&self, slot_rect: Rect, m: &Motion) -> Rect {
        let t = self.collapse_progress(m);
        if t <= 0.0 {
            return slot_rect;
        }
        lerp_rect(slot_rect, self.rect, t)
    }

    /// The translation a card undergoes while being absorbed.
    ///
    /// Exposed separately from the size change for painters that keep a shadow
    /// attached to the card.
    #[must_use]
    pub fn absorb_offset(&self, slot_rect: Rect, m: &Motion) -> Vec2 {
        self.absorb(slot_rect, m).min - slot_rect.min
    }
}

/// A timeline rewound so that it is already `done` of the way through.
fn back_dated(m: &Motion, done: f32, token: Duration, ease: Ease) -> Timeline {
    let span = m.resolve(token);
    let start = m.now() - f64::from(span * done.clamp(0.0, 1.0));
    Timeline::starting_at(start, token, ease)
}

/// Linear interpolation between two rectangles, corner by corner.
#[must_use]
pub fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    let min = a.min + (b.min - a.min) * t;
    let max = a.max + (b.max - a.max) * t;
    Rect::from_min_max(pos2(min.x, min.y), pos2(max.x, max.y))
}

/// Derives the dock's rectangle from slot 0's rectangle.
///
/// Same width, ~1/6 the height, sharing slot 0's bottom edge — so the dock
/// occupies the footprint the oldest capture would have stood on, which is what
/// makes the collapse read as the pile settling into it rather than vanishing
/// behind an unrelated widget.
#[must_use]
pub fn rect_for_slot0(slot0: Rect) -> Rect {
    let h = (slot0.height() * HEIGHT_RATIO)
        .max(MIN_HEIGHT)
        .min(slot0.height());
    Rect::from_min_size(
        pos2(slot0.left(), slot0.bottom() - h),
        vec2(slot0.width(), h),
    )
}
