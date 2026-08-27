//! The **capture stack** — the app's primary surface (D12, D21, D28).
//!
//! A pile of thumbnail cards anchored to the **bottom-left of the work area**,
//! growing **upward**. Cards enter and leave **only from the left**, and — the
//! invariant the whole design turns on — **a card never moves upward**.
//!
//! ```text
//!    slot 5  <- 6th capture
//!    slot 4
//!    slot 3
//!    slot 2
//!    slot 1  <- 2nd capture
//!    slot 0  <- 1st capture, and the first to leave
//!    ------- bottom-left of the WORK AREA
//! ```
//!
//! 1. The first capture appears at slot 0, sliding in from the left.
//! 2. Each next capture slides into the next slot up. **Existing cards do not
//!    move at all** while the pile is growing.
//! 3. When full, one coordinated motion: the oldest slides out left, every
//!    remaining card falls down one slot, the new card arrives at the top.
//! 4. Dismissing any card drops the cards above it by one slot. Cards below it
//!    never move.
//!
//! # Why the invariant is structural, not remembered
//!
//! A card's slot **is** its index in [`CaptureStack::cards`]. Arrival is a
//! `push`; departure is a `remove`, which shifts exactly the cards above it down
//! by one and cannot touch the cards below. There is no code path that assigns a
//! card a higher slot than it was born into, so the invariant is a property of
//! the data structure rather than something the animation layer has to keep
//! getting right. [`CaptureStack::check_no_card_moved_up`] asserts it anyway.
//!
//! The UI spike stored newest-first and drew index 0 at the bottom, so every
//! arrival shoved the existing pile upward — the exact failure D28 was written
//! to prevent. Its physics and gesture handling were sound and are carried over;
//! its layout is not.
//!
//! # Virtual clock (D25)
//!
//! Every entry point takes an explicit [`Motion`] instant. Nothing here reads
//! `Instant::now()` or any global. Scripted motion is *closed-form keyframed*:
//! position is a pure function of elapsed time, so a harness can ask for "card
//! entry at t = 180 ms" and get the exact frame without stepping through the
//! frames before it.
//!
//! [`CaptureStack::frame`] is pure and idempotent. [`CaptureStack::advance`] is
//! a separate garbage-collection step that promotes finished animations and
//! reaps departed cards; `frame` is correct whether or not it has been called.
//!
//! # What is stubbed
//!
//! Painting, and the OS promised-file hand-off behind [`Intent::DragOut`]. The
//! stack takes the drag-out to the point of launching the shared exit animation
//! and reports the release; performing the platform drag is the shell's job.

#[path = "dock.rs"]
pub mod dock;

use crate::motion::{Activity, Duration, Ease, Motion, Timeline};
use dock::Dock;
use egui::{Pos2, Rect, Vec2, pos2, vec2};

/// Milliseconds — the harness's unit for naming an instant (D25).
///
/// Convenience only: build the clock with [`Motion::at_ms`] and pass that.
pub type Millis = u64;

// ---------------------------------------------------------------------------
// Slot geometry
// ---------------------------------------------------------------------------

/// The most slots the stack will ever offer.
///
/// Six is the target on a 16-inch MacBook Pro. Beyond that the pile stops
/// reading as "recent captures" and starts reading as a file manager, which is
/// not what this surface is for.
pub const MAX_SLOTS: usize = 6;

/// The fewest slots a usable stack can have.
pub const MIN_SLOTS: usize = 1;

/// The dimensions a card and its surroundings are laid out from.
///
/// These are parameters rather than constants so the design system can feed them
/// in once its card tokens land, and so tests can exercise slot derivation
/// without pretending to be a particular display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardMetrics {
    /// Card width in points.
    pub width: f32,
    /// Card height in points.
    pub height: f32,
    /// Vertical gap between adjacent cards.
    pub gap: f32,
    /// Inset from the top and bottom work-area edges.
    pub margin: f32,
    /// Inset from the work area's left edge.
    pub left_margin: f32,
    /// Extra travel past the screen edge before a departing card is considered
    /// gone. Without it a card with a shadow leaves a smudge at the edge.
    pub clearance: f32,
}

impl Default for CardMetrics {
    fn default() -> Self {
        Self {
            width: 210.0,
            height: 150.0,
            gap: 8.0,
            margin: 8.0,
            left_margin: 40.0,
            clearance: 24.0,
        }
    }
}

impl CardMetrics {
    /// Distance between the tops of adjacent slots.
    #[must_use]
    pub fn pitch(&self) -> f32 {
        self.height + self.gap
    }

    /// How many cards fit in `usable_height` points of vertical room.
    ///
    /// `n` cards occupy `n * height + (n - 1) * gap`, so the count is
    /// `floor((usable + gap) / pitch)`.
    #[must_use]
    pub fn slots_for_height(&self, usable_height: f32) -> usize {
        let pitch = self.pitch();
        if pitch <= 0.0 || !usable_height.is_finite() {
            return MIN_SLOTS;
        }
        let fits = ((usable_height + self.gap) / pitch).floor();
        if fits < MIN_SLOTS as f32 {
            MIN_SLOTS
        } else if fits > MAX_SLOTS as f32 {
            MAX_SLOTS
        } else {
            fits as usize
        }
    }
}

