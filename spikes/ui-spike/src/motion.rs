//! Motion token layer — the fourth token layer, alongside colour, spacing and type.
//!
//! Per D19 motion is a *design system concern*, not something you scatter as
//! ad-hoc `lerp`s through drawing code. So this module owns:
//!
//! * **duration tokens** (`INSTANT` / `FAST` / `BASE` / `SLOW`),
//! * **easing curves** as pure `fn(f32) -> f32` over normalised time, including
//!   two real second-order springs (critically damped, and slightly underdamped
//!   so it overshoots),
//! * a **per-element helper** keyed by `egui::Id` so a call site is one line,
//! * a **stagger** helper so grouped elements cascade instead of popping,
//! * a **global duration multiplier** and a **reduce-motion switch** that
//!   collapses every duration to zero (D13, non-negotiable),
//! * `Spring1` / `Spring2` — real dt-integrated springs for the things egui's
//!   `animate_*` primitives cannot express (pointer-follow lag, settle-back,
//!   ballistic fling).
//!
//! Note on repaints: `Context::animate_bool_with_time*` and
//! `animate_value_with_time` already call `request_repaint()` for you while a
//! value is in flight, and stop once it settles. The springs below do **not** —
//! they return an `active` bool, and the caller is responsible for asking for
//! another frame. That split is the whole immediate-mode animation contract and
//! is why `Spring*::step` returns something you must not ignore.

#![allow(dead_code)] // A token layer defines the full vocabulary; not every
                     // token has a caller yet in a spike this size.

use egui::{Color32, Context, Id, Vec2};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Duration tokens (seconds). Named, not numeric, at every call site.
// ---------------------------------------------------------------------------

/// State flips that must not read as animation at all (press feedback).
pub const INSTANT: f32 = 0.080;
/// Small chrome: button hover wash, icon tint.
pub const FAST: f32 = 0.140;
/// The default. Hover reveals, highlights, most opacity work.
pub const BASE: f32 = 0.220;
/// Something entering or leaving the scene.
pub const SLOW: f32 = 0.320;

/// Default gap between staggered siblings.
pub const STEP: f32 = 0.030;

// ---------------------------------------------------------------------------
// Global controls (process-wide; the tuner overlay drives these live).
// ---------------------------------------------------------------------------

const ONE_BITS: u32 = 0x3f80_0000; // 1.0f32.to_bits()
static SCALE: AtomicU32 = AtomicU32::new(ONE_BITS);
static REDUCE: AtomicBool = AtomicBool::new(false);
/// 0 = "as designed" (each call site keeps its own curve); otherwise
/// `CURVES[idx - 1]` overrides every curve so the difference is feelable.
static CURVE_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

pub fn set_scale(v: f32) {
    SCALE.store(v.clamp(0.05, 8.0).to_bits(), Ordering::Relaxed);
}
pub fn scale() -> f32 {
    f32::from_bits(SCALE.load(Ordering::Relaxed))
}

/// D13: the OS reduce-motion setting collapses every duration to zero. Motion is
/// never load-bearing, so everything still *reaches* its end state — instantly.
pub fn set_reduce(on: bool) {
    REDUCE.store(on, Ordering::Relaxed);
}
pub fn reduced() -> bool {
    REDUCE.load(Ordering::Relaxed)
}

pub fn set_curve_override(idx: usize) {
    CURVE_OVERRIDE.store(idx.min(CURVES.len()), Ordering::Relaxed);
}
pub fn curve_override() -> usize {
    CURVE_OVERRIDE.load(Ordering::Relaxed)
}
pub fn curve_override_name() -> &'static str {
    match curve_override() {
        0 => "as designed",
        i => CURVES[i - 1].0,
    }
}

/// Resolve a token duration into real seconds, honouring the global multiplier
/// and reduce-motion. **Every** duration in the app goes through here.
pub fn dur(token: f32) -> f32 {
    if reduced() {
        0.0
    } else {
        (token * scale()).max(0.0)
    }
}

// ---------------------------------------------------------------------------
// Easing curves — pure functions over normalised time, f(0) == 0, f(1) == 1.
// ---------------------------------------------------------------------------

pub type Ease = fn(f32) -> f32;

pub fn linear(t: f32) -> f32 {
    t
}

/// The workhorse: fast out of the gate, gentle arrival. Use for anything
/// responding to the user (hover, press, reveal).
pub fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t;
    1.0 - u * u * u
}

/// Symmetric — for things that move on their own with no user at either end.
pub fn ease_in_out_cubic(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let u = -2.0 * t + 2.0;
        1.0 - u * u * u / 2.0
    }
}

