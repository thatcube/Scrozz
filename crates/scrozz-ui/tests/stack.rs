//! Logic tests for the capture stack — layout, state machine, gestures.
//!
//! Everything here runs with **no display**. The stack takes an explicit
//! [`Motion`] instant (D25), so a "frame" is a pure function call and a test can
//! ask for t = 180 ms directly without stepping through the frames before it.

use egui::{Rect, Vec2, pos2, vec2};
use scrozz_ui::motion::Motion;
use scrozz_ui::stack::{
    CaptureStack, CardFrame, CardId, CardMetrics, CardState, Dir, GestureConfig, Intent, MAX_SLOTS,
    MIN_SLOTS, StackLayout, Timing, classify,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A 16-inch MacBook Pro's work area: full screen less the menu bar and Dock.
fn mbp16() -> Rect {
    Rect::from_min_size(pos2(0.0, 37.0), vec2(1728.0, 1022.0))
}

fn work_area(height: f32) -> Rect {
    Rect::from_min_size(pos2(0.0, 0.0), vec2(1440.0, height))
}

fn stack() -> CaptureStack {
    CaptureStack::for_work_area(mbp16())
}

fn at(ms: u64) -> Motion {
    Motion::at_ms(ms)
}

/// Long enough for every scripted animation to have finished.
const SETTLED: u64 = 4_000;

fn frame_of(frames: &[CardFrame], id: CardId) -> CardFrame {
    *frames
        .iter()
        .find(|f| f.id == id)
        .unwrap_or_else(|| panic!("no frame for {id:?}"))
}

fn assert_rect_eq(a: Rect, b: Rect, what: &str) {
    let tol = 0.01;
    assert!(
        (a.min.x - b.min.x).abs() < tol
            && (a.min.y - b.min.y).abs() < tol
            && (a.max.x - b.max.x).abs() < tol
            && (a.max.y - b.max.y).abs() < tol,
        "{what}: {a:?} != {b:?}"
    );
}

/// Watches every mutation for a card being handed a higher slot than it has had
/// before. This is the invariant the whole design turns on (D28).
#[derive(Default)]
struct NoMoveUp {
    seen: Vec<(CardId, usize)>,
}

impl NoMoveUp {
    fn check(&mut self, stack: &CaptureStack, after: &str) {
        stack
            .check_no_card_moved_up()
            .unwrap_or_else(|e| panic!("after {after}: {e}"));

        for (id, slot) in stack.slot_snapshot() {
            match self.seen.iter_mut().find(|(seen_id, _)| *seen_id == id) {
                Some((_, best)) => {
                    assert!(
                        slot <= *best,
                        "after {after}: {id:?} moved UP from slot {best} to slot {slot}"
                    );
                    *best = slot;
                }
                None => self.seen.push((id, slot)),
            }
        }

        // A card that has left keeps the slot it left from; it must never have
        // been a higher one than it ever occupied while resident.
        for d in stack.departing() {
            if let Some((_, best)) = self.seen.iter().find(|(id, _)| *id == d.id()) {
                assert!(
                    d.from_slot() <= *best,
                    "after {after}: {:?} departed from slot {} having only reached {best}",
                    d.id(),
                    d.from_slot()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Slot derivation
// ---------------------------------------------------------------------------

#[test]
fn sixteen_inch_macbook_pro_gets_six_slots() {
    assert_eq!(StackLayout::new(mbp16(), CardMetrics::default()).slots(), 6);
}

#[test]
fn slot_count_derives_from_work_area_height() {
    let m = CardMetrics::default();
    let cases = [
        (1022.0, 6), // 16" MacBook Pro
        (861.0, 5),  // 13" laptop
        (745.0, 4),  // 1280x800 panel
        (580.0, 3),  // 1024x600 netbook
        (300.0, 1),  // a sliver
    ];
    for (h, want) in cases {
        let layout = StackLayout::new(work_area(h), m);
        assert_eq!(layout.slots(), want, "work area {h} tall");
    }
}

#[test]
fn slot_count_clamps_at_both_ends() {
    let m = CardMetrics::default();
    assert_eq!(
        StackLayout::new(work_area(100.0), m).slots(),
        MIN_SLOTS,
        "a work area too short for one card still offers one slot"
    );
    assert_eq!(
        StackLayout::new(work_area(4000.0), m).slots(),
        MAX_SLOTS,
        "a very tall display does not turn the pile into a file manager"
    );
}

#[test]
fn slot_count_follows_card_metrics_not_a_constant() {
    let small = CardMetrics {
        height: 60.0,
        gap: 6.0,
        ..CardMetrics::default()
    };
    // Same work area, smaller cards, more slots — up to the cap.
    assert_eq!(StackLayout::new(work_area(580.0), small).slots(), MAX_SLOTS);
    assert_eq!(
        StackLayout::new(work_area(580.0), CardMetrics::default()).slots(),
        3
    );
}

#[test]
fn slots_stack_upward_from_the_bottom_of_the_work_area() {
    let layout = StackLayout::new(mbp16(), CardMetrics::default());
    let m = layout.metrics();

    let slot0 = layout.slot_rect(0);
    assert_rect_eq(
        slot0,
        Rect::from_min_size(
            pos2(
                mbp16().left() + m.margin,
                mbp16().bottom() - m.margin - m.height,
            ),
            vec2(m.width, m.height),
        ),
        "slot 0 sits on the bottom-left of the work area",
    );

    for slot in 1..layout.slots() {
        let below = layout.slot_rect(slot - 1);
        let here = layout.slot_rect(slot);
        assert!(
            here.top() < below.top(),
            "slot {slot} must be higher on screen than slot {}",
            slot - 1
        );
        assert_eq!(here.left(), below.left(), "the pile has one left edge");
        assert!(
            (below.top() - here.top() - m.pitch()).abs() < 0.01,
            "slots are one pitch apart"
        );
    }
}

#[test]
fn the_pile_never_reaches_above_the_work_area() {
    let layout = StackLayout::new(mbp16(), CardMetrics::default());
    let top = layout.slot_rect(layout.slots() - 1);
    assert!(
        top.top() >= layout.work_area().top(),
        "top slot {} escapes the work area",
        layout.slots() - 1
    );
}

#[test]
fn cards_enter_from_off_the_left_edge() {
    let layout = StackLayout::new(mbp16(), CardMetrics::default());
    for slot in 0..layout.slots() {
        let entry = layout.entry_rect(slot);
        let rest = layout.slot_rect(slot);
        assert!(
            entry.right() < layout.work_area().left(),
            "slot {slot} entry must start fully off-screen"
        );
        assert_eq!(entry.top(), rest.top(), "entry is horizontal only");
    }
}

// ---------------------------------------------------------------------------
// Filling the pile
// ---------------------------------------------------------------------------

#[test]
fn first_capture_lands_at_slot_zero() {
    let mut s = stack();
    let id = s.push(&at(0));
    assert_eq!(s.slot_of(id), Some(0));

    let f = s.frame_of(id, &at(SETTLED)).unwrap();
    assert_eq!(f.slot, 0);
    assert_rect_eq(f.rect, s.layout().slot_rect(0), "settled at slot 0");
}

#[test]
fn captures_fill_slots_upward_in_order() {
    let mut s = stack();
    let ids: Vec<_> = (0..s.capacity())
        .map(|i| s.push(&at(i as u64 * 500)))
        .collect();

    for (expected_slot, id) in ids.iter().enumerate() {
        assert_eq!(
            s.slot_of(*id),
            Some(expected_slot),
            "capture {expected_slot} belongs in slot {expected_slot}"
        );
    }
    assert_eq!(s.len(), s.capacity());
}

#[test]
fn existing_cards_do_not_move_at_all_while_the_pile_grows() {
    let mut s = stack();
    let mut watcher = NoMoveUp::default();
    let mut t = 0;

    let mut settled: Vec<(CardId, Rect)> = Vec::new();
    for n in 0..s.capacity() {
        s.push(&at(t));
        t += SETTLED;
        s.advance(&at(t));
        watcher.check(&s, &format!("push #{n}"));

        let frames = s.frame(&at(t));
        for (id, was) in &settled {
            let now = frame_of(&frames, *id);
            assert_rect_eq(
                now.rect,
                *was,
                "a card already in the pile moved on arrival",
            );
        }
        settled = frames.iter().map(|f| (f.id, f.rect)).collect();
    }
}

#[test]
fn an_arriving_card_is_the_only_thing_animating() {
    let mut s = stack();
    s.push(&at(0));
    s.advance(&at(SETTLED));
    assert!(!s.is_animating(&at(SETTLED)), "a settled pile is idle");

    s.push(&at(SETTLED));
    let frames = s.frame(&at(SETTLED + 40));
    assert_eq!(frames[0].state, CardState::Resting, "slot 0 does not stir");
    assert_eq!(frames[1].state, CardState::Entering);
}

// ---------------------------------------------------------------------------
// Overflow
// ---------------------------------------------------------------------------

#[test]
fn overflow_evicts_exactly_the_oldest() {
    let mut s = stack();
    let ids: Vec<_> = (0..s.capacity()).map(|_| s.push(&at(0))).collect();
    s.advance(&at(SETTLED));

    let newcomer = s.push(&at(SETTLED));

    assert_eq!(s.len(), s.capacity(), "the pile stays exactly full");
    assert_eq!(s.slot_of(ids[0]), None, "the oldest capture is gone");
    assert_eq!(s.departing().len(), 1, "exactly one card is leaving");
    assert_eq!(s.departing()[0].id(), ids[0]);

    for (i, id) in ids.iter().skip(1).enumerate() {
        assert_eq!(s.slot_of(*id), Some(i), "survivors fell exactly one slot");
    }
    assert_eq!(
        s.slot_of(newcomer),
        Some(s.capacity() - 1),
        "the new capture arrives at the top"
    );
}

#[test]
fn the_evicted_card_leaves_from_the_bottom_going_left() {
    let mut s = stack();
    for _ in 0..s.capacity() {
        s.push(&at(0));
    }
    s.advance(&at(SETTLED));
    s.push(&at(SETTLED));

    let leaving = &s.departing()[0];
    assert_eq!(leaving.from_slot(), 0, "it leaves from the bottom");
    assert_eq!(leaving.direction(), Dir::Left, "the same way it arrived");

    let early = frame_of(&s.frame(&at(SETTLED + 20)), leaving.id());
    let later = frame_of(&s.frame(&at(SETTLED + 120)), leaving.id());
    assert!(later.rect.left() < early.rect.left(), "it travels leftward");
    assert!(
        (later.rect.top() - early.rect.top()).abs() < 0.01,
        "and does not drift vertically on the way out"
    );
}

#[test]
fn overflow_and_manual_dismissal_use_the_same_exit() {
    let mut s = stack();
    for _ in 0..s.capacity() {
        s.push(&at(0));
    }
    s.advance(&at(SETTLED));

    let mut overflowed = s.clone();
    overflowed.push(&at(SETTLED));

    let mut dismissed = s.clone();
    let oldest = dismissed.cards()[0].id();
    dismissed.dismiss(oldest, &at(SETTLED));

    let a = frame_of(&overflowed.frame(&at(SETTLED + 100)), oldest);
    let b = frame_of(&dismissed.frame(&at(SETTLED + 100)), oldest);
    assert_rect_eq(a.rect, b.rect, "one shared departure animation (D21)");
}

// ---------------------------------------------------------------------------
// Dismissal
// ---------------------------------------------------------------------------

#[test]
fn dismissing_a_middle_card_drops_only_the_cards_above_it() {
    let mut s = stack();
    let ids: Vec<_> = (0..s.capacity()).map(|_| s.push(&at(0))).collect();
    s.advance(&at(SETTLED));

    let below: Vec<Rect> = s.frame(&at(SETTLED))[..2].iter().map(|f| f.rect).collect();

    s.dismiss(ids[2], &at(SETTLED));
    s.advance(&at(SETTLED * 2));

    assert_eq!(s.slot_of(ids[0]), Some(0), "cards below never move");
    assert_eq!(s.slot_of(ids[1]), Some(1), "cards below never move");
    assert_eq!(s.slot_of(ids[2]), None);
    for (i, id) in ids.iter().skip(3).enumerate() {
        assert_eq!(s.slot_of(*id), Some(2 + i), "cards above fell exactly one");
    }

    let after = s.frame(&at(SETTLED * 2));
    for (i, was) in below.iter().enumerate() {
        assert_rect_eq(after[i].rect, *was, "a card below the gap moved");
    }
}

#[test]
fn cards_above_a_gap_fall_downward_only() {
    let mut s = stack();
    let ids: Vec<_> = (0..s.capacity()).map(|_| s.push(&at(0))).collect();
    s.advance(&at(SETTLED));

    let before = frame_of(&s.frame(&at(SETTLED)), ids[4]).rect;
    s.dismiss(ids[1], &at(SETTLED));

    let mut previous = before.top();
    for step in 1..=20 {
        let now = frame_of(&s.frame(&at(SETTLED + step * 15)), ids[4]).rect;
        assert!(
            now.top() >= previous - 0.01,
            "a falling card rose at t+{}ms",
            step * 15
        );
        previous = now.top();
    }
    assert!(previous > before.top(), "and it did in fact fall");
}

#[test]
fn no_card_ever_rises_on_screen_during_a_scripted_sequence() {
    // The pixel-level reading of D28: sample every frame through a long
    // sequence and assert no card's top edge ever decreases. User drags and
    // dock expansion are excluded — both are transient offsets the user asked
    // for, not the pile reassigning a slot.
    let mut s = stack();
    let mut highest: Vec<(CardId, f32)> = Vec::new();
    let mut t = 0u64;

    let sample = |s: &CaptureStack, highest: &mut Vec<(CardId, f32)>, t: u64| {
        for f in s.frame(&at(t)) {
            match highest.iter_mut().find(|(id, _)| *id == f.id) {
                Some((_, top)) => {
                    assert!(
                        f.rect.top() >= *top - 0.5,
                        "{:?} rose from y={} to y={} at t={t}ms",
                        f.id,
                        top,
                        f.rect.top()
                    );
                    *top = f.rect.top();
                }
                None => highest.push((f.id, f.rect.top())),
            }
        }
    };

    // Fill past capacity, then dismiss from the middle repeatedly, sampling
    // every 8 ms throughout.
    for round in 0..14 {
        if round % 3 == 2 && s.len() > 2 {
            let id = s.cards()[1].id();
            s.dismiss(id, &at(t));
        } else {
            s.push(&at(t));
        }
        for _ in 0..60 {
            t += 8;
            sample(&s, &mut highest, t);
            s.advance(&at(t));
        }
    }
    s.check_no_card_moved_up().unwrap();
}

#[test]
fn the_fall_curve_must_not_overshoot() {
    // An overshoot settles by travelling back the way it came. For a fall that
    // is upward motion, which D28 forbids — so the invariant, not taste, picks
    // this curve.
    assert!(
        !Timing::default().fall_ease.overshoots(),
        "a bouncing fall lifts the card back up past its slot"
    );
}

#[test]
fn dismissing_the_top_card_moves_nothing() {
    let mut s = stack();
    let ids: Vec<_> = (0..s.capacity()).map(|_| s.push(&at(0))).collect();
    s.advance(&at(SETTLED));
    let before: Vec<Rect> = s.frame(&at(SETTLED)).iter().map(|f| f.rect).collect();

    s.dismiss(*ids.last().unwrap(), &at(SETTLED));
    let after = s.frame(&at(SETTLED + 10));

    for (i, was) in before.iter().take(s.capacity() - 1).enumerate() {
        assert_rect_eq(after[i].rect, *was, "slot {i} stirred for no reason");
    }
}

#[test]
fn dismiss_all_empties_the_pile() {
    let mut s = stack();
    for _ in 0..s.capacity() {
        s.push(&at(0));
    }
    s.advance(&at(SETTLED));

    s.dismiss_all(&at(SETTLED));
    assert!(s.is_empty());
    assert_eq!(s.departing().len(), 6, "every card is on its way out");

    s.advance(&at(SETTLED * 2));
    assert!(s.departing().is_empty(), "and eventually gone");
    assert!(!s.is_animating(&at(SETTLED * 2)));
}

#[test]
fn dismissing_an_unknown_card_is_a_no_op() {
    let mut s = stack();
    s.push(&at(0));
    assert!(!s.dismiss(CardId(999), &at(0)));
    assert_eq!(s.len(), 1);
}

// ---------------------------------------------------------------------------
// The invariant
// ---------------------------------------------------------------------------

#[test]
fn a_card_never_moves_up_across_every_operation() {
    let mut s = stack();
    let mut watcher = NoMoveUp::default();
    let mut t = 0u64;
    let tick = |t: &mut u64| {
        *t += 250;
        at(*t)
    };

    // Fill.
    for n in 0..s.capacity() {
        s.push(&tick(&mut t));
        watcher.check(&s, &format!("push {n}"));
    }
    // Overflow, repeatedly.
    for n in 0..8 {
        s.push(&tick(&mut t));
        watcher.check(&s, &format!("overflow {n}"));
        s.advance(&tick(&mut t));
        watcher.check(&s, &format!("advance after overflow {n}"));
    }
    // Dismiss from the middle, the bottom and the top.
    for pick in [2usize, 0, 1] {
        if let Some(card) = s.cards().get(pick) {
            let id = card.id();
            s.dismiss(id, &tick(&mut t));
            watcher.check(&s, &format!("dismiss slot {pick}"));
        }
    }
    // Refill and shrink the display underneath it.
    for _ in 0..6 {
        s.push(&tick(&mut t));
    }
    watcher.check(&s, "refill");
    s.resize(work_area(580.0), &tick(&mut t));
    watcher.check(&s, "shrink to a 3-slot display");
    s.resize(mbp16(), &tick(&mut t));
    watcher.check(&s, "grow back to 6 slots");

    // Collapse and expand.
    s.collapse(&tick(&mut t));
    watcher.check(&s, "collapse");
    s.push(&tick(&mut t));
    watcher.check(&s, "capture while stowed");
    s.expand(&tick(&mut t));
    watcher.check(&s, "expand");

    // Drag, in every direction.
    for dir in Dir::ALL {
        if s.is_empty() {
            s.push(&tick(&mut t));
        }
        let id = s.cards()[0].id();
        let origin = pos2(120.0, 900.0);
        s.begin_drag(id, origin, &tick(&mut t));
        s.drag_to(origin + dir.unit() * 200.0, &tick(&mut t));
        s.release_drag(&tick(&mut t));
        watcher.check(&s, &format!("drag {dir:?}"));
    }

    s.dismiss_all(&tick(&mut t));
    watcher.check(&s, "dismiss all");
}

#[test]
fn growing_the_display_moves_nothing() {
    let mut s = CaptureStack::for_work_area(work_area(580.0));
    for _ in 0..s.capacity() {
        s.push(&at(0));
    }
    s.advance(&at(SETTLED));
    let ids: Vec<CardId> = s.cards().iter().map(|c| c.id()).collect();

    s.resize(work_area(1022.0), &at(SETTLED));

    assert_eq!(s.capacity(), 6, "the taller display offers more slots");
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(s.slot_of(*id), Some(i), "nothing was reshuffled");
    }
    s.check_no_card_moved_up().unwrap();
}

#[test]
fn shrinking_the_display_retires_from_the_bottom() {
    let mut s = stack();
    let ids: Vec<_> = (0..6).map(|_| s.push(&at(0))).collect();
    s.advance(&at(SETTLED));

    s.resize(work_area(580.0), &at(SETTLED));

    assert_eq!(s.capacity(), 3);
    assert_eq!(s.len(), 3);
    for id in &ids[..3] {
        assert_eq!(s.slot_of(*id), None, "the three oldest were retired");
    }
    for (i, id) in ids[3..].iter().enumerate() {
        assert_eq!(s.slot_of(*id), Some(i));
    }
    s.check_no_card_moved_up().unwrap();
}

// ---------------------------------------------------------------------------
// Gestures — direction is intent (D21)
// ---------------------------------------------------------------------------

#[test]
fn direction_maps_to_intent() {
    assert_eq!(Dir::Left.intent(), Intent::Dismiss);
    assert_eq!(Dir::Right.intent(), Intent::DragOut);
    assert_eq!(Dir::Up.intent(), Intent::DragOut);
    assert_eq!(Dir::Down.intent(), Intent::Collapse);
}

#[test]
fn a_committed_swipe_classifies_by_direction() {
    let cfg = GestureConfig::default();
    let far = 400.0;
    let cases = [
        (vec2(-far, 0.0), Intent::Dismiss, Dir::Left),
        (vec2(far, 0.0), Intent::DragOut, Dir::Right),
        (vec2(0.0, -far), Intent::DragOut, Dir::Up),
        (vec2(0.0, far), Intent::Collapse, Dir::Down),
    ];
    for (travel, intent, dir) in cases {
        assert_eq!(
            classify(travel, Vec2::ZERO, &cfg),
            (intent, Some(dir)),
            "travel {travel:?}"
        );
    }
}

#[test]
fn a_short_slow_drag_springs_back() {
    let cfg = GestureConfig::default();
    for dir in Dir::ALL {
        let travel = dir.unit() * 20.0;
        assert_eq!(
            classify(travel, dir.unit() * 40.0, &cfg),
            (Intent::SpringBack, None),
            "{dir:?} should not have committed"
        );
    }
}

#[test]
fn distance_alone_commits_a_gesture() {
    let cfg = GestureConfig::default();
    let travel = vec2(-(cfg.dismiss_dist + 1.0), 0.0);
    assert_eq!(
        classify(travel, Vec2::ZERO, &cfg),
        (Intent::Dismiss, Some(Dir::Left)),
        "a long, dead-slow drag is still a clear statement of intent"
    );
    let short = vec2(-(cfg.dismiss_dist - 1.0), 0.0);
    assert_eq!(classify(short, Vec2::ZERO, &cfg).0, Intent::SpringBack);
}

#[test]
fn speed_alone_commits_a_gesture() {
    let cfg = GestureConfig::default();
    let flick = vec2(-(cfg.dismiss_vel + 1.0), 0.0);
    assert_eq!(
        classify(vec2(-4.0, 0.0), flick, &cfg),
        (Intent::Dismiss, Some(Dir::Left)),
        "a sharp flick that barely moved is still a dismiss"
    );
    let slow = vec2(-(cfg.dismiss_vel - 1.0), 0.0);
    assert_eq!(classify(vec2(-4.0, 0.0), slow, &cfg).0, Intent::SpringBack);
}

#[test]
fn collapse_commits_sooner_than_dismiss() {
    let cfg = GestureConfig::default();
    assert!(
        cfg.collapse_dist < cfg.dismiss_dist && cfg.collapse_vel < cfg.dismiss_vel,
        "collapse costs nothing to undo, so it should ask for less conviction"
    );
    let travel = 95.0;
    assert_eq!(
        classify(vec2(0.0, travel), Vec2::ZERO, &cfg).0,
        Intent::Collapse
    );
    assert_eq!(
        classify(vec2(-travel, 0.0), Vec2::ZERO, &cfg).0,
        Intent::SpringBack,
        "the same travel leftward is not yet a dismiss"
    );
}

#[test]
fn the_strongest_direction_wins_a_diagonal() {
    let cfg = GestureConfig::default();
    // Mostly left, a little down: a dismiss, not a collapse.
    assert_eq!(
        classify(vec2(-400.0, 100.0), Vec2::ZERO, &cfg),
        (Intent::Dismiss, Some(Dir::Left))
    );
    // Mostly down, a little left: a collapse.
    assert_eq!(
        classify(vec2(-100.0, 400.0), Vec2::ZERO, &cfg),
        (Intent::Collapse, Some(Dir::Down))
    );
}

#[test]
fn classification_is_deterministic_for_a_perfect_tie() {
    let cfg = GestureConfig::default();
    let both = vec2(-400.0, 0.0);
    let once = classify(both, Vec2::ZERO, &cfg);
    for _ in 0..50 {
        assert_eq!(classify(both, Vec2::ZERO, &cfg), once);
    }
}

#[test]
fn a_swipe_left_dismisses_the_card() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));

    let origin = pos2(120.0, 900.0);
    s.begin_drag(id, origin, &at(SETTLED));
    s.drag_to(origin + vec2(-200.0, 0.0), &at(SETTLED + 60));
    let release = s.release_drag(&at(SETTLED + 80)).unwrap();

    assert_eq!(release.intent, Intent::Dismiss);
    assert_eq!(release.direction, Some(Dir::Left));
    assert!(s.is_empty());
    assert_eq!(s.departing()[0].direction(), Dir::Left);
}

#[test]
fn a_swipe_right_or_up_begins_a_drag_out() {
    for (delta, dir) in [(vec2(240.0, 0.0), Dir::Right), (vec2(0.0, -240.0), Dir::Up)] {
        let mut s = stack();
        let id = s.push(&at(0));
        s.advance(&at(SETTLED));

        let origin = pos2(120.0, 900.0);
        s.begin_drag(id, origin, &at(SETTLED));
        s.drag_to(origin + delta, &at(SETTLED + 60));
        let release = s.release_drag(&at(SETTLED + 80)).unwrap();

        assert_eq!(release.intent, Intent::DragOut, "{dir:?}");
        assert_eq!(release.direction, Some(dir));
        assert!(s.is_empty(), "the capture leaves the pile with the drag");
    }
}

#[test]
fn an_unavailable_promised_file_drag_springs_back_without_retiring_the_card() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));

    let origin = pos2(120.0, 900.0);
    s.begin_drag(id, origin, &at(SETTLED));
    s.drag_to(origin + vec2(240.0, 0.0), &at(SETTLED + 60));
    let release = s
        .release_drag_with_promised_file(&at(SETTLED + 80), false)
        .unwrap();

    assert_eq!(release.intent, Intent::SpringBack);
    assert_eq!(release.direction, None);
    assert_eq!(
        s.slot_of(id),
        Some(0),
        "unsupported drag must keep the card"
    );
    assert!(
        s.departing().is_empty(),
        "descriptor scaffolding is not a successful OS drop"
    );
}

