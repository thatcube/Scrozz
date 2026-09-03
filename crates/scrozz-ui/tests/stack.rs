//! Logic tests for the capture stack — layout, state machine, gestures.
//!
//! Everything here runs with **no display**. The stack takes an explicit
//! [`Motion`] instant (D25), so a "frame" is a pure function call and a test can
//! ask for t = 180 ms directly without stepping through the frames before it.

use egui::{Rect, Vec2, pos2, vec2};
use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize, ScaleFactor};
use scrozz_ui::motion::Motion;
use scrozz_ui::stack::{
    CaptureStack, CardFrame, CardId, CardMetrics, CardState, DRAG_LOCK_SLOP, DRAG_SOURCE_ALPHA,
    Dir, GestureConfig, Intent, MIN_SLOTS, RecentCapturesPlacement, StackLayout, Timing, classify,
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
fn sixteen_inch_macbook_pro_uses_preferred_sixteen_by_ten_cards() {
    let metrics = CardMetrics::default();
    assert_eq!(metrics.width, 288.0);
    assert_eq!(metrics.height, 180.0);
    assert_eq!(metrics.gap, 8.0);
    assert_eq!(metrics.margin, 2.0);
    assert_eq!(metrics.side_margin, 40.0);
    assert_eq!(StackLayout::new(mbp16(), metrics).slots(), 5);
}

#[test]
fn slot_count_derives_from_work_area_height() {
    let m = CardMetrics::default();
    let cases = [
        (1022.0, 5), // 16" MacBook Pro
        (861.0, 4),  // 13" laptop
        (745.0, 3),  // 1280x800 panel
        (580.0, 3),  // 1024x600 netbook
        (300.0, 1),  // a sliver
    ];
    for (h, want) in cases {
        let layout = StackLayout::new(work_area(h), m);
        assert_eq!(layout.slots(), want, "work area {h} tall");
    }
}

#[test]
fn slot_count_has_a_visibility_floor_but_no_product_ceiling() {
    let m = CardMetrics::default();
    assert_eq!(
        StackLayout::new(work_area(100.0), m).slots(),
        MIN_SLOTS,
        "a work area too short for one card still offers one slot"
    );
    assert_eq!(StackLayout::new(work_area(4000.0), m).slots(), 21);
}

#[test]
fn tall_work_areas_keep_more_than_six_recent_captures() {
    let layout = StackLayout::new(work_area(2200.0), CardMetrics::default());
    assert_eq!(layout.slots(), 11);
    assert!(layout.slots() > 6);
}

#[test]
fn capacity_matches_the_exact_floor_formula() {
    let metrics = CardMetrics::default();
    for count in 1usize..=12 {
        let occupied = count as f32 * metrics.height + count.saturating_sub(1) as f32 * metrics.gap;
        let layout = StackLayout::new(work_area(occupied + metrics.margin * 2.0), metrics);
        assert_eq!(layout.slots(), count);
    }
}

#[test]
fn preferred_cards_adapt_down_without_losing_sixteen_by_ten() {
    let constrained = Rect::from_min_size(pos2(0.0, 0.0), vec2(250.0, 170.0));
    let layout = StackLayout::new(constrained, CardMetrics::default());
    let metrics = layout.metrics();
    assert!(metrics.width < CardMetrics::PREFERRED_WIDTH);
    assert!(metrics.height < CardMetrics::PREFERRED_HEIGHT);
    assert!((metrics.width / metrics.height - 1.6).abs() < 0.001);
    assert!(layout.slot_rect(0).right() <= constrained.right());
}

#[test]
fn moving_from_a_constrained_display_restores_the_requested_card_size() {
    let constrained = Rect::from_min_size(pos2(0.0, 0.0), vec2(250.0, 170.0));
    let mut stack = CaptureStack::for_work_area(constrained);
    assert!(stack.layout().metrics().width < CardMetrics::PREFERRED_WIDTH);

    stack.resize(mbp16(), &at(100));

    assert_eq!(
        stack.layout().metrics(),
        CardMetrics::default(),
        "effective metrics from the old display must not become the next display's preference"
    );
}

#[test]
fn density_and_monitor_scale_are_applied_once_in_separate_spaces() {
    let compact = CardMetrics::for_density(0.5);
    let regular = CardMetrics::for_density(1.0);
    let large = CardMetrics::for_density(1.4);
    assert_eq!(compact.width, CardMetrics::MIN_WIDTH);
    assert_eq!(regular.width, CardMetrics::PREFERRED_WIDTH);
    assert_eq!(large.width, CardMetrics::MAX_WIDTH);

    for scale in [1.0, 1.5, 2.0] {
        let factor = ScaleFactor::new(scale);
        let physical = LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(f64::from(regular.width), f64::from(regular.height)),
        )
        .to_physical(factor)
        .size;
        assert_eq!(physical.width, (f64::from(regular.width) * scale).round());
        assert_eq!(physical.height, (f64::from(regular.height) * scale).round());
    }
}

