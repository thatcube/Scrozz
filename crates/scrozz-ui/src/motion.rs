//! Motion tokens — the fourth token layer, alongside colour, spacing and type.
//!
//! Per decision **D19** motion is a design-system concern, not something
//! scattered as ad-hoc `lerp`s through drawing code. This module owns the whole
//! vocabulary: duration tokens, easing curves, springs, stagger scheduling, the
//! reduce-motion switch, and the repaint-scheduling discipline immediate mode
//! imposes.
//!
//! # Three rules this module exists to enforce
//!
//! **1. Motion belongs to objects, not controls (D19).** Cards slide, tilt and
//! fling; buttons, pills and menu rows change state *instantly*. An eased
//! control reads as lag between intent and acknowledgement. Nothing here should
//! ever be wired to a button's hover or press state — see [`crate::paint`],
//! where every control is a plain `if`.
//!
//! **2. Reduce-motion collapses every duration to zero (D13).** There is
//! exactly one choke point, [`Motion::resolve`], and every duration in the app
//! passes through it. Springs snap to target through the same flag. Motion is
//! never load-bearing: the end state is always reached, just instantly.
//!
//! **3. Time is a parameter, never a global (D25).** This is the property the
//! headless screenshot harness depends on. Every animation in this module
//! computes its value from a [`Motion`] handed to it, so a test can ask for
//! "card entry at t = 180 ms" and get the same float every single time:
//!
//! ```
//! use scrozz_ui::motion::{Duration, Ease, Motion, Timeline};
//!
//! let entry = Timeline::starting_at(0.0, Duration::SLOW, Ease::SpringOvershoot);
//! let a = entry.value(&Motion::at_ms(180));
//! let b = entry.value(&Motion::at_ms(180));
//! assert_eq!(a, b);
//! ```
//!
//! Stateful integrators ([`Spring1`], [`Spring2`]) reach the same property a
//! different way: they advance by a *fixed* sub-step, so replaying the same
//! elapsed time from the same state always lands on the same number.
//!
//! # Repaint scheduling
//!
//! egui repaints on input only. An animation with no `request_repaint()` does
//! not error — it silently crawls forward one frame per mouse move, which reads
//! as "the animation is broken" rather than "the scheduler is wrong". But
//! repainting unconditionally pins the CPU and would undermine the
//! native-performance argument for choosing egui at all.
//!
//! [`Activity`] is the resolution. Every surface returns one, they OR together,
//! and the app applies the merged result once. It also distinguishes *animating*
//! (repaint now) from *dwelling* (sleep until the deadline), which is a real
//! distinction: a toast that holds `animating` for its whole 1.25 s dwell burns
//! ~75 identical frames for nothing.

use egui::{Color32, Context, Id, Vec2};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Duration tokens
// ---------------------------------------------------------------------------

/// A named duration token, in seconds.
///
/// Call sites name a token; they never write a number. The concrete seconds a
/// token resolves to depend on the user's accessibility preferences, which is
/// why resolution happens against a [`Motion`] rather than at the constant.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Duration(f32);

impl Duration {
    /// No motion at all. What every token collapses to under reduce-motion.
    pub const ZERO: Self = Self(0.0);

    /// A state flip that must not read as animation (80 ms).
    ///
    /// Reserved for object state; per D19 controls do not use even this.
    pub const INSTANT: Self = Self(0.080);

    /// Small chrome: an icon tint, a scrim (140 ms).
    pub const FAST: Self = Self(0.140);

    /// The default. Hover reveals, highlights, most opacity work (220 ms).
    pub const BASE: Self = Self(0.220);

    /// Something entering or leaving the scene (320 ms).
    pub const SLOW: Self = Self(0.320);

    /// Default gap between staggered siblings (30 ms).
    pub const STAGGER_STEP: Self = Self(0.030);

    /// A custom duration in seconds. Prefer a named token.
    #[must_use]
    pub const fn from_secs(secs: f32) -> Self {
        Self(secs)
    }

    /// A custom duration in milliseconds. Prefer a named token.
    #[must_use]
    pub const fn from_millis(ms: u32) -> Self {
        Self(ms as f32 / 1000.0)
    }

    /// The token's designed length in seconds, *before* preferences apply.
    ///
    /// Almost nothing should call this — use [`Motion::resolve`], which honours
    /// reduce-motion. This exists for tuning UI and tests.
    #[must_use]
    pub const fn designed_secs(self) -> f32 {
        self.0
    }

    /// This token stretched or compressed by a factor.
    #[must_use]
    pub fn scaled(self, factor: f32) -> Self {
        Self((self.0 * factor).max(0.0))
    }
}

// ---------------------------------------------------------------------------
// Preferences
// ---------------------------------------------------------------------------

/// The user's motion preferences, as a value.
///
/// Deliberately a plain value rather than ambient state: a test builds one,
/// hands it to a [`Motion`], and never touches anything another test can see.
/// The spike learned this the hard way — process-wide motion globals made
/// `cargo test`'s parallel threads stomp each other and had to be serialised
/// behind a mutex.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionPrefs {
    /// D13: the OS "reduce motion" setting. Collapses every duration to zero
    /// and snaps every spring.
    pub reduce_motion: bool,

    /// Multiplier applied to every duration. `2.0` runs the whole app at half
    /// speed; `0.5` at double. Exists for tuning and for slow-motion capture.
    pub time_scale: f32,
}