#[test]
fn a_swipe_down_collapses_without_dismissing() {
    let mut s = stack();
    let a = s.push(&at(0));
    let b = s.push(&at(0));
    s.advance(&at(SETTLED));

    let origin = pos2(120.0, 900.0);
    s.begin_drag(b, origin, &at(SETTLED));
    s.drag_to(origin + vec2(0.0, 200.0), &at(SETTLED + 60));
    let release = s.release_drag(&at(SETTLED + 80)).unwrap();

    assert_eq!(release.intent, Intent::Collapse);
    assert_eq!(s.len(), 2, "collapsing must never dismiss (D20)");
    assert_eq!(s.slot_of(a), Some(0));
    assert_eq!(s.slot_of(b), Some(1));
    assert!(s.dock().is_stowing());
}

#[test]
fn an_uncommitted_drag_springs_back_to_its_slot() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    let home = s.frame_of(id, &at(SETTLED)).unwrap().rect;

    let origin = pos2(120.0, 900.0);
    s.begin_drag(id, origin, &at(SETTLED));
    s.drag_to(origin + vec2(-30.0, 0.0), &at(SETTLED + 400));
    let release = s.release_drag(&at(SETTLED + 500)).unwrap();

    assert_eq!(release.intent, Intent::SpringBack);
    assert_eq!(s.len(), 1);
    s.advance(&at(SETTLED * 2));
    assert_rect_eq(
        s.frame_of(id, &at(SETTLED * 2)).unwrap().rect,
        home,
        "it must land back exactly where it was",
    );
}

