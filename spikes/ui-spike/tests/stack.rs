//! End-to-end motion test for the **real** live stack.
//!
//! `tests/motion.rs` proves the primitives. This proves the surface: it runs
//! the actual `Stack::show` drawing code — the same function the binary calls —
//! frame by frame over simulated time, and asserts that spawning, settling and
//! dismissing genuinely animate and genuinely stop.
//!
//! Uses `egui_kittest` because `Stack` draws text and icons, which need real
//! fonts and rasterised SVG textures. The two-pass font dance is the same one
//! `snapshot.rs` documents: `set_fonts` only takes effect on the *next*
//! begin-pass, so pass 1 installs and bails without drawing.

#[path = "../src/theme.rs"]
mod theme;
#[path = "../src/icons.rs"]
mod icons;
#[path = "../src/motion.rs"]
mod motion;
#[path = "../src/paint.rs"]
mod paint;
#[path = "../src/stack.rs"]
mod stack;
#[path = "../src/surfaces.rs"]
mod surfaces;

use egui_kittest::Harness;
use icons::IconStore;
use stack::Stack;

/// What one simulated frame of the stack reported.
#[derive(Debug, Clone)]
struct Frame {
    /// `Stack::show`'s return value: "something is still physically moving".
    active: bool,
    /// Every animated value in the stack this frame. If this never changes,
    /// nothing is moving on screen no matter what `active` claims.
    pose: Vec<f32>,
    /// Cards in the deck.
    len: usize,
}

impl Frame {
    /// Largest change in any animated value versus another frame.
    fn delta(&self, other: &Frame) -> f32 {
        if self.pose.len() != other.pose.len() {
            return f32::INFINITY;
        }
        self.pose
            .iter()
            .zip(&other.pose)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max)
    }
}

struct Sim {
    harness: Harness<'static, State>,
}

struct State {
    icons: Option<IconStore>,
    stack: Stack,
    /// Queued commands to apply at the top of the next drawn frame.
    spawn: u32,
    dismiss: u32,
    last: Frame,
}

impl Sim {
    fn new() -> Self {
        let harness = Harness::builder()
            .with_size(egui::vec2(600.0, 470.0))
            .with_theme(egui::Theme::Dark)
            .build_ui_state(
                |ui, st: &mut State| {
                    let ctx = ui.ctx().clone();
                    if st.icons.is_none() {
                        theme::install_fonts(&ctx);
                        theme::install_style(&ctx, &theme::Palette::dark());
                        st.icons = Some(IconStore::new(&ctx));
                        ctx.request_repaint();
                        return;
                    }
                    for _ in 0..std::mem::take(&mut st.spawn) {
                        st.stack.spawn();
                    }
                    for _ in 0..std::mem::take(&mut st.dismiss) {
                        st.stack.dismiss_top();
                    }
                    let pal = theme::Palette::dark();
                    let scene = ui.max_rect();
                    let active = st.stack.show(ui, st.icons.as_ref().unwrap(), &pal, scene);
                    st.last = Frame {
                        active,
                        pose: st.stack.pose(),
                        len: st.stack.len(),
                    };
                },
                State {
                    icons: None,
                    stack: Stack::new(),
                    spawn: 0,
                    dismiss: 0,
                    last: Frame { active: false, pose: Vec::new(), len: 0 },
                },
            );
        let mut sim = Sim { harness };
        // Burn the font-install pass plus a couple of settle frames.
        for _ in 0..3 {
            sim.step();
        }
        sim
    }

    fn step(&mut self) -> Frame {
        self.harness.step();
        self.harness.state().last.clone()
    }

    /// Run `n` frames, returning every frame's report.
    fn run(&mut self, n: usize) -> Vec<Frame> {
        (0..n).map(|_| self.step()).collect()
    }

    /// Run until nothing is moving, or give up. Returns frames elapsed.
    fn settle(&mut self, limit: usize) -> usize {
        for i in 0..limit {
            if !self.step().active {
                return i;
            }
        }
        limit
    }
}

fn reset() {
    motion::set_scale(1.0);
    motion::set_reduce(false);
    motion::set_curve_override(0);
}