/// Sharper arrival than cubic — good for large travel that must not feel slow.
pub fn ease_out_quint(t: f32) -> f32 {
    let u = 1.0 - t;
    1.0 - u * u * u * u * u
}

/// A second-order spring, critically damped: fastest approach with **no**
/// overshoot. `1 - (1 + wt)e^{-wt}`, normalised so f(1) is exactly 1.
pub fn spring(t: f32) -> f32 {
    const W: f32 = 9.0;
    fn raw(t: f32) -> f32 {
        1.0 - (1.0 + W * t) * (-W * t).exp()
    }
    raw(t) / raw(1.0)
}

/// A second-order spring, slightly *under*damped: overshoots ~8% and settles.
/// This is the one that makes an entry feel physical rather than merely eased.
pub fn spring_overshoot(t: f32) -> f32 {
    const Z: f32 = 0.34; // damping ratio
    const W: f32 = 13.5; // natural frequency
    fn raw(t: f32) -> f32 {
        let wd = W * (1.0 - Z * Z).sqrt();
        1.0 - (-Z * W * t).exp() * ((wd * t).cos() + (Z * W / wd) * (wd * t).sin())
    }
    raw(t) / raw(1.0)
}

/// Classic "back" overshoot — snappier and more mechanical than the spring.
pub fn ease_out_back(t: f32) -> f32 {
    const C1: f32 = 1.70158;
    const C3: f32 = C1 + 1.0;
    let u = t - 1.0;
    1.0 + C3 * u * u * u + C1 * u * u
}

/// Every curve, in tuner order.
pub const CURVES: &[(&str, Ease)] = &[
    ("ease_out_cubic", ease_out_cubic),
    ("ease_in_out_cubic", ease_in_out_cubic),
    ("ease_out_quint", ease_out_quint),
    ("spring (critical)", spring),
    ("spring (overshoot)", spring_overshoot),
    ("ease_out_back", ease_out_back),
    ("linear", linear),
];

/// Apply the tuner's global curve override, if one is set.
fn resolve(want: Ease) -> Ease {
    match curve_override() {
        0 => want,
        i => CURVES[i - 1].1,
    }
}

// ---------------------------------------------------------------------------
// Per-element animation, keyed by `egui::Id`. One line per call site.
// ---------------------------------------------------------------------------

/// Animate a boolean toward 0/1 with a token duration and a named curve.
///
/// egui flips the easing on the way back down, so a reversal (mouse-out) reads
/// as the mirror of the reveal rather than an abrupt ease-out in reverse.
/// Returns 0.0/1.0 exactly at rest, which is also what stops the repaint loop.
pub fn anim(ctx: &Context, id: Id, on: bool, token: f32, ease: Ease) -> f32 {
    ctx.animate_bool_with_time_and_easing(id, on, dur(token), resolve(ease))
}

/// Animate toward an arbitrary `f32` (linear — egui has no eased variant for
/// values). Use for things whose *target* keeps moving, where an ease would
/// fight the retarget anyway.
pub fn anim_value(ctx: &Context, id: Id, target: f32, token: f32) -> f32 {
    ctx.animate_value_with_time(id, target, dur(token))
}

// ---------------------------------------------------------------------------
// Stagger — grouped elements cascade instead of popping together.
// ---------------------------------------------------------------------------

/// A resolved stagger group. Drive one master timeline, then ask it for each
/// child's own eased progress by index.
#[derive(Clone, Copy)]
pub struct Stagger {
    master: f32,
    elapsed: f32,
    each: f32,
    step: f32,
    ease: Ease,
}

impl Stagger {
    /// Eased 0..1 progress for sibling `i`.
    pub fn at(&self, i: usize) -> f32 {
        if self.each <= 0.0 {
            // Reduce-motion (or a zero duration): master is exactly 0.0 or 1.0.
            return self.master;
        }
        let t = ((self.elapsed - self.step * i as f32) / self.each).clamp(0.0, 1.0);
        (self.ease)(t)
    }

    /// Convenience: `(alpha, rise_offset)` for the standard "fade + slight rise"
    /// reveal, so call sites don't restate it.
    pub fn rise(&self, i: usize, distance: f32) -> (f32, f32) {
        let t = self.at(i);
        (t.clamp(0.0, 1.0), (1.0 - t) * distance)
    }

    /// True while any sibling is mid-flight.
    pub fn animating(&self) -> bool {
        self.master > 0.0 && self.master < 1.0
    }
}