#[test]
fn a_held_card_follows_the_pointer_one_to_one() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    let home = s.frame_of(id, &at(SETTLED)).unwrap().rect;

    let origin = pos2(120.0, 900.0);
    s.begin_drag(id, origin, &at(SETTLED));
    let delta = vec2(-42.0, 17.0);
    s.drag_to(origin + delta, &at(SETTLED + 16));

    let f = s.frame_of(id, &at(SETTLED + 16)).unwrap();
    assert_rect_eq(
        f.rect,
        home.translate(delta),
        "a card must not lag the finger",
    );
    assert_eq!(f.state, CardState::Dragging);
    assert_eq!(f.lift, 1.0, "lift is instant, never eased (D19)");
}

#[test]
fn a_cancelled_drag_returns_the_card() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    let home = s.frame_of(id, &at(SETTLED)).unwrap().rect;

    let origin = pos2(120.0, 900.0);
    s.begin_drag(id, origin, &at(SETTLED));
    s.drag_to(origin + vec2(-400.0, 0.0), &at(SETTLED + 40));
    s.cancel_drag(&at(SETTLED + 60));

    assert_eq!(s.len(), 1, "a cancelled gesture commits to nothing");
    s.advance(&at(SETTLED * 2));
    assert_rect_eq(s.frame_of(id, &at(SETTLED * 2)).unwrap().rect, home, "home");
}