/// Same story as `tests/motion.rs`: the motion switches are process-global, and
/// cargo runs these tests in parallel threads of one process. Without this lock
/// `slow_motion_multiplier_*` resets the scale out from under
/// `reduce_motion_*` and both flap. Serialising them is the honest fix; the
/// underlying cost is a real property of the global-token design.
static GLOBALS: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Globals(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl Globals {
    fn lock() -> Self {
        let g = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        Self(g)
    }
}

impl Drop for Globals {
    fn drop(&mut self) {
        reset();
    }
}

/// How many leading frames reported motion.
fn moving(frames: &[Frame]) -> usize {
    frames.iter().take_while(|f| f.active).count()
}

// ---------------------------------------------------------------------------

/// The whole point: a new capture must *animate* into the deck — many frames of
/// movement — and then the app must fall completely idle.
#[test]
fn new_capture_animates_in_then_the_app_goes_idle() {
    let _g = Globals::lock();
    let mut sim = Sim::new();
    sim.settle(400);

    // A resting stack must not be asking for frames. If this fails the app is
    // burning a core at idle, which is exactly what we promised not to do.
    let at_rest = sim.step();
    assert!(
        !at_rest.active,
        "the stack never went idle — it would spin the CPU forever at rest"
    );

    sim.harness.state_mut().spawn += 1;
    let frames = sim.run(120);
    let n = moving(&frames);
    assert!(
        n >= 12,
        "a new capture settled in {n} frames — that is a pop, not an animation \
         (expected roughly 20-60 at 60fps)"
    );
    assert!(n < 120, "the entry animation never finished inside 2s — it would never idle");

    // The card really arrived.
    assert_eq!(frames[n.saturating_sub(1)].len, at_rest.len + 1, "no card was added");

    // And the animated values genuinely changed frame to frame — this is the
    // assertion the previous spike would have failed.
    let mut biggest_step = 0.0_f32;
    for w in frames[..n.max(2)].windows(2) {
        biggest_step = biggest_step.max(w[0].delta(&w[1]));
    }
    assert!(
        biggest_step > 0.05,
        "animated values barely moved between frames (max step {biggest_step}) — \
         this is a static picture with a timer behind it"
    );

    // Total travel across the whole animation should be substantial.
    let total = frames[0].delta(&frames[n.saturating_sub(1).max(1)]);
    assert!(total > 0.5, "the entry animation covered almost no distance ({total})");

    assert_eq!(sim.settle(200), 0, "stack did not stay idle after settling");
}

/// Dismiss must throw the card out on momentum and then clean up after itself.
#[test]
fn dismiss_flings_the_card_and_then_stops() {
    let _g = Globals::lock();
    let mut sim = Sim::new();
    sim.settle(400);
    let before = sim.step().len;

    sim.harness.state_mut().dismiss += 1;
    let frames = sim.run(240);
    let n = moving(&frames);
    assert!(n >= 10, "the card vanished in {n} frames instead of flying out");
    assert!(n < 240, "the flung card never left — it animates forever");

    assert_eq!(frames[n.saturating_sub(1)].len, before - 1, "the card was not removed");

    // The flung card must actually travel a long way, not just fade.
    let travel = frames
        .iter()
        .take(n.max(2))
        .flat_map(|f| f.pose.chunks(6))
        .map(|c| c[1].abs())
        .fold(0.0_f32, f32::max);
    assert!(travel > 80.0, "the fling only moved the card {travel}px — no momentum");

    assert_eq!(sim.settle(200), 0, "still busy long after the fling finished");
}

/// D13: with reduce-motion on, the same interactions must resolve immediately
/// and must never schedule a run of animation frames.
#[test]
fn reduce_motion_makes_the_stack_instant() {
    let _g = Globals::lock();
    motion::set_reduce(true);
    let mut sim = Sim::new();
    sim.settle(400);

    sim.harness.state_mut().spawn += 1;
    let n = moving(&sim.run(30));
    assert!(n <= 2, "reduce-motion still animated the entry for {n} frames");

    sim.harness.state_mut().dismiss += 1;
    let n = moving(&sim.run(30));
    assert!(n <= 2, "reduce-motion still animated the dismissal for {n} frames");
}

/// The duration multiplier has to reach the *surface*, not just the primitives.
#[test]
fn slow_motion_multiplier_lengthens_the_entry_animation() {
    let _g = Globals::lock();
    let mut sim = Sim::new();
    sim.settle(400);
    sim.harness.state_mut().spawn += 1;
    let base = moving(&sim.run(200));
    sim.settle(400);

    motion::set_scale(3.0);
    sim.harness.state_mut().spawn += 1;
    let slow = moving(&sim.run(600));
    motion::set_scale(1.0);

    assert!(
        slow > base + 5,
        "3x duration multiplier barely changed the surface animation \
         (1x took {base} frames, 3x took {slow}) — the tuner slider is a lie"
    );
}

/// Repeatedly replaying must not leak cards or wedge the stack permanently busy.
#[test]
fn replay_restarts_the_animation_without_leaking() {
    let _g = Globals::lock();
    let mut sim = Sim::new();
    sim.settle(400);
    let len = sim.step().len;

    for round in 0..5 {
        sim.harness.state_mut().stack.replay();
        let n = moving(&sim.run(300));
        assert!(n >= 5, "replay {round} did not restart any motion ({n} frames)");
        assert!(n < 300, "replay {round} never settled");
        sim.settle(300);
        assert_eq!(sim.step().len, len, "replay {round} changed the card count");
    }
    assert_eq!(sim.settle(120), 0, "stack stayed busy after five replays");
}

// ---------------------------------------------------------------------------
// The corrected scope: cards enter from, and leave toward, the anchored edge.
// ---------------------------------------------------------------------------

/// The front card's horizontal offset from its home position this frame.
/// `pose()` lays each card out as [depth, x, y, angle, alpha, entry], and the
/// deck's front card is always first.
fn front_x(f: &Frame) -> f32 {
    f.pose.get(1).copied().unwrap_or(0.0)
}

/// The headline of the whole spike: a new capture must arrive as a *slide from
/// off-screen past the anchored edge*, not a fade in place. So the first frame
/// after a spawn has to put the card a long way out on the anchor side, and the
/// settled frame has to put it home.
#[test]
fn entry_slides_in_from_the_anchored_edge() {
    let _g = Globals::lock();
    let mut sim = Sim::new();
    sim.settle(400);

    sim.harness.state_mut().spawn += 1;
    let first = sim.step();

    // Left anchor ⇒ the card starts far out to the *left* (negative x).
    assert!(
        front_x(&first) < -300.0,
        "a new card should start off-screen left, but front_x was {:.1}",
        front_x(&first)
    );

    // And it must actually travel — inward, over several frames — rather than
    // teleporting. (kittest advances ~50ms per step, so a ~500ms spring settle
    // is on the order of ten frames, not thirty.)
    let mut travel = vec![first.clone()];
    travel.extend(sim.run(90));
    let inward: f32 = travel
        .windows(2)
        .map(|w| (front_x(&w[1]) - front_x(&w[0])).max(0.0))
        .sum();
    let frames = travel
        .windows(2)
        .filter(|w| front_x(&w[1]) - front_x(&w[0]) > 0.5)
        .count();
    assert!(
        inward > 280.0 && frames >= 4,
        "expected a long inward slide over several frames, got {inward:.0}px over {frames} frames"
    );

    // A spring settle, not an ease-out: the card must carry past its home
    // position and come back. If this ever reads 0 the motion has quietly
    // degraded into a plain tween.
    let overshoot = travel.iter().map(|f| front_x(f)).fold(f32::MIN, f32::max);
    assert!(
        overshoot > 2.0,
        "the entry should overshoot and settle back, peak was {overshoot:.1}px"
    );

    sim.settle(400);
    let home = sim.step();
    assert!(
        front_x(&home).abs() < 2.0,
        "the card should settle flush in the stack, but sits at {:.1}",
        front_x(&home)
    );
}

/// The anchor is a parameter, not a hardcoded left. Docking the overlay to the
/// right edge must mirror the entire gesture language: in from the right, out
/// to the right.
#[test]
fn flipping_the_anchor_mirrors_the_entry() {
    let _g = Globals::lock();
    let mut sim = Sim::new();
    sim.settle(400);

    sim.harness.state_mut().stack.anchor = stack::Anchor::Right;
    sim.harness.state_mut().spawn += 1;
    let first = sim.step();

    assert!(
        front_x(&first) > 300.0,
        "with a right anchor the card should start off-screen right, got {:.1}",
        front_x(&first)
    );

    sim.settle(400);
    assert!(front_x(&sim.step()).abs() < 2.0, "should still settle home");
}

/// A dismissal must carry the card *toward the anchored edge* and off-screen —
/// not just fade it out where it stands. This is the visible half of "swipe it
/// off to the left".
#[test]
fn dismissal_travels_toward_the_anchored_edge() {
    let _g = Globals::lock();
    let mut sim = Sim::new();
    sim.settle(400);

    let before = sim.harness.state().last.len;
    sim.harness.state_mut().dismiss += 1;
    sim.step();

    // Track how far left the flying card gets before it is reaped.
    let mut furthest = 0.0_f32;
    for f in sim.run(120) {
        for c in f.pose.chunks(6) {
            furthest = furthest.min(c[1]);
        }
    }
    assert!(
        furthest < -200.0,
        "the dismissed card should fly off to the left; furthest x was {furthest:.1}"
    );

    sim.settle(400);
    assert_eq!(
        sim.step().len,
        before - 1,
        "the dismissed card should have left the deck"
    );
}