impl Default for MotionPrefs {
    fn default() -> Self {
        Self {
            reduce_motion: false,
            time_scale: 1.0,
        }
    }
}

impl MotionPrefs {
    /// Preferences with reduce-motion on. Every duration resolves to zero.
    #[must_use]
    pub fn reduced() -> Self {
        Self {
            reduce_motion: true,
            ..Self::default()
        }
    }

    /// The lowest and highest time scales that will be honoured.
    const SCALE_RANGE: (f32, f32) = (0.05, 8.0);

    fn clamped_scale(self) -> f32 {
        let (lo, hi) = Self::SCALE_RANGE;
        if self.time_scale.is_finite() {
            self.time_scale.clamp(lo, hi)
        } else {
            1.0
        }
    }
}

const SCALE_ONE_BITS: u32 = 0x3f80_0000; // 1.0_f32.to_bits()
static SYSTEM_REDUCE: AtomicBool = AtomicBool::new(false);
static SYSTEM_SCALE: AtomicU32 = AtomicU32::new(SCALE_ONE_BITS);

/// The preferences published by the host application, normally mirrored from
/// the OS accessibility settings once at startup.
///
/// This is the *only* ambient motion state in the crate, and it exists solely
/// so the app does not have to thread preferences through every call. Nothing
/// downstream reads it: [`Motion::from_context`] snapshots it once per frame
/// into a value, and every animation reads that value. Tests construct a
/// [`Motion`] directly and never observe this.
#[must_use]
pub fn system_prefs() -> MotionPrefs {
    MotionPrefs {
        reduce_motion: SYSTEM_REDUCE.load(Ordering::Relaxed),
        time_scale: f32::from_bits(SYSTEM_SCALE.load(Ordering::Relaxed)),
    }
}