#[test]
fn right_placement_mirrors_the_anchor_without_changing_capacity() {
    let left = StackLayout::with_placement(
        mbp16(),
        CardMetrics::default(),
        RecentCapturesPlacement::Left,
    );
    let right = StackLayout::with_placement(
        mbp16(),
        CardMetrics::default(),
        RecentCapturesPlacement::Right,
    );
    assert_eq!(left.slots(), right.slots());
    assert_eq!(left.slot_rect(0).size(), right.slot_rect(0).size());
    assert_eq!(
        left.slot_rect(0).left() - mbp16().left(),
        mbp16().right() - right.slot_rect(0).right()
    );
    assert!(right.entry_rect(0).left() > mbp16().right());
}

#[test]
fn runtime_size_change_reflows_current_cards_and_reapplies_capacity() {
    let mut stack = CaptureStack::configured(
        work_area(580.0),
        RecentCapturesPlacement::Left,
        CardMetrics::MIN_WIDTH,
    );
    for _ in 0..stack.capacity() {
        stack.push(&at(0));
    }

    let compact_capacity = stack.capacity();

    assert!(
        !stack.configuration_preserves_residents(
            RecentCapturesPlacement::Right,
            CardMetrics::MAX_WIDTH,
        ),
        "the runtime settings host must defer a width that would discard residents"
    );
    stack.configure(
        RecentCapturesPlacement::Right,
        CardMetrics::MAX_WIDTH,
        &at(100),
    );

    assert!(stack.capacity() < compact_capacity);
    assert_eq!(stack.len(), stack.capacity());
    assert_eq!(stack.layout().placement(), RecentCapturesPlacement::Right);
    assert_eq!(stack.layout().metrics().width, CardMetrics::MAX_WIDTH);
}

#[test]
fn configured_stack_animates_arrival_without_animating_resident_reflow() {
    let mut s = CaptureStack::configured(
        mbp16(),
        RecentCapturesPlacement::Left,
        CardMetrics::default().width,
    );

    let first = s.push(&at(0));
    let first_frame = s.frame_of(first, &at(0)).unwrap();
    assert_eq!(first_frame.state, CardState::Entering);
    assert!(
        first_frame.rect.max.x <= s.layout().slot_rect(0).min.x,
        "a new live card must begin outside the selected edge"
    );
    assert_rect_eq(
        s.frame_of(first, &at(500)).unwrap().rect,
        s.layout().slot_rect(0),
        "the new card lands exactly in its slot",
    );

    while s.len() < s.capacity() {
        s.push(&at(0));
    }
    let survivor = s.cards()[1].id();
    assert!(s.dismiss(first, &at(500)));
    let survivor_frame = s.frame_of(survivor, &at(500)).unwrap();
    assert_eq!(survivor_frame.state, CardState::Resting);
    assert_rect_eq(
        survivor_frame.rect,
        s.layout().slot_rect(0),
        "remaining cards compact atomically instead of traversing intermediate rows",
    );
}

