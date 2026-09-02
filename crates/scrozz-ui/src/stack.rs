//! The **capture stack** — the app's primary surface (D12, D21, D28).
//!
//! A pile of thumbnail cards anchored to a chosen side of the work area,
//! growing **upward**. Cards enter and leave through that outer edge, and — the
//! invariant the whole design turns on — **a card never moves upward**.
//!
//! ```text
//!    slot 5  <- 6th capture
//!    slot 4
//!    slot 3
//!    slot 2
//!    slot 1  <- 2nd capture
//!    slot 0  <- 1st capture, and the first to leave
//!    ------- bottom edge of the WORK AREA
//! ```
//!
//! 1. The first capture appears at slot 0, sliding in from the selected side.
//! 2. Each next capture slides into the next slot up. **Existing cards do not
//!    move at all** while the pile is growing.
//! 3. When full, one coordinated motion: the oldest slides out through the
//!    selected side, every
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
//! Painting, and the OS hand-off behind [`Intent::DragOut`]. The stack reports
//! a committed live gesture while the button is still down; a drag-out first
//! discovered at release springs back because no native session can start then.

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

/// Side of the display that owns the Recent Captures Overlay.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecentCapturesPlacement {
    /// Anchor cards to the lower-left and dismiss them toward the left edge.
    #[default]
    Left,
    /// Anchor cards to the lower-right and dismiss them toward the right edge.
    Right,
}

impl RecentCapturesPlacement {
    /// Stable persisted setting value.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    /// Parses a persisted setting value.
    #[must_use]
    pub fn from_slug(value: &str) -> Option<Self> {
        match value {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }

    const fn outward(self) -> Dir {
        match self {
            Self::Left => Dir::Left,
            Self::Right => Dir::Right,
        }
    }

    const fn intent_for(self, direction: Dir) -> Intent {
        match (self, direction) {
            (_, Dir::Down) => Intent::Collapse,
            (Self::Left, Dir::Left) | (Self::Right, Dir::Right) => Intent::Dismiss,
            _ => Intent::DragOut,
        }
    }
}

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
    /// Inset from the selected work-area edge.
    pub side_margin: f32,
    /// Extra travel past the screen edge before a departing card is considered
    /// gone. Without it a card with a shadow leaves a smudge at the edge.
    pub clearance: f32,
}

impl Default for CardMetrics {
    fn default() -> Self {
        Self {
            width: Self::PREFERRED_WIDTH,
            height: Self::PREFERRED_HEIGHT,
            gap: 8.0,
            margin: 2.0,
            side_margin: 40.0,
            clearance: 24.0,
        }
    }
}

impl CardMetrics {
    /// Minimum designed card width, in logical points.
    pub const MIN_WIDTH: f32 = 224.0;
    /// Minimum designed card height, in logical points.
    pub const MIN_HEIGHT: f32 = 140.0;
    /// Preferred 16:10 card width, in logical points.
    pub const PREFERRED_WIDTH: f32 = 288.0;
    /// Preferred 16:10 card height, in logical points.
    pub const PREFERRED_HEIGHT: f32 = 180.0;
    /// Maximum designed card width, in logical points.
    pub const MAX_WIDTH: f32 = 320.0;
    /// Maximum designed card height, in logical points.
    pub const MAX_HEIGHT: f32 = 200.0;

    /// Preferred metrics adjusted for an OS text/accessibility density.
    #[must_use]
    pub fn for_density(scale: f32) -> Self {
        let scale = if scale.is_finite() { scale } else { 1.0 };
        let width = (Self::PREFERRED_WIDTH * scale).clamp(Self::MIN_WIDTH, Self::MAX_WIDTH);
        Self {
            width,
            height: width / 1.6,
            ..Self::default()
        }
    }

    fn constrained_to(mut self, work_area: Rect) -> Self {
        let width_room = (work_area.width() - self.side_margin - self.margin).max(1.0);
        let height_room = (work_area.height() - self.margin * 2.0).max(1.0);
        let scale = (width_room / self.width)
            .min(height_room / self.height)
            .min(1.0);
        if scale.is_finite() && scale > 0.0 {
            self.width *= scale;
            self.height *= scale;
        }
        self
    }

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
        scrozz_core::layout::vertical_capacity(
            f64::from(usable_height),
            f64::from(self.height),
            f64::from(self.gap),
        )
    }
}