/// Where the slots are, and how many there are.
///
/// Anchored to the **work area**, not the raw screen: on macOS the raw bottom
/// edge is underneath the Dock, and slot 0 would be half-hidden by it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StackLayout {
    work_area: Rect,
    metrics: CardMetrics,
    slots: usize,
}

impl StackLayout {
    /// Derives a layout — and a slot count — from a work area.
    #[must_use]
    pub fn new(work_area: Rect, metrics: CardMetrics) -> Self {
        let usable = work_area.height() - 2.0 * metrics.margin;
        let slots = metrics.slots_for_height(usable);
        Self {
            work_area,
            metrics,
            slots,
        }
    }

    /// The work area this layout was derived from.
    #[must_use]
    pub fn work_area(&self) -> Rect {
        self.work_area
    }

    /// The card metrics in force.
    #[must_use]
    pub fn metrics(&self) -> CardMetrics {
        self.metrics
    }

    /// How many cards the pile holds before it starts retiring the oldest.
    #[must_use]
    pub fn slots(&self) -> usize {
        self.slots
    }

    /// Where a card resting in `slot` sits.
    ///
    /// Slot 0 is at the bottom. A higher slot index is *higher on screen*, which
    /// in egui's downward-positive coordinates means a **smaller** `y`.
    #[must_use]
    pub fn slot_rect(&self, slot: usize) -> Rect {
        self.slot_rect_f(slot as f32)
    }

    /// [`StackLayout::slot_rect`] at a fractional slot, for a card mid-fall.
    #[must_use]
    pub fn slot_rect_f(&self, slot: f32) -> Rect {
        let m = self.metrics;
        let top = self.work_area.bottom() - m.margin - m.height - slot * m.pitch();
        Rect::from_min_size(
            pos2(self.work_area.left() + m.left_margin, top),
            vec2(m.width, m.height),
        )
    }

    /// Where a card sits before it has slid in — fully off the left edge.
    #[must_use]
    pub fn entry_rect(&self, slot: usize) -> Rect {
        self.entry_rect_for(self.slot_rect(slot))
    }

    /// Where an arbitrary card rectangle sits before horizontal entry.
    #[must_use]
    pub fn entry_rect_for(&self, rest: Rect) -> Rect {
        let dx = self.work_area.left() - self.metrics.clearance - rest.right();
        rest.translate(vec2(dx, 0.0))
    }

    /// The dock's rectangle for this layout.
    #[must_use]
    pub fn dock_rect(&self) -> Rect {
        dock::rect_for_slot0(self.slot_rect(0))
    }

    /// How far a card at `rect` must travel in `dir` to be off-screen.
    ///
    /// The **minimum** over the axes `dir` contributes to: a card is gone the
    /// moment it separates on either axis, and taking the max would leave a
    /// diagonal throw crawling long after it stopped being visible.
    #[must_use]
    pub fn escape_distance(&self, rect: Rect, dir: Dir) -> f32 {
        let w = self.work_area;
        let c = self.metrics.clearance;
        let d = match dir {
            Dir::Left => rect.right() - w.left(),
            Dir::Right => w.right() - rect.left(),
            Dir::Up => rect.bottom() - w.top(),
            Dir::Down => w.bottom() - rect.top(),
        };
        d.max(0.0) + c
    }
}

// ---------------------------------------------------------------------------
// Gestures — direction is intent (D21)
// ---------------------------------------------------------------------------

/// A cardinal direction, in screen terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    /// Toward the left edge. The way cards arrive, and the way they leave.
    Left,
    /// Toward the right edge.
    Right,
    /// Toward the top edge.
    Up,
    /// Toward the bottom edge.
    Down,
}

impl Dir {
    /// Every direction, in the order ties are broken.
    pub const ALL: [Self; 4] = [Self::Left, Self::Right, Self::Up, Self::Down];

    /// A unit vector in egui coordinates, where `y` grows downward.
    #[must_use]
    pub fn unit(self) -> Vec2 {
        match self {
            Self::Left => vec2(-1.0, 0.0),
            Self::Right => vec2(1.0, 0.0),
            Self::Up => vec2(0.0, -1.0),
            Self::Down => vec2(0.0, 1.0),
        }
    }

    /// How far `v` points this way. Never negative.
    #[must_use]
    pub fn component(self, v: Vec2) -> f32 {
        match self {
            Self::Left => (-v.x).max(0.0),
            Self::Right => v.x.max(0.0),
            Self::Up => (-v.y).max(0.0),
            Self::Down => v.y.max(0.0),
        }
    }

    /// What a release in this direction means (D21).
    #[must_use]
    pub fn intent(self) -> Intent {
        match self {
            Self::Left => Intent::Dismiss,
            Self::Right | Self::Up => Intent::DragOut,
            Self::Down => Intent::Collapse,
        }
    }
}

/// What the user meant by a gesture.
///
/// Direction *is* intent — the same drag means four different things depending
/// on where it goes, which is what lets the card carry no controls at rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intent {
    /// Swipe left: retire this capture. It leaves the way it arrived.
    Dismiss,
    /// Swipe right or up: begin a drag onto another application.
    ///
    /// The hero action (D12) — drag a capture straight into Slack, Figma, an
    /// email. Two directions map here because the target app is as likely to be
    /// beside the pile as above it, and guessing wrong should not destroy a
    /// capture.
    DragOut,
    /// Swipe down: collapse the whole pile into the dock (D20). Non-destructive.
    Collapse,
    /// Nothing committed. The card returns to its slot.
    SpringBack,
}