#[test]
fn slot_count_follows_card_metrics_not_a_constant() {
    let small = CardMetrics {
        height: 60.0,
        gap: 6.0,
        ..CardMetrics::default()
    };
    // Same work area, smaller cards, more slots — with no arbitrary item cap.
    assert_eq!(StackLayout::new(work_area(580.0), small).slots(), 8);
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
                mbp16().left() + m.side_margin,
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
fn live_pushes_use_fixed_size_spacing_and_margins() {
    let mut stack = stack();
    let metrics = stack.layout().metrics();
    for _ in 0..3 {
        stack.push(&at(0));
    }
    stack.advance(&at(SETTLED));

    let expected = vec2(288.0, 180.0);
    for frame in stack.frame(&at(SETTLED)) {
        assert_eq!(frame.rect.size(), expected);
        assert_eq!(frame.rect.left(), mbp16().left() + 40.0);
    }
    let frames = stack.frame(&at(SETTLED));
    assert_eq!(frames[0].rect.bottom(), mbp16().bottom() - metrics.margin);
    for pair in frames.windows(2) {
        assert_eq!(pair[0].rect.top() - pair[1].rect.bottom(), metrics.gap);
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

#[test]
fn card_entry_glides_forward_and_lands_without_overshoot() {
    let mut stack = stack();
    let id = stack.push(&at(0));
    let target = stack.layout().slot_rect(0);
    let times = [0, 80, 160, 240, 320, 400, 440];
    let positions: Vec<_> = times
        .into_iter()
        .map(|ms| stack.frame_of(id, &at(ms)).unwrap().rect.left())
        .collect();

    assert!(
        positions.windows(2).all(|pair| pair[0] <= pair[1]),
        "entry must move continuously toward its slot: {positions:?}"
    );
    assert!(
        positions.iter().all(|x| *x <= target.left()),
        "entry must never bounce past its resting slot: {positions:?}"
    );
    assert!((positions.last().unwrap() - target.left()).abs() < 0.01);
    assert!(!Timing::default().enter_ease.overshoots());
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
fn a_fall_does_not_reapply_an_active_entry_transform() {
    let mut stack = stack();
    let ids: Vec<_> = (0..stack.capacity()).map(|_| stack.push(&at(0))).collect();
    let instant = at(40);
    let before: Vec<_> = ids[1..]
        .iter()
        .map(|id| (*id, stack.frame_of(*id, &instant).unwrap().rect))
        .collect();

    stack.push(&instant);

    for (id, expected) in before {
        assert_rect_eq(
            stack.frame_of(id, &instant).unwrap().rect,
            expected,
            "overflow must not double-apply entry motion",
        );
    }
}

#[test]
fn a_fall_does_not_reapply_an_active_drag_transform() {
    let mut stack = stack();
    let lower = stack.push(&at(0));
    let upper = stack.push(&at(0));
    stack.advance(&at(SETTLED));
    let instant = at(SETTLED);
    let origin = stack.frame_of(upper, &instant).unwrap().rect.center();
    assert!(stack.begin_drag(upper, origin, &instant));
    stack.drag_to(origin + vec2(-70.0, 35.0), &instant);
    let before = stack.frame_of(upper, &instant).unwrap().rect;

    assert!(stack.dismiss(lower, &instant));

    assert_rect_eq(
        stack.frame_of(upper, &instant).unwrap().rect,
        before,
        "dismissal below a held card must not double its drag offset",
    );

    stack.drag_to(origin + vec2(-90.0, 55.0), &instant);
    let after = stack.frame_of(upper, &instant).unwrap().rect;
    assert!(
        (after.min - before.min - vec2(-20.0, 20.0)).length() < 0.01,
        "a falling held card stopped tracking 1:1: {before:?} -> {after:?}"
    );
}

#[test]
fn a_fall_does_not_reapply_an_active_dock_transform() {
    let mut stack = stack();
    let lower = stack.push(&at(0));
    let upper = stack.push(&at(0));
    stack.advance(&at(SETTLED));
    stack.collapse(&at(SETTLED));
    let instant = at(SETTLED + 160);
    let before = stack.frame_of(upper, &instant).unwrap().rect;

    assert!(stack.dismiss(lower, &instant));

    assert_rect_eq(
        stack.frame_of(upper, &instant).unwrap().rect,
        before,
        "dismissal during collapse must not absorb the card twice",
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
    let count = s.len();
    s.advance(&at(SETTLED));

    s.dismiss_all(&at(SETTLED));
    assert!(s.is_empty());
    assert_eq!(s.departing().len(), count, "every card is on its way out");

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
    for _ in 0..s.capacity() {
        s.push(&tick(&mut t));
    }
    watcher.check(&s, "refill");
    s.resize(work_area(580.0), &tick(&mut t));
    watcher.check(&s, "shrink to a 3-slot display");
    s.resize(mbp16(), &tick(&mut t));
    watcher.check(&s, "grow back to the taller layout");

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

    assert_eq!(s.capacity(), 5, "the taller display offers more slots");
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(s.slot_of(*id), Some(i), "nothing was reshuffled");
    }
    s.check_no_card_moved_up().unwrap();
}

#[test]
fn local_geometry_rebase_preserves_active_card_frames() {
    let mut s = stack();
    let returning = s.push(&at(0));
    let departing = s.push(&at(0));
    s.advance(&at(SETTLED));
    let gesture = at(SETTLED + 10);
    let origin = frame_of(&s.frame(&gesture), returning).rect.center();
    s.begin_drag(returning, origin, &gesture);
    s.drag_to(origin + vec2(24.0, 0.0), &gesture);
    let _ = s.release_drag(&gesture);
    s.dismiss(departing, &gesture);

    let frame = at(SETTLED + 50);
    let before = s.frame(&frame);
    let translation = vec2(0.0, 220.0);
    s.resize(mbp16().translate(translation), &frame);
    let after = s.frame(&frame);

    for id in [returning, departing] {
        assert_rect_eq(
            frame_of(&after, id).rect,
            frame_of(&before, id).rect.translate(translation),
            "active animation must rebase without a global jump",
        );
    }
}

#[test]
fn work_area_resize_does_not_misapply_a_coordinate_rebase() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    let gesture = at(SETTLED + 10);
    let origin = frame_of(&s.frame(&gesture), id).rect.center();
    s.begin_drag(id, origin, &gesture);
    s.drag_to(origin + vec2(24.0, 0.0), &gesture);
    let _ = s.release_drag(&gesture);

    let frame = at(SETTLED + 50);
    let before = frame_of(&s.frame(&frame), id).rect;
    let resized = Rect::from_min_max(pos2(0.0, 137.0), mbp16().max);
    s.resize(resized, &frame);
    let after = frame_of(&s.frame(&frame), id).rect;

    assert_rect_eq(
        after,
        before,
        "changing work-area size with a stable bottom anchor is not a local-coordinate translation",
    );
}

#[test]
fn shrinking_the_display_retires_from_the_bottom() {
    let mut s = CaptureStack::for_work_area(work_area(1400.0));
    let ids: Vec<_> = (0..s.capacity()).map(|_| s.push(&at(0))).collect();
    s.advance(&at(SETTLED));

    s.resize(work_area(580.0), &at(SETTLED));

    assert_eq!(s.capacity(), 3);
    assert_eq!(s.len(), 3);
    let retired = ids.len() - 3;
    for id in &ids[..retired] {
        assert_eq!(s.slot_of(*id), None, "the oldest cards were retired");
    }
    for (i, id) in ids[retired..].iter().enumerate() {
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

fn live_for(delta: Vec2, elapsed_ms: u64) -> Option<(Intent, Option<Dir>)> {
    let mut stack = stack();
    let id = stack.push(&at(0));
    stack.advance(&at(SETTLED));
    let origin = stack.frame_of(id, &at(SETTLED)).unwrap().rect.center();
    stack.begin_drag(id, origin, &at(SETTLED));
    stack.drag_to(origin + delta, &at(SETTLED + elapsed_ms));
    stack
        .live_gesture(&at(SETTLED + elapsed_ms))
        .map(|gesture| (gesture.intent, gesture.direction))
}

#[test]
fn collapse_uses_a_narrow_vertical_cone_not_positive_y_alone() {
    let length = 120.0;
    let down_right = |degrees: f32| {
        let radians = degrees.to_radians();
        vec2(length * radians.cos(), length * radians.sin())
    };
    for delta in [
        down_right(30.0),
        down_right(45.0),
        down_right(60.0),
        vec2(120.0, 20.0),
        vec2(90.0, -60.0),
    ] {
        assert!(
            matches!(live_for(delta, 120), Some((Intent::DragOut, _))),
            "{delta:?}"
        );
    }
    for delta in [vec2(0.0, 100.0), vec2(10.0, 100.0)] {
        assert_eq!(
            live_for(delta, 120),
            Some((Intent::Collapse, Some(Dir::Down))),
            "{delta:?}"
        );
    }
}

#[test]
fn direction_lock_respects_slop_speed_and_never_switches() {
    assert_eq!(live_for(vec2(DRAG_LOCK_SLOP - 0.1, 0.0), 5), None);
    assert_eq!(
        live_for(vec2(DRAG_LOCK_SLOP + 0.1, 0.0), 5),
        Some((Intent::DragOut, Some(Dir::Right)))
    );
    assert_eq!(
        live_for(vec2(DRAG_LOCK_SLOP + 0.1, 0.0), 500),
        Some((Intent::DragOut, Some(Dir::Right)))
    );

    let mut stack = stack();
    let id = stack.push(&at(0));
    stack.advance(&at(SETTLED));
    let origin = stack.frame_of(id, &at(SETTLED)).unwrap().rect.center();
    stack.begin_drag(id, origin, &at(SETTLED));
    stack.drag_to(origin + vec2(4.0, 45.0), &at(SETTLED + 50));
    stack.drag_to(origin + vec2(100.0, 20.0), &at(SETTLED + 100));
    assert!(stack.live_gesture(&at(SETTLED + 100)).is_none());
    assert_eq!(
        stack.release_drag(&at(SETTLED + 100)).unwrap().intent,
        Intent::SpringBack
    );
}

#[test]
fn collapse_candidate_scrubs_the_whole_stack_and_can_reverse() {
    let mut stack = stack();
    let ids: Vec<_> = (0..3).map(|_| stack.push(&at(0))).collect();
    stack.advance(&at(SETTLED));
    let before: Vec<_> = ids
        .iter()
        .map(|id| stack.frame_of(*id, &at(SETTLED)).unwrap())
        .collect();
    let origin = before[2].rect.center();
    stack.begin_drag(ids[2], origin, &at(SETTLED));
    stack.drag_to(origin + vec2(4.0, 44.0), &at(SETTLED + 50));
    let preview: Vec<_> = ids
        .iter()
        .map(|id| stack.frame_of(*id, &at(SETTLED + 50)).unwrap())
        .collect();
    for (before, preview) in before.iter().zip(&preview) {
        assert!(preview.rect.top() > before.rect.top());
        assert!(preview.rect.height() < before.rect.height());
        assert!(preview.alpha < before.alpha);
    }
    assert_eq!(
        ids.iter()
            .map(|id| stack.slot_of(*id).unwrap())
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

    stack.drag_to(origin + vec2(100.0, 20.0), &at(SETTLED + 100));
    assert!(stack.live_gesture(&at(SETTLED + 100)).is_none());
    let release = stack.release_drag(&at(SETTLED + 100)).unwrap();
    assert_eq!(release.intent, Intent::SpringBack);
    stack.advance(&at(SETTLED + 1_000));
    for (id, before) in ids.iter().zip(before) {
        assert_eq!(
            stack.frame_of(*id, &at(SETTLED + 1_000)).unwrap().rect,
            before.rect
        );
    }
}

#[test]
fn reduced_motion_previews_collective_collapse_with_fade_not_travel() {
    let calm = Motion::at_ms(SETTLED).with_reduce_motion(true);
    let mut stack = stack();
    let ids: Vec<_> = (0..3).map(|_| stack.push(&calm)).collect();
    stack.advance(&calm);
    let before: Vec<_> = ids
        .iter()
        .map(|id| stack.frame_of(*id, &calm).unwrap())
        .collect();
    let origin = before[2].rect.center();
    stack.begin_drag(ids[2], origin, &calm);
    stack.drag_to(origin + vec2(2.0, 44.0), &calm);
    for (id, before) in ids.iter().zip(before) {
        let preview = stack.frame_of(*id, &calm).unwrap();
        assert_eq!(preview.rect, before.rect);
        assert!(preview.alpha < before.alpha);
    }
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
fn a_swipe_right_or_up_requests_a_drag_without_releasing_the_card() {
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
        assert_eq!(release.pointer, origin + delta);
        assert_eq!(
            s.len(),
            1,
            "a release-time drag-out is too late to deliver and must spring back"
        );
        assert_eq!(
            s.slot_of(id),
            Some(0),
            "the source remains in its exact slot until the native drop is accepted"
        );
        assert!(s.departing().is_empty());
    }
}

// ---------------------------------------------------------------------------
// Mid-gesture hand-off
// ---------------------------------------------------------------------------
//
// A native drag has to be started while the button is still down: AppKit
// refuses `beginDraggingSessionWithItems:` from a released mouse. So the stack
// has to answer "is this already a drag-out?" *during* the gesture, not at
// release. These tests pin that answer down.

#[test]
fn a_live_gesture_is_only_reported_once_it_has_travelled() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));

    let origin = pos2(120.0, 900.0);
    s.begin_drag(id, origin, &at(SETTLED));
    assert!(
        s.live_gesture(&at(SETTLED)).is_none(),
        "a press that has not moved is not a drag-out"
    );

    s.drag_to(origin + vec2(DRAG_LOCK_SLOP / 2.0, 0.0), &at(SETTLED + 10));
    assert!(
        s.live_gesture(&at(SETTLED + 10)).is_none(),
        "a twitch is not a drag-out"
    );

    s.drag_to(origin + vec2(240.0, 0.0), &at(SETTLED + 60));
    let live = s
        .live_gesture(&at(SETTLED + 60))
        .expect("a committed sideways drag is a drag-out mid-gesture");
    assert_eq!(live.id, id);
    assert_eq!(live.intent, Intent::DragOut);
}

#[test]
fn a_live_gesture_reports_where_the_card_and_pointer_are_now() {
    // Both halves matter downstream: the rect places the drag image and the
    // pointer fixes where inside it the cursor grabbed. Get the rect wrong and
    // the thumbnail jumps to the wrong place the instant the drag starts.
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));

    let home = frame_of(&s.frame(&at(SETTLED)), id).rect;
    let origin = home.center();
    let travel = vec2(240.0, 0.0);
    let moved = origin + travel;
    s.begin_drag(id, origin, &at(SETTLED));
    s.drag_to(moved, &at(SETTLED + 60));

    let live = s.live_gesture(&at(SETTLED + 60)).expect("drag-out");
    assert_eq!(live.pointer, moved, "the drag image follows the pointer");
    assert_eq!(live.rect, home, "the source must stay pinned to its slot");
}

#[test]
fn outward_direction_lock_pins_and_fades_the_source_without_reflow() {
    let mut s = stack();
    let lower = s.push(&at(0));
    let source = s.push(&at(0));
    s.advance(&at(SETTLED));
    let home = s.frame_of(source, &at(SETTLED)).expect("source").rect;
    let lower_slot = s.slot_of(lower);
    let source_slot = s.slot_of(source);
    let origin = home.center();
    s.begin_drag(source, origin, &at(SETTLED));

    s.drag_to(origin + vec2(DRAG_LOCK_SLOP / 2.0, 1.0), &at(SETTLED + 10));
    assert_eq!(s.frame_of(source, &at(SETTLED + 10)).unwrap().rect, home);
    assert!(s.live_gesture(&at(SETTLED + 10)).is_none());

    s.drag_to(origin + vec2(DRAG_LOCK_SLOP + 1.0, 1.0), &at(SETTLED + 20));
    let live = s.live_gesture(&at(SETTLED + 20)).expect("outward lock");
    assert_eq!(live.direction, Some(Dir::Right));
    assert_eq!(live.rect, home);
    assert_eq!(s.slot_of(lower), lower_slot);
    assert_eq!(s.slot_of(source), source_slot);

    let start = s.frame_of(source, &at(SETTLED + 20)).unwrap();
    let middle = s.frame_of(source, &at(SETTLED + 80)).unwrap();
    let faded = s.frame_of(source, &at(SETTLED + 500)).unwrap();
    assert_eq!(start.rect, home);
    assert_eq!(middle.rect, home);
    assert_eq!(faded.rect, home);
    assert!(start.alpha > middle.alpha && middle.alpha > DRAG_SOURCE_ALPHA);
    assert!((faded.alpha - DRAG_SOURCE_ALPHA).abs() < 0.001);
}

#[test]
fn vertical_leading_motion_locks_without_turning_downward_motion_into_a_drag_out() {
    for (delta, expected) in [
        (vec2(1.0, -DRAG_LOCK_SLOP - 1.0), Some(Dir::Up)),
        (vec2(1.0, DRAG_LOCK_SLOP + 1.0), None),
    ] {
        let mut s = stack();
        let id = s.push(&at(0));
        s.advance(&at(SETTLED));
        let origin = s.frame_of(id, &at(SETTLED)).unwrap().rect.center();
        s.begin_drag(id, origin, &at(SETTLED));
        s.drag_to(origin + delta, &at(SETTLED + 20));
        assert_eq!(
            s.live_gesture(&at(SETTLED + 20))
                .and_then(|live| live.direction),
            expected
        );
    }
}

#[test]
fn reduced_motion_fades_and_restores_the_drag_source_immediately() {
    let calm = Motion::at_ms(SETTLED).with_reduce_motion(true);
    let later = Motion::at_ms(SETTLED + 1).with_reduce_motion(true);
    let mut s = stack();
    let id = s.push(&calm);
    s.advance(&calm);
    let home = s.frame_of(id, &calm).unwrap().rect;
    let origin = home.center();
    s.begin_drag(id, origin, &calm);
    s.drag_to(origin + vec2(DRAG_LOCK_SLOP + 1.0, 0.0), &calm);
    assert_eq!(s.frame_of(id, &calm).unwrap().alpha, DRAG_SOURCE_ALPHA);

    assert!(s.settle_drag(id, &later));
    assert_eq!(s.frame_of(id, &later).unwrap().alpha, 1.0);
    assert_eq!(s.frame_of(id, &later).unwrap().rect, home);
}

#[test]
fn an_entering_card_freezes_the_exact_visible_rect_when_drag_out_locks() {
    let mut s = stack();
    let id = s.push(&at(0));
    let instant = at(120);
    let visible = s.frame_of(id, &instant).expect("entering card").rect;
    let origin = visible.center();
    s.begin_drag(id, origin, &instant);
    s.drag_to(origin + vec2(DRAG_LOCK_SLOP + 1.0, 0.0), &instant);

    assert_eq!(s.live_gesture(&instant).expect("armed").rect, visible);
    assert_eq!(s.frame_of(id, &at(300)).expect("pinned").rect, visible);
}

#[test]
fn accepted_departure_preserves_the_faded_source_alpha() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    let origin = s.frame_of(id, &at(SETTLED)).unwrap().rect.center();
    s.begin_drag(id, origin, &at(SETTLED));
    s.drag_to(origin + vec2(DRAG_LOCK_SLOP + 1.0, 0.0), &at(SETTLED));
    let settled = at(SETTLED + 500);
    assert_eq!(s.frame_of(id, &settled).unwrap().alpha, DRAG_SOURCE_ALPHA);

    assert!(s.settle_drag(id, &settled));
    assert!(s.dismiss(id, &settled));
    let departing = frame_of(&s.frame(&settled), id);
    assert_eq!(departing.alpha, DRAG_SOURCE_ALPHA);
}