#[test]
fn a_dead_stop_before_release_reads_as_zero_speed() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));

    // Drag decisively, then hold still well past the velocity window.
    let origin = pos2(120.0, 900.0);
    s.begin_drag(id, origin, &at(SETTLED));
    let held = origin + vec2(-40.0, 0.0);
    s.drag_to(held, &at(SETTLED + 40));
    for step in 1..=10 {
        s.drag_to(held, &at(SETTLED + 40 + step * 30));
    }
    let release = s.release_drag(&at(SETTLED + 400)).unwrap();

    assert_eq!(
        release.velocity,
        Vec2::ZERO,
        "a stopped pointer must not read as a throw"
    );
    assert_eq!(release.intent, Intent::SpringBack);
}

#[test]
fn a_hard_flick_leaves_faster_than_a_shove() {
    let make = |samples: &[(u64, f32)]| {
        let mut s = stack();
        let id = s.push(&at(0));
        s.advance(&at(SETTLED));
        let origin = pos2(400.0, 900.0);
        s.begin_drag(id, origin, &at(SETTLED));
        for (ms, dx) in samples {
            s.drag_to(origin + vec2(*dx, 0.0), &at(SETTLED + ms));
        }
        let last = samples.last().unwrap().0;
        s.release_drag(&at(SETTLED + last)).unwrap();
        s
    };

    // Same travel, very different speed.
    let flick = make(&[(8, -60.0), (16, -130.0), (24, -200.0)]);
    let shove = make(&[(200, -70.0), (400, -140.0), (600, -200.0)]);

    let id = flick.departing()[0].id();
    let t = SETTLED + 700;
    let flick_x = frame_of(&flick.frame(&at(t + 60)), id).rect.left();
    let shove_x = frame_of(&shove.frame(&at(SETTLED + 660)), shove.departing()[0].id())
        .rect
        .left();
    assert!(
        flick_x <= shove_x,
        "a flung card should be no slower to leave than a shoved one"
    );
}