/// Publish the host application's motion preferences.
///
/// Call once at startup and again whenever the OS setting changes.
pub fn set_system_prefs(prefs: MotionPrefs) {
    SYSTEM_REDUCE.store(prefs.reduce_motion, Ordering::Relaxed);
    SYSTEM_SCALE.store(prefs.clamped_scale().to_bits(), Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// The clock
// ---------------------------------------------------------------------------

/// One frame's worth of time, plus the preferences that govern it.
///
/// This is the virtual clock D25 requires. Nothing in this module reads a
/// wall clock; everything reads a `Motion` it was handed. Construct it from an
/// [`egui::Context`] in the app, or from an explicit instant in a test or in
/// the headless screenshot harness.
///
/// ```
/// use scrozz_ui::motion::{Duration, Motion};
///
/// // A named instant, reproducible forever.
/// let m = Motion::at_ms(180);
/// assert_eq!(m.now(), 0.180);
///
/// // The reduce-motion choke point.
/// assert_eq!(m.resolve(Duration::SLOW), 0.320);
/// assert_eq!(m.with_reduce_motion(true).resolve(Duration::SLOW), 0.0);
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Motion {
    now: f64,
    dt: f32,
    prefs: MotionPrefs,
}

/// The frame delta assumed by [`Motion::at`] and friends: 60 fps.
///
/// The harness renders at a nominal 60 fps so a named instant in milliseconds
/// maps onto a whole frame index.
pub const NOMINAL_FRAME: f32 = 1.0 / 60.0;

/// Frame deltas are clamped into this range so a stalled frame — a debugger
/// breakpoint, a window drag — cannot launch a spring into orbit.
pub const DT_RANGE: (f32, f32) = (1.0 / 480.0, 1.0 / 20.0);

impl Motion {
    /// A clock frozen at `now` seconds, assuming a 60 fps frame.
    #[must_use]
    pub fn at(now: f64) -> Self {
        Self {
            now,
            dt: NOMINAL_FRAME,
            prefs: MotionPrefs::default(),
        }
    }

    /// A clock frozen at `ms` milliseconds. The harness's unit of choice.
    #[must_use]
    pub fn at_ms(ms: u64) -> Self {
        Self::at(ms as f64 / 1000.0)
    }

    /// A clock frozen at frame `index` of a `fps`-rate render.
    ///
    /// Both `now` and `dt` follow from the frame rate, so stepping frames and
    /// integrating springs agree with each other.
    #[must_use]
    pub fn at_frame(index: u64, fps: f32) -> Self {
        let fps = if fps > 0.0 && fps.is_finite() {
            fps
        } else {
            60.0
        };
        // The instant is computed in f64 so that frame 1800 of a 60 fps
        // sequence is exactly 30 s, not 30.000004 s. A golden image named
        // "t = 500 ms" must land on 500 ms however the harness got there.
        Self {
            now: index as f64 / f64::from(fps),
            dt: 1.0 / fps,
            prefs: MotionPrefs::default(),
        }
    }

    /// This frame's clock, read from a live egui context.
    ///
    /// Picks up the preferences published via [`set_system_prefs`].
    #[must_use]
    pub fn from_context(ctx: &Context) -> Self {
        let (now, dt) = ctx.input(|i| (i.time, i.stable_dt));
        Self {
            now,
            dt: clamp_dt(dt),
            prefs: system_prefs(),
        }
    }

    /// The next frame, `dt` later. Lets a harness walk a timeline.
    #[must_use]
    pub fn stepped(self) -> Self {
        self.advanced_by(self.dt)
    }

    /// This clock advanced by `secs`, keeping `dt` and preferences.
    #[must_use]
    pub fn advanced_by(self, secs: f32) -> Self {
        Self {
            now: self.now + f64::from(secs),
            ..self
        }
    }

    /// This clock with a different frame delta.
    #[must_use]
    pub fn with_dt(self, dt: f32) -> Self {
        Self {
            dt: clamp_dt(dt),
            ..self
        }
    }

    /// This clock with different preferences.
    #[must_use]
    pub fn with_prefs(self, prefs: MotionPrefs) -> Self {
        Self { prefs, ..self }
    }

    /// This clock with reduce-motion forced on or off.
    #[must_use]
    pub fn with_reduce_motion(self, on: bool) -> Self {
        Self {
            prefs: MotionPrefs {
                reduce_motion: on,
                ..self.prefs
            },
            ..self
        }
    }

    /// Seconds since the context started. The absolute instant being rendered.
    #[must_use]
    pub fn now(&self) -> f64 {
        self.now
    }

    /// The real frame delta, clamped to [`DT_RANGE`].
    ///
    /// This is wall-clock elapsed time. Integrators want
    /// [`Motion::integration_dt`], which also applies the time scale.
    #[must_use]
    pub fn dt(&self) -> f32 {
        self.dt
    }

    /// The frame delta a physics integrator should advance by.
    ///
    /// Divided by the time scale, so `time_scale = 2.0` makes springs settle in
    /// twice the wall-clock time, exactly as it doubles every duration token.
    #[must_use]
    pub fn integration_dt(&self) -> f32 {
        self.dt / self.prefs.clamped_scale()
    }

    /// The preferences this clock carries.
    #[must_use]
    pub fn prefs(&self) -> MotionPrefs {
        self.prefs
    }

    /// Whether reduce-motion is in effect.
    #[must_use]
    pub fn is_reduced(&self) -> bool {
        self.prefs.reduce_motion
    }

    /// **The reduce-motion choke point (D13).**
    ///
    /// Every duration in the app resolves through here. Under reduce-motion it
    /// returns exactly `0.0`, which makes every timeline finish on its first
    /// frame and every stagger land as one — the end state is still reached,
    /// just instantly.
    #[must_use]
    pub fn resolve(&self, token: Duration) -> f32 {
        if self.prefs.reduce_motion {
            0.0
        } else {
            (token.designed_secs() * self.prefs.clamped_scale()).max(0.0)
        }
    }

    /// Seconds elapsed since an absolute instant. Never negative.
    #[must_use]
    pub fn since(&self, start: f64) -> f32 {
        ((self.now - start).max(0.0)) as f32
    }

    /// Linear `0.0..=1.0` progress of a `token`-long span that began at `start`.
    ///
    /// Returns `1.0` immediately under reduce-motion.
    #[must_use]
    pub fn progress(&self, start: f64, token: Duration) -> f32 {
        let span = self.resolve(token);
        if span <= 0.0 {
            1.0
        } else {
            (self.since(start) / span).clamp(0.0, 1.0)
        }
    }

    /// [`Motion::progress`] shaped by an easing curve.
    #[must_use]
    pub fn eased(&self, start: f64, token: Duration, ease: Ease) -> f32 {
        ease.apply(self.progress(start, token))
    }
}

fn clamp_dt(dt: f32) -> f32 {
    let (lo, hi) = DT_RANGE;
    if dt.is_finite() { dt.clamp(lo, hi) } else { lo }
}

// ---------------------------------------------------------------------------
// Easing
// ---------------------------------------------------------------------------

/// A named easing curve.
///
/// An enum rather than a function pointer so curves are comparable, printable,
/// enumerable ([`Ease::ALL`]) and usable as design tokens. Every curve is
/// anchored: `apply(0.0)` is exactly `0.0` and `apply(1.0)` is exactly `1.0`.
/// Two of them deliberately leave the unit interval in between.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ease {
    /// No shaping. For values whose target keeps moving, where a curve would
    /// only fight the retarget.
    Linear,

    /// The workhorse: fast out of the gate, gentle arrival. Anything responding
    /// to the user.
    OutCubic,

    /// Symmetric — for things that move on their own, with no user at either
    /// end.
    InOutCubic,

    /// A sharper arrival than cubic. Large travel that must not feel slow.
    OutQuint,

    /// A critically damped second-order spring: fastest approach with **no**
    /// overshoot.
    Spring,

    /// A slightly underdamped spring — overshoots and settles back. This is the
    /// curve that makes a card entry read as physical rather than merely eased.
    SpringOvershoot,

    /// Classic "back" overshoot: snappier and more mechanical than a spring.
    OutBack,
}

impl Ease {
    /// Every curve, in design-system order.
    pub const ALL: &'static [Self] = &[
        Self::Linear,
        Self::OutCubic,
        Self::InOutCubic,
        Self::OutQuint,
        Self::Spring,
        Self::SpringOvershoot,
        Self::OutBack,
    ];

    /// Shape a normalised time. Input is clamped to `0.0..=1.0`; output may
    /// exceed it for the overshooting curves.
    #[must_use]
    pub fn apply(self, t: f32) -> f32 {
        let t = if t.is_finite() {
            t.clamp(0.0, 1.0)
        } else {
            0.0
        };
        match self {
            Self::Linear => t,
            Self::OutCubic => {
                let u = 1.0 - t;
                1.0 - u * u * u
            }
            Self::InOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    let u = -2.0 * t + 2.0;
                    1.0 - u * u * u / 2.0
                }
            }
            Self::OutQuint => {
                let u = 1.0 - t;
                1.0 - u * u * u * u * u
            }
            Self::Spring => {
                // 1 - (1 + wt)e^{-wt}, normalised so f(1) is exactly 1.
                const W: f32 = 9.0;
                fn raw(t: f32) -> f32 {
                    1.0 - (1.0 + W * t) * (-W * t).exp()
                }
                raw(t) / raw(1.0)
            }
            Self::SpringOvershoot => {
                const Z: f32 = 0.34; // damping ratio
                const W: f32 = 13.5; // natural frequency
                fn raw(t: f32) -> f32 {
                    let wd = W * (1.0 - Z * Z).sqrt();
                    1.0 - (-Z * W * t).exp() * ((wd * t).cos() + (Z * W / wd) * (wd * t).sin())
                }
                raw(t) / raw(1.0)
            }
            Self::OutBack => {
                const C1: f32 = 1.701_58;
                const C3: f32 = C1 + 1.0;
                let u = t - 1.0;
                1.0 + C3 * u * u * u + C1 * u * u
            }
        }
    }

    /// Whether this curve is allowed to leave `0.0..=1.0` mid-flight.
    ///
    /// Call sites that interpolate a size or an alpha must clamp when this is
    /// true — a negative width is a panic and an alpha above 255 wraps.
    #[must_use]
    pub fn overshoots(self) -> bool {
        matches!(self, Self::SpringOvershoot | Self::OutBack)
    }

    /// A stable identifier, for tuning UI and test failure messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::OutCubic => "out-cubic",
            Self::InOutCubic => "in-out-cubic",
            Self::OutQuint => "out-quint",
            Self::Spring => "spring",
            Self::SpringOvershoot => "spring-overshoot",
            Self::OutBack => "out-back",
        }
    }
}