#[test]
fn drag_restoration_is_continuous_during_dock_motion() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    s.collapse(&at(SETTLED));
    let instant = at(SETTLED + 160);
    let pinned = s.frame_of(id, &instant).unwrap().rect;
    let origin = pinned.center();
    s.begin_drag(id, origin, &instant);
    s.drag_to(origin + vec2(DRAG_LOCK_SLOP + 1.0, 0.0), &instant);
    assert_eq!(s.live_gesture(&instant).unwrap().rect, pinned);

    assert!(s.settle_drag(id, &instant));
    assert_eq!(
        s.frame_of(id, &instant).unwrap().rect,
        pinned,
        "settling reapplied the dock transform to the pinned source"
    );
}

#[test]
fn stack_reflow_does_not_jump_a_returning_drag_source() {
    let mut s = stack();
    let lower = s.push(&at(0));
    let source = s.push(&at(0));
    s.advance(&at(SETTLED));
    let origin = s.frame_of(source, &at(SETTLED)).unwrap().rect.center();
    s.begin_drag(source, origin, &at(SETTLED));
    s.drag_to(origin + vec2(DRAG_LOCK_SLOP + 1.0, 0.0), &at(SETTLED));
    s.settle_drag(source, &at(SETTLED + 200));
    let instant = at(SETTLED + 260);
    let before = s.frame_of(source, &instant).unwrap().rect;

    assert!(s.dismiss(lower, &instant));
    let after = s.frame_of(source, &instant).unwrap().rect;

    assert_eq!(after, before, "reflow attenuated the continuity offset");
}