// ---------------------------------------------------------------------------
// Hover chrome
// ---------------------------------------------------------------------------

#[test]
fn chrome_is_hidden_at_rest_and_revealed_on_hover() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    assert_eq!(
        s.frame_of(id, &at(SETTLED)).unwrap().reveal,
        0.0,
        "at rest a card carries no controls"
    );

    s.set_hover(Some(id), &at(SETTLED));
    assert!(s.frame_of(id, &at(SETTLED + 200)).unwrap().reveal > 0.9);

    s.set_hover(None, &at(SETTLED + 200));
    assert!(s.frame_of(id, &at(SETTLED + 500)).unwrap().reveal < 0.05);
}

#[test]
fn an_interrupted_reveal_retracts_continuously() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));

    s.set_hover(Some(id), &at(SETTLED));
    let half = s.frame_of(id, &at(SETTLED + 70)).unwrap().reveal;
    assert!(half > 0.1 && half < 0.9, "should be mid-reveal, was {half}");

    s.set_hover(None, &at(SETTLED + 70));
    let just_after = s.frame_of(id, &at(SETTLED + 71)).unwrap().reveal;
    assert!(
        (just_after - half).abs() < 0.1,
        "reversal jumped: {half} -> {just_after}"
    );
}

#[test]
fn chrome_hides_while_a_card_is_held() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    s.set_hover(Some(id), &at(SETTLED));
    assert!(s.frame_of(id, &at(SETTLED + 300)).unwrap().reveal > 0.9);

    s.begin_drag(id, pos2(120.0, 900.0), &at(SETTLED + 300));
    assert_eq!(s.frame_of(id, &at(SETTLED + 300)).unwrap().reveal, 0.0);
}

