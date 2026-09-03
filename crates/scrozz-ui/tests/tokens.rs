//! Headless tests for the design-system foundation.
//!
//! Every test here runs with no display, no GPU and no window, because the
//! properties being checked are properties of *arithmetic*, not of rendering.
//! That is the whole argument for a virtual clock: motion becomes something you
//! can assert about rather than something you have to watch.
//!
//! The four load-bearing guarantees, each of which something else depends on:
//!
//! 1. **Easing curves hit their endpoints exactly** — otherwise an animation
//!    settles a fraction short of its target and a golden image drifts.
//! 2. **Springs converge, and do not oscillate forever** — otherwise the app
//!    repaints at 60 fps indefinitely and pins a CPU core.
//! 3. **Reduce-motion zeroes every duration through one choke point** (D13) —
//!    otherwise "every" is a claim nobody can check.
//! 4. **The virtual clock is deterministic** (D25) — same instant in, same value
//!    out, always. The headless screenshot harness rests entirely on this.

use scrozz_ui::motion::{
    self, Activity, AnimBool, Duration, Ease, Motion, MotionPrefs, Spring1, Spring2, SpringParams,
    Stagger, Timeline, VelocityTracker,
};
use scrozz_ui::theme::{
    self, Appearance, Contrast, Elevation, Palette, Radius, Space, Text, Theme, Weight,
};

/// Tight enough to catch real drift, loose enough not to fail on the last bit
/// of an f32 mantissa.
const EPS: f32 = 1e-6;

fn assert_close(actual: f32, expected: f32, what: &str) {
    assert!(
        (actual - expected).abs() < EPS,
        "{what}: expected {expected}, got {actual}"
    );
}

// ===========================================================================
// 1. Easing
// ===========================================================================

#[test]
fn every_ease_hits_both_endpoints_exactly() {
    for &ease in Ease::ALL {
        // Exactly, not approximately. An animation that ends at 0.999 leaves a
        // sub-pixel offset that a golden-image diff will find.
        assert_eq!(
            ease.apply(0.0),
            0.0,
            "{} must start at exactly 0.0",
            ease.name()
        );
        assert_eq!(
            ease.apply(1.0),
            1.0,
            "{} must end at exactly 1.0",
            ease.name()
        );
    }
}

#[test]
fn eases_clamp_their_input_but_not_their_output() {
    for &ease in Ease::ALL {
        assert_eq!(ease.apply(-5.0), 0.0, "{} below range", ease.name());
        assert_eq!(ease.apply(7.5), 1.0, "{} above range", ease.name());
        assert_eq!(ease.apply(f32::NAN), 0.0, "{} on NaN", ease.name());
    }
}

#[test]
fn only_the_overshooting_eases_leave_the_unit_range() {
    for &ease in Ease::ALL {
        let mut max: f32 = 0.0;
        let mut min: f32 = 0.0;
        for step in 0..=1000 {
            let v = ease.apply(step as f32 / 1000.0);
            max = max.max(v);
            min = min.min(v);
        }
        let escapes = max > 1.0 + 1e-4 || min < -1e-4;
        assert_eq!(
            escapes,
            ease.overshoots(),
            "{} reports overshoots()={} but ranges {min}..={max}",
            ease.name(),
            ease.overshoots()
        );
    }
}

#[test]
fn non_overshooting_eases_are_monotonic() {
    for &ease in Ease::ALL.iter().filter(|e| !e.overshoots()) {
        let mut prev = f32::NEG_INFINITY;
        for step in 0..=1000 {
            let v = ease.apply(step as f32 / 1000.0);
            assert!(
                v >= prev - 1e-5,
                "{} went backwards at t={}: {prev} then {v}",
                ease.name(),
                step as f32 / 1000.0
            );
            prev = v;
        }
    }
}

#[test]
fn ease_is_a_pure_function() {
    for &ease in Ease::ALL {
        for step in 0..=200 {
            let t = step as f32 / 200.0;
            assert_eq!(ease.apply(t), ease.apply(t), "{} is not pure", ease.name());
        }
    }
}

// ===========================================================================
// 2. The virtual clock (D25)
// ===========================================================================

#[test]
fn the_same_instant_always_yields_the_same_value() {
    let timeline = Timeline::starting_at(0.0, Duration::BASE, Ease::OutCubic);

    // Ten independent clocks at the same instant, constructed in different
    // orders, with different histories. Every one must agree.
    let clocks = [
        Motion::at_ms(180),
        Motion::at(0.180),
        Motion::at(0.0).advanced_by(0.180),
        Motion::at(0.100).advanced_by(0.080),
        Motion::at_ms(180).with_dt(1.0 / 30.0),
        Motion::at_ms(180).with_dt(1.0 / 120.0),
        Motion::at_ms(90).advanced_by(0.090),
        Motion::at_frame(0, 60.0).advanced_by(0.180),
        Motion::at_ms(500).advanced_by(-0.320),
        Motion::at(0.180).with_prefs(MotionPrefs::default()),
    ];

    let expected = timeline.value(&clocks[0]);
    for (i, m) in clocks.iter().enumerate() {
        assert_close(
            timeline.value(m),
            expected,
            &format!("clock {i} disagreed at t=180ms"),
        );
    }
    // And a real value, not an accidental zero or one.
    assert!(
        expected > 0.5 && expected < 1.0,
        "180ms into a 220ms OutCubic should be mid-flight, got {expected}"
    );
}

#[test]
fn replaying_a_timeline_is_reproducible() {
    let timeline = Timeline::starting_at(0.25, Duration::SLOW, Ease::OutQuint);
    let sample = |ms: u64| timeline.value(&Motion::at_ms(ms));

    let first: Vec<f32> = (0..600).step_by(7).map(sample).collect();
    let second: Vec<f32> = (0..600).step_by(7).map(sample).collect();
    // Sampled backwards, to prove nothing is accumulating.
    let steps: Vec<u64> = (0..600).step_by(7).collect();
    let third: Vec<f32> = steps.iter().rev().map(|ms| sample(*ms)).collect();

    assert_eq!(first, second, "two forward passes disagreed");
    assert_eq!(
        first,
        third.into_iter().rev().collect::<Vec<_>>(),
        "a reverse pass disagreed with a forward pass"
    );
}