#[test]
fn a_small_outward_direction_lock_arms_without_waiting_for_release_speed() {
    let flick = vec2(60.0, 0.0);
    let origin = pos2(120.0, 900.0);

    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    s.begin_drag(id, origin, &at(SETTLED));
    s.drag_to(origin, &at(SETTLED + 40));
    s.drag_to(origin + flick, &at(SETTLED + 100));

    assert!(
        s.live_gesture(&at(SETTLED + 100)).is_some(),
        "an outward drag should arm after native slop, not 110 points"
    );

    // And the same gesture released right there *is* a drag-out, which is what
    // makes the paragraph above a real distinction rather than a coincidence.
    let release = s.release_drag(&at(SETTLED + 100)).expect("a live drag");
    assert_eq!(release.intent, Intent::DragOut);
    assert_eq!(
        s.len(),
        1,
        "a velocity-only release cannot start a native drop and must keep the card"
    );
}

#[test]
fn a_downward_gesture_is_never_armed_as_a_drag_out() {
    for delta in [vec2(-240.0, 0.0), vec2(0.0, 240.0)] {
        let mut s = stack();
        let id = s.push(&at(0));
        s.advance(&at(SETTLED));

        let origin = pos2(120.0, 900.0);
        s.begin_drag(id, origin, &at(SETTLED));
        s.drag_to(origin + delta, &at(SETTLED + 60));

        let live = s.live_gesture(&at(SETTLED + 60)).expect("committed");
        assert_ne!(
            live.intent,
            Intent::DragOut,
            "{delta:?} is a dismiss or a collapse, not a drag-out"
        );
    }
}