// ---------------------------------------------------------------------------
// Timelines
// ---------------------------------------------------------------------------

/// A one-shot animation pinned to an absolute start instant.
///
/// The duration is stored as a *token*, not as seconds, so reduce-motion still
/// applies to a timeline that was started before the setting changed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timeline {
    start: f64,
    token: Duration,
    ease: Ease,
}

impl Timeline {
    /// A timeline beginning at absolute instant `start`.
    #[must_use]
    pub fn starting_at(start: f64, token: Duration, ease: Ease) -> Self {
        Self { start, token, ease }
    }

    /// A timeline beginning now.
    #[must_use]
    pub fn starting(m: &Motion, token: Duration, ease: Ease) -> Self {
        Self::starting_at(m.now(), token, ease)
    }

    /// The instant this timeline began.
    #[must_use]
    pub fn start(&self) -> f64 {
        self.start
    }

    /// Unshaped `0.0..=1.0` progress at `m`.
    #[must_use]
    pub fn progress(&self, m: &Motion) -> f32 {
        m.progress(self.start, self.token)
    }

    /// Eased value at `m`. May exceed `1.0` for an overshooting curve.
    #[must_use]
    pub fn value(&self, m: &Motion) -> f32 {
        self.ease.apply(self.progress(m))
    }

    /// Whether the timeline is still in flight at `m`.
    #[must_use]
    pub fn is_active(&self, m: &Motion) -> bool {
        self.progress(m) < 1.0
    }

    /// Seconds remaining at `m`, or `0.0` once finished.
    #[must_use]
    pub fn remaining(&self, m: &Motion) -> f32 {
        (m.resolve(self.token) - m.since(self.start)).max(0.0)
    }

    /// What this timeline contributes to the frame schedule.
    #[must_use]
    pub fn activity(&self, m: &Motion) -> Activity {
        Activity::when_animating(self.is_active(m))
    }
}

// ---------------------------------------------------------------------------
// Reversible boolean animation
// ---------------------------------------------------------------------------

/// A `0.0..=1.0` value that eases toward a boolean and reverses for free.
///
/// The stored progress is *linear*; the curve is applied on read. That is what
/// makes an interrupted reveal retract as the mirror of itself rather than
/// snapping — the single biggest ergonomic win immediate mode hands you, since
/// there is no animation object to cancel.
///
/// Advancing is driven by the [`Motion`]'s absolute clock, not by its `dt`, so
/// calling [`AnimBool::advance`] twice in one frame is a no-op the second time
/// and replaying a virtual clock is exact.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AnimBool {
    raw: f32,
    last: Option<f64>,
}

impl AnimBool {
    /// A value resting at `on`.
    ///
    /// Note the deliberate product behaviour, which the spike found by
    /// surprise: an [`AnimBool`] that has never been advanced **snaps** to its
    /// target on the first frame. A widget that appears already-hovered shows
    /// its chrome immediately instead of fading in from nothing. Tests that
    /// want to observe motion must therefore prime the value in its resting
    /// state for one frame first, or they pass trivially with zero frames of
    /// movement.
    #[must_use]
    pub fn new(on: bool) -> Self {
        Self {
            raw: if on { 1.0 } else { 0.0 },
            last: None,
        }
    }

    /// A value starting at an explicit progress, already primed.
    #[must_use]
    pub fn starting_at(raw: f32, m: &Motion) -> Self {
        Self {
            raw: raw.clamp(0.0, 1.0),
            last: Some(m.now()),
        }
    }