#[test]
fn a_timeline_is_exact_at_its_endpoints() {
    let timeline = Timeline::starting_at(1.0, Duration::BASE, Ease::OutCubic);
    assert_eq!(timeline.value(&Motion::at(1.0)), 0.0, "at start");
    assert_eq!(timeline.value(&Motion::at(1.220)), 1.0, "at end");
    assert_eq!(timeline.value(&Motion::at(99.0)), 1.0, "long after");
    assert_eq!(timeline.value(&Motion::at(0.0)), 0.0, "before start");
}

#[test]
fn a_timeline_stops_asking_for_repaints_once_it_lands() {
    let timeline = Timeline::starting_at(0.0, Duration::BASE, Ease::OutCubic);
    assert!(timeline.is_active(&Motion::at_ms(100)), "mid-flight");
    assert!(!timeline.is_active(&Motion::at_ms(400)), "after landing");
    assert!(
        timeline.activity(&Motion::at_ms(400)).is_idle(),
        "a landed timeline must not keep the app awake"
    );
}

#[test]
fn frame_indices_map_to_exact_instants() {
    for fps in [30.0_f32, 60.0, 120.0] {
        for frame in [0_u64, 1, 7, 60, 3600] {
            let m = Motion::at_frame(frame, fps);
            let expected = f64::from(frame as u32) / f64::from(fps);
            assert!(
                (m.now() - expected).abs() < 1e-12,
                "frame {frame} @ {fps}fps: expected {expected}, got {}",
                m.now()
            );
        }
    }
}

#[test]
fn stepping_a_clock_is_associative_over_time() {
    // Advancing in one jump and advancing in pieces must reach the same place;
    // a harness that steps frame by frame and a test that jumps straight to an
    // instant have to agree, or golden images become order-dependent.
    let one_jump = Motion::at(0.0).advanced_by(0.5);
    let mut in_pieces = Motion::at(0.0);
    for _ in 0..5 {
        in_pieces = in_pieces.advanced_by(0.1);
    }
    // The tolerance is f32, not f64, and deliberately so: `advanced_by` takes
    // an f32 delta, so five accumulations carry five roundings. That is the
    // honest guarantee, and it is why the harness should address an instant
    // directly (`Motion::at_ms`) rather than accumulate toward it.
    assert!(
        (one_jump.now() - in_pieces.now()).abs() < 1e-6,
        "{} vs {}",
        one_jump.now(),
        in_pieces.now()
    );
}

// ===========================================================================
// 3. Reduce motion (D13)
// ===========================================================================

#[test]
fn reduce_motion_zeroes_every_duration() {
    let m = Motion::at(0.0).with_reduce_motion(true);
    let tokens = [
        ("ZERO", Duration::ZERO),
        ("INSTANT", Duration::INSTANT),
        ("FAST", Duration::FAST),
        ("BASE", Duration::BASE),
        ("SLOW", Duration::SLOW),
        ("STAGGER_STEP", Duration::STAGGER_STEP),
        ("custom", Duration::from_millis(5_000)),
        ("scaled", Duration::SLOW.scaled(10.0)),
    ];
    for (name, token) in tokens {
        assert_eq!(m.resolve(token), 0.0, "{name} survived reduce-motion");
    }
}

#[test]
fn reduce_motion_lands_every_animation_on_frame_one() {
    let m = Motion::at(0.0).with_reduce_motion(true);

    // A timeline is finished the instant it starts.
    for &ease in Ease::ALL {
        let t = Timeline::starting_at(0.0, Duration::SLOW, ease);
        assert_eq!(t.value(&m), 1.0, "{} did not land", ease.name());
        assert!(!t.is_active(&m), "{} stayed active", ease.name());
    }

    // A boolean animation is at its target on its first advance.
    let mut b = AnimBool::new(false);
    assert_eq!(b.advance(true, Duration::SLOW, &m), 1.0);
    assert!(!b.is_active());

    // A stagger has no cascade left to run.
    let s = Stagger::since(
        &m,
        0.0,
        6,
        Duration::BASE,
        Duration::STAGGER_STEP,
        Ease::OutCubic,
    );
    for i in 0..6 {
        assert_eq!(s.at(i), 1.0, "stagger step {i}");
    }
    assert!(!s.is_active());
    assert_eq!(s.total(), 0.0, "a reduced stagger has no total duration");

    // A spring is already at its target.
    let mut sp = Spring1::at(0.0);
    let activity = sp.step(100.0, SpringParams::SNAP, &m);
    assert!(
        activity.is_idle(),
        "a reduced spring must arrive at once and ask for no repaint"
    );
    assert_eq!(sp.pos, 100.0);
    assert_eq!(sp.vel, 0.0, "and must not carry velocity");
}

#[test]
fn reduce_motion_still_produces_a_drawable_still() {
    // The point of D13 is that the UI is *complete*, not that it disappears.
    let m = Motion::at_ms(180).with_reduce_motion(true);
    let t = Timeline::starting_at(0.0, Duration::BASE, Ease::OutCubic);
    assert_eq!(t.value(&m), 1.0);
    assert_eq!(t.remaining(&m), 0.0);
}

#[test]
fn reduce_motion_is_carried_by_the_clock_not_read_from_a_global() {
    // Two clocks, opposite preferences, alive at the same time in the same
    // process. This is what makes the test suite parallelisable, and it is
    // exactly what the spike's global `set_reduce` made impossible.
    let normal = Motion::at_ms(50);
    let reduced = Motion::at_ms(50).with_reduce_motion(true);
    let t = Timeline::starting_at(0.0, Duration::BASE, Ease::Linear);

    assert!(t.value(&normal) < 0.5, "normal clock should be mid-flight");
    assert_eq!(t.value(&reduced), 1.0, "reduced clock should be done");
    assert!(!normal.is_reduced());
    assert!(reduced.is_reduced());
}

#[test]
fn time_scale_slows_motion_without_breaking_determinism() {
    let t = Timeline::starting_at(0.0, Duration::BASE, Ease::Linear);
    let slow = Motion::at_ms(220).with_prefs(MotionPrefs {
        reduce_motion: false,
        time_scale: 2.0,
    });
    // At 2× duration, 220 ms into a 220 ms animation is halfway.
    assert_close(t.value(&slow), 0.5, "time_scale=2 at t=220ms");
    assert_eq!(t.value(&slow), t.value(&slow), "still pure");
}