/// Build a stagger group of `n` siblings: each runs for `each`, offset by `step`.
///
/// Reversing works for free — the master timeline runs backwards, so the group
/// un-cascades from the far end, which is the natural way a reveal retracts.
pub fn stagger(
    ctx: &Context,
    id: Id,
    on: bool,
    n: usize,
    each: f32,
    step: f32,
    ease: Ease,
) -> Stagger {
    let each_s = dur(each);
    let step_s = dur(step);
    let total = each_s + step_s * n.saturating_sub(1) as f32;
    let master = ctx.animate_bool_with_time(id, on, total);
    Stagger {
        master,
        elapsed: master * total,
        each: each_s,
        step: step_s,
        ease: resolve(ease),
    }
}

// ---------------------------------------------------------------------------
// Real springs — dt-integrated, for the things `animate_*` cannot express.
// ---------------------------------------------------------------------------

/// Snappy: card settle, depth reshuffle.
pub const K_SNAP: f32 = 320.0;
pub const C_SNAP: f32 = 30.0;
/// Loose: pointer-follow lag, so a dragged card trails the cursor slightly.
pub const K_DRAG: f32 = 900.0;
pub const C_DRAG: f32 = 48.0;
/// Soft: the deck settling back after a card leaves.
pub const K_SOFT: f32 = 190.0;
pub const C_SOFT: f32 = 22.0;

// ---------------------------------------------------------------------------
// Live-tunable gesture physics.
//
// These are the numbers that actually decide how the card gestures *feel*, so
// they are runtime globals rather than consts — the `M` overlay writes them
// while the app runs. Durations and easing barely matter for spatial motion;
// stiffness, damping, friction and the dismiss threshold are everything.
// ---------------------------------------------------------------------------

macro_rules! f32_global {
    ($store:ident, $get:ident, $set:ident, $default:expr, $lo:expr, $hi:expr) => {
        static $store: AtomicU32 = AtomicU32::new(0);
        pub fn $get() -> f32 {
            let raw = $store.load(Ordering::Relaxed);
            if raw == 0 {
                $default
            } else {
                f32::from_bits(raw)
            }
        }
        pub fn $set(v: f32) {
            $store.store(v.clamp($lo, $hi).to_bits(), Ordering::Relaxed);
        }
    };
}

// Settle spring for a card arriving at, or springing back to, its home slot.
f32_global!(SETTLE_K, settle_k, set_settle_k, 210.0, 20.0, 900.0);
f32_global!(SETTLE_C, settle_c, set_settle_c, 22.0, 2.0, 90.0);
// Depth spring for the cards *underneath* reflowing. Softer on purpose.
f32_global!(DECK_K, deck_k, set_deck_k, 150.0, 20.0, 900.0);
f32_global!(DECK_C, deck_c, set_deck_c, 19.0, 2.0, 90.0);
// Air drag on a flung card, per second. Higher = stops sooner.
f32_global!(FLING_DRAG, fling_drag, set_fling_drag, 1.1, 0.0, 8.0);
// Release speed (px/s) toward the anchored edge that counts as a throw.
f32_global!(DISMISS_VEL, dismiss_vel, set_dismiss_vel, 420.0, 40.0, 2000.0);
// Displacement (px) toward the anchored edge that dismisses on release even
// without speed.
f32_global!(DISMISS_DIST, dismiss_dist, set_dismiss_dist, 110.0, 10.0, 400.0);
// Per-card delay (s) applied down the deck so the stack cascades rather than
// moving as one rigid body.
f32_global!(SETTLE_STAGGER, settle_stagger, set_settle_stagger, 0.045, 0.0, 0.30);