/// Distance and speed thresholds per direction.
///
/// Each direction carries its own numbers because the directions differ in what
/// they cost. Collapse is free to undo, so it commits sooner; dismiss and
/// drag-out both remove a capture from the pile and ask for more conviction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GestureConfig {
    /// Travel that commits a leftward dismiss, in points.
    pub dismiss_dist: f32,
    /// Release speed that commits a leftward dismiss, in points per second.
    pub dismiss_vel: f32,
    /// Travel that commits a drag-out.
    pub dragout_dist: f32,
    /// Release speed that commits a drag-out.
    pub dragout_vel: f32,
    /// Travel that commits a collapse.
    pub collapse_dist: f32,
    /// Release speed that commits a collapse.
    pub collapse_vel: f32,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            dismiss_dist: 110.0,
            dismiss_vel: 420.0,
            dragout_dist: 110.0,
            dragout_vel: 420.0,
            collapse_dist: 88.0,
            collapse_vel: 380.0,
        }
    }
}

impl GestureConfig {
    /// The thresholds guarding `dir`.
    #[must_use]
    pub fn thresholds(&self, dir: Dir) -> (f32, f32) {
        match dir.intent() {
            Intent::Dismiss => (self.dismiss_dist, self.dismiss_vel),
            Intent::DragOut => (self.dragout_dist, self.dragout_vel),
            Intent::Collapse => (self.collapse_dist, self.collapse_vel),
            Intent::SpringBack => (f32::INFINITY, f32::INFINITY),
        }
    }

    /// How committed a gesture is toward `dir`. `>= 1.0` means committed.
    ///
    /// Distance and speed are alternatives, not conjuncts: a long slow drag and
    /// a short sharp flick are both perfectly clear statements of intent.
    #[must_use]
    pub fn score(&self, dir: Dir, travel: Vec2, velocity: Vec2) -> f32 {
        let (dist, vel) = self.thresholds(dir);
        let d = if dist > 0.0 {
            dir.component(travel) / dist
        } else {
            0.0
        };
        let v = if vel > 0.0 {
            dir.component(velocity) / vel
        } else {
            0.0
        };
        d.max(v)
    }
}

/// Reads a gesture's intent from its travel and release speed.
///
/// All four directions are scored independently and the strongest committed one
/// wins, rather than picking a dominant axis first and then testing it. That is
/// what lets each direction carry its own thresholds. Ties break in
/// [`Dir::ALL`] order, so the classification is deterministic — a property the
/// tests rely on.
#[must_use]
pub fn classify(travel: Vec2, velocity: Vec2, cfg: &GestureConfig) -> (Intent, Option<Dir>) {
    let mut best: Option<(Dir, f32)> = None;
    for dir in Dir::ALL {
        let score = cfg.score(dir, travel, velocity);
        if score >= 1.0 && best.is_none_or(|(_, b)| score > b) {
            best = Some((dir, score));
        }
    }
    match best {
        Some((dir, _)) => (dir.intent(), Some(dir)),
        None => (Intent::SpringBack, None),
    }
}

/// The window a release speed is measured over, in seconds.
///
/// Ported from the spike, which found egui's own smoothed pointer velocity
/// unusable here: it keeps reporting motion after the pointer has stopped, so
/// "drag slowly, stop, release" became an unwanted throw. Differencing over a
/// short window makes a dead stop read as exactly zero.
pub const VELOCITY_WINDOW: f32 = 0.080;

/// Release speed, measured by differencing over [`VELOCITY_WINDOW`].
#[derive(Debug, Clone, Default)]
pub struct DragVelocity {
    samples: Vec<(f64, Pos2)>,
}

impl DragVelocity {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets every sample.
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Records where the pointer is at this instant.
    pub fn push(&mut self, pos: Pos2, m: &Motion) {
        let now = m.now();
        self.samples.push((now, pos));
        let cutoff = now - f64::from(VELOCITY_WINDOW) * 2.0;
        self.samples.retain(|(t, _)| *t >= cutoff);
    }

    /// Points per second over the window, or zero if the pointer has stopped.
    #[must_use]
    pub fn velocity(&self, m: &Motion) -> Vec2 {
        let now = m.now();
        let cutoff = now - f64::from(VELOCITY_WINDOW);
        let Some(&(_, last)) = self.samples.last() else {
            return Vec2::ZERO;
        };
        let oldest = self
            .samples
            .iter()
            .find(|(t, _)| *t >= cutoff)
            .copied()
            .or_else(|| self.samples.first().copied());
        let Some((t0, p0)) = oldest else {
            return Vec2::ZERO;
        };
        let dt = (now - t0) as f32;
        if dt <= 1e-4 {
            return Vec2::ZERO;
        }
        (last - p0) / dt
    }
}

// ---------------------------------------------------------------------------
// Motion tokens
// ---------------------------------------------------------------------------