    /// Advance toward `on` and return the new *linear* progress.
    pub fn advance(&mut self, on: bool, token: Duration, m: &Motion) -> f32 {
        let target = if on { 1.0 } else { 0.0 };
        let Some(previous) = self.last else {
            // First sight of this value: snap. See [`AnimBool::new`].
            self.raw = target;
            self.last = Some(m.now());
            return self.raw;
        };
        let dt = (m.now() - previous).max(0.0) as f32;
        self.last = Some(m.now());

        let span = m.resolve(token);
        if span <= 0.0 || dt >= span {
            self.raw = target;
        } else {
            let delta = dt / span;
            self.raw = if on {
                (self.raw + delta).min(1.0)
            } else {
                (self.raw - delta).max(0.0)
            };
        }
        self.raw
    }

    /// The unshaped progress.
    #[must_use]
    pub fn raw(&self) -> f32 {
        self.raw
    }

    /// The progress shaped by a curve.
    #[must_use]
    pub fn value(&self, ease: Ease) -> f32 {
        ease.apply(self.raw)
    }

    /// Whether the value is strictly between its two rest states.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.raw > 0.0 && self.raw < 1.0
    }

    /// What this value contributes to the frame schedule.
    #[must_use]
    pub fn activity(&self) -> Activity {
        Activity::when_animating(self.is_active())
    }
}

/// Animate a boolean stored against an [`egui::Id`], returning the eased value.
///
/// Unlike egui's own `animate_bool_*` helpers this advances off the [`Motion`]
/// you pass rather than egui's internal clock, which is what keeps it drivable
/// from a virtual clock (D25). Like them, it **self-schedules**: it requests a
/// repaint while the value is in flight, so a call site cannot silently freeze.
///
/// Springs do not self-schedule — they return an [`Activity`] the caller must
/// honour. That split is the whole immediate-mode animation contract.
pub fn anim_bool(ctx: &Context, id: Id, on: bool, token: Duration, ease: Ease, m: &Motion) -> f32 {
    let mut state = ctx.data_mut(|d| d.get_temp::<AnimBool>(id).unwrap_or(AnimBool::new(on)));
    state.advance(on, token, m);
    ctx.data_mut(|d| d.insert_temp(id, state));
    state.activity().apply(ctx);
    state.value(ease)
}

// ---------------------------------------------------------------------------
// Stagger
// ---------------------------------------------------------------------------

/// A cascade across `count` siblings: each runs for `each`, offset by `step`.
///
/// A stagger is arithmetic, not orchestration — one master timeline plus an
/// index offset. There are no keyframe objects to build or tear down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stagger {
    elapsed: f32,
    each: f32,
    step: f32,
    count: usize,
    ease: Ease,
}

impl Stagger {
    /// A cascade that began at absolute instant `start`.
    #[must_use]
    pub fn since(
        m: &Motion,
        start: f64,
        count: usize,
        each: Duration,
        step: Duration,
        ease: Ease,
    ) -> Self {
        Self {
            elapsed: m.since(start),
            each: m.resolve(each),
            step: m.resolve(step),
            count,
            ease,
        }
    }

    /// A cascade driven by an external `0.0..=1.0` master progress.
    ///
    /// Use this when the group must be able to *reverse*: drive the master with
    /// an [`AnimBool`] and the cascade un-plays from the far end for free.
    #[must_use]
    pub fn from_master(
        m: &Motion,
        master: f32,
        count: usize,
        each: Duration,
        step: Duration,
        ease: Ease,
    ) -> Self {
        let each = m.resolve(each);
        let step = m.resolve(step);
        let total = each + step * count.saturating_sub(1) as f32;
        Self {
            elapsed: master.clamp(0.0, 1.0) * total,
            each,
            step,
            count,
            ease,
        }
    }

    /// Eased progress for sibling `i`.
    #[must_use]
    pub fn at(&self, i: usize) -> f32 {
        if self.each <= 0.0 {
            // Reduce-motion, or a zero-length token: the whole group is done.
            return 1.0;
        }
        let t = ((self.elapsed - self.step * i as f32) / self.each).clamp(0.0, 1.0);
        self.ease.apply(t)
    }

    /// The standard "fade in while rising" reveal for sibling `i`: an alpha in
    /// `0.0..=1.0` and a draw-only offset that shrinks to zero.
    ///
    /// The offset must never move a hit target, only the paint — otherwise a
    /// control's clickable area slides out from under the pointer mid-reveal.
    #[must_use]
    pub fn rise(&self, i: usize, distance: f32) -> (f32, f32) {
        let t = self.at(i);
        (t.clamp(0.0, 1.0), (1.0 - t) * distance)
    }

    /// Total length of the cascade in seconds.
    #[must_use]
    pub fn total(&self) -> f32 {
        self.each + self.step * self.count.saturating_sub(1) as f32
    }

    /// Whether any sibling is still moving.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.each > 0.0 && self.elapsed < self.total()
    }

    /// What this cascade contributes to the frame schedule.
    #[must_use]
    pub fn activity(&self) -> Activity {
        Activity::when_animating(self.is_active())
    }
}

// ---------------------------------------------------------------------------
// Springs
// ---------------------------------------------------------------------------

/// Stiffness and damping for a unit-mass second-order spring.
///
/// These are the numbers that decide how a *gesture* feels. Durations and
/// easing barely matter once an object is being thrown around; stiffness,
/// damping and friction are everything. Expect to re-tune them by hand — they
/// are reasoned, not felt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringParams {
    /// Restoring force per unit of displacement.
    pub stiffness: f32,
    /// Velocity-proportional drag.
    pub damping: f32,
}

impl SpringParams {
    /// Snappy: a card settling into its slot, a depth reshuffle.
    pub const SNAP: Self = Self {
        stiffness: 320.0,
        damping: 30.0,
    };

