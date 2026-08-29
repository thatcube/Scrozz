//! Undo and redo.

mod common;

use common::{document, rect, window_capture};
use scrozz_annotate::{
    Annotation, Document, History, RedactStyle, Style, document::DocumentData,
    history::DEFAULT_LIMIT,
};
use scrozz_core::LogicalPoint;

/// A document with one rectangle, and its history snapshotted at that state.
fn started() -> (Document, History) {
    let mut doc = document(200, 200);
    doc.add(
        Annotation::Rectangle(rect(10.0, 10.0, 40.0, 40.0)),
        Style::stroked(),
    );
    let history = History::new(&doc);
    (doc, history)
}

#[test]
fn a_fresh_history_has_nothing_to_undo_or_redo() {
    let (_, history) = started();
    assert!(!history.can_undo());
    assert!(!history.can_redo());
}

#[test]
fn undo_restores_the_previous_state() {
    let (mut doc, mut history) = started();
    doc.add(
        Annotation::Ellipse(rect(60.0, 60.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit(&doc);
    assert_eq!(doc.len(), 2);

    assert!(history.undo(&mut doc).unwrap());
    assert_eq!(doc.len(), 1);
    assert!(!history.can_undo());
    assert!(history.can_redo());
}

#[test]
fn redo_replays_what_undo_took_away() {
    let (mut doc, mut history) = started();
    doc.add(
        Annotation::Ellipse(rect(60.0, 60.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit(&doc);
    history.undo(&mut doc).unwrap();

    assert!(history.redo(&mut doc).unwrap());
    assert_eq!(doc.len(), 2);
    assert!(history.can_undo());
    assert!(!history.can_redo());
}

#[test]
fn undo_and_redo_report_false_at_the_ends_rather_than_erroring() {
    let (mut doc, mut history) = started();
    assert!(!history.undo(&mut doc).unwrap());
    assert!(!history.redo(&mut doc).unwrap());
    assert_eq!(doc.len(), 1, "a no-op must leave the document alone");
}

#[test]
fn a_commit_that_changes_nothing_does_not_cost_an_undo() {
    let (doc, mut history) = started();
    history.commit(&doc);
    history.commit(&doc);
    assert!(
        !history.can_undo(),
        "selecting or clicking without editing must not make the user press undo twice"
    );
}

#[test]
fn a_drag_that_ends_where_it_started_costs_nothing() {
    let (mut doc, mut history) = started();
    let id = doc.annotations()[0].id;
    doc.translate(id, 30.0, 0.0);
    doc.translate(id, -30.0, 0.0);
    history.commit(&doc);
    assert!(!history.can_undo());
}

#[test]
fn a_new_edit_after_undo_discards_the_redo_branch() {
    let (mut doc, mut history) = started();
    doc.add(
        Annotation::Ellipse(rect(60.0, 60.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit(&doc);
    history.undo(&mut doc).unwrap();
    assert!(history.can_redo());

    doc.add(
        Annotation::Highlight(rect(5.0, 5.0, 10.0, 10.0)),
        Style::highlighter(),
    );
    history.commit(&doc);
    assert!(
        !history.can_redo(),
        "editing from an undone state must abandon the future, not interleave with it"
    );
}

#[test]
fn coalesced_commits_collapse_into_one_step() {
    let (mut doc, mut history) = started();
    let id = doc.annotations()[0].id;
    for _ in 0..20 {
        doc.translate(id, 1.0, 0.0);
        history.commit_coalesced(&doc, "nudge");
    }
    assert_eq!(history.undo_depth(), 1, "a gesture is one undo, not twenty");

    history.undo(&mut doc).unwrap();
    assert_eq!(
        doc.annotations()[0].annotation.bounds().origin.x,
        10.0,
        "one undo must revert the whole gesture"
    );
}

#[test]
fn a_different_tag_starts_a_new_step() {
    let (mut doc, mut history) = started();
    let id = doc.annotations()[0].id;
    doc.translate(id, 5.0, 0.0);
    history.commit_coalesced(&doc, "nudge");
    doc.set_style(id, Style::stroked().with_stroke_width(9.0));
    history.commit_coalesced(&doc, "stroke-width");
    assert_eq!(history.undo_depth(), 2);
}

#[test]
fn sealing_ends_a_coalescing_group() {
    let (mut doc, mut history) = started();
    let id = doc.annotations()[0].id;
    doc.translate(id, 5.0, 0.0);
    history.commit_coalesced(&doc, "nudge");
    history.seal();
    doc.translate(id, 5.0, 0.0);
    history.commit_coalesced(&doc, "nudge");
    assert_eq!(
        history.undo_depth(),
        2,
        "a gesture that ended and began again is two steps"
    );
}

#[test]
fn undo_closes_an_open_coalescing_group() {
    let (mut doc, mut history) = started();
    let id = doc.annotations()[0].id;
    doc.translate(id, 5.0, 0.0);
    history.commit_coalesced(&doc, "nudge");
    history.undo(&mut doc).unwrap();
    history.redo(&mut doc).unwrap();
    doc.translate(id, 5.0, 0.0);
    history.commit_coalesced(&doc, "nudge");
    assert_eq!(history.undo_depth(), 2);
}

#[test]
fn history_is_bounded_and_drops_the_oldest_step() {
    let mut doc = document(200, 200);
    let mut history = History::with_limit(&doc, 3);
    for i in 0..10 {
        doc.add(
            Annotation::Rectangle(rect(f64::from(i), 0.0, 5.0, 5.0)),
            Style::stroked(),
        );
        history.commit(&doc);
    }
    assert_eq!(history.undo_depth(), 3);
    for _ in 0..3 {
        assert!(history.undo(&mut doc).unwrap());
    }
    assert!(!history.can_undo());
    assert_eq!(
        doc.len(),
        7,
        "undoing to the bottom lands on the oldest state still kept, not on empty"
    );
}

#[test]
fn the_default_limit_is_deep_enough_to_be_invisible() {
    const { assert!(DEFAULT_LIMIT >= 100) };
}

#[test]
fn undo_restores_counter_numbering_correctly() {
    let mut doc = document(300, 300);
    for i in 0..4 {
        doc.add_default(Annotation::Counter {
            at: LogicalPoint::new(f64::from(i) * 30.0 + 10.0, 20.0),
            index: 0,
        });
    }
    let mut history = History::new(&doc);
    let second = doc.annotations()[1].id;
    doc.remove(second);
    history.commit(&doc);

    // Removing marker 2 renumbers 3 and 4 down to 2 and 3. This is exactly the
    // coupling that makes an inverse-command undo wrong.
    let after: Vec<u32> = doc
        .annotations()
        .iter()
        .filter_map(|o| match o.annotation {
            Annotation::Counter { index, .. } => Some(index),
            _ => None,
        })
        .collect();
    assert_eq!(after, vec![1, 2, 3]);

    history.undo(&mut doc).unwrap();
    let restored: Vec<u32> = doc
        .annotations()
        .iter()
        .filter_map(|o| match o.annotation {
            Annotation::Counter { index, .. } => Some(index),
            _ => None,
        })
        .collect();
    assert_eq!(restored, vec![1, 2, 3, 4]);
}

#[test]
fn undo_restores_crop() {
    let (mut doc, mut history) = started();
    doc.set_crop(Some(rect(20.0, 20.0, 100.0, 80.0))).unwrap();
    history.commit(&doc);
    assert!(doc.crop().is_some());

    history.undo(&mut doc).unwrap();
    assert_eq!(doc.crop(), None, "undo must restore the full frame");

    history.redo(&mut doc).unwrap();
    assert_eq!(doc.crop(), Some(rect(20.0, 20.0, 100.0, 80.0)));
}

#[test]
fn undo_never_disturbs_the_source_image() {
    let (mut doc, mut history) = started();
    let before = doc.source.frame.data.clone();
    doc.add_default(Annotation::Redact {
        area: rect(0.0, 0.0, 50.0, 50.0),
        style: RedactStyle::Solid,
    });
    history.commit(&doc);
    history.undo(&mut doc).unwrap();
    assert_eq!(
        doc.source.frame.data, before,
        "the source is never part of a snapshot, so it can never be restored over"
    );
}

#[test]
fn restore_refuses_state_a_document_cannot_hold() {
    // D9 still forbids any snapshot that would restyle native window pixels.
    let mut doc = Document::new(window_capture(100, 100));
    let data = DocumentData {
        beautification: Some(scrozz_annotate::Beautification {
            corner_radius: 12.0,
            ..scrozz_annotate::Beautification::default()
        }),
        ..DocumentData::default()
    };
    assert!(doc.restore(data).is_err());
}

#[test]
fn a_failed_restore_leaves_the_history_usable() {
    let (mut doc, mut history) = started();
    doc.add(
        Annotation::Ellipse(rect(60.0, 60.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit(&doc);
    assert!(history.undo(&mut doc).unwrap());
    assert_eq!(doc.len(), 1);
    assert!(history.can_redo());
}

#[test]
fn reset_forgets_every_step() {
    let (mut doc, mut history) = started();
    doc.add(
        Annotation::Ellipse(rect(60.0, 60.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit(&doc);
    history.undo(&mut doc).unwrap();
    history.reset(&doc);
    assert!(!history.can_undo());
    assert!(!history.can_redo());
}

/// Adds a rectangle at `x` and seals it as its own undo step.
fn step(doc: &mut Document, history: &mut History, x: f64) {
    doc.add(
        Annotation::Rectangle(rect(x, 0.0, 5.0, 5.0)),
        Style::stroked(),
    );
    history.commit(doc);
}

#[test]
fn a_refused_abandon_takes_nothing_so_a_later_one_can_still_roll_back() {
    // Two edits and an undo, so there is a redo branch worth protecting.
    let mut doc = document(200, 200);
    let mut history = History::new(&doc);
    step(&mut doc, &mut history, 10.0);
    step(&mut doc, &mut history, 20.0);
    history.undo(&mut doc).unwrap();
    assert!(history.can_redo(), "the branch this test is about");
    let before = doc.len();

    // An edit opens, and the user then undoes back behind where it began.
    history.begin();
    step(&mut doc, &mut history, 30.0);
    history.undo(&mut doc).unwrap();
    history.undo(&mut doc).unwrap();

    assert!(
        !history.abandon(&mut doc).unwrap(),
        "the rollback point is behind us, so there is nothing to roll back to"
    );

    // Redo brings the rollback point back within reach. If the refusal above
    // had consumed the open edit, this would be impossible.
    history.redo(&mut doc).unwrap();
    assert!(
        history.abandon(&mut doc).unwrap(),
        "a refusal must not have thrown the open edit away"
    );

    assert_eq!(doc.len(), before, "rolled back to where the edit began");
    assert!(
        history.can_redo(),
        "and the redo branch from before the edit came back with it"
    );
    history.redo(&mut doc).unwrap();
    assert_eq!(
        doc.len(),
        before + 1,
        "redoing follows the branch that existed before the edit, not the abandoned one"
    );
}

#[test]
fn a_refused_abandon_leaves_a_full_history_exactly_as_it_found_it() {
    // A history small enough that a handful of edits pushes the rollback point
    // off the back of it, with a redo branch open when the edit begins.
    let mut doc = document(200, 200);
    let mut history = History::with_limit(&doc, 2);
    step(&mut doc, &mut history, 10.0);
    step(&mut doc, &mut history, 20.0);
    history.undo(&mut doc).unwrap();
    assert!(history.can_redo());

    history.begin();
    for x in 0..5 {
        step(&mut doc, &mut history, f64::from(x) * 3.0);
    }

    let depth = history.undo_depth();
    let len = doc.len();
    assert!(
        !history.abandon(&mut doc).unwrap(),
        "the point the edit began has fallen off the back of the history"
    );
    assert_eq!(history.undo_depth(), depth, "a refusal is not an edit");
    assert_eq!(doc.len(), len, "and it does not touch the document either");

    // Refusing twice must say the same thing, which it can only do if the first
    // refusal left the open edit where it was.
    assert!(!history.abandon(&mut doc).unwrap());
    assert_eq!(history.undo_depth(), depth);
}

#[test]
fn an_abandon_refuses_when_its_place_in_the_history_was_taken_by_other_work() {
    // Three drawings, then a label started on top of the third.
    let mut doc = document(200, 200);
    let mut history = History::new(&doc);
    step(&mut doc, &mut history, 10.0);
    step(&mut doc, &mut history, 20.0);
    step(&mut doc, &mut history, 30.0);
    history.begin();
    step(&mut doc, &mut history, 40.0);

    // The user undoes their way back behind the point the label was started at.
    for _ in 0..3 {
        history.undo(&mut doc).unwrap();
    }
    assert!(
        !history.abandon(&mut doc).unwrap(),
        "the rollback point is above where the document now stands"
    );

    // Then draws something else, which discards the branch the label was on and
    // builds a different past to exactly the same depth.
    step(&mut doc, &mut history, 50.0);
    step(&mut doc, &mut history, 60.0);
    assert_eq!(
        history.undo_depth(),
        3,
        "the same depth the label was started at, reached by different work"
    );

    let depth = history.undo_depth();
    let len = doc.len();
    assert!(
        !history.abandon(&mut doc).unwrap(),
        "the depth fits but the history under it is somebody else's"
    );
    assert_eq!(history.undo_depth(), depth, "a refusal is not an edit");
    assert_eq!(doc.len(), len, "and it does not touch the document either");

    // The point of refusing: undoing the new work must give back the new work,
    // not a document from the branch that was discarded.
    history.undo(&mut doc).unwrap();
    assert_eq!(doc.len(), 2);
    let after_one = doc.data();
    history.undo(&mut doc).unwrap();
    history.redo(&mut doc).unwrap();
    assert_eq!(
        doc.data(),
        after_one,
        "the history still describes the work that is actually in it"
    );
}

#[test]
fn an_abandon_still_works_after_navigating_within_the_edit() {
    // Undoing and redoing *inside* the edit does not replace anything, so the
    // rollback must still be offered: this is the ⌘Z-then-Escape case the open
    // edit exists for, and refusing it would be the bug it was built to stop.
    let mut doc = document(200, 200);
    let mut history = History::new(&doc);
    step(&mut doc, &mut history, 10.0);
    let before = doc.len();

    history.begin();
    step(&mut doc, &mut history, 20.0);
    step(&mut doc, &mut history, 30.0);
    history.undo(&mut doc).unwrap();

    assert!(
        history.abandon(&mut doc).unwrap(),
        "nothing outside the edit changed, so it can still be taken back"
    );
    assert_eq!(doc.len(), before);
}

#[test]
fn an_abandon_refuses_after_its_rollback_point_was_undone_past_and_rebuilt() {
    // The step below the rollback point is a liar. Undoing does not destroy it,
    // it moves it into the present, and the next commit puts that same step back
    // where it was — same identity, same stamp. A guard that only looked
    // downwards saw its parent and agreed, then restored the abandoned work over
    // the top of whatever had been drawn since and dropped it with no redo.
    let mut doc = document(200, 200);
    let mut history = History::new(&doc);
    step(&mut doc, &mut history, 10.0); // A

    history.begin();
    step(&mut doc, &mut history, 20.0); // the edit
    history.undo(&mut doc).unwrap(); // the edit is set aside
    history.undo(&mut doc).unwrap(); // and so is A: the rollback point is gone

    assert!(
        !history.abandon(&mut doc).unwrap(),
        "the rollback point is below where the history now stands"
    );

    step(&mut doc, &mut history, 30.0); // B, pushed onto A's old place
    let b = doc.data();
    assert_eq!(doc.len(), 1);

    assert!(
        !history.abandon(&mut doc).unwrap(),
        "A's stamp is back underneath, but what stands at the rollback point is B"
    );
    assert_eq!(doc.data(), b, "B is untouched");

    // And B is still the thing the history describes, not a survivor of a
    // branch that was thrown away.
    history.undo(&mut doc).unwrap();
    assert_eq!(doc.len(), 0);
    history.redo(&mut doc).unwrap();
    assert_eq!(doc.data(), b, "B is still redoable");
    assert!(!history.can_redo(), "and nothing else is hiding above it");
}

#[test]
fn an_edit_begun_inside_a_coalescing_group_can_still_be_cancelled() {
    // `begin` marks the state an edit can be taken back to, and `abandon` checks
    // that state is still standing before rolling back to it. A coalescing group
    // still in force broke that check: the edit's first commit carried the same
    // tag, so it *replaced* the marked state instead of pushing it into the past,
    // and the rollback point vanished from under the open edit.
    let (mut doc, mut history) = started();
    doc.add(
        Annotation::Ellipse(rect(60.0, 60.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit_coalesced(&doc, "drag");
    let depth = history.undo_depth();

    history.begin();
    doc.add(
        Annotation::Ellipse(rect(90.0, 90.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit_coalesced(&doc, "drag");
    assert_eq!(doc.len(), 3);

    assert!(
        history.abandon(&mut doc).unwrap(),
        "the edit is still cancellable"
    );
    assert_eq!(
        doc.len(),
        2,
        "and cancelling it went back to where it began"
    );
    assert_eq!(
        history.undo_depth(),
        depth,
        "leaving nothing of the edit behind in the past"
    );
}

#[test]
fn cancelling_an_edit_hands_the_coalescing_group_back() {
    // Ending the group for the duration of the edit must not end it for good: a
    // gesture interrupted by an edit that came to nothing should carry on
    // folding into the same step.
    let (mut doc, mut history) = started();
    doc.add(
        Annotation::Ellipse(rect(60.0, 60.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit_coalesced(&doc, "drag");
    let depth = history.undo_depth();

    history.begin();
    doc.add(
        Annotation::Ellipse(rect(90.0, 90.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit_coalesced(&doc, "drag");
    history.abandon(&mut doc).unwrap();

    // The gesture continues.
    doc.add(
        Annotation::Ellipse(rect(120.0, 120.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit_coalesced(&doc, "drag");
    assert_eq!(
        history.undo_depth(),
        depth,
        "the continuation folded into the step the gesture was already building"
    );

    assert!(history.undo(&mut doc).unwrap());
    assert_eq!(doc.len(), 1, "and that step is the whole gesture");
}

#[test]
fn an_edit_kept_inside_a_coalescing_group_becomes_its_own_step() {
    // The other side of the same coin. Once an edit is accepted rather than
    // cancelled, the group it interrupted has genuinely ended, so the edit is
    // undoable on its own instead of being folded into the gesture before it.
    let (mut doc, mut history) = started();
    doc.add(
        Annotation::Ellipse(rect(60.0, 60.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit_coalesced(&doc, "drag");

    history.begin();
    doc.add(
        Annotation::Ellipse(rect(90.0, 90.0, 20.0, 20.0)),
        Style::stroked(),
    );
    history.commit_coalesced(&doc, "drag");
    history.finish();
    history.seal();

    assert!(history.undo(&mut doc).unwrap());
    assert_eq!(doc.len(), 2, "the edit came back on its own");
    assert!(history.undo(&mut doc).unwrap());
    assert_eq!(doc.len(), 1, "and the gesture before it is still one step");
}

// ---------------------------------------------------------------------------
// Telling a refusal that will pass from one that never will
// ---------------------------------------------------------------------------

#[test]
fn a_history_with_no_open_edit_has_no_rollback_point_to_reach() {
    let (_, history) = started();
    assert!(
        !history.abandon_is_still_reachable(),
        "nothing is open, so there is nothing waiting to become cancellable"
    );
}

#[test]
fn an_open_edit_the_document_has_not_left_is_reachable() {
    let (mut doc, mut history) = started();
    history.begin();
    step(&mut doc, &mut history, 40.0);
    assert!(history.abandon_is_still_reachable());
    assert!(history.abandon(&mut doc).unwrap(), "and it really can be");
}

#[test]
fn a_rollback_point_the_document_only_wandered_away_from_is_still_reachable() {
    // Undoing behind the point refuses, but the point is still in the chain:
    // one redo brings the document back to it and the rollback works again.
    let (mut doc, mut history) = started();
    step(&mut doc, &mut history, 20.0);
    history.begin();
    step(&mut doc, &mut history, 40.0);

    history.undo(&mut doc).unwrap();
    history.undo(&mut doc).unwrap();
    assert!(
        !history.abandon(&mut doc).unwrap(),
        "the document is behind the point, so not yet"
    );
    assert!(
        history.abandon_is_still_reachable(),
        "but the point itself is still there to come back to"
    );

    history.redo(&mut doc).unwrap();
    assert!(
        history.abandon(&mut doc).unwrap(),
        "and coming back to it makes the rollback work"
    );
}

#[test]
fn a_rollback_point_whose_branch_was_truncated_is_gone_for_good() {
    // The refusal that can never be waited out: committing new work discards
    // the branch the rollback point was sitting in, and nothing brings a
    // discarded branch back.
    let (mut doc, mut history) = started();
    step(&mut doc, &mut history, 20.0);
    history.begin();
    step(&mut doc, &mut history, 40.0);

    history.undo(&mut doc).unwrap();
    history.undo(&mut doc).unwrap();
    assert!(history.abandon_is_still_reachable(), "still there for now");

    step(&mut doc, &mut history, 60.0);
    assert!(
        !history.abandon(&mut doc).unwrap(),
        "the point is not in the history any more"
    );
    assert!(
        !history.abandon_is_still_reachable(),
        "and no amount of undoing or redoing will put it back"
    );

    // Whatever the caller does next, the refusal stands.
    history.undo(&mut doc).unwrap();
    assert!(!history.abandon_is_still_reachable());
    history.redo(&mut doc).unwrap();
    assert!(!history.abandon_is_still_reachable());
}

#[test]
fn a_rollback_point_evicted_from_a_full_history_is_gone_for_good() {
    let (mut doc, mut history) = (
        document(200, 200),
        History::with_limit(&document(200, 200), 2),
    );
    history.begin();
    step(&mut doc, &mut history, 10.0);
    assert!(history.abandon_is_still_reachable());

    for x in 2..8 {
        step(&mut doc, &mut history, f64::from(x) * 10.0);
    }
    assert!(
        !history.abandon_is_still_reachable(),
        "the state the edit began at has fallen off the back of the history"
    );
    assert!(!history.abandon(&mut doc).unwrap());
}

#[test]
fn a_rollback_point_replaced_by_different_work_is_gone_for_good() {
    // The lineage case: the same depth, rebuilt out of somebody else's steps.
    let mut doc = document(200, 200);
    let mut history = History::new(&doc);
    step(&mut doc, &mut history, 10.0);
    step(&mut doc, &mut history, 20.0);
    history.begin();
    step(&mut doc, &mut history, 30.0);

    for _ in 0..3 {
        history.undo(&mut doc).unwrap();
    }
    step(&mut doc, &mut history, 40.0);
    step(&mut doc, &mut history, 50.0);
    assert_eq!(history.undo_depth(), 2, "the same depth, different work");
    assert!(
        !history.abandon_is_still_reachable(),
        "depth alone is not the point: that state is not in this history"
    );
}

#[test]
fn finishing_a_stranded_edit_leaves_the_history_it_found() {
    // What the editor does once it learns the wait is over: close the edit and
    // let go. Nothing the user actually did may move.
    let (mut doc, mut history) = started();
    step(&mut doc, &mut history, 20.0);
    history.begin();
    step(&mut doc, &mut history, 40.0);
    history.undo(&mut doc).unwrap();
    history.undo(&mut doc).unwrap();
    step(&mut doc, &mut history, 60.0);

    let depth = history.undo_depth();
    let data = doc.data();
    history.finish();
    assert!(
        !history.abandon_is_still_reachable(),
        "there is no open edit left at all"
    );
    assert_eq!(history.undo_depth(), depth, "closing it is not an edit");
    assert_eq!(doc.data(), data, "and it does not touch the document");

    history.undo(&mut doc).unwrap();
    history.redo(&mut doc).unwrap();
    assert_eq!(
        doc.data(),
        data,
        "the work committed over the stranded point is still the user's"
    );
}