#[test]
fn motion_prefs_reject_absurd_scales() {
    for bad in [0.0_f32, -3.0, f32::NAN, f32::INFINITY, 1e9] {
        let m = Motion::at(0.0).with_prefs(MotionPrefs {
            reduce_motion: false,
            time_scale: bad,
        });
        let resolved = m.resolve(Duration::BASE);
        assert!(
            resolved.is_finite() && resolved > 0.0,
            "time_scale {bad} produced a duration of {resolved}"
        );
    }
}

// ===========================================================================
// 4. Springs
// ===========================================================================

#[test]
fn every_spring_preset_converges() {
    for (name, params) in [
        ("SNAP", SpringParams::SNAP),
        ("DRAG", SpringParams::DRAG),
        ("SOFT", SpringParams::SOFT),
        ("SETTLE", SpringParams::SETTLE),
        ("DECK", SpringParams::DECK),
    ] {
        let mut s = Spring1::at(0.0);
        let mut settled_after = None;
        for step in 0..600 {
            if s.advance(1.0, params, 1.0 / 60.0).is_idle() {
                settled_after = Some(step);
                break;
            }
        }
        let steps = settled_after
            .unwrap_or_else(|| panic!("{name} never settled in 10 seconds (pos={})", s.pos));
        assert!(
            steps < 180,
            "{name} took {steps} frames (>3s) to settle — too slow to feel like a UI"
        );
        assert!(
            (s.pos - 1.0).abs() < 0.01,
            "{name} settled at {} instead of 1.0",
            s.pos
        );
    }
}

#[test]
fn a_settled_spring_snaps_exactly_and_stays_put() {
    let mut s = Spring1::at(0.0);
    while s
        .advance(1.0, SpringParams::SNAP, 1.0 / 60.0)
        .is_animating()
    {}
    assert_eq!(s.pos, 1.0, "settling must snap exactly onto the target");
    assert_eq!(s.vel, 0.0, "and must kill residual velocity");

    // Once settled it must not creep, or the app never stops repainting.
    for _ in 0..1000 {
        assert!(
            s.advance(1.0, SpringParams::SNAP, 1.0 / 60.0).is_idle(),
            "a settled spring asked to be repainted"
        );
        assert_eq!(s.pos, 1.0);
    }
}

#[test]
fn springs_do_not_oscillate_forever() {
    // A spring that keeps crossing its target keeps requesting repaints. Count
    // the crossings: a usable UI spring settles after at most a couple.
    for (name, params) in [
        ("SNAP", SpringParams::SNAP),
        ("DRAG", SpringParams::DRAG),
        ("SOFT", SpringParams::SOFT),
        ("SETTLE", SpringParams::SETTLE),
        ("DECK", SpringParams::DECK),
    ] {
        let mut s = Spring1::at(0.0);
        let mut crossings = 0;
        let mut below = true;
        for _ in 0..600 {
            let moving = s.advance(1.0, params, 1.0 / 60.0).is_animating();
            let now_below = s.pos < 1.0;
            if now_below != below {
                crossings += 1;
                below = now_below;
            }
            if !moving {
                break;
            }
        }
        assert!(
            crossings <= 2,
            "{name} crossed its target {crossings} times — it is ringing"
        );
    }
}

#[test]
fn damping_ratio_predicts_overshoot() {
    for (name, params) in [
        ("SNAP", SpringParams::SNAP),
        ("DRAG", SpringParams::DRAG),
        ("SOFT", SpringParams::SOFT),
        ("SETTLE", SpringParams::SETTLE),
        ("DECK", SpringParams::DECK),
    ] {
        let mut s = Spring1::at(0.0);
        let mut peak: f32 = 0.0;
        for _ in 0..600 {
            let moving = s.advance(1.0, params, 1.0 / 60.0).is_animating();
            peak = peak.max(s.pos);
            if !moving {
                break;
            }
        }
        let overshot = peak > 1.0001;
        assert_eq!(
            overshot,
            params.overshoots(),
            "{name}: damping_ratio()={:.3} predicts overshoots()={}, but peak was {peak}",
            params.damping_ratio(),
            params.overshoots()
        );
    }
}

#[test]
fn spring_replay_is_deterministic() {
    // The property the harness needs: identical state plus identical elapsed
    // time gives an identical result, regardless of how the time was chopped up
    // by the caller's frame rate.
    let run = |dt: f32, frames: usize| {
        let mut s = Spring1::at(0.0);
        for _ in 0..frames {
            let _ = s.advance(1.0, SpringParams::SOFT, dt);
        }
        s.pos
    };

    assert_eq!(run(1.0 / 60.0, 12), run(1.0 / 60.0, 12), "not reproducible");

    // 12 frames at 60fps and 24 at 120fps span the same 200ms. Fixed sub-
    // stepping means they land in the same place, so a golden image does not
    // depend on the machine's refresh rate.
    let a = run(1.0 / 60.0, 12);
    let b = run(1.0 / 120.0, 24);
    assert!(
        (a - b).abs() < 0.02,
        "60fps gave {a} but 120fps gave {b} over the same 200ms"
    );
}

#[test]
fn springs_survive_a_hostile_delta() {
    // A dropped frame, a debugger pause, or a laptop lid must not launch the
    // spring into space.
    for dt in [0.0_f32, -1.0, 5.0, f32::NAN, f32::INFINITY] {
        let mut s = Spring1::at(0.0);
        let _ = s.advance(1.0, SpringParams::SNAP, dt);
        assert!(
            s.pos.is_finite() && s.vel.is_finite(),
            "dt={dt} produced pos={} vel={}",
            s.pos,
            s.vel
        );
        assert!(
            s.pos >= -1.0 && s.pos <= 2.0,
            "dt={dt} threw the spring to {}",
            s.pos
        );
    }
}

