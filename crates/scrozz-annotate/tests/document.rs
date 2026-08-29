//! The document model: identity, z-order, hit-testing, editing and numbering.

mod common;

use common::{document, every_annotation, rect};
use scrozz_annotate::{
    Annotation, AnnotationKind, ArrowStyle, Color, Document, RedactStyle, Style,
};
use scrozz_core::LogicalPoint;

#[test]
fn add_returns_stable_unique_ids() {
    let mut doc = document(200, 120);
    let a = doc.add(
        Annotation::Rectangle(rect(0.0, 0.0, 10.0, 10.0)),
        Style::stroked(),
    );
    let b = doc.add(
        Annotation::Rectangle(rect(0.0, 0.0, 10.0, 10.0)),
        Style::stroked(),
    );
    assert_ne!(a, b);

    doc.remove(a);
    let c = doc.add(
        Annotation::Rectangle(rect(0.0, 0.0, 10.0, 10.0)),
        Style::stroked(),
    );
    assert_ne!(
        c, a,
        "an id must never be reused: a stale reference would silently address a different object"
    );
    assert!(doc.get(a).is_none());
    assert!(doc.get(b).is_some());
    assert!(doc.get(c).is_some());
}

#[test]
fn hit_test_picks_the_topmost_annotation() {
    let mut doc = document(200, 200);
    let under = doc.add(
        Annotation::Rectangle(rect(10.0, 10.0, 100.0, 100.0)),
        Style::stroked().with_fill(Some(Color::rgb(255, 0, 0))),
    );
    let over = doc.add(
        Annotation::Rectangle(rect(30.0, 30.0, 60.0, 60.0)),
        Style::stroked().with_fill(Some(Color::rgb(0, 255, 0))),
    );

    let inside_both = LogicalPoint::new(50.0, 50.0);
    assert_eq!(doc.hit_test(inside_both), Some(over));

    // And only the lower one where they do not overlap.
    assert_eq!(doc.hit_test(LogicalPoint::new(15.0, 15.0)), Some(under));

    let all = doc.hit_test_all(inside_both);
    assert_eq!(
        all,
        vec![over, under],
        "hit_test_all must be ordered top-most first"
    );
}

#[test]
fn hit_test_follows_z_order_changes() {
    let mut doc = document(200, 200);
    let first = doc.add(
        Annotation::Rectangle(rect(10.0, 10.0, 100.0, 100.0)),
        Style::stroked().with_fill(Some(Color::rgb(255, 0, 0))),
    );
    let second = doc.add(
        Annotation::Rectangle(rect(10.0, 10.0, 100.0, 100.0)),
        Style::stroked().with_fill(Some(Color::rgb(0, 255, 0))),
    );
    let probe = LogicalPoint::new(50.0, 50.0);
    assert_eq!(doc.hit_test(probe), Some(second));

    doc.bring_to_front(first);
    assert_eq!(doc.hit_test(probe), Some(first));
    assert_eq!(doc.z_index(first), Some(1));
    assert_eq!(doc.z_index(second), Some(0));

    doc.send_to_back(first);
    assert_eq!(doc.hit_test(probe), Some(second));

    doc.raise(first);
    assert_eq!(doc.hit_test(probe), Some(first));

    doc.lower(first);
    assert_eq!(doc.hit_test(probe), Some(second));
}

#[test]
fn hit_test_misses_outside_the_tolerance() {
    let mut doc = document(200, 200);
    doc.add(
        Annotation::Rectangle(rect(50.0, 50.0, 40.0, 40.0)),
        Style::stroked(),
    );
    // An unfilled rectangle is hit on its outline, not its interior: clicking
    // the hollow middle of a frame should not select it.
    assert!(doc.hit_test(LogicalPoint::new(70.0, 70.0)).is_none());
    assert!(doc.hit_test(LogicalPoint::new(50.0, 60.0)).is_some());
    assert!(doc.hit_test(LogicalPoint::new(140.0, 140.0)).is_none());
}