// ---------------------------------------------------------------------------
// The dock (D20)
// ---------------------------------------------------------------------------

#[test]
fn the_dock_is_card_width_and_a_sixth_the_height() {
    let layout = StackLayout::new(mbp16(), CardMetrics::default());
    let slot0 = layout.slot_rect(0);
    let dock = layout.dock_rect();

    assert!((dock.width() - slot0.width()).abs() < 0.01, "same width");
    assert!(
        dock.height() < slot0.height() / 4.0,
        "and much shorter: {} vs {}",
        dock.height(),
        slot0.height()
    );
    assert!(
        (dock.bottom() - slot0.bottom()).abs() < 0.01,
        "shares the floor"
    );
}

#[test]
fn collapsing_stows_every_card_without_dismissing_any() {
    let mut s = stack();
    for _ in 0..s.capacity() {
        s.push(&at(0));
    }
    s.advance(&at(SETTLED));

    s.collapse(&at(SETTLED));
    s.advance(&at(SETTLED * 2));

    assert_eq!(s.len(), 6, "nothing was lost");
    assert!(s.dock().is_collapsed());

    let dock_rect = s.layout().dock_rect();
    for f in s.frame(&at(SETTLED * 2)) {
        assert_rect_eq(f.rect, dock_rect, "every card is absorbed into the dock");
    }
}