#[test]
fn a_two_dimensional_spring_matches_two_one_dimensional_ones() {
    let mut flat = Spring2::at(egui::Vec2::ZERO);
    let mut x = Spring1::at(0.0);
    let mut y = Spring1::at(0.0);
    let target = egui::vec2(120.0, -40.0);
    for _ in 0..60 {
        let _ = flat.advance(target, SpringParams::DRAG, 1.0 / 60.0);
        let _ = x.advance(target.x, SpringParams::DRAG, 1.0 / 60.0);
        let _ = y.advance(target.y, SpringParams::DRAG, 1.0 / 60.0);
    }
    assert_close(flat.pos.x, x.pos, "x axis");
    assert_close(flat.pos.y, y.pos, "y axis");
}

#[test]
fn coasting_slows_down_and_stops() {
    let mut s = Spring2::at(egui::Vec2::ZERO);
    s.vel = egui::vec2(900.0, 0.0);
    let mut last = f32::INFINITY;
    for _ in 0..240 {
        s.coast(1.0 / 60.0, motion::FLING_DRAG_PER_SEC, egui::Vec2::ZERO);
        let speed = s.vel.length();
        assert!(speed <= last + 1e-3, "a coast sped up: {last} then {speed}");
        last = speed;
    }
    assert!(last < 100.0, "still travelling at {last} after 4 seconds");
}

// ===========================================================================
// 5. Boolean animation and stagger
// ===========================================================================

#[test]
fn anim_bool_runs_forward_and_back() {
    // Primed at rest, so the documented first-sight snap is already spent and
    // what follows is real motion.
    let mut m = Motion::at(0.0);
    let mut b = AnimBool::starting_at(0.0, &m);

    for _ in 0..8 {
        m = m.advanced_by(1.0 / 60.0);
        b.advance(true, Duration::BASE, &m);
    }
    let peak = b.raw();
    assert!(peak > 0.4 && peak < 1.0, "mid-flight value was {peak}");

    // Reverse before it finishes: it must retreat from where it is, not restart.
    m = m.advanced_by(1.0 / 60.0);
    let after = b.advance(false, Duration::BASE, &m);
    assert!(
        after < peak,
        "reversal went the wrong way: {peak} -> {after}"
    );
    assert!(after > 0.0, "reversal teleported to 0");
}

#[test]
fn anim_bool_is_idempotent_within_one_frame() {
    // Immediate mode redraws a widget many times per frame in some layouts.
    // Advancing off the absolute clock rather than a delta means the second and
    // third call in the same frame change nothing.
    let m = Motion::at_ms(16);
    let mut b = AnimBool::starting_at(0.0, &m);
    let first = b.advance(true, Duration::BASE, &m);
    let second = b.advance(true, Duration::BASE, &m);
    let third = b.advance(true, Duration::BASE, &m);
    assert_eq!(first, second);
    assert_eq!(second, third);
}

#[test]
fn anim_bool_settles_and_stops_asking_for_repaints() {
    let mut m = Motion::at(0.0);
    let mut b = AnimBool::starting_at(0.0, &m);
    for _ in 0..60 {
        m = m.advanced_by(1.0 / 60.0);
        b.advance(true, Duration::BASE, &m);
    }
    assert_eq!(b.raw(), 1.0);
    assert!(!b.is_active());
    assert!(b.activity().is_idle());
}

#[test]
fn a_stagger_cascades_in_index_order() {
    let m = Motion::at_ms(60);
    let s = Stagger::since(
        &m,
        0.0,
        5,
        Duration::BASE,
        Duration::STAGGER_STEP,
        Ease::OutCubic,
    );
    let values: Vec<f32> = (0..5).map(|i| s.at(i)).collect();
    for pair in values.windows(2) {
        assert!(
            pair[0] >= pair[1] - EPS,
            "a later card led an earlier one: {values:?}"
        );
    }
    assert!(values[0] > 0.0, "the first card had not started");
    assert!(values[4] < 1.0, "the last card had already finished");
}

#[test]
fn a_stagger_completes_and_reports_its_total() {
    let m = Motion::at(0.0);
    let s = Stagger::since(
        &m,
        0.0,
        6,
        Duration::BASE,
        Duration::STAGGER_STEP,
        Ease::OutCubic,
    );
    let total = s.total();
    assert_close(total, 0.220 + 0.030 * 5.0, "total span");

    let done = Motion::at(f64::from(total) + 0.001);
    let finished = Stagger::since(
        &done,
        0.0,
        6,
        Duration::BASE,
        Duration::STAGGER_STEP,
        Ease::OutCubic,
    );
    for i in 0..6 {
        assert_eq!(finished.at(i), 1.0, "card {i} never landed");
    }
    assert!(!finished.is_active());
}

#[test]
fn a_stagger_rise_ends_at_zero_offset() {
    let done = Motion::at(10.0);
    let s = Stagger::since(
        &done,
        0.0,
        3,
        Duration::BASE,
        Duration::STAGGER_STEP,
        Ease::OutCubic,
    );
    for i in 0..3 {
        let (opacity, offset) = s.rise(i, 24.0);
        assert_eq!(opacity, 1.0, "card {i} opacity");
        assert_eq!(offset, 0.0, "card {i} offset");
    }
}

#[test]
fn an_out_of_range_stagger_index_is_not_a_panic() {
    let s = Stagger::since(
        &Motion::at(10.0),
        0.0,
        3,
        Duration::BASE,
        Duration::STAGGER_STEP,
        Ease::OutCubic,
    );
    assert_eq!(s.at(99), 1.0, "an unknown index should read as settled");
}

// ===========================================================================
// 6. Velocity
// ===========================================================================

#[test]
fn a_throw_reads_as_velocity() {
    let mut v = VelocityTracker::new();
    for step in 0..10 {
        let t = f64::from(step) / 120.0;
        v.sample(t, egui::vec2(step as f32 * 10.0, 0.0));
    }
    let vel = v.velocity();
    assert!(vel.x > 500.0, "expected a fast throw, got {vel:?}");
    assert!(vel.y.abs() < 1.0, "invented vertical motion: {vel:?}");
}

#[test]
fn a_drag_that_stopped_before_release_reads_as_zero() {
    // The bug this exists to prevent: pressing, dragging, pausing, then letting
    // go should *not* fling. Only motion inside the sampling window counts.
    let mut v = VelocityTracker::new();
    for step in 0..10 {
        v.sample(f64::from(step) / 120.0, egui::vec2(step as f32 * 10.0, 0.0));
    }
    // Held still well past the window.
    for step in 0..20 {
        v.sample(0.5 + f64::from(step) / 120.0, egui::vec2(90.0, 0.0));
    }
    assert!(
        v.velocity().length() < 20.0,
        "a stalled drag flung at {:?}",
        v.velocity()
    );
}