    /// Tight: pointer-follow, where any visible lag reads as the object
    /// fighting the user.
    pub const DRAG: Self = Self {
        stiffness: 900.0,
        damping: 48.0,
    };

    /// Soft: the deck settling back after a card leaves.
    pub const SOFT: Self = Self {
        stiffness: 190.0,
        damping: 22.0,
    };

    /// A card arriving at, or springing back to, its home slot. Slightly
    /// underdamped on purpose so the arrival overshoots and settles.
    pub const SETTLE: Self = Self {
        stiffness: 210.0,
        damping: 22.0,
    };

    /// The cards *underneath* reflowing. Softer than [`Self::SETTLE`] so the
    /// deck ripples instead of moving as one rigid body.
    pub const DECK: Self = Self {
        stiffness: 150.0,
        damping: 19.0,
    };

    /// Custom parameters.
    #[must_use]
    pub const fn new(stiffness: f32, damping: f32) -> Self {
        Self { stiffness, damping }
    }

    /// The damping ratio ζ, for unit mass: `c / (2·√k)`.
    ///
    /// Below `1.0` the spring overshoots and rings; at `1.0` it is critically
    /// damped; above it crawls in without overshoot. Anything at or above about
    /// `0.2` settles quickly enough not to read as a wobble.
    #[must_use]
    pub fn damping_ratio(&self) -> f32 {
        if self.stiffness <= 0.0 {
            f32::INFINITY
        } else {
            self.damping / (2.0 * self.stiffness.sqrt())
        }
    }

    /// Whether this spring will overshoot its target at least once.
    #[must_use]
    pub fn overshoots(&self) -> bool {
        self.damping_ratio() < 1.0
    }
}

/// Fixed integration sub-step, in seconds.
///
/// Springs sub-step at this rate regardless of the frame delta, so the same
/// elapsed time always produces the same result. That fixed step is what makes
/// a stateful integrator replayable from a virtual clock.
pub const INTEGRATION_STEP: f32 = 1.0 / 240.0;

/// Upper bound on sub-steps per advance, so a pathological elapsed time cannot
/// stall a frame.
const MAX_SUBSTEPS: f32 = 32.0;

/// Displacement below which a scalar spring is considered arrived.
const SETTLE_POS_EPS: f32 = 0.0015;
/// Speed below which a scalar spring is considered stopped.
const SETTLE_VEL_EPS: f32 = 0.02;

/// A scalar second-order spring.
///
/// [`Spring1::step`] returns whether the spring is still moving, and the caller
/// **must** honour it: unlike [`anim_bool`], springs do not schedule their own
/// repaints, so ignoring the result is exactly how an animation silently
/// freezes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spring1 {
    /// Current value.
    pub pos: f32,
    /// Current rate of change, per second.
    pub vel: f32,
}

impl Spring1 {
    /// A spring at rest at `pos`.
    #[must_use]
    pub fn at(pos: f32) -> Self {
        Self { pos, vel: 0.0 }
    }

    /// Integrate one frame toward `target`, honouring reduce-motion.
    ///
    /// Returns the [`Activity`] this spring contributes to the frame. Under
    /// reduce-motion (D13) it arrives immediately and reports [`Activity::IDLE`].
    #[must_use = "a moving spring needs a repaint: merge this into the frame's Activity"]
    pub fn step(&mut self, target: f32, params: SpringParams, m: &Motion) -> Activity {
        if m.is_reduced() {
            self.snap_to(target);
            return Activity::IDLE;
        }
        self.advance(target, params, m.integration_dt())
    }

    /// Integrate `elapsed` seconds toward `target`, ignoring preferences.
    ///
    /// The deterministic core: fixed sub-steps mean replaying the same elapsed
    /// time from the same state always lands on the same number. The harness
    /// uses this to reproduce "spring state at t = 180 ms" exactly.
    ///
    /// Returns the [`Activity`] this spring contributes to the frame.
    #[must_use = "a moving spring needs a repaint: merge this into the frame's Activity"]
    pub fn advance(&mut self, target: f32, params: SpringParams, elapsed: f32) -> Activity {
        let (steps, h) = substeps(elapsed);
        for _ in 0..steps {
            let accel = (target - self.pos) * params.stiffness - self.vel * params.damping;
            self.vel += accel * h;
            self.pos += self.vel * h;
        }
        if !self.pos.is_finite() || !self.vel.is_finite() {
            // Nothing sane can follow a non-finite state, and letting it
            // propagate turns one bad frame into a permanently broken widget.
            self.snap_to(target);
            return Activity::IDLE;
        }
        if (target - self.pos).abs() < SETTLE_POS_EPS && self.vel.abs() < SETTLE_VEL_EPS {
            self.snap_to(target);
            Activity::IDLE
        } else {
            Activity::animating()
        }
    }

    /// Whether the spring has arrived at `target` and stopped.
    #[must_use]
    pub fn is_settled(&self, target: f32) -> bool {
        (target - self.pos).abs() < SETTLE_POS_EPS && self.vel.abs() < SETTLE_VEL_EPS
    }

    /// Place the spring at `pos` with no velocity.
    pub fn snap_to(&mut self, pos: f32) {
        self.pos = pos;
        self.vel = 0.0;
    }
}

/// Displacement below which a 2-D spring is considered arrived, in points.
const SETTLE_POS_EPS_2D: f32 = 0.06;
/// Speed below which a 2-D spring is considered stopped, in points per second.
const SETTLE_VEL_EPS_2D: f32 = 0.6;