/// Where the slots are, and how many there are.
///
/// Anchored to the **work area**, not the raw screen: on macOS the raw bottom
/// edge is underneath the Dock, and slot 0 would be half-hidden by it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StackLayout {
    work_area: Rect,
    requested_metrics: CardMetrics,
    metrics: CardMetrics,
    slots: usize,
    placement: RecentCapturesPlacement,
}

impl StackLayout {
    /// Derives a layout — and a slot count — from a work area.
    #[must_use]
    pub fn new(work_area: Rect, metrics: CardMetrics) -> Self {
        Self::with_placement(work_area, metrics, RecentCapturesPlacement::Left)
    }

    /// Derives a layout for a specific Recent Captures Overlay side.
    #[must_use]
    pub fn with_placement(
        work_area: Rect,
        metrics: CardMetrics,
        placement: RecentCapturesPlacement,
    ) -> Self {
        let requested_metrics = metrics;
        let metrics = metrics.constrained_to(work_area);
        let usable = work_area.height() - 2.0 * metrics.margin;
        let slots = metrics.slots_for_height(usable);
        Self {
            work_area,
            requested_metrics,
            metrics,
            slots,
            placement,
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

    /// The edge this layout is anchored to.
    #[must_use]
    pub const fn placement(&self) -> RecentCapturesPlacement {
        self.placement
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
        let left = match self.placement {
            RecentCapturesPlacement::Left => self.work_area.left() + m.side_margin,
            RecentCapturesPlacement::Right => self.work_area.right() - m.side_margin - m.width,
        };
        Rect::from_min_size(pos2(left, top), vec2(m.width, m.height))
    }

    /// Where a card sits before it has slid in — fully beyond the selected edge.
    #[must_use]
    pub fn entry_rect(&self, slot: usize) -> Rect {
        self.entry_rect_for(self.slot_rect(slot))
    }

    /// Where an arbitrary card rectangle sits before horizontal entry.
    #[must_use]
    pub fn entry_rect_for(&self, rest: Rect) -> Rect {
        let dx = match self.placement {
            RecentCapturesPlacement::Left => {
                self.work_area.left() - self.metrics.clearance - rest.right()
            }
            RecentCapturesPlacement::Right => {
                self.work_area.right() + self.metrics.clearance - rest.left()
            }
        };
        rest.translate(vec2(dx, 0.0))
    }

    const fn outward(&self) -> Dir {
        self.placement.outward()
    }

    const fn intent_for(&self, direction: Dir) -> Intent {
        self.placement.intent_for(direction)
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
/// drag-out both leave the pointer surface and ask for more conviction.
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

fn lock_direction(travel: Vec2) -> Option<Dir> {
    if travel.length() < DRAG_LOCK_SLOP {
        return None;
    }
    if travel.y > 0.0 {
        return Some(if travel.x >= 0.0 {
            Dir::Right
        } else {
            Dir::Left
        });
    }
    Some(if travel.x.abs() >= travel.y.abs() {
        if travel.x >= 0.0 {
            Dir::Right
        } else {
            Dir::Left
        }
    } else {
        Dir::Up
    })
}

fn in_collapse_cone(travel: Vec2) -> bool {
    travel.y > 0.0 && travel.x.abs() <= travel.y * COLLAPSE_CONE_RATIO
}

/// The window a release speed is measured over, in seconds.
///
/// Ported from the spike, which found egui's own smoothed pointer velocity
/// unusable here: it keeps reporting motion after the pointer has stopped, so
/// "drag slowly, stop, release" became an unwanted throw. Differencing over a
/// short window makes a dead stop read as exactly zero.
pub const VELOCITY_WINDOW: f32 = 0.080;

/// Pointer travel before a held card commits to one gesture direction.
pub const DRAG_LOCK_SLOP: f32 = 8.0;

/// Opacity of the stationary source while the native drag ghost is active.
pub const DRAG_SOURCE_ALPHA: f32 = 0.5;

/// Maximum horizontal drift relative to downward travel that still previews collapse.
pub const COLLAPSE_CONE_RATIO: f32 = 0.35;

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
    /// Easing for the entry. This must land smoothly without overshoot: the
    /// capture should glide into place rather than bounce against its slot.
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
    /// Source thumbnail fading when a native drag takes over or finishes.
    pub drag_source_fade: Duration,
    /// The pile collapsing into the dock.
    pub collapse: Duration,
    /// The pile coming back out.
    pub expand: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            enter: Duration::from_millis(440),
            enter_ease: Ease::InOutCubic,
            exit: Duration::BASE,
            exit_min: Duration::from_millis(90),
            exit_ease: Ease::Linear,
            fall: Duration::BASE,
            fall_ease: Ease::Spring,
            spring_back: Duration::BASE,
            spring_back_ease: Ease::SpringOvershoot,
            reveal: Duration::FAST,
            reveal_ease: Ease::OutCubic,
            drag_source_fade: Duration::FAST,
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
            drag_source_fade: Duration::ZERO,
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
    /// When the entry animation settles, as an absolute [`Motion::now`]
    /// instant — fixed at birth, independent of `entry` below, and `None` for
    /// a card the stack was seeded with rather than one that arrived.
    ///
    /// `entry` is a GC'd transient: [`CaptureStack::advance`] nulls it the
    /// first frame it is no longer active, which is exactly the instant a
    /// landing effect would want to start reading it. This field exists so
    /// painting has something to measure "time since landed" against that
    /// survives that collection.
    landed_at: Option<f64>,
    entry: Option<Timeline>,
    fall: Option<(Timeline, Vec2)>,
    ret: Option<ReturnTransition>,
    drag: Option<Vec2>,
    drag_fade: Option<AlphaTransition>,
    hover_on: bool,
    hover_since: f64,
    lifted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct AlphaTransition {
    timeline: Timeline,
    from: f32,
    to: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ReturnTransition {
    timeline: Timeline,
    from: Rect,
}

impl AlphaTransition {
    fn value(self, m: &Motion) -> f32 {
        self.timeline
            .value(m)
            .mul_add(self.to - self.from, self.from)
            .clamp(0.0, 1.0)
    }
}

/// Seconds since `card` settled, or `None` if it never announced itself.
///
/// Reads `landed_at`, which is fixed at birth and outlives `entry`'s
/// collection, so this stays correct for as long as the card is resident
/// rather than for the one frame after it settles.
fn landed_since(card: &Card, m: &Motion) -> Option<f32> {
    let since = m.now() - card.landed_at?;
    #[allow(clippy::cast_possible_truncation)]
    (since >= 0.0).then_some(since as f32)
}

fn drag_alpha(card: &Card, m: &Motion) -> f32 {
    card.drag_fade.map_or(1.0, |fade| fade.value(m))
}

fn start_drag_fade(card: &mut Card, to: f32, m: &Motion, duration: Duration) {
    let from = drag_alpha(card, m);
    if (from - to).abs() <= f32::EPSILON {
        if to >= 1.0 {
            card.drag_fade = None;
        }
        return;
    }
    card.drag_fade = Some(AlphaTransition {
        timeline: Timeline::starting(m, duration, Ease::OutCubic),
        from,
        to,
    });
}

fn fall_offset(card: &Card, m: &Motion) -> Vec2 {
    card.fall.map_or(Vec2::ZERO, |(timeline, displacement)| {
        displacement * (1.0 - timeline.value(m).clamp(0.0, 1.0))
    })
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
    from_alpha: f32,
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
    /// Seconds since this card's entry animation settled, or `None` if it
    /// never had one (a card the stack was seeded with at startup) or has not
    /// settled yet.
    ///
    /// The landing glow's whole clock. `None` is what keeps a restored pile
    /// from lighting up as though every capture in it had just been taken.
    pub landed: Option<f32>,
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
    /// Where the pointer was when the card was let go.
    pub pointer: Pos2,
    /// Release speed, in points per second.
    pub velocity: Vec2,
}

/// What a drag currently in progress would mean if the pointer stopped now.
///
/// This exists because one platform's drag-out cannot wait for the release.
/// AppKit will only start a dragging session while a mouse button is still
/// down; a session begun after the button comes up ends instantly, which is
/// exactly what "the card animates away and nothing ever drops" looks like. So
/// the host is told a drag-out has committed *during* the gesture, and takes
/// over from there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveGesture {
    /// The card being held.
    pub id: CardId,
    /// What the gesture means so far.
    pub intent: Intent,
    /// Which way it is going, if it has committed.
    pub direction: Option<Dir>,
    /// Where the card is right now, in window coordinates.
    pub rect: Rect,
    /// Where the pointer is right now, in window coordinates.
    pub pointer: Pos2,
}

/// The card currently under the pointer.
#[derive(Debug, Clone)]
struct ActiveDrag {
    id: CardId,
    origin: Pos2,
    latest: Pos2,
    velocity: DragVelocity,
    direction: Option<Dir>,
    pinned_rect: Option<Rect>,
    collapse_candidate: bool,
    collapse_progress: f32,
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

    /// An empty stack configured for the current Recent Captures preferences.
    #[must_use]
    pub fn configured(
        work_area: Rect,
        placement: RecentCapturesPlacement,
        card_width: f32,
    ) -> Self {
        let width = card_width.clamp(CardMetrics::MIN_WIDTH, CardMetrics::MAX_WIDTH);
        let metrics = CardMetrics {
            width,
            height: width / 1.6,
            ..CardMetrics::default()
        };
        Self::new(
            StackLayout::with_placement(work_area, metrics, placement),
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

    /// Whether changing placement and width can keep every current card resident.
    #[must_use]
    pub fn configuration_preserves_residents(
        &self,
        placement: RecentCapturesPlacement,
        card_width: f32,
    ) -> bool {
        let width = card_width.clamp(CardMetrics::MIN_WIDTH, CardMetrics::MAX_WIDTH);
        let metrics = CardMetrics {
            width,
            height: width / 1.6,
            ..CardMetrics::default()
        };
        StackLayout::with_placement(self.layout.work_area, metrics, placement).slots()
            >= self.cards.len()
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
            self.retire_slot(0, Intent::Dismiss, self.layout.outward(), Vec2::ZERO, m);
        }
        let slot = self.cards.len();
        let id = CardId(self.next_id);
        self.next_id += 1;
        self.cards.push(Card {
            id,
            born_slot: slot,
            landed_at: Some(m.now() + f64::from(m.resolve(self.timing.enter))),
            entry: Some(Timeline::starting(
                m,
                self.timing.enter,
                self.timing.enter_ease,
            )),
            fall: None,
            ret: None,
            drag: None,
            drag_fade: None,
            hover_on: false,
            hover_since: m.now(),
            lifted: false,
        });
        id
    }

    /// Adds a capture that is already there: no entry animation, and no
    /// landing announcement.
    ///
    /// For a pile restored rather than captured — whatever was already
    /// waiting when the overlay drew its first frame. Those cards did not
    /// just arrive, and animating them would say they had.
    pub fn push_settled(&mut self, m: &Motion) -> CardId {
        let id = self.push(m);
        if let Some(card) = self.cards.last_mut() {
            card.entry = None;
            card.landed_at = None;
        }
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
        self.retire_slot(slot, Intent::Dismiss, self.layout.outward(), Vec2::ZERO, m);
        true
    }

    /// Retires the oldest capture, from the bottom, leftward.
    pub fn dismiss_oldest(&mut self, m: &Motion) -> bool {
        if self.cards.is_empty() {
            return false;
        }
        self.retire_slot(0, Intent::Dismiss, self.layout.outward(), Vec2::ZERO, m);
        true
    }

    /// Clears the pile — what Escape does.
    ///
    /// D27 requires this surface be escapable on the first guess. Retiring from
    /// the top down means no card is ever asked to fall while the pile empties.
    pub fn dismiss_all(&mut self, m: &Motion) {
        self.drag = None;
        while let Some(top) = self.cards.len().checked_sub(1) {
            self.retire_slot(top, Intent::Dismiss, self.layout.outward(), Vec2::ZERO, m);
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
        let from_alpha = drag_alpha(&self.cards[slot], m);

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
            from_alpha,
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
        let translation = work_area.min - self.layout.work_area.min;
        let pure_translation = work_area.size() == self.layout.work_area.size();
        if pure_translation && translation != Vec2::ZERO {
            for card in &mut self.cards {
                if let Some(returning) = &mut card.ret {
                    returning.from = returning.from.translate(translation);
                }
            }
            for departing in &mut self.departing {
                departing.from = departing.from.translate(translation);
                departing.to = departing.to.translate(translation);
            }
        }
        self.layout = StackLayout::with_placement(
            work_area,
            self.layout.requested_metrics,
            self.layout.placement,
        );
        self.dock.set_rect(self.layout.dock_rect());
        while self.cards.len() > self.capacity() {
            self.retire_slot(0, Intent::Dismiss, self.layout.outward(), Vec2::ZERO, m);
        }
    }

    /// Applies placement and card size to current and future cards.
    ///
    /// A size reduction keeps the normal capacity contract: oldest cards retire
    /// first until every remaining card has a slot.
    pub fn configure(&mut self, placement: RecentCapturesPlacement, card_width: f32, m: &Motion) {
        let width = card_width.clamp(CardMetrics::MIN_WIDTH, CardMetrics::MAX_WIDTH);
        let metrics = CardMetrics {
            width,
            height: width / 1.6,
            ..CardMetrics::default()
        };
        self.layout = StackLayout::with_placement(self.layout.work_area, metrics, placement);
        self.dock.set_rect(self.layout.dock_rect());
        self.drag = None;
        while self.cards.len() > self.capacity() {
            self.retire_slot(0, Intent::Dismiss, self.layout.outward(), Vec2::ZERO, m);
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
            direction: None,
            pinned_rect: None,
            collapse_candidate: false,
            collapse_progress: 0.0,
        });
        let card = &mut self.cards[slot];
        card.drag = Some(Vec2::ZERO);
        card.ret = None;
        card.lifted = true;
        true
    }

    /// Moves an internal gesture with the pointer after direction lock.
    ///
    /// An outward/upward gesture is a native drag source, so the resident card
    /// stays pinned while the OS-owned ghost follows the pointer.
    pub fn drag_to(&mut self, pointer: Pos2, m: &Motion) {
        let (id, offset, direction, newly_locked, collapse_candidate, collapse_progress) = {
            let Some(drag) = self.drag.as_mut() else {
                return;
            };
            drag.latest = pointer;
            drag.velocity.push(pointer, m);
            let offset = pointer - drag.origin;
            let newly_locked = if drag.direction.is_none() && !drag.collapse_candidate {
                if in_collapse_cone(offset) && offset.length() >= DRAG_LOCK_SLOP {
                    drag.collapse_candidate = true;
                    None
                } else {
                    drag.direction = lock_direction(offset);
                    drag.direction
                }
            } else {
                None
            };
            if drag.collapse_candidate {
                drag.collapse_progress = if in_collapse_cone(offset) {
                    (offset.y / self.gestures.collapse_dist).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if drag.collapse_progress >= 1.0 {
                    drag.direction = Some(Dir::Down);
                }
            }
            (
                drag.id,
                offset,
                drag.direction,
                newly_locked,
                drag.collapse_candidate,
                drag.collapse_progress,
            )
        };

        if collapse_candidate {
            if m.is_reduced() {
                self.dock.scrub(0.0);
            } else if collapse_progress > 0.0 {
                self.dock.scrub(collapse_progress);
            } else {
                self.dock.expand(m);
            }
            if collapse_progress >= 1.0 {
                self.dock.collapse(m);
            }
        }

        if newly_locked
            .is_some_and(|direction| self.layout.intent_for(direction) == Intent::DragOut)
            && let Some(slot) = self.slot_of(id)
        {
            let pinned = self.rect_of_resident(slot, m);
            if let Some(drag) = self.drag.as_mut() {
                drag.pinned_rect = Some(pinned);
            }
        }
        if let Some(card) = self.cards.iter_mut().find(|c| c.id == id) {
            if direction
                .is_some_and(|direction| self.layout.intent_for(direction) == Intent::DragOut)
            {
                card.drag = Some(Vec2::ZERO);
                card.lifted = false;
                if newly_locked.is_some() {
                    card.entry = None;
                    card.fall = None;
                    card.ret = None;
                    start_drag_fade(card, DRAG_SOURCE_ALPHA, m, self.timing.drag_source_fade);
                }
            } else if direction.is_some() && direction != Some(Dir::Down) {
                card.drag = Some(offset);
            } else if collapse_candidate {
                card.drag = Some(Vec2::ZERO);
                card.lifted = false;
            }
        }
    }

    /// What the drag in progress means so far, if it means anything yet.
    ///
    /// `None` covers both "nothing is held" and "held, but not yet committed
    /// to any direction". Those are the same answer to the only question the
    /// caller is asking — *should the platform take over now?* — and collapsing
    /// them means a caller cannot arm a native drag off a gesture that is still
    /// undecided by forgetting to check the intent.
    ///
    /// Deliberately scored on **distance only**. Speed is a statement made at
    /// release — mid-gesture it is noise, and a fast pass through the commit
    /// zone on the way somewhere else is not a drag-out. Requiring the travel
    /// means the user has to actually pull the card clear before the operating
    /// system takes over.
    ///
    /// See [`LiveGesture`] for why this is needed at all.
    #[must_use]
    pub fn live_gesture(&self, m: &Motion) -> Option<LiveGesture> {
        let drag = self.drag.as_ref()?;
        let direction = drag.direction?;
        let intent = self.layout.intent_for(direction);
        let travel = drag.latest - drag.origin;
        if intent != Intent::DragOut && self.gestures.score(direction, travel, Vec2::ZERO) < 1.0 {
            return None;
        }
        let slot = self.slot_of(drag.id)?;
        Some(LiveGesture {
            id: drag.id,
            intent,
            direction: Some(direction),
            rect: if intent == Intent::DragOut {
                drag.pinned_rect.unwrap_or_else(|| self.home_rect(slot))
            } else {
                self.rect_of_resident(slot, m)
            },
            pointer: drag.latest,
        })
    }

    /// Lets go, and acts on what the gesture meant.
    ///
    /// Returns `None` if nothing was held.
    pub fn release_drag(&mut self, m: &Motion) -> Option<DragRelease> {
        let drag = self.drag.take()?;
        let travel = drag.latest - drag.origin;
        let velocity = drag.velocity.velocity(m);
        let direction = drag.direction;
        let intent = direction.map_or(Intent::SpringBack, |direction| {
            if self.gestures.score(direction, travel, velocity) >= 1.0 {
                self.layout.intent_for(direction)
            } else {
                Intent::SpringBack
            }
        });
        let slot = self.slot_of(drag.id)?;
        let rect = drag
            .pinned_rect
            .unwrap_or_else(|| self.rect_of_resident(slot, m));

        match intent {
            Intent::Dismiss => {
                let dir = direction.unwrap_or_else(|| self.layout.outward());
                self.retire_slot(slot, intent, dir, velocity, m);
            }
            // Too late to begin a native session. A distance-committed drag-out
            // was already handed off through `live_gesture`; reaching this arm
            // means only release velocity crossed the threshold. Losing the card
            // without delivering a drop is never an acceptable shortcut.
            Intent::DragOut => self.spring_back(slot, m, drag.pinned_rect),
            Intent::Collapse => {
                self.spring_back(slot, m, drag.pinned_rect);
                self.dock.collapse(m);
            }
            Intent::SpringBack => {
                self.spring_back(slot, m, drag.pinned_rect);
                if drag.collapse_candidate {
                    self.dock.expand(m);
                }
            }
        }

        Some(DragRelease {
            id: drag.id,
            intent,
            direction,
            rect,
            pointer: drag.latest,
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
            self.spring_back(slot, m, drag.pinned_rect);
        }
        if drag.collapse_candidate {
            self.dock.expand(m);
        }
    }

    /// Ends `id`'s drag from outside the gesture, and springs the card back.
    ///
    /// # Why this is not [`Self::cancel_drag`]
    ///
    /// Two differences, and both matter.
    ///
    /// It names the card. `cancel_drag` ends whatever is held, which is right
    /// when the pointer itself let go, and wrong when the caller is a native
    /// drag session finishing: by then the user may already be holding a
    /// different card, and cancelling *that* gesture because an older one
    /// finished would yank a card out from under the pointer.
    ///
    /// It does not require a drag to be in progress. After a native drag the
    /// platform runs a modal event loop that can swallow the mouse-up entirely,
    /// so the release this surface was waiting for may never arrive — the card
    /// is left held, displaced, and unable to be dragged again. Equally, the
    /// release may have arrived normally and been handled already. This copes
    /// with both, because the caller cannot know which happened and should not
    /// have to.
    ///
    /// Returns whether anything actually changed, which is what lets a caller
    /// tell "the gesture was still stuck" from "the release came through
    /// normally" without inspecting private state.
    pub fn settle_drag(&mut self, id: CardId, m: &Motion) -> bool {
        let mut changed = false;

        if self.drag.as_ref().is_some_and(|drag| drag.id == id) {
            let drag = self.drag.take().expect("the matching drag exists");
            if let Some(slot) = self.slot_of(id) {
                self.spring_back(slot, m, drag.pinned_rect);
            }
            changed = true;
        }

        // Displacement outlives the drag record, so this is checked separately
        // rather than only when a drag was found.
        if let Some(slot) = self.slot_of(id)
            && self
                .cards
                .get(slot)
                .is_some_and(|card| card.drag.is_some() || card.lifted)
        {
            self.spring_back(slot, m, None);
            changed = true;
        }

        changed
    }

    /// Sends a card back to its slot from wherever it is.
    fn spring_back(&mut self, slot: usize, m: &Motion, from: Option<Rect>) {
        if slot >= self.cards.len() {
            return;
        }
        let from = from.unwrap_or_else(|| self.rect_of_resident(slot, m));
        let target = self.base_rect_of_resident(slot, m);
        let fall_offset = fall_offset(&self.cards[slot], m);
        let from = from.translate(-fall_offset);
        let card = &mut self.cards[slot];
        card.drag = None;
        card.lifted = false;
        start_drag_fade(card, 1.0, m, self.timing.drag_source_fade);
        card.ret = if from == target {
            None
        } else {
            Some(ReturnTransition {
                timeline: Timeline::starting(
                    m,
                    self.timing.spring_back,
                    self.timing.spring_back_ease,
                ),
                from,
            })
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
            if card.ret.is_some_and(|ret| !ret.timeline.is_active(m)) {
                card.ret = None;
            }
            if card
                .drag_fade
                .is_some_and(|fade| !fade.timeline.is_active(m) && fade.to >= 1.0)
            {
                card.drag_fade = None;
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
                    || card.ret.is_some_and(|ret| ret.timeline.is_active(m))
                    || card
                        .drag_fade
                        .is_some_and(|fade| fade.timeline.is_active(m))
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

    /// What the landing glow contributes to the frame schedule.
    ///
    /// Separate from [`CaptureStack::activity`] because the one rule the
    /// stack cannot know is whether a card's editor is open: `suppressed`
    /// answers that, and a suppressed card schedules nothing, exactly as it
    /// draws nothing. Under reduce-motion this is idle for every card.
    ///
    /// Keeping the predicate here rather than in the caller is what stops a
    /// card drawing light nobody is scheduling frames for.
    #[must_use]
    pub fn glow_activity(&self, m: &Motion, suppressed: impl Fn(CardId) -> bool) -> Activity {
        let mut a = Activity::IDLE;
        for card in &self.cards {
            a |= Activity::when_animating(crate::card::glow::is_active(
                landed_since(card, m),
                suppressed(card.id),
                m.is_reduced(),
            ));
        }
        a
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
        if let Some(pinned) = self
            .drag
            .as_ref()
            .filter(|drag| drag.id == card.id)
            .and_then(|drag| drag.pinned_rect)
        {
            return pinned;
        }
        let mut rect = self.base_rect_of_resident(slot, m);
        if let Some(ret) = card.ret {
            rect = dock::lerp_rect(ret.from, rect, ret.timeline.value(m).clamp(0.0, 1.0));
        }
        if let Some((timeline, displacement)) = card.fall {
            rect = rect.translate(displacement * (1.0 - timeline.value(m).clamp(0.0, 1.0)));
        }
        if let Some(offset) = card.drag {
            rect = rect.translate(offset);
        }
        rect
    }

    fn base_rect_of_resident(&self, slot: usize, m: &Motion) -> Rect {
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
        rect = self.dock.absorb(rect, m);
        rect
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
        } else if card.ret.is_some_and(|ret| ret.timeline.is_active(m)) {
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
            alpha: entry_alpha * drag_alpha(card, m) * self.collapse_preview_alpha(),
            reveal: self.timing.reveal_ease.apply(reveal) * flat,
            lift: if card.lifted { 1.0 } else { 0.0 },
            angle,
            state,
            landed: landed_since(card, m),
        }
    }

    fn collapse_preview_alpha(&self) -> f32 {
        let progress = self
            .drag
            .as_ref()
            .filter(|drag| drag.collapse_candidate)
            .map_or(0.0, |drag| drag.collapse_progress);
        1.0 - progress * 0.35
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
            alpha: d.from_alpha * ((1.0 - v) * 2.0).clamp(0.0, 1.0),
            reveal: 0.0,
            lift: 0.0,
            angle: 0.0,
            state: CardState::Departing,
            // A card on its way out is not arriving.
            landed: None,
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