#[test]
fn an_empty_tracker_has_no_velocity() {
    let v = VelocityTracker::new();
    assert!(v.is_empty());
    assert_eq!(v.velocity(), egui::Vec2::ZERO);
}

#[test]
fn clearing_a_tracker_forgets_the_gesture() {
    let mut v = VelocityTracker::new();
    for step in 0..10 {
        v.sample(f64::from(step) / 120.0, egui::vec2(step as f32 * 10.0, 0.0));
    }
    v.clear();
    assert!(v.is_empty());
    assert_eq!(v.velocity(), egui::Vec2::ZERO);
}

// ===========================================================================
// 7. Repaint scheduling
// ===========================================================================

#[test]
fn activity_merges_toward_the_most_urgent() {
    assert!(Activity::IDLE.is_idle());
    assert!(Activity::animating().is_animating());

    // Animating beats waiting, which beats idle.
    assert!(Activity::IDLE.merge(Activity::animating()).is_animating());
    assert!(
        Activity::waiting(1.0)
            .merge(Activity::animating())
            .is_animating()
    );
    // The soonest wake-up wins.
    let soon = Activity::waiting(0.2).merge(Activity::waiting(3.0));
    assert_eq!(soon.wake_after(), Some(0.2));
}

#[test]
fn activity_sums_over_a_list_of_elements() {
    let all: Activity = [
        Activity::IDLE,
        Activity::waiting(2.0),
        Activity::IDLE,
        Activity::animating(),
    ]
    .into_iter()
    .sum();
    assert!(all.is_animating(), "one animating card must wake the frame");

    let quiet: Activity = [Activity::IDLE, Activity::IDLE].into_iter().sum();
    assert!(quiet.is_idle(), "a settled surface must let the CPU sleep");
}

#[test]
fn a_waiting_activity_is_not_an_animating_one() {
    // The spike's toast bug: a dwell timer that reported itself as animating
    // repainted at 60fps for its whole three-second life.
    let waiting = Activity::waiting(3.0);
    assert!(!waiting.is_animating());
    assert!(!waiting.is_idle());
    assert_eq!(waiting.wake_after(), Some(3.0));
}

// ===========================================================================
// 8. Interpolation helpers
// ===========================================================================

#[test]
fn colour_interpolation_carries_alpha() {
    // The regression that cost the spike 1132 drifted pixels: `from_rgb` forces
    // alpha to 255, so a fade between two translucent tints silently turns
    // opaque part-way through.
    let a = egui::Color32::from_rgba_premultiplied(10, 10, 10, 0);
    let b = egui::Color32::from_rgba_premultiplied(200, 200, 200, 255);
    let mid = motion::lerp_color(a, b, 0.5);
    assert!(
        mid.a() > 100 && mid.a() < 160,
        "alpha was not interpolated: {}",
        mid.a()
    );
    assert_eq!(motion::lerp_color(a, b, 0.0), a, "t=0 must be exact");
    assert_eq!(motion::lerp_color(a, b, 1.0), b, "t=1 must be exact");
}

#[test]
fn fade_and_alpha_clamp() {
    let c = egui::Color32::from_rgb(255, 255, 255);
    assert_eq!(motion::fade(c, 2.0), motion::fade(c, 1.0));
    assert_eq!(motion::fade(c, -1.0), motion::fade(c, 0.0));
    assert_eq!(motion::alpha(200, 0.5), 100);
    assert_eq!(motion::alpha(200, 5.0), 200);
    assert_eq!(motion::alpha(200, -5.0), 0);
}

#[test]
fn lerp_does_not_clamp() {
    // Deliberate: an overshooting curve must be able to overshoot.
    assert_eq!(motion::lerp(0.0, 10.0, 1.5), 15.0);
    assert_eq!(motion::lerp(0.0, 10.0, -0.5), -5.0);
}

// ===========================================================================
// 9. Theme tokens
// ===========================================================================

#[test]
fn the_spacing_scale_ascends_on_a_four_point_grid() {
    let scale = [
        Space::HAIR,
        Space::XS,
        Space::SM,
        Space::MD,
        Space::LG,
        Space::XL,
        Space::XXL,
        Space::HUGE,
    ];
    for pair in scale.windows(2) {
        assert!(
            pair[0] < pair[1],
            "spacing scale is not ascending: {scale:?}"
        );
    }
    for step in &scale[1..] {
        assert_eq!(step % Space::UNIT, 0.0, "{step} is off the 4pt grid");
    }
    assert_eq!(Space::units(4.0), Space::LG);
}

#[test]
fn the_radius_family_ascends() {
    let family = [
        Radius::CHIP,
        Radius::BUTTON,
        Radius::THUMB,
        Radius::BAR,
        Radius::CARD,
    ];
    for pair in family.windows(2) {
        assert!(pair[0] < pair[1], "radius family is not ascending");
    }
    assert_eq!(Radius::pill(34.0), 17.0);
}

#[test]
fn radii_quantise_safely() {
    // These reach egui as u8 per corner, so the conversion must be total.
    for r in [-10.0_f32, 0.0, 0.4, 19.6, 1e9, f32::NAN, f32::INFINITY] {
        let c = theme::corner(r);
        assert_eq!(c.nw, c.se, "corner {r} was not uniform");
    }
    assert_eq!(theme::corner(20.0).nw, 20);
    assert_eq!(theme::corner(19.6).nw, 20);
    assert_eq!(theme::corner_top(20.0).sw, 0);
    assert_eq!(theme::corner_bottom(20.0).nw, 0);
}

#[test]
fn elevation_ascends_and_flat_draws_nothing() {
    let palette = Palette::dark();
    assert!(Elevation::Flat.shadows(&palette).is_none());

    let levels = [Elevation::Resting, Elevation::Raised, Elevation::Lifted];
    let mut last_blur = 0;
    for level in levels {
        let (ambient, key) = level.shadows(&palette).expect("not flat");
        assert!(
            key.blur > ambient.blur,
            "{level:?}: the key shadow must be softer than the contact shadow"
        );
        assert!(
            key.blur > last_blur,
            "{level:?} is not more elevated than the level below"
        );
        last_blur = key.blur;
    }
    assert_eq!(Elevation::Flat.lift(), 0.0);
}