#[test]
fn cards_travel_into_the_dock_rather_than_fading() {
    let mut s = stack();
    for _ in 0..3 {
        s.push(&at(0));
    }
    s.advance(&at(SETTLED));
    let top = s.cards()[2].id();
    let home = s.frame_of(top, &at(SETTLED)).unwrap().rect;

    s.collapse(&at(SETTLED));
    let mid = s.frame_of(top, &at(SETTLED + 160)).unwrap().rect;

    assert!(mid.top() > home.top(), "it must move down toward the dock");
    assert!(mid.height() < home.height(), "and shrink on the way");
    assert!(
        mid.top() < s.layout().dock_rect().top(),
        "and not have arrived yet"
    );
}

#[test]
fn expanding_brings_the_pile_back() {
    let mut s = stack();
    let ids: Vec<_> = (0..4).map(|_| s.push(&at(0))).collect();
    s.advance(&at(SETTLED));
    let before: Vec<Rect> = s.frame(&at(SETTLED)).iter().map(|f| f.rect).collect();

    s.collapse(&at(SETTLED));
    s.advance(&at(SETTLED * 2));
    s.expand(&at(SETTLED * 2));
    s.advance(&at(SETTLED * 3));

    for (i, id) in ids.iter().enumerate() {
        assert_eq!(s.slot_of(*id), Some(i));
        assert_rect_eq(
            s.frame_of(*id, &at(SETTLED * 3)).unwrap().rect,
            before[i],
            "a restored card must land back in its slot",
        );
    }
}

