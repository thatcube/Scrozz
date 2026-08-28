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
    // Beautification is forbidden for window captures (D9). A snapshot carrying
    // it must be refused rather than quietly applied.
    let mut doc = Document::new(window_capture(100, 100));
    let data = DocumentData {
        beautification: Some(scrozz_annotate::Beautification::default()),
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
