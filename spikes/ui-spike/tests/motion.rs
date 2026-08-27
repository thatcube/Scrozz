//! Headless frame-stepping tests for the motion layer.
//!
//! I cannot see the screen, so "it animates" has to be proven mechanically.
//! These tests drive a real `egui::Context` over *simulated* time — feeding a
//! synthetic `RawInput` with an advancing clock, one 60 Hz frame at a time —
//! and assert on the numbers the drawing code would have used.
//!
//! They exist to catch the two failure modes that are invisible from the
//! outside and that the previous spike would have shipped blind:
//!
//! 1. **The value never moves.** An easing bug, a `dur()` returning 0, a
//!    mis-keyed `Id` — the frame renders fine, it just renders the *same*
//!    frame forever.
//! 2. **`request_repaint` was forgotten.** In immediate mode egui only paints
//!    on input. An animation whose value is perfect will still look frozen if
//!    nothing schedules the next frame. `repaint_delay` is the observable:
//!    ~0 means "paint again immediately", `Duration::MAX` means "sleep".
//!
//! We also assert the *inverse* of #2 — that everything eventually goes idle —
//! because "animate by repainting forever" is a real way to fake this test
//! while pinning a CPU core, and pinning a core would undermine the native
//! performance claim that justified picking egui in the first place.

#[path = "../src/motion.rs"]
mod motion;

use egui::{Context, Id, RawInput};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

const FRAME: f64 = 1.0 / 60.0;

/// The motion layer's scale / reduce / curve-override switches are **process
/// global** on purpose: any drawing code, however deep, can honour them without
/// threading a config struct through every call. The price is that `cargo test`
/// runs tests in parallel threads *in one process*, so two tests fighting over
/// `set_reduce` will corrupt each other. This lock serialises the ones that
/// touch globals, so a plain `cargo test` is green with no `--test-threads=1`.
///
/// Worth writing down as a real cost of the global-state design, not a test bug.
static GLOBALS: Mutex<()> = Mutex::new(());