#[test]
fn filled_shapes_are_hit_in_their_interior() {
    let mut doc = document(200, 200);
    doc.add(
        Annotation::Ellipse(rect(50.0, 50.0, 60.0, 60.0)),
        Style::stroked().with_fill(Some(Color::rgb(0, 0, 255))),
    );
    assert!(doc.hit_test(LogicalPoint::new(80.0, 80.0)).is_some());
    // Still outside the ellipse even though it is inside the bounding box.
    assert!(doc.hit_test(LogicalPoint::new(52.0, 52.0)).is_none());
}

#[test]
fn arrows_are_hit_along_the_line_not_the_bounding_box() {
    let mut doc = document(200, 200);
    doc.add(
        Annotation::Arrow {
            from: LogicalPoint::new(0.0, 0.0),
            to: LogicalPoint::new(100.0, 100.0),
        },
        Style::stroked(),
    );
    assert!(doc.hit_test(LogicalPoint::new(50.0, 50.0)).is_some());
    assert!(
        doc.hit_test(LogicalPoint::new(95.0, 5.0)).is_none(),
        "the far corner of an arrow's bounding box is empty space"
    );
}

#[test]
fn curved_arrow_bounds_and_hit_testing_follow_the_bend_and_full_head() {
    let mut doc = document(240, 180);
    let id = doc.add(
        Annotation::Arrow {
            from: LogicalPoint::new(30.0, 100.0),
            to: LogicalPoint::new(210.0, 100.0),
        },
        Style::stroked()
            .with_stroke_width(12.0)
            .with_arrow_style(ArrowStyle::Curved)
            .with_arrow_bend(-0.5),
    );
    let object = doc.get(id).unwrap();
    let visual = object.visual_bounds();
    assert!(visual.origin.y < 60.0, "{visual:?}");
    let bend = object.arrow_bend_handle().unwrap();
    assert!(object.hit(bend), "the curved body was not hittable");
    assert!(
        object.hit(LogicalPoint::new(202.0, 88.0)),
        "the broad head was not hittable"
    );
}