/// A two-dimensional second-order spring — a card's position.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spring2 {
    /// Current position.
    pub pos: Vec2,
    /// Current velocity, per second.
    pub vel: Vec2,
}

impl Spring2 {
    /// A spring at rest at `pos`.
    #[must_use]
    pub fn at(pos: Vec2) -> Self {
        Self {
            pos,
            vel: Vec2::ZERO,
        }
    }

    /// Integrate one frame toward `target`, honouring reduce-motion.
    ///
    /// Returns the [`Activity`] this spring contributes to the frame. Under
    /// reduce-motion (D13) it arrives immediately and reports [`Activity::IDLE`].
    #[must_use = "a moving spring needs a repaint: merge this into the frame's Activity"]
    pub fn step(&mut self, target: Vec2, params: SpringParams, m: &Motion) -> Activity {
        if m.is_reduced() {
            self.snap_to(target);
            return Activity::IDLE;
        }
        self.advance(target, params, m.integration_dt())
    }

    /// Integrate `elapsed` seconds toward `target`, ignoring preferences.
    ///
    /// Returns the [`Activity`] this spring contributes. See [`Spring1::advance`].
    #[must_use = "a moving spring needs a repaint: merge this into the frame's Activity"]
    pub fn advance(&mut self, target: Vec2, params: SpringParams, elapsed: f32) -> Activity {
        let (steps, h) = substeps(elapsed);
        for _ in 0..steps {
            let accel = (target - self.pos) * params.stiffness - self.vel * params.damping;
            self.vel += accel * h;
            self.pos += self.vel * h;
        }
        if !self.pos.is_finite() || !self.vel.is_finite() {
            self.snap_to(target);
            return Activity::IDLE;
        }
        if (target - self.pos).length() < SETTLE_POS_EPS_2D && self.vel.length() < SETTLE_VEL_EPS_2D
        {
            self.snap_to(target);
            Activity::IDLE
        } else {
            Activity::animating()
        }
    }

    /// Whether the spring has arrived at `target` and stopped.
    #[must_use]
    pub fn is_settled(&self, target: Vec2) -> bool {
        (target - self.pos).length() < SETTLE_POS_EPS_2D && self.vel.length() < SETTLE_VEL_EPS_2D
    }

    /// Ballistic integration — no target, just momentum under drag.
    ///
    /// This is a fling: the card is no longer seeking anything, it is simply
    /// leaving. `drag_per_sec` is the fraction of velocity shed each second.
    pub fn coast(&mut self, elapsed: f32, drag_per_sec: f32, gravity: Vec2) {
        let (steps, h) = substeps(elapsed);
        for _ in 0..steps {
            self.vel += gravity * h;
            self.vel *= (1.0 - drag_per_sec * h).clamp(0.0, 1.0);
            self.pos += self.vel * h;
        }
        if !self.pos.is_finite() || !self.vel.is_finite() {
            self.vel = Vec2::ZERO;
        }
    }

    /// Place the spring at `pos` with no velocity.
    pub fn snap_to(&mut self, pos: Vec2) {
        self.pos = pos;
        self.vel = Vec2::ZERO;
    }
}

/// Split `elapsed` into fixed integration sub-steps.
///
/// Two properties matter here. First, the sub-step size never exceeds
/// [`INTEGRATION_STEP`], because an explicit Euler spring goes unstable when
/// `h` approaches `2/sqrt(stiffness)` and simply explodes to infinity — a
/// 5-second frame gap would otherwise fling a card to `-8e32`.
///
/// Second, time beyond [`MAX_SUBSTEPS`] steps is *discarded* rather than
/// simulated. A gap that long means the process was suspended — the laptop lid
/// closed, a debugger stopped the world. Faithfully replaying those seconds
/// would teleport every object to its target anyway, so it is both cheaper and
/// less surprising to advance the physics by one frame's worth and move on.
fn substeps(elapsed: f32) -> (u32, f32) {
    if !elapsed.is_finite() || elapsed <= 0.0 {
        return (0, 0.0);
    }
    let budget = MAX_SUBSTEPS * INTEGRATION_STEP;
    let elapsed = elapsed.min(budget);
    let steps = (elapsed / INTEGRATION_STEP).ceil().clamp(1.0, MAX_SUBSTEPS);
    (steps as u32, (elapsed / steps).min(INTEGRATION_STEP))
}

/// How much velocity a flung card sheds per second of coasting. Higher stops
/// it sooner.
pub const FLING_DRAG_PER_SEC: f32 = 1.1;

/// Release speed, in points per second, that counts as a throw rather than a
/// release.
pub const DISMISS_VELOCITY: f32 = 420.0;

/// Displacement, in points, that dismisses on release even with no speed.
pub const DISMISS_DISTANCE: f32 = 110.0;

// ---------------------------------------------------------------------------
// Pointer velocity
// ---------------------------------------------------------------------------

/// How far back to difference when estimating throw speed.
pub const VELOCITY_WINDOW: f32 = 0.080;

/// A rolling window of pointer samples, used to carry a *real* throw velocity
/// into a fling.
///
/// egui exposes `PointerState::velocity()`, but it is smoothed for kinetic
/// scrolling and keeps reporting motion for a while after the pointer stops.
/// That turns "drag slowly, stop, release" — a gesture the user performs
/// meaning *put it back* — into an unwanted throw, and the result feels
/// haunted. Differencing our own samples over the last ~80 ms makes a dead stop
/// read as an actual zero.
///
/// Samples carry absolute timestamps, so this is drivable from a virtual clock
/// like everything else here.
#[derive(Clone, Debug, Default)]
pub struct VelocityTracker {
    samples: Vec<(f64, Vec2)>,
}