struct Globals(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Globals {
    /// Take the lock and reset every global to its default.
    fn lock() -> Self {
        let g = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        motion::set_scale(1.0);
        motion::set_reduce(false);
        motion::set_curve_override(0);
        Self(g)
    }
}

impl Drop for Globals {
    fn drop(&mut self) {
        // Leave the world clean even if the test panicked.
        motion::set_scale(1.0);
        motion::set_reduce(false);
        motion::set_curve_override(0);
    }
}

/// A tiny deterministic driver: owns a `Context` and a clock, and runs one
/// frame of an arbitrary closure per `step()`.
struct Driver {
    ctx: Context,
    time: f64,
    /// Repaint delay the *last* frame asked for.
    delay: Duration,
}

impl Driver {
    fn new() -> Self {
        let ctx = Context::default();
        // Nothing here should depend on fonts or a display.
        Self { ctx, time: 0.0, delay: Duration::MAX }
    }

    /// Run one simulated frame. Returns whatever the closure returns.
    fn step<R>(&mut self, mut f: impl FnMut(&Context) -> R) -> R {
        self.time += FRAME;
        let input = RawInput {
            time: Some(self.time),
            predicted_dt: FRAME as f32,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut out = None;
        let mut full = self.ctx.run_ui(input, |ui| {
            out = Some(f(ui.ctx()));
        });
        self.delay = full
            .viewport_output
            .values()
            .map(|v| v.repaint_delay)
            .min()
            .unwrap_or(Duration::MAX);
        // epaint panics on drop if a texture delta was produced and never
        // handed to a renderer. There is no renderer here, so discard it
        // explicitly — this is a required step for *any* headless egui driver
        // and is not obvious from the API.
        full.textures_delta.clear();
        out.unwrap()
    }

    /// True if the last frame asked to be painted again promptly.
    fn wants_repaint(&self) -> bool {
        self.delay < Duration::from_millis(50)
    }

    /// egui deliberately *snaps* the first frame it ever sees an `Id`
    /// (`AnimationManager::animate_bool`: `None => { last_value: end }`), so a
    /// widget that appears already-hovered doesn't wipe in from nothing. That's
    /// the right product behaviour, but it means a test has to introduce the id
    /// in its resting state for one frame before flipping it.
    fn prime(&mut self, id: Id, token: f32, ease: motion::Ease) {
        self.step(|ctx| motion::anim(ctx, id, false, token, ease));
    }
}

// ---------------------------------------------------------------------------
// 1. The value actually moves, and settles exactly on target.
// ---------------------------------------------------------------------------

#[test]
fn animation_actually_moves_and_settles() {
    let _g = Globals::lock();
    let mut d = Driver::new();
    let id = Id::new("test-move");
    d.prime(id, motion::BASE, motion::ease_out_cubic);

    // First frame after the flip: still at (or extremely near) the origin.
    let first = d.step(|ctx| motion::anim(ctx, id, true, motion::BASE, motion::ease_out_cubic));
    assert!(
        first < 0.35,
        "an animation jumped {first} of the way on its first frame — that's a snap, not motion"
    );

    // Step through the BASE duration collecting every value.
    let mut values = vec![first];
    for _ in 0..24 {
        values.push(d.step(|ctx| {
            motion::anim(ctx, id, true, motion::BASE, motion::ease_out_cubic)
        }));
    }

    // It moved.
    let moved = values.iter().any(|v| *v > 0.001);
    assert!(moved, "value never left 0 — the animation is dead: {values:?}");

    // It moved *monotonically* (an eased ramp should never go backwards).
    for w in values.windows(2) {
        assert!(
            w[1] >= w[0] - 1e-6,
            "eased ramp went backwards: {:?} -> {:?}",
            w[0],
            w[1]
        );
    }

    // It moved gradually, not in one hop: at least 6 distinct intermediate
    // values. This is the assertion that would have caught the old spike.
    let mids = values.iter().filter(|v| **v > 0.001 && **v < 0.999).count();
    assert!(mids >= 6, "only {mids} intermediate frames — this is a jump cut: {values:?}");

    // It landed exactly on the target, not near it. egui clamps to 1.0 at rest,
    // which is what lets us stop repainting.
    let last = *values.last().unwrap();
    assert_eq!(last, 1.0, "animation never reached target; got {last} from {values:?}");

    // And it reversed just as well.
    let mut back = Vec::new();
    for _ in 0..24 {
        back.push(d.step(|ctx| {
            motion::anim(ctx, id, false, motion::BASE, motion::ease_out_cubic)
        }));
    }
    assert!(back.iter().any(|v| *v < 0.999), "reverse never started");
    let mids = back.iter().filter(|v| **v > 0.001 && **v < 0.999).count();
    assert!(mids >= 6, "reverse was a jump cut: {back:?}");
    assert_eq!(*back.last().unwrap(), 0.0, "reverse never reached 0: {back:?}");
}

// ---------------------------------------------------------------------------
// 2. The repaint-scheduling contract: paint while moving, sleep when settled.
// ---------------------------------------------------------------------------

#[test]
fn in_flight_animation_schedules_repaints_then_goes_idle() {
    let _g = Globals::lock();
    let mut d = Driver::new();
    let id = Id::new("test-repaint");
    d.prime(id, motion::SLOW, motion::ease_out_cubic);

    // Kick it off.
    d.step(|ctx| motion::anim(ctx, id, true, motion::SLOW, motion::ease_out_cubic));
    assert!(
        d.wants_repaint(),
        "an animation in flight did not schedule a repaint — this is the #1 \
         immediate-mode gotcha; on screen it would look completely frozen"
    );

    // Halfway through it must *still* be asking for frames.
    for _ in 0..6 {
        d.step(|ctx| motion::anim(ctx, id, true, motion::SLOW, motion::ease_out_cubic));
        assert!(d.wants_repaint(), "stopped requesting repaints mid-animation");
    }

    // Run well past the end.
    for _ in 0..40 {
        d.step(|ctx| motion::anim(ctx, id, true, motion::SLOW, motion::ease_out_cubic));
    }
    // One more clean frame with nothing to do.
    d.step(|ctx| motion::anim(ctx, id, true, motion::SLOW, motion::ease_out_cubic));
    assert!(
        !d.wants_repaint(),
        "still requesting repaints after the animation settled — that pins a \
         CPU core forever and would undermine the whole performance argument"
    );
}

// ---------------------------------------------------------------------------
// 3. Reduce-motion (D13) must collapse *everything* to instant.
// ---------------------------------------------------------------------------

#[test]
fn reduce_motion_snaps_on_the_first_frame() {
    let _g = Globals::lock();
    motion::set_reduce(true);
    let mut d = Driver::new();
    let id = Id::new("test-reduce");
    d.prime(id, motion::SLOW, motion::spring_overshoot);

    let v = d.step(|ctx| motion::anim(ctx, id, true, motion::SLOW, motion::spring_overshoot));
    assert_eq!(v, 1.0, "reduce-motion must land on target on frame 1, got {v}");

    let v = d.step(|ctx| motion::anim(ctx, id, false, motion::SLOW, motion::spring_overshoot));
    assert_eq!(v, 0.0, "reduce-motion reverse must be instant too, got {v}");

    // And no repaint storm.
    d.step(|ctx| motion::anim(ctx, id, false, motion::SLOW, motion::spring_overshoot));
    assert!(!d.wants_repaint(), "reduce-motion should never schedule animation frames");

    // Every duration token collapses at the single choke point.
    for t in [motion::INSTANT, motion::FAST, motion::BASE, motion::SLOW, motion::STEP] {
        assert_eq!(motion::dur(t), 0.0, "a duration survived reduce-motion");
    }
}

// ---------------------------------------------------------------------------
// 4. The global duration multiplier really scales time.
// ---------------------------------------------------------------------------

#[test]
fn duration_multiplier_scales_time() {
    let _g = Globals::lock();
    assert!((motion::dur(motion::BASE) - motion::BASE).abs() < 1e-6);

    motion::set_scale(3.0);
    assert!((motion::dur(motion::BASE) - motion::BASE * 3.0).abs() < 1e-5);

    motion::set_scale(0.25);
    assert!((motion::dur(motion::BASE) - motion::BASE * 0.25).abs() < 1e-5);

    // A 3x-slowed animation must genuinely still be running at a point where a
    // 1x one has already finished.
    motion::set_scale(3.0);
    let mut d = Driver::new();
    let id = Id::new("test-scale");
    d.prime(id, motion::FAST, motion::linear);
    let mut v = 0.0;
    // FAST is 140ms; at 3x that's 420ms = ~25 frames. Step only 10.
    for _ in 0..10 {
        v = d.step(|ctx| motion::anim(ctx, id, true, motion::FAST, motion::linear));
    }
    assert!(v < 0.95, "3x multiplier did not slow the animation down (v={v})");
    assert!(v > 0.0, "3x multiplier stalled the animation entirely");

    // Sanity: at 1x the same number of frames finishes it.
    motion::set_scale(1.0);
    let mut d = Driver::new();
    let id = Id::new("test-scale-1x");
    d.prime(id, motion::FAST, motion::linear);
    let mut v = 0.0;
    for _ in 0..10 {
        v = d.step(|ctx| motion::anim(ctx, id, true, motion::FAST, motion::linear));
    }
    assert_eq!(v, 1.0, "1x FAST should be done inside 10 frames, got {v}");
}

// ---------------------------------------------------------------------------
// 5. Stagger: grouped elements cascade instead of popping together.
// ---------------------------------------------------------------------------

#[test]
fn stagger_cascades_in_order() {
    let _g = Globals::lock();
    let mut d = Driver::new();
    let id = Id::new("test-stagger");
    // Prime the underlying master timeline in its resting state.
    d.step(|ctx| {
        motion::stagger(ctx, id, false, 4, motion::BASE, motion::STEP, motion::ease_out_cubic);
    });

    // Sample a few frames in, while the cascade is mid-flight.
    let mut sample = Vec::new();
    for _ in 0..6 {
        sample = d.step(|ctx| {
            let s = motion::stagger(
                ctx,
                id,
                true,
                4,
                motion::BASE,
                motion::STEP,
                motion::ease_out_cubic,
            );
            (0..4).map(|i| s.at(i)).collect::<Vec<f32>>()
        });
    }

    // Earlier items must be strictly ahead of later ones.
    for i in 0..3 {
        assert!(
            sample[i] >= sample[i + 1],
            "stagger item {i} ({}) is not ahead of item {} ({}) — they're popping together: {sample:?}",
            sample[i],
            i + 1,
            sample[i + 1]
        );
    }
    assert!(
        sample[0] > sample[3] + 0.02,
        "stagger spread is too small to see: {sample:?}"
    );

    // The rise offset must shrink to zero as the item arrives.
    let (_, y_early) = d.step(|ctx| {
        motion::stagger(ctx, id, true, 4, motion::BASE, motion::STEP, motion::ease_out_cubic)
            .rise(3, 10.0)
    });
    assert!(y_early > 0.1, "last staggered item has no rise offset yet ({y_early})");

    for _ in 0..40 {
        d.step(|ctx| {
            motion::stagger(ctx, id, true, 4, motion::BASE, motion::STEP, motion::ease_out_cubic);
        });
    }
    let (a, y) = d.step(|ctx| {
        motion::stagger(ctx, id, true, 4, motion::BASE, motion::STEP, motion::ease_out_cubic)
            .rise(3, 10.0)
    });
    assert_eq!(a, 1.0, "stagger never completed");
    assert!(y.abs() < 1e-4, "rise offset never returned to 0 (got {y})");
}

// ---------------------------------------------------------------------------
// 6. Easing curves are sane: anchored at 0 and 1, and actually curved.
// ---------------------------------------------------------------------------

#[test]
fn easing_curves_are_well_formed() {
    let _g = Globals::lock();
    for (name, f) in motion::CURVES {
        assert!(f(0.0).abs() < 1e-5, "{name}: f(0) = {} (must be 0)", f(0.0));
        assert!((f(1.0) - 1.0).abs() < 1e-4, "{name}: f(1) = {} (must be 1)", f(1.0));
        // Nothing may run away — overshoot is fine, exploding is not.
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let v = f(t);
            assert!(v.is_finite(), "{name}: non-finite at t={t}");
            assert!((-0.4..=1.4).contains(&v), "{name}: wild value {v} at t={t}");
        }
    }

    // ease_out_cubic must front-load: more than half the distance by t=0.5.
    assert!(
        motion::ease_out_cubic(0.5) > 0.6,
        "ease_out_cubic is not decelerating"
    );
    // ease_in_out_cubic must be symmetric about the midpoint.
    assert!((motion::ease_in_out_cubic(0.5) - 0.5).abs() < 1e-5);
    // The overshoot spring must actually overshoot somewhere.
    let peak = (0..=100)
        .map(|i| motion::spring_overshoot(i as f32 / 100.0))
        .fold(0.0_f32, f32::max);
    assert!(peak > 1.01, "spring_overshoot never overshoots (peak {peak})");
    // The critically-damped one must not.
    let peak = (0..=100)
        .map(|i| motion::spring(i as f32 / 100.0))
        .fold(0.0_f32, f32::max);
    assert!(peak <= 1.001, "critically-damped spring overshot (peak {peak})");
}

// ---------------------------------------------------------------------------
// 7. Hand-rolled springs converge and then report themselves inactive.
// ---------------------------------------------------------------------------

#[test]
fn springs_converge_and_report_idle() {
    let _g = Globals::lock();
    let mut s = motion::Spring1::at(0.0);
    let mut frames = 0;
    let mut active = true;
    let mut moved = false;
    while active && frames < 600 {
        active = s.step(1.0, FRAME as f32, motion::K_DRAG, motion::C_DRAG);
        if s.pos > 0.01 {
            moved = true;
        }
        frames += 1;
    }
    assert!(moved, "Spring1 never moved toward its target");
    assert!(!active, "Spring1 never settled — it would repaint forever");
    assert!((s.pos - 1.0).abs() < 1e-3, "Spring1 settled at {} not 1.0", s.pos);
    assert!(frames > 4, "Spring1 teleported in {frames} frames — that's not a spring");

    // 2D spring, driven to an offset target.
    let mut s2 = motion::Spring2::at(egui::vec2(0.0, 0.0));
    let target = egui::vec2(-40.0, 120.0);
    let mut frames = 0;
    let mut active = true;
    while active && frames < 600 {
        active = s2.step(target, FRAME as f32, motion::K_SOFT, motion::C_SOFT);
        frames += 1;
    }
    assert!(!active, "Spring2 never settled");
    assert!((s2.pos - target).length() < 1e-2, "Spring2 settled at {:?}", s2.pos);

    // Coasting (the fling) must lose energy and stop.
    let mut s3 = motion::Spring2::at(egui::vec2(0.0, 0.0));
    s3.vel = egui::vec2(900.0, -200.0);
    for _ in 0..240 {
        s3.coast(FRAME as f32, 1.6, egui::vec2(0.0, 900.0));
    }
    assert!(s3.pos.x > 50.0, "a fling with 900px/s did not travel ({:?})", s3.pos);
    assert!(s3.vel.x < 900.0, "drag never slowed the fling down");

    // Reduce-motion makes springs teleport, so nothing lingers on screen.
    motion::set_reduce(true);
    let mut s4 = motion::Spring1::at(0.0);
    let active = s4.step(1.0, FRAME as f32, motion::K_DRAG, motion::C_DRAG);
    assert_eq!(s4.pos, 1.0, "reduce-motion spring did not snap");
    assert!(!active, "reduce-motion spring still reported active");
}

// ---------------------------------------------------------------------------
// 8. The curve override (the tuner's live curve switcher) really takes effect.
// ---------------------------------------------------------------------------

#[test]
fn curve_override_changes_the_shape() {
    let _g = Globals::lock();
    let mut d = Driver::new();

    fn sample(d: &mut Driver, id: Id) -> f32 {
        d.prime(id, motion::SLOW, motion::linear);
        let mut v = 0.0;
        for _ in 0..5 {
            v = d.step(|ctx| motion::anim(ctx, id, true, motion::SLOW, motion::linear));
        }
        v
    }

    let plain = sample(&mut d, Id::new("curve-a"));

    // CURVES index + 1, because 0 means "as designed" (no override).
    let idx = motion::CURVES
        .iter()
        .position(|(n, _)| *n == "ease_out_cubic")
        .expect("ease_out_cubic must be offerable in the tuner")
        + 1;
    motion::set_curve_override(idx);
    assert_eq!(motion::curve_override_name(), "ease_out_cubic");

    let overridden = sample(&mut d, Id::new("curve-b"));
    assert!(
        overridden > plain + 0.05,
        "curve override had no effect: linear={plain}, ease_out_cubic={overridden}"
    );
}