#[test]
fn cancelling_an_armed_drag_keeps_the_card() {
    // The native session owns the capture from the moment it is armed. The
    // pile must not also retire it, or a refused drop would lose the shot.
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));

    let origin = pos2(120.0, 900.0);
    s.begin_drag(id, origin, &at(SETTLED));
    s.drag_to(origin + vec2(240.0, 0.0), &at(SETTLED + 60));
    assert!(s.live_gesture(&at(SETTLED + 60)).is_some());

    s.cancel_drag(&at(SETTLED + 80));
    assert!(
        !s.is_empty(),
        "the capture stays until the drop is accepted"
    );
    assert!(
        s.departing().is_empty(),
        "and it is not animating away either"
    );
}

#[test]
fn a_live_gesture_needs_a_live_drag() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    assert!(s.live_gesture(&at(SETTLED)).is_none(), "nothing is pressed");

    let origin = pos2(120.0, 900.0);
    s.begin_drag(id, origin, &at(SETTLED));
    s.drag_to(origin + vec2(240.0, 0.0), &at(SETTLED + 60));
    s.cancel_drag(&at(SETTLED + 80));
    assert!(
        s.live_gesture(&at(SETTLED + 90)).is_none(),
        "the gesture is over"
    );
}

#[test]
fn settling_a_native_drag_releases_a_stuck_gesture_without_dismissing() {
    let mut s = stack();
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    let home = s.frame_of(id, &at(SETTLED)).unwrap().rect;
    let origin = home.center();
    s.begin_drag(id, origin, &at(SETTLED));
    s.drag_to(origin + vec2(240.0, 0.0), &at(SETTLED + 60));

    assert!(s.settle_drag(id, &at(SETTLED + 80)));
    assert_eq!(s.len(), 1);
    assert!(s.departing().is_empty());
    s.advance(&at(SETTLED + 4_000));
    assert_eq!(s.frame_of(id, &at(SETTLED + 4_000)).unwrap().rect, home);
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

    assert_eq!(s.len(), s.capacity(), "nothing was lost");
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
    for _ in 0..s.capacity() {
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
    let ids: Vec<_> = (0..s.capacity()).map(|_| s.push(&at(0))).collect();
    s.dismiss(ids[2], &at(0));
    s.advance(&at(0));

    assert_eq!(s.len(), ids.len() - 1);
    assert!(s.departing().is_empty(), "the exit finished instantly");
    assert_eq!(s.slot_of(ids[3]), Some(2), "and the pile still closed up");
    s.check_no_card_moved_up().unwrap();
}

// ---------------------------------------------------------------------------
// Settling a drag the platform ran
// ---------------------------------------------------------------------------
//
// Once a native drag starts, the platform runs a *modal* event loop. AppKit's
// dragging session and Windows' `DoDragDrop` both do, and both can consume the
// mouse-up entirely. So the release this stack is waiting for may never arrive:
// the card stays held and displaced under a pointer the user let go of, and —
// because a gesture only arms when nothing else is held — no card can ever be
// dragged again. `settle_drag` is how the host says "that one is over" without
// having to know whether the release came through.

/// A card whose outward gesture has armed a native drag, mid-gesture.
///
/// Returns the card and the slot it came from. The slot has to be read
/// *before* the drag, because a frame taken during one already carries the
/// displacement — a fixture that reads it afterwards is asserting the card
/// returns to where the pointer left it, which it must not.
fn dragged_out(s: &mut CaptureStack) -> (CardId, Rect) {
    let id = s.push(&at(0));
    s.advance(&at(SETTLED));
    let home = frame_of(&s.frame(&at(SETTLED)), id).rect;
    let origin = home.center();
    s.begin_drag(id, origin, &at(SETTLED));
    s.drag_to(origin + vec2(240.0, 0.0), &at(SETTLED + 60));
    assert!(
        s.live_gesture(&at(SETTLED + 60)).is_some(),
        "the fixture must be a committed drag-out"
    );
    assert_eq!(
        frame_of(&s.frame(&at(SETTLED + 60)), id).rect,
        home,
        "the fixture must keep the source pinned in its slot"
    );
    (id, home)
}

#[test]
fn a_rejected_drop_springs_the_card_back() {
    let mut s = stack();
    let (id, home) = dragged_out(&mut s);

    assert!(
        s.settle_drag(id, &at(SETTLED + 100)),
        "the drag was still held, so settling must have changed something"
    );

    // The card is still on the pile — a refused drop loses nothing — and it is
    // on its way home rather than stranded where the pointer left it.
    assert_eq!(s.len(), 1, "a rejected drop must not discard the card");
    assert!(
        frame_of(&s.frame(&at(SETTLED + 100)), id).alpha < 1.0,
        "restoration should reverse from the faded source"
    );
    s.advance(&at(SETTLED + 4_000));
    assert_rect_eq(
        frame_of(&s.frame(&at(SETTLED + 4_000)), id).rect,
        home,
        "the card did not return to its slot after a rejected drop",
    );
    assert_eq!(frame_of(&s.frame(&at(SETTLED + 4_000)), id).alpha, 1.0);
}

#[test]
fn settling_frees_the_stack_for_the_next_drag() {
    // The failure this prevents is total: one stuck gesture and the pile is
    // inert for the rest of the session.
    let mut s = stack();
    let (id, _) = dragged_out(&mut s);
    s.settle_drag(id, &at(SETTLED + 100));

    assert!(
        s.live_gesture(&at(SETTLED + 100)).is_none(),
        "the finished drag is still being reported as live"
    );

    // A second gesture on the same card must arm and commit exactly as the
    // first one did.
    s.advance(&at(SETTLED + 4_000));
    let t = SETTLED + 4_000;
    let origin = frame_of(&s.frame(&at(t)), id).rect.center();
    s.begin_drag(id, origin, &at(t));
    s.drag_to(origin + vec2(240.0, 0.0), &at(t + 60));
    assert!(
        s.live_gesture(&at(t + 60)).is_some(),
        "the stack never recovered enough to start a second drag"
    );
}

#[test]
fn settling_a_drag_that_already_ended_normally_is_harmless() {
    // The other half of the modal-loop problem: sometimes the release *does*
    // arrive, and the host cannot tell which happened. Settling anyway must be
    // a no-op rather than disturbing whatever the stack is doing now.
    let mut s = stack();
    let (id, _) = dragged_out(&mut s);
    s.cancel_drag(&at(SETTLED + 80));
    s.advance(&at(SETTLED + 4_000));

    assert!(
        !s.settle_drag(id, &at(SETTLED + 4_000)),
        "settling an already-finished drag reported a change it did not make"
    );
    assert_eq!(s.len(), 1);
    assert!(s.live_gesture(&at(SETTLED + 4_000)).is_none());
}

#[test]
fn settling_one_card_leaves_another_cards_gesture_alone() {
    // Why this is not `cancel_drag`. A native session that finishes late must
    // not cancel whatever the user is holding *now*.
    let mut s = stack();
    let (first, _) = dragged_out(&mut s);
    s.settle_drag(first, &at(SETTLED + 100));
    s.advance(&at(SETTLED + 4_000));

    let t = SETTLED + 4_000;
    let second = s.push(&at(t));
    s.advance(&at(t + 4_000));
    let t = t + 4_000;
    let origin = frame_of(&s.frame(&at(t)), second).rect.center();
    s.begin_drag(second, origin, &at(t));
    s.drag_to(origin + vec2(240.0, 0.0), &at(t + 60));

    // The stale outcome for `first` arrives now.
    assert!(
        !s.settle_drag(first, &at(t + 60)),
        "a stale outcome touched a card that was not dragging"
    );
    let live = s
        .live_gesture(&at(t + 60))
        .expect("the live gesture was cancelled by an unrelated card's outcome");
    assert_eq!(live.id, second);
}

#[test]
fn settling_an_unknown_card_changes_nothing() {
    let mut s = stack();
    let (id, _) = dragged_out(&mut s);
    assert!(
        !s.settle_drag(CardId(id.0.wrapping_add(999)), &at(SETTLED + 100)),
        "an outcome for a card that is not here reported a change"
    );
    assert!(
        s.live_gesture(&at(SETTLED + 100)).is_some(),
        "the live gesture was disturbed by an unrelated card's outcome"
    );
}