#[test]
fn counters_number_from_one_and_renumber_after_deletion() {
    let mut doc = document(300, 300);
    let ids: Vec<_> = (0..4)
        .map(|i| {
            doc.add(
                Annotation::Counter {
                    at: LogicalPoint::new(20.0 * f64::from(i) + 10.0, 20.0),
                    index: 0,
                },
                Style::stroked(),
            )
        })
        .collect();

    assert_eq!(counter_indices(&doc), vec![1, 2, 3, 4]);

    doc.remove(ids[1]);
    assert_eq!(
        counter_indices(&doc),
        vec![1, 2, 3],
        "deleting the second marker must close the gap, not leave a 1,3,4 sequence"
    );

    // The survivors keep their relative order.
    let remaining: Vec<u32> = doc
        .annotations()
        .iter()
        .filter_map(|o| match &o.annotation {
            Annotation::Counter { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(remaining, vec![1, 2, 3]);

    doc.add(
        Annotation::Counter {
            at: LogicalPoint::new(200.0, 20.0),
            index: 99,
        },
        Style::stroked(),
    );
    assert_eq!(
        counter_indices(&doc),
        vec![1, 2, 3, 4],
        "a supplied index is advisory; the document owns the numbering"
    );
}

#[test]
fn counter_numbering_follows_creation_order_not_z_order() {
    let mut doc = document(300, 300);
    let first = doc.add(
        Annotation::Counter {
            at: LogicalPoint::new(10.0, 10.0),
            index: 0,
        },
        Style::stroked(),
    );
    let second = doc.add(
        Annotation::Counter {
            at: LogicalPoint::new(60.0, 10.0),
            index: 0,
        },
        Style::stroked(),
    );

    doc.bring_to_front(first);

    // Raising a marker so it draws above its neighbour must not silently
    // renumber the sequence the user has already read and referred to.
    assert_eq!(index_of(&doc, first), Some(1));
    assert_eq!(index_of(&doc, second), Some(2));
}

#[test]
fn counters_ignore_non_counter_annotations() {
    let mut doc = document(300, 300);
    doc.add(
        Annotation::Counter {
            at: LogicalPoint::new(10.0, 10.0),
            index: 0,
        },
        Style::stroked(),
    );
    doc.add(
        Annotation::Rectangle(rect(0.0, 0.0, 5.0, 5.0)),
        Style::stroked(),
    );
    let third = doc.add(
        Annotation::Counter {
            at: LogicalPoint::new(60.0, 10.0),
            index: 0,
        },
        Style::stroked(),
    );
    assert_eq!(index_of(&doc, third), Some(2));
    assert_eq!(doc.counter_count(), 2);
}

#[test]
fn resizing_to_the_current_bounds_is_a_no_op() {
    // The invariant that keeps a resize handle from creeping: whatever
    // `bounds()` reports must be exactly what `set_bounds` accepts back.
    let mut doc = document(300, 300);
    for (annotation, style) in every_annotation() {
        let id = doc.add(annotation, style);
        let before = doc.get(id).unwrap().annotation.clone();
        let bounds = doc.get(id).unwrap().bounds();
        doc.set_bounds(id, bounds);
        let after = doc.get(id).unwrap().annotation.clone();
        assert_eq!(
            before,
            after,
            "{:?} drifted when resized to its own bounds",
            doc.get(id).unwrap().kind()
        );
    }
}

#[test]
fn visual_bounds_covers_the_stroke_and_geometric_bounds_does_not() {
    let mut doc = document(300, 300);
    let id = doc.add(
        Annotation::Rectangle(rect(50.0, 50.0, 40.0, 40.0)),
        Style::stroked().with_stroke_width(10.0),
    );
    let object = doc.get(id).unwrap();
    assert!((object.bounds().size.width - 40.0).abs() < 1e-9);
    assert!(
        object.visual_bounds().size.width > 40.0,
        "a 10pt stroke paints outside the geometry it outlines"
    );
}

#[test]
fn translate_and_set_bounds_move_and_resize() {
    let mut doc = document(300, 300);
    let id = doc.add(
        Annotation::Rectangle(rect(10.0, 10.0, 20.0, 20.0)),
        Style::stroked(),
    );

    doc.translate(id, 15.0, -5.0);
    let moved = doc.get(id).unwrap().bounds();
    assert!((moved.origin.x - 25.0).abs() < 1e-9);
    assert!((moved.origin.y - 5.0).abs() < 1e-9);
    assert!((moved.size.width - 20.0).abs() < 1e-9);

    doc.set_bounds(id, rect(0.0, 0.0, 40.0, 80.0));
    let resized = doc.get(id).unwrap().bounds();
    assert!((resized.size.width - 40.0).abs() < 1e-9);
    assert!((resized.size.height - 80.0).abs() < 1e-9);
}

#[test]
fn resizing_freehand_remaps_every_point_proportionally() {
    let mut doc = document(300, 300);
    let id = doc.add(
        Annotation::Freehand(vec![
            LogicalPoint::new(0.0, 0.0),
            LogicalPoint::new(10.0, 5.0),
            LogicalPoint::new(20.0, 10.0),
        ]),
        Style::stroked(),
    );
    doc.set_bounds(id, rect(100.0, 100.0, 40.0, 20.0));

    let Some(Annotation::Freehand(points)) = doc.get(id).map(|o| o.annotation.clone()) else {
        panic!("expected freehand");
    };
    assert_eq!(points.len(), 3);
    assert!((points[0].x - 100.0).abs() < 1e-6);
    assert!(
        (points[1].x - 120.0).abs() < 1e-6,
        "the midpoint stays the midpoint"
    );
    assert!((points[2].x - 140.0).abs() < 1e-6);
    assert!((points[2].y - 120.0).abs() < 1e-6);
}

#[test]
fn resizing_a_degenerate_shape_does_not_produce_nan() {
    let mut doc = document(300, 300);
    // A perfectly horizontal freehand stroke has zero height; the remap must not
    // divide by it.
    let id = doc.add(
        Annotation::Freehand(vec![
            LogicalPoint::new(0.0, 50.0),
            LogicalPoint::new(40.0, 50.0),
        ]),
        Style::stroked(),
    );
    doc.set_bounds(id, rect(10.0, 10.0, 80.0, 30.0));
    let Some(Annotation::Freehand(points)) = doc.get(id).map(|o| o.annotation.clone()) else {
        panic!("expected freehand");
    };
    assert!(points.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
}

#[test]
fn get_mut_renumbers_when_an_edit_changes_the_kind_balance() {
    let mut doc = document(300, 300);
    let a = doc.add(
        Annotation::Counter {
            at: LogicalPoint::new(10.0, 10.0),
            index: 0,
        },
        Style::stroked(),
    );
    doc.add(
        Annotation::Counter {
            at: LogicalPoint::new(40.0, 10.0),
            index: 0,
        },
        Style::stroked(),
    );

    // Turning the first marker into a rectangle must renumber the rest.
    {
        let mut handle = doc.get_mut(a).unwrap();
        *handle.annotation() = Annotation::Rectangle(rect(0.0, 0.0, 10.0, 10.0));
    }
    assert_eq!(counter_indices(&doc), vec![1]);
}

#[test]
fn set_style_replaces_only_the_style() {
    let mut doc = document(300, 300);
    let id = doc.add(
        Annotation::Rectangle(rect(1.0, 2.0, 3.0, 4.0)),
        Style::stroked(),
    );
    doc.set_style(id, Style::stroked().with_stroke(Color::rgb(1, 2, 3)));

    let object = doc.get(id).unwrap();
    assert_eq!(object.style.stroke, Color::rgb(1, 2, 3));
    assert_eq!(object.kind(), AnnotationKind::Rectangle);
    assert!((object.bounds().origin.x - 1.0).abs() < 1e-9);
}

#[test]
fn clear_removes_everything_but_keeps_ids_unique() {
    let mut doc = document(100, 100);
    let a = doc.add(
        Annotation::Rectangle(rect(0.0, 0.0, 5.0, 5.0)),
        Style::stroked(),
    );
    doc.clear();
    assert!(doc.is_empty());
    let b = doc.add(
        Annotation::Rectangle(rect(0.0, 0.0, 5.0, 5.0)),
        Style::stroked(),
    );
    assert_ne!(a, b);
}

#[test]
fn logical_bounds_covers_the_source() {
    let doc = document(300, 150);
    let size = doc.logical_size();
    assert!((size.width - 300.0).abs() < 1e-9);
    assert!((size.height - 150.0).abs() < 1e-9);
    let bounds = doc.logical_bounds();
    assert!((bounds.origin.x).abs() < 1e-9);
    assert!((bounds.size.height - 150.0).abs() < 1e-9);
}

#[test]
fn every_variant_reports_a_kind_and_a_bounding_box() {
    let mut doc = document(200, 200);
    let fixture = every_annotation();
    let expected = fixture.len();
    for (annotation, style) in fixture {
        let id = doc.add(annotation, style);
        let object = doc.get(id).unwrap();
        let bounds = object.bounds();
        assert!(
            bounds.size.width >= 0.0 && bounds.size.height >= 0.0,
            "{:?} produced a negative bounding box",
            object.kind()
        );
    }
    assert_eq!(doc.len(), expected);
}

#[test]
fn the_coverage_fixture_includes_every_annotation_kind() {
    // Guards the rest of the suite: several tests sweep `every_annotation()` to
    // assert a property holds for all kinds, and each one silently weakens if a
    // new variant is added to the enum but not to the fixture.
    let covered: Vec<AnnotationKind> = every_annotation()
        .iter()
        .map(|(annotation, _)| annotation.kind())
        .collect();
    let all = [
        AnnotationKind::Arrow,
        AnnotationKind::Line,
        AnnotationKind::Rectangle,
        AnnotationKind::Ellipse,
        AnnotationKind::Freehand,
        AnnotationKind::Text,
        AnnotationKind::Counter,
        AnnotationKind::Highlight,
        AnnotationKind::Redact,
    ];
    for kind in all {
        assert!(
            covered.contains(&kind),
            "{kind:?} is missing from every_annotation(), so nothing sweeps it"
        );
    }
}

#[test]
fn only_redactions_report_as_destructive() {
    for (annotation, _) in every_annotation() {
        let expected = matches!(annotation, Annotation::Redact { .. });
        assert_eq!(
            annotation.is_destructive(),
            expected,
            "{:?} misreported whether it destroys pixels",
            annotation.kind()
        );
    }
    assert!(
        Annotation::Redact {
            area: rect(0.0, 0.0, 1.0, 1.0),
            style: RedactStyle::Solid,
        }
        .is_destructive()
    );
}

fn counter_indices(doc: &Document) -> Vec<u32> {
    let mut out: Vec<u32> = doc
        .annotations()
        .iter()
        .filter_map(|o| match &o.annotation {
            Annotation::Counter { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out
}

fn index_of(doc: &Document, id: scrozz_annotate::AnnotationId) -> Option<u32> {
    match doc.get(id)?.annotation {
        Annotation::Counter { index, .. } => Some(index),
        _ => None,
    }
}

#[test]
fn a_line_is_hit_along_its_length_not_its_bounding_box() {
    let mut doc = document(200, 200);
    let id = doc.add_default(Annotation::Line {
        from: LogicalPoint::new(20.0, 20.0),
        to: LogicalPoint::new(180.0, 180.0),
    });
    assert_eq!(doc.hit_test(LogicalPoint::new(100.0, 100.0)), Some(id));
    assert_eq!(
        doc.hit_test(LogicalPoint::new(170.0, 30.0)),
        None,
        "the far corner of the bounding box is nowhere near the line"
    );
}

#[test]
fn a_line_resizes_by_remapping_its_endpoints() {
    let mut doc = document(400, 400);
    let id = doc.add_default(Annotation::Line {
        from: LogicalPoint::new(0.0, 0.0),
        to: LogicalPoint::new(100.0, 50.0),
    });
    doc.set_bounds(id, rect(200.0, 100.0, 200.0, 100.0));
    let Annotation::Line { from, to } = doc.get(id).unwrap().annotation.clone() else {
        panic!("kind changed under resize");
    };
    assert_eq!(from, LogicalPoint::new(200.0, 100.0));
    assert_eq!(to, LogicalPoint::new(400.0, 200.0));
}

#[test]
fn a_line_translates_both_endpoints_together() {
    let mut doc = document(200, 200);
    let id = doc.add_default(Annotation::Line {
        from: LogicalPoint::new(10.0, 10.0),
        to: LogicalPoint::new(60.0, 40.0),
    });
    doc.translate(id, 5.0, -3.0);
    let Annotation::Line { from, to } = doc.get(id).unwrap().annotation.clone() else {
        panic!("kind changed under translate");
    };
    assert_eq!(from, LogicalPoint::new(15.0, 7.0));
    assert_eq!(to, LogicalPoint::new(65.0, 37.0));
}

#[test]
fn a_line_reports_its_own_kind() {
    let mut doc = document(200, 200);
    let id = doc.add_default(Annotation::Line {
        from: LogicalPoint::new(0.0, 0.0),
        to: LogicalPoint::new(10.0, 10.0),
    });
    assert_eq!(doc.get(id).unwrap().annotation.kind(), AnnotationKind::Line);
    assert_ne!(
        doc.get(id).unwrap().annotation.kind(),
        AnnotationKind::Arrow,
        "a line is not a headless arrow"
    );
}