/// The duration and easing tokens this surface animates with (D19).
///
/// Cards animate; **controls do not**. There is deliberately no token here for a
/// button hover or press — those flip instantly, which is what makes them feel
/// responsive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timing {
    /// A card sliding in from the left.
    pub enter: Duration,
    /// Easing for the entry. Overshoot is what makes the arrival read as an
    /// object with mass rather than a fade.
    pub enter_ease: Ease,
    /// The shared departure animation (D21), at its longest.
    pub exit: Duration,
    /// The floor a hard flick compresses the exit to.
    pub exit_min: Duration,
    /// Easing for the departure.
    ///
    /// Linear: a card leaving the scene is not arriving anywhere, and a
    /// decelerating curve makes it look like it is braking at the screen edge.
    pub exit_ease: Ease,
    /// A card falling one slot after the card below it left.
    pub fall: Duration,
    /// Easing for the fall.
    ///
    /// **Must not overshoot.** An overshoot settles by travelling back the way
    /// it came, and for a fall that means the card moves *upward* — which D28
    /// forbids outright. `Ease::Spring` is critically damped: the fastest
    /// approach with no bounce. This is the one place in the surface where the
    /// invariant rules out the curve that would otherwise feel best.
    pub fall_ease: Ease,
    /// A card returning to its slot after an uncommitted gesture.
    pub spring_back: Duration,
    /// Easing for the return.
    pub spring_back_ease: Ease,
    /// Hover chrome appearing or retracting.
    pub reveal: Duration,
    /// Easing for the reveal.
    pub reveal_ease: Ease,
    /// The pile collapsing into the dock.
    pub collapse: Duration,
    /// The pile coming back out.
    pub expand: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            enter: Duration::SLOW,
            enter_ease: Ease::SpringOvershoot,
            exit: Duration::BASE,
            exit_min: Duration::from_millis(90),
            exit_ease: Ease::Linear,
            fall: Duration::BASE,
            fall_ease: Ease::Spring,
            spring_back: Duration::BASE,
            spring_back_ease: Ease::SpringOvershoot,
            reveal: Duration::FAST,
            reveal_ease: Ease::OutCubic,
            collapse: Duration::SLOW,
            expand: Duration::from_millis(280),
        }
    }
}

impl Timing {
    /// Every duration collapsed to zero (D13/D19).
    ///
    /// Rarely needed — [`Motion::with_reduce_motion`] is the real choke point,
    /// and it applies to tokens that were already in flight. This exists for
    /// tests that want a stack with no motion at all regardless of clock.
    #[must_use]
    pub fn reduced() -> Self {
        Self {
            enter: Duration::ZERO,
            exit: Duration::ZERO,
            exit_min: Duration::ZERO,
            fall: Duration::ZERO,
            spring_back: Duration::ZERO,
            reveal: Duration::ZERO,
            collapse: Duration::ZERO,
            expand: Duration::ZERO,
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Cards
// ---------------------------------------------------------------------------

/// A stable handle to a capture in the pile.
///
/// Monotonic and never reused, so a handle held across an eviction cannot
/// silently start referring to a different capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CardId(pub u64);

/// What a card is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CardState {
    /// Sliding in from the left.
    Entering,
    /// Sitting in its slot.
    Resting,
    /// Falling to a lower slot after something below it left.
    Falling,
    /// Following the pointer.
    Dragging,
    /// Springing back after an uncommitted gesture.
    Returning,
    /// On its way off-screen.
    Departing,
}

/// A capture resident in the pile.
///
/// The animation slots are **orthogonal and per-axis**, not one `Phase` enum.
/// [`Card::entry`] is horizontal-only and [`Card::fall`] is vertical-and-down —
/// which is half of why a card cannot move up: there is no animation that
/// carries one that way.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    id: CardId,
    born_slot: usize,
    entry: Option<Timeline>,
    fall: Option<(Timeline, Vec2)>,
    ret: Option<(Timeline, Vec2)>,
    drag: Option<Vec2>,
    hover_on: bool,
    hover_since: f64,
    lifted: bool,
}

impl Card {
    /// The card's handle.
    #[must_use]
    pub fn id(&self) -> CardId {
        self.id
    }

    /// The slot this card was born into — its high-water mark, since slots only
    /// ever decrease.
    #[must_use]
    pub fn born_slot(&self) -> usize {
        self.born_slot
    }

    /// Whether the pointer is over this card.
    #[must_use]
    pub fn is_hovered(&self) -> bool {
        self.hover_on
    }

    /// Whether the card is being held.
    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }
}

/// A capture on its way off-screen.
///
/// Departing cards live in their own list so that resident indices stay exactly
/// equal to slot numbers — the moment a departure occupied a slot, the invariant
/// would stop being structural.
#[derive(Debug, Clone, PartialEq)]
pub struct Departing {
    id: CardId,
    born_slot: usize,
    from_slot: usize,
    from: Rect,
    to: Rect,
    timeline: Timeline,
    dir: Dir,
    intent: Intent,
}

impl Departing {
    /// The departing card's handle.
    #[must_use]
    pub fn id(&self) -> CardId {
        self.id
    }

    /// The slot it left from.
    #[must_use]
    pub fn from_slot(&self) -> usize {
        self.from_slot
    }

    /// Why it is leaving.
    #[must_use]
    pub fn intent(&self) -> Intent {
        self.intent
    }

    /// Which way it went.
    #[must_use]
    pub fn direction(&self) -> Dir {
        self.dir
    }
}