#[test]
fn an_interrupted_collapse_reverses_from_where_it_is() {
    let mut s = stack();
    s.push(&at(0));
    s.advance(&at(SETTLED));

    s.collapse(&at(SETTLED));
    let half = s.dock().collapse_progress(&at(SETTLED + 160));
    assert!(half > 0.1 && half < 0.9, "should be mid-collapse: {half}");

    s.expand(&at(SETTLED + 160));
    let just_after = s.dock().collapse_progress(&at(SETTLED + 161));
    assert!(
        (just_after - half).abs() < 0.1,
        "the reversal jumped: {half} -> {just_after}"
    );
}

#[test]
fn a_capture_taken_while_stowed_brings_the_dock_back_up() {
    let mut s = stack();
    s.push(&at(0));
    s.collapse(&at(SETTLED));
    s.advance(&at(SETTLED * 2));
    assert!(s.dock().is_collapsed());

    let id = s.push(&at(SETTLED * 2));
    assert!(
        !s.dock().is_stowing(),
        "a new capture is worth seeing — collapse meant 'get out of the way', \
         not 'stop showing me captures'"
    );
    assert_eq!(s.slot_of(id), Some(1));
}

#[test]
fn toggling_the_dock_alternates() {
    let mut s = stack();
    s.push(&at(0));
    s.advance(&at(SETTLED));

    s.toggle_dock(&at(SETTLED));
    assert!(s.dock().is_stowing());
    s.advance(&at(SETTLED * 2));

    s.toggle_dock(&at(SETTLED * 2));
    assert!(!s.dock().is_stowing());
}

// ---------------------------------------------------------------------------
// Determinism (D25)
// ---------------------------------------------------------------------------

#[test]
fn the_same_instant_always_yields_the_same_layout() {
    let mut s = stack();
    for i in 0..4 {
        s.push(&at(i * 90));
    }
    let once = s.frame(&at(180));
    for _ in 0..20 {
        assert_eq!(s.frame(&at(180)), once, "frame() must be pure");
    }
}

#[test]
fn jumping_to_an_instant_matches_stepping_to_it() {
    let build = || {
        let mut s = stack();
        for i in 0..6 {
            s.push(&at(i * 40));
        }
        s
    };

    let jumped = build();
    let direct = jumped.frame(&at(180));

    let mut stepped = build();
    for f in 0..=10u64 {
        let m = at(f * 18);
        let _ = stepped.frame(&m);
        stepped.advance(&m);
    }
    let walked = stepped.frame(&at(180));

    assert_eq!(direct.len(), walked.len());
    for (a, b) in direct.iter().zip(&walked) {
        assert_eq!(a.id, b.id);
        assert_rect_eq(a.rect, b.rect, "walking the clock changed the answer");
    }
}

#[test]
fn two_stacks_driven_identically_render_identically() {
    let script = |s: &mut CaptureStack| {
        for i in 0..8 {
            s.push(&at(i * 120));
        }
        let id = s.cards()[2].id();
        s.dismiss(id, &at(1000));
        s.set_hover(Some(s.cards()[0].id()), &at(1050));
        s.collapse(&at(1100));
        s.expand(&at(1200));
    };
    let mut a = stack();
    let mut b = stack();
    script(&mut a);
    script(&mut b);

    for t in [0, 40, 180, 500, 1150, 1400, 5000] {
        assert_eq!(a.frame(&at(t)), b.frame(&at(t)), "divergence at t={t}ms");
    }
}

#[test]
fn advance_does_not_change_what_a_frame_looks_like() {
    let mut s = stack();
    for i in 0..6 {
        s.push(&at(i * 50));
    }
    let before = s.frame(&at(400));
    s.advance(&at(400));
    assert_eq!(s.frame(&at(400)), before, "advance is bookkeeping only");
}

#[test]
fn the_stack_goes_idle_once_everything_has_landed() {
    let mut s = stack();
    for i in 0..6 {
        s.push(&at(i * 50));
    }
    assert!(s.is_animating(&at(300)));
    s.advance(&at(SETTLED));
    assert!(
        !s.is_animating(&at(SETTLED)),
        "an idle overlay must let the app sleep (D19)"
    );
}

#[test]
fn reduce_motion_reaches_the_end_state_immediately() {
    let mut s = stack();
    let calm = Motion::at_ms(0).with_reduce_motion(true);
    for _ in 0..6 {
        s.push(&calm);
    }
    let frames = s.frame(&calm);
    for (slot, f) in frames.iter().enumerate() {
        assert_eq!(f.state, CardState::Resting, "nothing should be in flight");
        assert_rect_eq(f.rect, s.layout().slot_rect(slot), "already home");
    }
    assert!(!s.is_animating(&calm));
    s.check_no_card_moved_up().unwrap();
}

#[test]
fn reduced_timing_tokens_leave_the_state_machine_intact() {
    let layout = StackLayout::new(mbp16(), CardMetrics::default());
    let mut s = CaptureStack::new(layout, Timing::reduced());
    let ids: Vec<_> = (0..6).map(|_| s.push(&at(0))).collect();
    s.dismiss(ids[2], &at(0));
    s.advance(&at(0));

    assert_eq!(s.len(), 5);
    assert!(s.departing().is_empty(), "the exit finished instantly");
    assert_eq!(s.slot_of(ids[3]), Some(2), "and the pile still closed up");
    s.check_no_card_moved_up().unwrap();
}