#[test]
fn a_continuous_lift_is_clamped_and_total() {
    let palette = Palette::dark();
    for lift in [-5.0_f32, 0.0, 0.37, 1.0, 1e9] {
        let (ambient, key) = theme::shadows_for_lift(lift, &palette);
        assert!(key.blur >= ambient.blur, "lift {lift}");
    }
}

#[test]
fn the_type_ramp_descends_and_every_role_resolves() {
    for role in Text::ALL {
        assert!(role.size() > 0.0, "{role:?} has no size");
        let font = role.font();
        assert_eq!(font.size, role.size());
        assert_eq!(font.family, role.weight().family());
    }
    assert!(Text::Display.size() > Text::Title.size());
    assert!(Text::Body.size() > Text::Caption.size());
    assert_eq!(Text::Display.weight(), Weight::Bold);
    assert_eq!(Text::Caption.weight(), Weight::Regular);
}

#[test]
fn text_scale_multiplies_the_whole_ramp() {
    let big = Theme::dark().with_text_scale(1.5);
    for role in Text::ALL {
        assert_close(
            big.font(*role).size,
            role.size() * 1.5,
            &format!("{role:?} at 1.5x"),
        );
    }
    // And refuses to produce something unreadable or unrenderable.
    for bad in [0.0_f32, -1.0, 100.0, f32::NAN] {
        let t = Theme::dark().with_text_scale(bad);
        assert!(t.text_scale >= 0.75 && t.text_scale <= 2.0, "scale {bad}");
    }
}

#[test]
fn both_appearances_resolve_and_differ() {
    let dark = Palette::dark();
    let light = Palette::light();
    assert!(dark.is_dark());
    assert!(!light.is_dark());
    assert_ne!(dark.text, light.text);
    assert_ne!(dark.canvas(), light.canvas());
    assert_eq!(Palette::for_appearance(Appearance::Dark), dark);
    assert_eq!(Appearance::Dark.inverted(), Appearance::Light);
}

#[test]
fn font_definitions_bind_every_weight() {
    let fonts = theme::font_definitions();
    for role in Text::ALL {
        let family = role.weight().family();
        let bound = fonts
            .families
            .get(&family)
            .map(|names| !names.is_empty())
            .unwrap_or(false);
        assert!(bound, "{role:?} resolves to unbound family {family:?}");
    }
    // Every named family must reference font data that was actually inserted,
    // or egui panics at draw time with a message about an unbound family.
    for (family, names) in &fonts.families {
        for name in names {
            assert!(
                fonts.font_data.contains_key(name),
                "family {family:?} references missing font data {name}"
            );
        }
    }
}

// ===========================================================================
// 10. Accessibility: contrast (D13)
// ===========================================================================

#[test]
fn contrast_matches_the_wcag_reference_values() {
    let white = egui::Color32::WHITE;
    let black = egui::Color32::BLACK;
    // Relative tolerance: the three luminance coefficients sum to 1.0 only to
    // within an f32 ulp, so the ideal 21.0 lands a couple of ulps low.
    let full = theme::contrast_ratio(white, black);
    assert!((full - 21.0).abs() < 1e-4, "white on black was {full}");
    assert_close(theme::contrast_ratio(white, white), 1.0, "white on white");
    // Symmetric by definition.
    assert_eq!(
        theme::contrast_ratio(white, black),
        theme::contrast_ratio(black, white)
    );
    assert!(
        (theme::relative_luminance(white) - 1.0).abs() < 1e-5,
        "white luminance"
    );
    assert_close(theme::relative_luminance(black), 0.0, "black luminance");
}

#[test]
fn primary_text_meets_wcag_aa_in_both_appearances() {
    for appearance in [Appearance::Dark, Appearance::Light] {
        let p = Palette::for_appearance(appearance);
        let canvas = p.canvas();
        // Text is drawn on a card, not on bare canvas, so measure it there.
        let card = theme::flatten_onto(p.card_fill, canvas);
        let ratio = theme::contrast_ratio(p.flatten(p.text), card);
        assert!(
            ratio >= Contrast::AA_TEXT,
            "{appearance:?}: primary text is {ratio:.2}:1 on a card, below AA ({}:1)",
            Contrast::AA_TEXT
        );
    }
}

#[test]
fn secondary_text_meets_wcag_aa_large_in_both_appearances() {
    for appearance in [Appearance::Dark, Appearance::Light] {
        let p = Palette::for_appearance(appearance);
        let card = theme::flatten_onto(p.card_fill, p.canvas());
        let ratio = theme::contrast_ratio(theme::flatten_onto(p.text_muted, card), card);
        assert!(
            ratio >= Contrast::AA_LARGE,
            "{appearance:?}: secondary text is {ratio:.2}:1, below AA-large"
        );
    }
}

#[test]
fn text_on_the_accent_fill_meets_wcag_aa() {
    // The one pairing that is easy to get wrong: a mid-tone accent with white
    // text on it is a very common failure.
    for appearance in [Appearance::Dark, Appearance::Light] {
        let p = Palette::for_appearance(appearance);
        for accent in [p.accent, p.accent_hi, p.accent_press] {
            let ratio = theme::contrast_ratio(p.on_accent, accent);
            assert!(
                ratio >= Contrast::AA_LARGE,
                "{appearance:?}: on_accent over an accent fill is {ratio:.2}:1"
            );
        }
    }
}

#[test]
fn both_appearances_use_the_canonical_ember_accent() {
    for appearance in [Appearance::Dark, Appearance::Light] {
        assert_eq!(
            Palette::for_appearance(appearance).accent,
            theme::BRAND_ACCENT
        );
    }
}

#[test]
fn accent_wash_text_and_links_meet_wcag_aa() {
    for palette in [Palette::dark(), Palette::light()] {
        let ink = palette.on_accent_wash();
        assert!(
            theme::contrast_ratio(ink, palette.accent_wash()) >= Contrast::AA_TEXT,
            "{:?} accent-wash text fails AA",
            palette.appearance
        );
        assert!(
            theme::contrast_ratio(ink, palette.canvas()) >= Contrast::AA_TEXT,
            "{:?} hyperlink text fails AA on the canvas",
            palette.appearance
        );
    }
}