impl VelocityTracker {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget every sample. Call when a gesture begins.
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Record the pointer's cumulative offset at an absolute instant.
    pub fn sample(&mut self, now: f64, pos: Vec2) {
        self.samples.push((now, pos));
        let cutoff = now - f64::from(VELOCITY_WINDOW) * 2.0;
        self.samples.retain(|(t, _)| *t >= cutoff);
    }

    /// Record the pointer's cumulative offset at this frame's instant.
    pub fn sample_at(&mut self, m: &Motion, pos: Vec2) {
        self.sample(m.now(), pos);
    }

    /// Average velocity in points per second over the trailing window.
    ///
    /// Zero if the pointer has been still, which is the behaviour that matters.
    #[must_use]
    pub fn velocity(&self) -> Vec2 {
        let Some(&(t_end, p_end)) = self.samples.last() else {
            return Vec2::ZERO;
        };
        let target = t_end - f64::from(VELOCITY_WINDOW);
        // The oldest sample still inside the window, so a long still-hold
        // differences against a recent identical position and yields zero.
        let base = self
            .samples
            .iter()
            .find(|(t, _)| *t >= target)
            .copied()
            .unwrap_or((t_end, p_end));
        let dt = (t_end - base.0) as f32;
        if dt < 1e-4 {
            Vec2::ZERO
        } else {
            (p_end - base.1) / dt
        }
    }

    /// How many samples are currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether no samples are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Repaint scheduling
// ---------------------------------------------------------------------------

/// What a surface needs from the frame scheduler.
///
/// Surfaces return one of these, callers OR them together, and the app applies
/// the merged result exactly once. Two states, deliberately distinguished:
///
/// * **animating** — a value is changing this frame, so repaint immediately;
/// * **waiting** — nothing is moving but something expires later, so *sleep*
///   until then.
///
/// Conflating them is a real bug, not a nicety: a 1.25 s toast that reports
/// itself as animating burns ~75 identical frames, and does so even with
/// reduce-motion on.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Activity {
    animating: bool,
    wake_after: Option<f32>,
}

impl Activity {
    /// Nothing is happening; the app may go to sleep.
    pub const IDLE: Self = Self {
        animating: false,
        wake_after: None,
    };

    /// Something is moving; repaint next frame.
    #[must_use]
    pub fn animating() -> Self {
        Self {
            animating: true,
            wake_after: None,
        }
    }

    /// [`Activity::animating`] when `active`, otherwise [`Activity::IDLE`].
    #[must_use]
    pub fn when_animating(active: bool) -> Self {
        if active {
            Self::animating()
        } else {
            Self::IDLE
        }
    }

    /// Nothing is moving, but wake up in `secs` — a dwell, a timeout, a toast.
    #[must_use]
    pub fn waiting(secs: f32) -> Self {
        Self {
            animating: false,
            wake_after: Some(secs.max(0.0)),
        }
    }

    /// Combine two requirements, taking the stronger of each.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            animating: self.animating || other.animating,
            wake_after: match (self.wake_after, other.wake_after) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        }
    }

    /// Whether anything is moving.
    #[must_use]
    pub fn is_animating(&self) -> bool {
        self.animating
    }

    /// Whether the app may sleep indefinitely.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        !self.animating && self.wake_after.is_none()
    }

    /// The pending wake deadline, if any.
    #[must_use]
    pub fn wake_after(&self) -> Option<f32> {
        self.wake_after
    }

    /// Ask egui for the frame this activity requires — and only that frame.
    pub fn apply(self, ctx: &Context) {
        if self.animating {
            ctx.request_repaint();
        } else if let Some(secs) = self.wake_after {
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(secs.max(0.0)));
        }
    }
}

impl std::ops::BitOr for Activity {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        self.merge(rhs)
    }
}

impl std::ops::BitOrAssign for Activity {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.merge(rhs);
    }
}

impl std::iter::Sum for Activity {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::IDLE, Self::merge)
    }
}

// ---------------------------------------------------------------------------
// Interpolation helpers
// ---------------------------------------------------------------------------

/// Linear interpolation. `t` is not clamped, so an overshooting curve still
/// overshoots.
#[must_use]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Fade a colour by an animation progress value.
///
/// `t` is clamped, because an overshooting curve can legitimately exceed `1.0`
/// and an alpha cannot.
#[must_use]
pub fn fade(color: Color32, t: f32) -> Color32 {
    color.linear_multiply(t.clamp(0.0, 1.0))
}

/// Scale a `0..=255` alpha by an animation progress value.
#[must_use]
pub fn alpha(base: u8, t: f32) -> u8 {
    (f32::from(base) * t.clamp(0.0, 1.0)).round() as u8
}

/// Interpolate between two colours, alpha included.
///
/// Interpolating alpha matters: `Color32::from_rgb` forces alpha to 255, which
/// silently turns every muted tint opaque part-way through a fade. `Color32` is
/// premultiplied, so a component-wise blend is the correct one.
#[must_use]
pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        mix(a.r(), b.r()),
        mix(a.g(), b.g()),
        mix(a.b(), b.b()),
        mix(a.a(), b.a()),
    )
}