/// Everything a painter needs about one card at one instant.
///
/// A pure function of the stack's state and a [`Motion`] instant — no interior
/// mutation, no frame ordering, no accumulated error.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardFrame {
    /// Which capture this is.
    pub id: CardId,
    /// The slot it belongs to. For a departing card, the slot it left from.
    pub slot: usize,
    /// Where to draw it.
    pub rect: Rect,
    /// Opacity, `0.0..=1.0`.
    pub alpha: f32,
    /// How far the hover chrome is revealed, `0.0..=1.0`.
    ///
    /// Copy and Save are the primary actions (D12); at rest all chrome is
    /// hidden.
    pub reveal: f32,
    /// `1.0` while held, `0.0` otherwise — **never interpolated**.
    ///
    /// D19: the instant state change is the point. Animating this would make the
    /// card feel laggy under the pointer.
    pub lift: f32,
    /// Lean, in radians, from lateral drag. Zero at rest.
    pub angle: f32,
    /// What the card is doing.
    pub state: CardState,
}

/// The outcome of releasing a drag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DragRelease {
    /// The card that was held.
    pub id: CardId,
    /// What the release meant.
    pub intent: Intent,
    /// Which way it went, if it committed.
    pub direction: Option<Dir>,
    /// Where the card was when it was let go.
    pub rect: Rect,
    /// Release speed, in points per second.
    pub velocity: Vec2,
}

/// The card currently under the pointer.
#[derive(Debug, Clone)]
struct ActiveDrag {
    id: CardId,
    origin: Pos2,
    latest: Pos2,
    velocity: DragVelocity,
}

// ---------------------------------------------------------------------------
// The stack
// ---------------------------------------------------------------------------

/// A card was assigned a higher slot than it had before.
///
/// The one thing this surface is not allowed to do (D28).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MovedUp {
    /// The offending card.
    pub id: CardId,
    /// The slot it was born into.
    pub born_slot: usize,
    /// The slot it is in now.
    pub slot: usize,
}

impl std::fmt::Display for MovedUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "card {:?} moved up: born at slot {}, now at slot {}",
            self.id, self.born_slot, self.slot
        )
    }
}

impl std::error::Error for MovedUp {}

/// The pile of captures.
///
/// Residents are held newest-**last**: `cards[0]` is the bottom of the pile, the
/// oldest capture, and the first to leave.
#[derive(Debug, Clone)]
pub struct CaptureStack {
    layout: StackLayout,
    timing: Timing,
    gestures: GestureConfig,
    cards: Vec<Card>,
    departing: Vec<Departing>,
    dock: Dock,
    drag: Option<ActiveDrag>,
    next_id: u64,
}

impl CaptureStack {
    /// An empty stack laid out against `layout`.
    #[must_use]
    pub fn new(layout: StackLayout, timing: Timing) -> Self {
        let dock = Dock::new(layout.dock_rect(), timing.collapse, timing.expand);
        Self {
            layout,
            timing,
            gestures: GestureConfig::default(),
            cards: Vec::new(),
            departing: Vec::new(),
            dock,
            drag: None,
            next_id: 0,
        }
    }

    /// An empty stack with default metrics and timing for `work_area`.
    #[must_use]
    pub fn for_work_area(work_area: Rect) -> Self {
        Self::new(
            StackLayout::new(work_area, CardMetrics::default()),
            Timing::default(),
        )
    }

    /// The slot geometry.
    #[must_use]
    pub fn layout(&self) -> &StackLayout {
        &self.layout
    }

    /// The motion tokens.
    #[must_use]
    pub fn timing(&self) -> &Timing {
        &self.timing
    }

    /// The gesture thresholds.
    #[must_use]
    pub fn gestures(&self) -> &GestureConfig {
        &self.gestures
    }

    /// Replaces the gesture thresholds.
    pub fn set_gestures(&mut self, cfg: GestureConfig) {
        self.gestures = cfg;
    }

    /// The dock.
    #[must_use]
    pub fn dock(&self) -> &Dock {
        &self.dock
    }