/// Restore every gesture-physics tunable to its designed default.
pub fn reset_tunables() {
    for s in [
        &SETTLE_K,
        &SETTLE_C,
        &DECK_K,
        &DECK_C,
        &FLING_DRAG,
        &DISMISS_VEL,
        &DISMISS_DIST,
        &SETTLE_STAGGER,
    ] {
        s.store(0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Pointer velocity tracking.
// ---------------------------------------------------------------------------

/// Rolling window of recent pointer positions, used to carry a *real* throw
/// velocity into a fling.
///
/// egui exposes `input.pointer.velocity()`, but it is smoothed over a fixed
/// window that is tuned for scrolling, and it keeps reporting motion for a
/// while after the pointer stops — which turns "drag slowly, then stop, then
/// release" into an unwanted throw. Sampling the drag deltas ourselves and
/// differencing over the last ~80ms makes a dead stop actually read as zero,
/// which is the difference between a dismissal that feels intentional and one
/// that feels twitchy.
#[derive(Clone, Debug, Default)]
pub struct VelocityTracker {
    samples: Vec<(f32, Vec2)>,
    t: f32,
}

/// How far back to difference when estimating throw speed.
const VEL_WINDOW: f32 = 0.080;

impl VelocityTracker {
    pub fn clear(&mut self) {
        self.samples.clear();
        self.t = 0.0;
    }

    /// Feed one frame of drag. `pos` is the cumulative drag offset.
    pub fn push(&mut self, dt: f32, pos: Vec2) {
        self.t += dt;
        self.samples.push((self.t, pos));
        let cutoff = self.t - VEL_WINDOW * 2.0;
        self.samples.retain(|(t, _)| *t >= cutoff);
    }

    /// Average velocity (px/s) over the trailing window. Zero if the pointer
    /// has been still, which is the behaviour that matters.
    pub fn velocity(&self) -> Vec2 {
        let Some(&(t_end, p_end)) = self.samples.last() else {
            return Vec2::ZERO;
        };
        let target = t_end - VEL_WINDOW;
        // Oldest sample still inside the window, so a long still-hold differences
        // against a recent identical position and yields zero.
        let base = self
            .samples
            .iter()
            .find(|(t, _)| *t >= target)
            .copied()
            .unwrap_or((t_end, p_end));
        let dt = t_end - base.0;
        if dt < 1e-4 {
            Vec2::ZERO
        } else {
            (p_end - base.1) / dt
        }
    }
}

/// Clamp a frame delta so a stalled frame (debugger, window drag) can't launch
/// a spring into orbit.
pub fn dt_of(ctx: &Context) -> f32 {
    ctx.input(|i| i.stable_dt).clamp(1.0 / 480.0, 1.0 / 20.0)
}

/// Scalar second-order spring.
#[derive(Clone, Copy, Debug, Default)]
pub struct Spring1 {
    pub pos: f32,
    pub vel: f32,
}

impl Spring1 {
    pub fn at(pos: f32) -> Self {
        Self { pos, vel: 0.0 }
    }
    /// Integrate one frame toward `target`. Returns `true` while still moving —
    /// **the caller must request another repaint when it does.**
    pub fn step(&mut self, target: f32, dt: f32, k: f32, c: f32) -> bool {
        if reduced() {
            self.pos = target;
            self.vel = 0.0;
            return false;
        }
        let dt = dt / scale().max(0.05);
        let steps = (dt / 0.004).ceil().clamp(1.0, 12.0);
        let h = dt / steps;
        for _ in 0..steps as i32 {
            let a = (target - self.pos) * k - self.vel * c;
            self.vel += a * h;
            self.pos += self.vel * h;
        }
        if (target - self.pos).abs() < 0.0015 && self.vel.abs() < 0.02 {
            self.pos = target;
            self.vel = 0.0;
            false
        } else {
            true
        }
    }
}

/// 2-D second-order spring.
#[derive(Clone, Copy, Debug, Default)]
pub struct Spring2 {
    pub pos: Vec2,
    pub vel: Vec2,
}

impl Spring2 {
    pub fn at(pos: Vec2) -> Self {
        Self { pos, vel: Vec2::ZERO }
    }
    /// Integrate one frame toward `target`. Returns `true` while still moving.
    pub fn step(&mut self, target: Vec2, dt: f32, k: f32, c: f32) -> bool {
        if reduced() {
            self.pos = target;
            self.vel = Vec2::ZERO;
            return false;
        }
        let dt = dt / scale().max(0.05);
        let steps = (dt / 0.004).ceil().clamp(1.0, 12.0);
        let h = dt / steps;
        for _ in 0..steps as i32 {
            let a = (target - self.pos) * k - self.vel * c;
            self.vel += a * h;
            self.pos += self.vel * h;
        }
        if (target - self.pos).length() < 0.06 && self.vel.length() < 0.6 {
            self.pos = target;
            self.vel = Vec2::ZERO;
            false
        } else {
            true
        }
    }

    /// Ballistic integration — no target, just momentum with mild air drag.
    /// Used for the fling, where the card is no longer seeking anything.
    pub fn coast(&mut self, dt: f32, drag_per_s: f32, gravity: Vec2) {
        let dt = if reduced() { dt } else { dt / scale().max(0.05) };
        self.vel += gravity * dt;
        self.vel *= (1.0 - drag_per_s * dt).clamp(0.0, 1.0);
        self.pos += self.vel * dt;
    }
}

// ---------------------------------------------------------------------------
// Small helpers so call sites stay one line.
// ---------------------------------------------------------------------------

/// Fade a colour by an animation progress value (clamped — overshoot curves can
/// legitimately exceed 1 and alpha cannot).
pub fn fade(c: Color32, t: f32) -> Color32 {
    c.linear_multiply(t.clamp(0.0, 1.0))
}

/// Scale an 0-255 alpha by an animation progress value.
pub fn alpha(base: u8, t: f32) -> u8 {
    (base as f32 * t.clamp(0.0, 1.0)).round() as u8
}

/// Linear interpolation with the clamp built in, so easing overshoot doesn't
/// silently produce negative sizes.
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