#[test]
fn the_focus_ring_is_visible_against_what_it_surrounds() {
    // A focus ring nobody can see is not a focus ring (D13).
    for appearance in [Appearance::Dark, Appearance::Light] {
        let p = Palette::for_appearance(appearance);
        let card = theme::flatten_onto(p.card_fill, p.canvas());
        let ratio = theme::contrast_ratio(p.flatten(p.focus_ring), card);
        assert!(
            ratio >= Contrast::AA_LARGE,
            "{appearance:?}: the focus ring is {ratio:.2}:1 against a card"
        );
    }
}

#[test]
fn flattening_a_transparent_colour_leaves_the_backdrop() {
    let backdrop = egui::Color32::from_rgb(20, 30, 40);
    assert_eq!(
        theme::flatten_onto(egui::Color32::TRANSPARENT, backdrop),
        backdrop
    );
    let opaque = egui::Color32::from_rgb(200, 100, 50);
    assert_eq!(theme::flatten_onto(opaque, backdrop), opaque);
}

// ===========================================================================
// 11. Icons
// ===========================================================================

#[test]
fn every_icon_rasterizes_without_a_display() {
    use scrozz_ui::icons::{self, Icon};
    for &icon in Icon::ALL {
        let image = icons::rasterize(icon, 64)
            .unwrap_or_else(|e| panic!("icon `{icon}` failed to rasterize: {e}"));
        assert_eq!(image.width().max(image.height()), 64, "icon `{icon}` size");
        // A blank mask means the SVG parsed but drew nothing — a silent failure
        // that would ship as an invisible button.
        let ink = image.pixels.iter().filter(|p| p.a() > 0).count();
        assert!(ink > 0, "icon `{icon}` rasterized to nothing");
    }
}

#[test]
fn icon_indices_match_declaration_order() {
    use scrozz_ui::icons::Icon;
    for (i, &icon) in Icon::ALL.iter().enumerate() {
        assert_eq!(icon.index(), i, "`{icon}` has the wrong table index");
    }
    assert_eq!(Icon::ALL.len(), Icon::COUNT);
}

#[test]
fn icon_slugs_are_unique() {
    use scrozz_ui::icons::Icon;
    let mut slugs: Vec<&str> = Icon::ALL.iter().map(|i| i.slug()).collect();
    slugs.sort_unstable();
    let count = slugs.len();
    slugs.dedup();
    assert_eq!(slugs.len(), count, "two icons share a slug");
}

#[test]
fn an_empty_icon_store_is_usable() {
    use scrozz_ui::icons::{Icon, IconStore};
    let store = IconStore::empty();
    assert!(!store.is_complete());
    assert!(store.texture(Icon::Check).is_none());
}

// ===========================================================================
// 12. Vibrancy
// ===========================================================================

#[test]
fn material_names_round_trip() {
    use scrozz_ui::vibrancy::Material;
    for &m in Material::ALL {
        assert_eq!(Material::parse(m.name()), m, "{m} did not round-trip");
    }
    assert_eq!(Material::parse("nonsense"), Material::None);
    assert_eq!(Material::parse("  VIBRANCY "), Material::Vibrancy);
    assert!(
        Material::None.supported(),
        "drawing it ourselves always works"
    );
}

#[test]
fn an_unapplied_material_reports_honestly() {
    use scrozz_ui::vibrancy::{Applied, Material};
    let none = Applied::NotRequested;
    assert!(!none.has_material());
    assert_eq!(none.material(), Material::None);

    let failed = Applied::Unavailable {
        wanted: Material::Glass,
        why: "no backend".to_owned(),
    };
    assert!(!failed.has_material());
    assert!(failed.to_string().contains("glass"));

    let fell_back = Applied::FellBack {
        wanted: Material::Glass,
        got: Material::Vibrancy,
        why: "needs macOS 26".to_owned(),
    };
    assert!(fell_back.has_material());
    assert_eq!(fell_back.material(), Material::Vibrancy);
}

// ===========================================================================
// 13. Paint geometry
// ===========================================================================

#[test]
fn a_rounded_polygon_stays_inside_its_rectangle() {
    use scrozz_ui::paint;
    let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 140.0));
    for radius in [0.0_f32, 6.0, 20.0, 500.0] {
        let pts = paint::rounded_poly(rect, radius);
        assert!(!pts.is_empty(), "radius {radius} produced no points");
        for p in &pts {
            assert!(
                rect.expand(0.01).contains(*p),
                "radius {radius}: point {p:?} escaped {rect:?}"
            );
        }
    }
}

#[test]
fn rotation_preserves_distance_from_the_pivot() {
    use scrozz_ui::paint;
    let pivot = egui::pos2(50.0, 50.0);
    let mut pts = vec![
        egui::pos2(0.0, 0.0),
        egui::pos2(100.0, 0.0),
        egui::pos2(100.0, 100.0),
    ];
    let before: Vec<f32> = pts.iter().map(|p| (*p - pivot).length()).collect();
    paint::rotate_pts(&mut pts, pivot, 0.7);
    for (p, r0) in pts.iter().zip(&before) {
        assert!(
            ((*p - pivot).length() - r0).abs() < 1e-3,
            "rotation changed the radius: {r0} -> {}",
            (*p - pivot).length()
        );
    }
}

#[test]
fn a_full_turn_returns_every_point() {
    use scrozz_ui::paint;
    let pivot = egui::pos2(3.0, -7.0);
    let original = vec![egui::pos2(11.0, 4.0), egui::pos2(-20.0, 60.0)];
    let mut pts = original.clone();
    paint::rotate_pts(&mut pts, pivot, std::f32::consts::TAU);
    for (a, b) in pts.iter().zip(&original) {
        assert!((*a - *b).length() < 1e-3, "{a:?} != {b:?}");
    }
}