    /// How many captures are in the pile.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cards.len()
    }

    /// Whether the pile is empty. Departing cards do not count — they are gone
    /// as far as the pile is concerned, they just have not finished leaving.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// How many captures fit before the oldest starts retiring.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.layout.slots()
    }

    /// The residents, bottom slot first.
    #[must_use]
    pub fn cards(&self) -> &[Card] {
        &self.cards
    }

    /// The cards currently leaving.
    #[must_use]
    pub fn departing(&self) -> &[Departing] {
        &self.departing
    }

    /// The slot a card is in, if it is still resident.
    #[must_use]
    pub fn slot_of(&self, id: CardId) -> Option<usize> {
        self.cards.iter().position(|c| c.id == id)
    }

    /// Every resident's slot, for a test to diff across an operation.
    #[must_use]
    pub fn slot_snapshot(&self) -> Vec<(CardId, usize)> {
        self.cards
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id, i))
            .collect()
    }

    // -- capture ------------------------------------------------------------

    /// Adds a capture at the top of the pile.
    ///
    /// If the pile is full this is one coordinated motion: the oldest card at
    /// slot 0 leaves to the **left** — the same way it arrived, and with the same
    /// animation a manual dismiss uses (D21) — every remaining card falls one
    /// slot, and the new card slides into the top slot. Otherwise **nothing
    /// already on screen moves at all**.
    ///
    /// A capture taken while the pile is stowed brings the dock back up: the
    /// user asked for the overlay to be out of the way, not to stop seeing what
    /// they captured.
    pub fn push(&mut self, m: &Motion) -> CardId {
        if self.dock.is_stowing() {
            self.dock.expand(m);
        }
        if self.cards.len() >= self.capacity() {
            self.retire_slot(0, Intent::Dismiss, Dir::Left, Vec2::ZERO, m);
        }
        let slot = self.cards.len();
        let id = CardId(self.next_id);
        self.next_id += 1;
        self.cards.push(Card {
            id,
            born_slot: slot,
            entry: Some(Timeline::starting(
                m,
                self.timing.enter,
                self.timing.enter_ease,
            )),
            fall: None,
            ret: None,
            drag: None,
            hover_on: false,
            hover_since: m.now(),
            lifted: false,
        });
        id
    }

    /// Retires a capture. Cards **above** it fall one slot; cards below it do
    /// not move.
    ///
    /// Returns whether the card was there to dismiss.
    pub fn dismiss(&mut self, id: CardId, m: &Motion) -> bool {
        let Some(slot) = self.slot_of(id) else {
            return false;
        };
        self.retire_slot(slot, Intent::Dismiss, Dir::Left, Vec2::ZERO, m);
        true
    }

    /// Retires the oldest capture, from the bottom, leftward.
    pub fn dismiss_oldest(&mut self, m: &Motion) -> bool {
        if self.cards.is_empty() {
            return false;
        }
        self.retire_slot(0, Intent::Dismiss, Dir::Left, Vec2::ZERO, m);
        true
    }

    /// Clears the pile — what Escape does.
    ///
    /// D27 requires this surface be escapable on the first guess. Retiring from
    /// the top down means no card is ever asked to fall while the pile empties.
    pub fn dismiss_all(&mut self, m: &Motion) {
        self.drag = None;
        while let Some(top) = self.cards.len().checked_sub(1) {
            self.retire_slot(top, Intent::Dismiss, Dir::Left, Vec2::ZERO, m);
        }
    }

    /// Moves a card from the pile to the departing list, and drops everything
    /// above it by one slot.
    ///
    /// `Vec::remove` is the whole invariant: it shifts exactly the entries above
    /// `slot` down by one and cannot touch the ones below.
    fn retire_slot(&mut self, slot: usize, intent: Intent, dir: Dir, velocity: Vec2, m: &Motion) {
        if slot >= self.cards.len() {
            return;
        }
        let from = self.rect_of_resident(slot, m);

        // Where each card above is *right now*, captured before the shift —
        // otherwise an interrupted fall restarts from the slot it is about to
        // land in and the card jumps.
        let previous: Vec<Rect> = (slot + 1..self.cards.len())
            .map(|i| self.rect_of_resident(i, m))
            .collect();

        let card = self.cards.remove(slot);

        if self.drag.as_ref().is_some_and(|d| d.id == card.id) {
            self.drag = None;
        }

        let escape = self.layout.escape_distance(from, dir);
        let to = from.translate(dir.unit() * escape);

        // A hard flick leaves faster than a shove, but never slower than the
        // scripted exit and never so fast it reads as a glitch.
        let speed = dir.component(velocity);
        let token = if speed > 1.0 {
            let secs = (escape / speed).clamp(
                self.timing.exit_min.designed_secs(),
                self.timing.exit.designed_secs(),
            );
            Duration::from_secs(secs)
        } else {
            self.timing.exit
        };

        self.departing.push(Departing {
            id: card.id,
            born_slot: card.born_slot,
            from_slot: slot,
            from,
            to,
            timeline: Timeline::starting(m, token, self.timing.exit_ease),
            dir,
            intent,
        });

        // Everything that was above the departing card now falls one slot.
        let fall = Timeline::starting(m, self.timing.fall, self.timing.fall_ease);
        for (index, was_at) in previous.into_iter().enumerate() {
            let target_slot = slot + index;
            self.cards[target_slot].fall = None;
            let target = self.rect_of_resident(target_slot, m);
            self.cards[target_slot].fall = Some((fall, was_at.min - target.min));
        }
    }

    // -- work area ----------------------------------------------------------

    /// Re-derives the layout for a new work area.
    ///
    /// Growing moves nothing — a free consequence of anchoring to the bottom.
    /// Shrinking retires from the bottom, oldest first, exactly as overflow
    /// does, so the invariant survives a display change.
    pub fn resize(&mut self, work_area: Rect, m: &Motion) {
        self.layout = StackLayout::new(work_area, self.layout.metrics);
        self.dock.set_rect(self.layout.dock_rect());
        while self.cards.len() > self.capacity() {
            self.retire_slot(0, Intent::Dismiss, Dir::Left, Vec2::ZERO, m);
        }
    }

    // -- pointer ------------------------------------------------------------

    /// Sets which card the pointer is over, revealing its chrome.
    ///
    /// Reversal is continuous: a reveal interrupted halfway retracts from where
    /// it is, not from fully shown.
    pub fn set_hover(&mut self, id: Option<CardId>, m: &Motion) {
        let now = m.now();
        let span = m.resolve(self.timing.reveal);
        for card in &mut self.cards {
            let want = Some(card.id) == id;
            if want == card.hover_on {
                continue;
            }
            let shown = reveal_progress(card, m, self.timing.reveal);
            let done = if want { shown } else { 1.0 - shown };
            card.hover_on = want;
            card.hover_since = now - f64::from(span * done.clamp(0.0, 1.0));
        }
    }

    /// Begins holding a card at `pointer`.
    pub fn begin_drag(&mut self, id: CardId, pointer: Pos2, m: &Motion) -> bool {
        let Some(slot) = self.slot_of(id) else {
            return false;
        };
        let mut velocity = DragVelocity::new();
        velocity.push(pointer, m);
        self.drag = Some(ActiveDrag {
            id,
            origin: pointer,
            latest: pointer,
            velocity,
        });
        let card = &mut self.cards[slot];
        card.drag = Some(Vec2::ZERO);
        card.ret = None;
        card.lifted = true;
        true
    }

    /// Moves the held card with the pointer, 1:1.
    ///
    /// No smoothing, no spring: inertia belongs to the release, not to the hold.
    /// A card that lags the finger feels broken.
    pub fn drag_to(&mut self, pointer: Pos2, m: &Motion) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        drag.latest = pointer;
        drag.velocity.push(pointer, m);
        let offset = pointer - drag.origin;
        let id = drag.id;
        if let Some(card) = self.cards.iter_mut().find(|c| c.id == id) {
            card.drag = Some(offset);
        }
    }

    /// Lets go, and acts on what the gesture meant.
    ///
    /// Returns `None` if nothing was held.
    pub fn release_drag(&mut self, m: &Motion) -> Option<DragRelease> {
        let drag = self.drag.take()?;
        let travel = drag.latest - drag.origin;
        let velocity = drag.velocity.velocity(m);
        let (intent, direction) = classify(travel, velocity, &self.gestures);
        let slot = self.slot_of(drag.id)?;
        let rect = self.rect_of_resident(slot, m);

        match intent {
            // Both remove the card from the pile, with the one shared exit
            // animation (D21). What differs is what the shell does next: a
            // dismiss is the end of the story, a drag-out hands the capture to
            // the OS.
            Intent::Dismiss | Intent::DragOut => {
                let dir = direction.unwrap_or(Dir::Left);
                self.retire_slot(slot, intent, dir, velocity, m);
            }
            Intent::Collapse => {
                self.spring_back(slot, m);
                self.dock.collapse(m);
            }
            Intent::SpringBack => self.spring_back(slot, m),
        }

        Some(DragRelease {
            id: drag.id,
            intent,
            direction,
            rect,
            velocity,
        })
    }

    /// Abandons a drag without acting on it — a cancelled gesture, a lost
    /// pointer capture.
    pub fn cancel_drag(&mut self, m: &Motion) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if let Some(slot) = self.slot_of(drag.id) {
            self.spring_back(slot, m);
        }
    }

    /// Sends a card back to its slot from wherever it is.
    fn spring_back(&mut self, slot: usize, m: &Motion) {
        let Some(card) = self.cards.get_mut(slot) else {
            return;
        };
        let offset = card.drag.take().unwrap_or(Vec2::ZERO);
        card.lifted = false;
        card.ret = if offset == Vec2::ZERO {
            None
        } else {
            Some((
                Timeline::starting(m, self.timing.spring_back, self.timing.spring_back_ease),
                offset,
            ))
        };
    }

    // -- dock ---------------------------------------------------------------

    /// Stows the pile into the dock (D20). Nothing is dismissed.
    pub fn collapse(&mut self, m: &Motion) {
        self.dock.collapse(m);
    }

    /// Brings the pile back out of the dock.
    pub fn expand(&mut self, m: &Motion) {
        self.dock.expand(m);
    }

    /// What the chevron does.
    pub fn toggle_dock(&mut self, m: &Motion) {
        self.dock.toggle(m);
    }

    // -- frames -------------------------------------------------------------

    /// Promotes finished animations and reaps departed cards.
    ///
    /// Separate from [`CaptureStack::frame`] so that rendering stays pure. A
    /// harness that only ever renders never has to call this; a live app calls
    /// it once a frame.
    pub fn advance(&mut self, m: &Motion) {
        self.dock.advance(m);
        for card in &mut self.cards {
            if card.entry.is_some_and(|t| !t.is_active(m)) {
                card.entry = None;
            }
            if card.fall.is_some_and(|(t, _)| !t.is_active(m)) {
                card.fall = None;
            }
            if card.ret.is_some_and(|(t, _)| !t.is_active(m)) {
                card.ret = None;
            }
        }
        self.departing.retain(|d| d.timeline.is_active(m));
    }

    /// What the stack contributes to the frame schedule.
    ///
    /// egui repaints only on input (D19), so a caller that ignores this will
    /// watch every animation freeze halfway.
    #[must_use]
    pub fn activity(&self, m: &Motion) -> Activity {
        let mut a = self.dock.activity(m);
        if self.drag.is_some() {
            a |= Activity::animating();
        }
        for card in &self.cards {
            a |= Activity::when_animating(
                card.entry.is_some_and(|t| t.is_active(m))
                    || card.fall.is_some_and(|(t, _)| t.is_active(m))
                    || card.ret.is_some_and(|(t, _)| t.is_active(m))
                    || m.progress(card.hover_since, self.timing.reveal) < 1.0,
            );
        }
        for d in &self.departing {
            a |= d.timeline.activity(m);
        }
        a
    }

    /// Whether anything is moving.
    #[must_use]
    pub fn is_animating(&self, m: &Motion) -> bool {
        self.activity(m).is_animating()
    }

    /// Every card to draw at this instant, bottom slot first, departing cards
    /// last.
    ///
    /// Pure: calling this twice with the same instant yields identical results,
    /// and jumping straight to an instant matches having stepped to it.
    #[must_use]
    pub fn frame(&self, m: &Motion) -> Vec<CardFrame> {
        let mut out: Vec<CardFrame> = (0..self.cards.len())
            .map(|slot| self.resident_frame(slot, m))
            .collect();
        out.extend(self.departing.iter().map(|d| self.departing_frame(d, m)));
        out
    }

    /// One card's frame, if it is still resident.
    #[must_use]
    pub fn frame_of(&self, id: CardId, m: &Motion) -> Option<CardFrame> {
        self.slot_of(id).map(|slot| self.resident_frame(slot, m))
    }

    /// Where a resident card is, chrome aside.
    fn rect_of_resident(&self, slot: usize, m: &Motion) -> Rect {
        let card = &self.cards[slot];
        let mut rect = self.home_rect(slot);

        // Horizontal entry. Vertical position is untouched by design: entry
        // never carries a card upward.
        if let Some(t) = card.entry {
            let v = t.value(m);
            let start = self.layout.entry_rect_for(rect).left();
            let target = rect.left();
            let x = start + (target - start) * v;
            rect = rect.translate(vec2(x - rect.left(), 0.0));
        }
        if let Some((t, offset)) = card.ret {
            rect = rect.translate(offset * (1.0 - t.value(m)));
        }
        if let Some(offset) = card.drag {
            rect = rect.translate(offset);
        }
        rect = self.dock.absorb(rect, m);
        card.fall.map_or(rect, |(timeline, displacement)| {
            rect.translate(displacement * (1.0 - timeline.value(m).clamp(0.0, 1.0)))
        })
    }

    fn home_rect(&self, slot: usize) -> Rect {
        self.layout.slot_rect(slot)
    }

    fn resident_frame(&self, slot: usize, m: &Motion) -> CardFrame {
        let card = &self.cards[slot];
        let rect = self.rect_of_resident(slot, m);

        let state = if card.drag.is_some() {
            CardState::Dragging
        } else if card.entry.is_some_and(|t| t.is_active(m)) {
            CardState::Entering
        } else if card.ret.is_some_and(|(t, _)| t.is_active(m)) {
            CardState::Returning
        } else if card.fall.is_some_and(|(t, _)| t.is_active(m)) {
            CardState::Falling
        } else {
            CardState::Resting
        };

        let entry_alpha = card
            .entry
            .map_or(1.0, |t| (t.progress(m) * 2.0).clamp(0.0, 1.0));

        let drag_offset = card.drag.unwrap_or(Vec2::ZERO);
        let angle = lean(drag_offset.x);

        // Chrome is damped by the lean: epaint cannot rotate text, so upright
        // labels on a tilted card look wrong. They have to be gone before the
        // tilt is visible.
        let flat = (1.0 - angle.abs() / MAX_LEAN).clamp(0.0, 1.0);
        let reveal = if card.drag.is_some() {
            0.0
        } else {
            reveal_progress(card, m, self.timing.reveal)
        };

        CardFrame {
            id: card.id,
            slot,
            rect,
            alpha: entry_alpha,
            reveal: self.timing.reveal_ease.apply(reveal) * flat,
            lift: if card.lifted { 1.0 } else { 0.0 },
            angle,
            state,
        }
    }

    fn departing_frame(&self, d: &Departing, m: &Motion) -> CardFrame {
        let v = d.timeline.value(m).clamp(0.0, 1.0);
        let rect = dock::lerp_rect(d.from, d.to, v);
        CardFrame {
            id: d.id,
            slot: d.from_slot,
            rect: self.dock.absorb(rect, m),
            // Fade over the second half of the travel, so the card is invisible
            // before it reaches the edge rather than clipping against it.
            alpha: ((1.0 - v) * 2.0).clamp(0.0, 1.0),
            reveal: 0.0,
            lift: 0.0,
            angle: 0.0,
            state: CardState::Departing,
        }
    }

    // -- the invariant ------------------------------------------------------

    /// Checks that **no card has moved up** (D28).
    ///
    /// A card's slot may only ever decrease, so its birth slot is its high-water
    /// mark. Departing cards are checked too, against the slot they left from.
    ///
    /// Two things are deliberately *not* violations, because neither is the pile
    /// reassigning a slot: a card following the pointer during a user drag, and
    /// cards travelling back up out of the dock when the user expands it. Both
    /// are transient offsets from a slot that has not changed.
    pub fn check_no_card_moved_up(&self) -> Result<(), MovedUp> {
        for (slot, card) in self.cards.iter().enumerate() {
            if slot > card.born_slot {
                return Err(MovedUp {
                    id: card.id,
                    born_slot: card.born_slot,
                    slot,
                });
            }
        }
        for d in &self.departing {
            if d.from_slot > d.born_slot {
                return Err(MovedUp {
                    id: d.id,
                    born_slot: d.born_slot,
                    slot: d.from_slot,
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The largest lean a dragged card takes, in radians (~6°).
pub const MAX_LEAN: f32 = 0.10;

/// How far a card leans for a given lateral drag.
///
/// Saturating, so a long drag does not spin the card; the lean is a hint that
/// the card is loose, not a readout of distance.
#[must_use]
pub fn lean(dx: f32) -> f32 {
    (dx / 900.0).clamp(-1.0, 1.0) * MAX_LEAN
}

/// How far a card's hover chrome is revealed, unshaped.
#[must_use]
pub fn reveal_progress(card: &Card, m: &Motion, token: Duration) -> f32 {
    let p = m.progress(card.hover_since, token);
    if card.hover_on { p } else { 1.0 - p }
}