#[test]
fn a_reveal_is_only_live_when_it_has_nearly_arrived() {
    use scrozz_ui::paint::Reveal;
    assert!(Reveal::SHOWN.is_live());
    assert!(!Reveal::new(0.5, egui::Vec2::ZERO).is_live());
    assert!(!Reveal::new(0.0, egui::Vec2::ZERO).is_live());
    // Out-of-range opacity is clamped, not trusted.
    assert_eq!(Reveal::new(9.0, egui::Vec2::ZERO).opacity, 1.0);
    assert_eq!(Reveal::new(-9.0, egui::Vec2::ZERO).opacity, 0.0);
}

#[test]
fn a_shortcut_has_both_a_glyph_form_and_a_spoken_form() {
    use scrozz_ui::paint::{Mod, Shortcut};
    let sc = Shortcut {
        mods: &[Mod::Shift, Mod::Cmd],
        key: "4",
    };
    assert!(sc.glyphs().ends_with('4'));
    // The spoken form must be words, for a screen reader (D13).
    let spoken = sc.spoken();
    assert!(spoken.contains("Shift"), "{spoken}");
    assert!(spoken.contains('4'), "{spoken}");
    assert!(
        spoken.is_ascii(),
        "a screen reader cannot pronounce a symbol: {spoken}"
    );
}

#[test]
fn control_state_defaults_to_enabled_and_quiet() {
    use scrozz_ui::paint::ControlState;
    let s = ControlState::new();
    assert!(s.enabled);
    assert!(!s.selected);
    assert!(!s.force_hover);
    assert!(!ControlState::disabled().enabled);
    assert!(ControlState::on().selected);
    assert!(ControlState::new().selected(true).selected);
}

// ===========================================================================
// Popups are surfaces, panels are not
// ===========================================================================

/// A dropdown's own background, in both appearances.
///
/// Scrozz's panels are transparent on purpose — the OS desktop or a captured
/// image is what sits behind a `CentralPanel`. A popup is not a panel, and
/// egui builds every menu, dropdown list and tooltip from `Frame::popup`,
/// which fills with `window_fill`. Leaving that transparent left a combo
/// box's options floating with the desktop showing straight through them,
/// readable only on whichever row happened to be selected.
#[test]
fn every_popup_has_an_opaque_background_its_text_is_legible_on() {
    for appearance in [Appearance::Dark, Appearance::Light] {
        let theme = Theme::for_appearance(appearance);
        let mut style = egui::Style::default();
        theme::apply_style(&mut style, &theme);

        let fill = style.visuals.window_fill;
        assert_eq!(
            fill.a(),
            255,
            "{appearance:?}: a dropdown drawn over arbitrary desktop content \
             cannot be translucent"
        );
        assert!(
            theme::contrast_ratio(fill, style.visuals.widgets.inactive.fg_stroke.color)
                >= Contrast::AA_TEXT,
            "{appearance:?}: an option's label must clear AA against the \
             popup it sits on"
        );
        assert!(
            style.visuals.window_stroke.width > 0.0,
            "{appearance:?}: the popup needs an edge, not just a fill"
        );
        assert!(
            style.visuals.popup_shadow != egui::epaint::Shadow::NONE,
            "{appearance:?}: a floating surface casts something"
        );

        // And the panel behind it stays transparent, which is the property
        // this must not have broken.
        assert_eq!(style.visuals.panel_fill, egui::Color32::TRANSPARENT);
    }
}

/// The popup surface reaches the actual dropdown, not just the style struct.
///
/// Driven through a real `Context` because the failure this guards against was
/// exactly a style that looked right in isolation: `ComboBox` opens its list in
/// its own `Area`, and an `Area` reads the *context* style rather than the one
/// a caller installed on some inner `Ui`.
#[test]
fn an_open_dropdown_paints_a_background_behind_its_options() {
    use egui::{Event, PointerButton, RawInput, Rect, pos2, vec2};

    for appearance in [Appearance::Dark, Appearance::Light] {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::install_style(&ctx, &Theme::for_appearance(appearance));
        let expected = {
            let mut style = egui::Style::default();
            theme::apply_style(&mut style, &Theme::for_appearance(appearance));
            style.visuals.window_fill
        };

        let mut selected = 0usize;
        let at = pos2(60.0, 16.0);
        let mut time = 0.0;
        let mut opened = false;
        let mut debug_rects: Vec<(egui::Color32, egui::Rect)> = Vec::new();

        for pass in 0..4 {
            time += 1.0 / 60.0;
            let mut events = Vec::new();
            if pass == 1 {
                events.push(Event::PointerMoved(at));
                for pressed in [true, false] {
                    events.push(Event::PointerButton {
                        pos: at,
                        button: PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::default(),
                    });
                }
            }
            let mut output = ctx.run_ui(
                RawInput {
                    time: Some(time),
                    predicted_dt: 1.0 / 60.0,
                    focused: true,
                    screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(320.0, 220.0))),
                    events,
                    ..Default::default()
                },
                |ui| {
                    egui::ComboBox::from_id_salt("probe")
                        .selected_text(["Save to export location", "Ask every time"][selected])
                        .width(220.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut selected, 0, "Save to export location");
                            ui.selectable_value(&mut selected, 1, "Ask every time");
                        });
                },
            );
            output.textures_delta.clear();
            if pass < 2 {
                continue;
            }
            // The popup's own frame: a filled rectangle below the closed
            // control, in the colour the style promised.
            // A frame with a shadow paints as a `Shape::Vec` of the shadow
            // and the frame, so the background is one level down.
            fn rects(shape: &egui::Shape, out: &mut Vec<(egui::Color32, egui::Rect)>) {
                match shape {
                    egui::Shape::Rect(rect) => out.push((rect.fill, rect.rect)),
                    egui::Shape::Vec(shapes) => {
                        for shape in shapes {
                            rects(shape, out);
                        }
                    }
                    _ => {}
                }
            }
            let mut found = Vec::new();
            for clipped in &output.shapes {
                rects(&clipped.shape, &mut found);
            }
            debug_rects.extend(found.iter().copied());
            opened |= found
                .iter()
                .any(|(fill, rect)| *fill == expected && rect.top() > 30.0 && rect.height() > 20.0);
        }
        assert!(
            opened,
            "{appearance:?}: the open dropdown painted no background behind \
             its options; expected {expected:?}, saw {:?}",
            debug_rects
        );
    }
}
