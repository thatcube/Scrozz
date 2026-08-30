//! The annotation editor's state machine, exercised without a window.
//!
//! [`EditorState`] is deliberately free of egui so that gestures, accelerators
//! and undo can be asserted as plain function calls. Everything the editor's
//! canvas does — press, drag, release — arrives here, so a test that drives
//! those three in sequence is testing the real path a mouse takes, not a
//! parallel one written for the test's convenience.

use std::sync::Arc;

use egui::CursorIcon;
use scrozz_annotate::{
    AnalysisCancellation, Annotation, AnnotationKind, ArrowStyle, Color, Document,
    ImageOrientation, RedactStyle, Style,
};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Provenance, ScaleFactor,
};
use scrozz_ui::editor::{
    Caret, Command, CropAspect, EditorState, Handle, Intent, MAX_ZOOM, MIN_DRAG, MIN_SIZE,
    MIN_ZOOM, TextEdit, Tool, crop::StructuralBoundaryIndex, paint::crop_cursor,
};

/// A flat capture, 400x300 logical at 2x, big enough that the minimum-size
/// clamp never fires by accident on the drags these tests perform.
fn capture() -> Capture {
    Capture {
        frame: Frame {
            data: vec![200u8; 800 * 600 * 4],
            size: PhysicalSize::new(800.0, 600.0),
            stride: 800 * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::new(2.0),
        },
        provenance: Provenance::Region,
        target: CaptureTarget::Region(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(400.0, 300.0),
        )),
    }
}

fn state() -> EditorState {
    EditorState::new(Document::new(capture()))
}

fn at(x: f64, y: f64) -> LogicalPoint {
    LogicalPoint::new(x, y)
}

/// Draws a shape by dragging, exactly as the canvas would.
fn drag(state: &mut EditorState, from: LogicalPoint, to: LogicalPoint) {
    state.pointer_pressed(from);
    state.pointer_dragged(to, false);
    state.pointer_released();
}

fn draft_crop(state: &mut EditorState, left: f64, top: f64, right: f64, bottom: f64) {
    state.set_tool(Tool::Crop);
    let full = state.pending_crop().expect("crop mode");
    drag(state, Handle::TopLeft.position(&full), at(left, top));
    let after_top_left = state.pending_crop().expect("crop after first edge");
    drag(
        state,
        Handle::BottomRight.position(&after_top_left),
        at(right, bottom),
    );
}

fn rect_tools() -> Vec<Tool> {
    Tool::ALL.into_iter().filter(|t| t.is_rect_drag()).collect()
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[test]
fn every_tool_has_a_unique_accelerator() {
    let mut seen = Vec::new();
    for tool in Tool::ALL {
        let key = tool.accelerator();
        assert!(
            !seen.contains(&key),
            "{tool:?} reuses the accelerator {key:?}"
        );
        seen.push(key);
    }
}

#[test]
fn crop_is_first_in_the_document_palette() {
    assert_eq!(Tool::ALL[0], Tool::Crop);
}

#[test]
fn accelerators_round_trip_to_their_tool() {
    for tool in Tool::ALL {
        assert_eq!(Tool::from_accelerator(tool.accelerator()), Some(tool));
    }
}

#[test]
fn accelerators_are_case_insensitive() {
    for tool in Tool::ALL {
        let upper = tool.accelerator().to_ascii_uppercase();
        assert_eq!(Tool::from_accelerator(upper), Some(tool));
    }
}

#[test]
fn redact_owns_p_and_legacy_privacy_shortcuts_are_not_exposed() {
    assert_eq!(Tool::from_accelerator('p'), Some(Tool::Redact));
    assert_eq!(Tool::from_accelerator('b'), None);
    assert_eq!(Tool::from_accelerator('x'), None);
}

#[test]
fn an_unbound_key_picks_no_tool() {
    assert_eq!(Tool::from_accelerator('!'), None);
}

#[test]
fn only_the_tools_that_edit_the_document_have_a_kind() {
    for tool in Tool::ALL {
        let edits = !matches!(tool, Tool::Select | Tool::Crop);
        assert_eq!(
            tool.kind().is_some(),
            edits,
            "{tool:?} disagrees about whether it adds an annotation"
        );
    }
}

#[test]
fn a_tool_is_never_both_a_drag_and_a_click() {
    for tool in Tool::ALL {
        assert!(
            !(tool.is_rect_drag() && tool.is_click_place()),
            "{tool:?} claims to be placed two different ways"
        );
    }
}

#[test]
fn the_pointer_and_the_crop_place_nothing() {
    for tool in [Tool::Select, Tool::Crop] {
        assert!(!tool.is_rect_drag());
        assert!(!tool.is_click_place());
    }
}

#[test]
fn picking_a_tool_clears_the_selection() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(20.0, 20.0), at(120.0, 90.0));
    assert!(state.selection().is_some());

    state.command(Command::Pick(Tool::Ellipse)).expect("pick");
    assert_eq!(state.tool(), Tool::Ellipse);
    assert_eq!(state.selection(), None);
}

#[test]
fn a_drawing_tool_stays_in_hand_after_a_stroke() {
    let mut state = state();
    state.set_tool(Tool::Arrow);
    drag(&mut state, at(10.0, 10.0), at(90.0, 70.0));
    assert_eq!(state.tool(), Tool::Arrow, "the tool reverted after one use");
}

fn arrow_state() -> EditorState {
    let mut state = state();
    state.set_tool(Tool::Arrow);
    drag(&mut state, at(40.0, 60.0), at(200.0, 160.0));
    state
}

fn arrow_points(state: &EditorState) -> (LogicalPoint, LogicalPoint) {
    match state.document().annotations()[0].annotation {
        Annotation::Arrow { from, to } => (from, to),
        _ => panic!("the fixture is not an arrow"),
    }
}

#[test]
fn an_arrow_has_no_handles_unselected_and_exactly_two_selected() {
    let mut state = arrow_state();
    assert_eq!(
        state.selection_handles(),
        vec![
            (Handle::ArrowStart, at(40.0, 60.0)),
            (Handle::ArrowEnd, at(200.0, 160.0)),
        ]
    );

    state.select(None);
    assert!(state.selection_handles().is_empty());
}

#[test]
fn the_arrow_tool_selects_an_existing_arrow_without_switching_tools() {
    let mut state = arrow_state();
    let id = state.document().annotations()[0].id;
    state.select(None);

    state.pointer_pressed(at(120.0, 110.0));
    state.pointer_released();

    assert_eq!(state.selection(), Some(id));
    assert_eq!(state.tool(), Tool::Arrow);
    assert_eq!(state.document().len(), 1, "the click created another arrow");
}

#[test]
fn either_arrow_endpoint_can_be_dragged_directly() {
    for (handle, moved) in [
        (Handle::ArrowStart, at(20.0, 30.0)),
        (Handle::ArrowEnd, at(250.0, 190.0)),
    ] {
        let mut state = arrow_state();
        let before = arrow_points(&state);
        let grab = state
            .selection_handles()
            .into_iter()
            .find(|(candidate, _)| *candidate == handle)
            .map(|(_, point)| point)
            .expect("the endpoint handle exists");

        drag(&mut state, grab, moved);

        let after = arrow_points(&state);
        match handle {
            Handle::ArrowStart => {
                assert_eq!(after.0, moved);
                assert_eq!(after.1, before.1);
            }
            Handle::ArrowEnd => {
                assert_eq!(after.0, before.0);
                assert_eq!(after.1, moved);
            }
            _ => unreachable!(),
        }
        assert_eq!(state.tool(), Tool::Arrow);
        assert_eq!(state.selection_handles().len(), 2);
    }
}

#[test]
fn overlapping_arrow_handles_choose_the_nearest_endpoint() {
    let mut state = state();
    state.set_tool(Tool::Arrow);
    drag(&mut state, at(100.0, 100.0), at(103.0, 100.0));
    assert_eq!(state.selection_handles().len(), 2);

    drag(&mut state, at(103.0, 100.0), at(120.0, 100.0));

    assert_eq!(arrow_points(&state), (at(100.0, 100.0), at(120.0, 100.0)));
}

#[test]
fn dragging_an_arrow_body_moves_both_endpoints_in_arrow_mode() {
    let mut state = arrow_state();
    let before = arrow_points(&state);

    drag(&mut state, at(88.0, 90.0), at(118.0, 110.0));

    let after = arrow_points(&state);
    assert_eq!(after.0, at(before.0.x + 30.0, before.0.y + 20.0));
    assert_eq!(after.1, at(before.1.x + 30.0, before.1.y + 20.0));
    assert_eq!(state.tool(), Tool::Arrow);
}

#[test]
fn empty_arrow_clicks_and_subthreshold_drags_create_nothing() {
    let mut state = state();
    state.set_tool(Tool::Arrow);

    state.pointer_pressed(at(280.0, 220.0));
    state.pointer_released();
    drag(
        &mut state,
        at(280.0, 220.0),
        at(280.0 + MIN_DRAG / 2.0, 220.0),
    );
    assert!(state.document().is_empty());

    drag(
        &mut state,
        at(280.0, 220.0),
        at(280.0 + MIN_DRAG * 2.0, 220.0),
    );
    assert_eq!(state.document().len(), 1);
    assert_eq!(state.tool(), Tool::Arrow);
}

#[test]
fn arrow_endpoint_hit_targets_stay_screen_sized_across_zoom() {
    let mut state = arrow_state();
    let endpoint = arrow_points(&state).1;

    state.set_view_scale(0.25);
    assert_eq!(
        state.handle_at(at(endpoint.x + 20.0, endpoint.y)),
        Some(Handle::ArrowEnd),
        "a 5pt screen offset should still reach the endpoint when zoomed out"
    );

    state.set_view_scale(4.0);
    assert_eq!(
        state.handle_at(at(endpoint.x + 1.0, endpoint.y)),
        Some(Handle::ArrowEnd)
    );
    assert_eq!(
        state.handle_at(at(endpoint.x + 5.0, endpoint.y)),
        None,
        "a 20pt screen offset should not hit an oversized invisible target"
    );
}

#[test]
fn one_endpoint_drag_is_one_undo_step() {
    let mut state = arrow_state();
    let before = arrow_points(&state);
    let depth = state.undo_depth();
    let start = before.0;

    drag(&mut state, start, at(10.0, 20.0));

    assert_eq!(state.undo_depth(), depth + 1);
    state.command(Command::Undo).expect("undo endpoint drag");
    assert_eq!(arrow_points(&state), before);
    assert_eq!(state.tool(), Tool::Arrow);
}

#[test]
fn curved_arrow_bend_uses_a_distinct_handle_and_one_undo_step() {
    let mut state = state();
    state.set_tool(Tool::Arrow);
    state.set_arrow_style(ArrowStyle::Curved);
    drag(&mut state, at(40.0, 60.0), at(200.0, 160.0));
    let endpoints = arrow_points(&state);
    let handle = state.arrow_bend_handle().expect("bend diamond");
    let depth = state.undo_depth();

    drag(&mut state, handle, at(handle.x + 25.0, handle.y - 30.0));

    assert_eq!(state.selection_handles().len(), 2);
    assert_ne!(state.arrow_bend(), 0.28);
    assert_eq!(state.undo_depth(), depth + 1);
    assert_eq!(arrow_points(&state), endpoints);
    state.command(Command::Undo).expect("undo bend");
    assert!((state.arrow_bend() - 0.28).abs() < 0.01);
}

#[test]
fn every_arrow_style_and_named_thickness_becomes_the_future_default() {
    for style in [
        ArrowStyle::Bold,
        ArrowStyle::Curved,
        ArrowStyle::Sketch,
        ArrowStyle::Double,
    ] {
        let mut state = state();
        state.set_tool(Tool::Arrow);
        state.set_arrow_style(style);
        state.set_stroke_width(14.0);
        drag(&mut state, at(30.0, 30.0), at(180.0, 120.0));
        let object = &state.document().annotations()[0];
        assert_eq!(object.style.arrow_style, style);
        assert_eq!(object.style.stroke_width, 14.0);
        assert_eq!(state.tool(), Tool::Arrow);
    }
}

#[test]
fn selected_arrow_style_thickness_and_bend_changes_coalesce_and_undo_together() {
    let mut state = arrow_state();
    let id = state.selection().expect("selected arrow");
    let original = state.document().get(id).unwrap().style;
    let depth = state.undo_depth();

    state.set_arrow_style(ArrowStyle::Sketch);
    state.set_stroke_width(14.0);
    state.set_arrow_bend(-0.35);

    let changed = state.document().get(id).unwrap().style;
    assert_eq!(changed.arrow_style, ArrowStyle::Sketch);
    assert_eq!(changed.stroke_width, 14.0);
    assert_eq!(changed.arrow_bend, -0.35);
    assert_eq!(state.undo_depth(), depth + 1);

    state.command(Command::Undo).expect("undo arrow appearance");
    assert_eq!(state.document().get(id).unwrap().style, original);
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

#[test]
fn each_drag_tool_creates_exactly_one_annotation() {
    for tool in rect_tools() {
        let mut state = state();
        state.set_tool(tool);
        drag(&mut state, at(30.0, 30.0), at(150.0, 120.0));
        assert_eq!(
            state.document().annotations().len(),
            1,
            "{tool:?} did not create one annotation"
        );
        assert_eq!(
            state.document().annotations()[0].annotation.kind(),
            tool.kind().expect("a drag tool has a kind"),
            "{tool:?} created the wrong kind"
        );
    }
}

#[test]
fn each_drag_tool_selects_what_it_just_drew() {
    for tool in rect_tools() {
        let mut state = state();
        state.set_tool(tool);
        drag(&mut state, at(30.0, 30.0), at(150.0, 120.0));
        let drawn = state.document().annotations()[0].id;
        assert_eq!(
            state.selection(),
            Some(drawn),
            "{tool:?} left nothing selected"
        );
    }
}

#[test]
fn a_click_that_never_moves_draws_nothing() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    state.pointer_pressed(at(40.0, 40.0));
    state.pointer_dragged(at(41.0, 41.0), false);
    state.pointer_released();
    assert!(
        state.document().annotations().is_empty(),
        "a sub-threshold twitch left a shape behind"
    );
}

#[test]
fn the_pen_records_more_than_two_points() {
    let mut state = state();
    state.set_tool(Tool::Pen);
    state.pointer_pressed(at(10.0, 10.0));
    for step in 1..=8 {
        state.pointer_dragged(at(10.0 + f64::from(step) * 9.0, 20.0), false);
    }
    state.pointer_released();

    let annotations = state.document().annotations();
    assert_eq!(annotations.len(), 1);
    let Annotation::Freehand(points) = &annotations[0].annotation else {
        panic!("the pen drew something other than a freehand stroke");
    };
    assert!(points.len() > 2, "only {} points recorded", points.len());
}

#[test]
fn a_counter_is_placed_by_a_single_click() {
    let mut state = state();
    state.set_tool(Tool::Counter);
    state.pointer_pressed(at(70.0, 70.0));
    state.pointer_released();
    assert_eq!(state.document().annotations().len(), 1);
    assert_eq!(
        state.document().annotations()[0].annotation.kind(),
        AnnotationKind::Counter
    );
}

#[test]
fn counters_number_themselves_in_order() {
    let mut state = state();
    state.set_tool(Tool::Counter);
    for step in 0..3 {
        state.pointer_pressed(at(40.0 + f64::from(step) * 30.0, 60.0));
        state.pointer_released();
    }

    let numbers: Vec<u32> = state
        .document()
        .annotations()
        .iter()
        .filter_map(|o| match o.annotation {
            Annotation::Counter { index, .. } => Some(index),
            _ => None,
        })
        .collect();
    assert_eq!(numbers, vec![1, 2, 3]);
}

#[test]
fn holding_shift_squares_a_rectangle() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    state.pointer_pressed(at(20.0, 20.0));
    state.pointer_dragged(at(140.0, 60.0), true);
    state.pointer_released();

    let bounds = state.document().annotations()[0].annotation.bounds();
    assert!(
        (bounds.size.width - bounds.size.height).abs() < 0.001,
        "constrained drag produced {bounds:?}"
    );
}

#[test]
fn holding_shift_locks_a_line_to_an_axis() {
    let mut state = state();
    state.set_tool(Tool::Line);
    state.pointer_pressed(at(20.0, 20.0));
    state.pointer_dragged(at(140.0, 26.0), true);
    state.pointer_released();

    let Annotation::Line { from, to } = state.document().annotations()[0].annotation else {
        panic!("the line tool drew something else");
    };
    assert!(
        (from.y - to.y).abs() < 0.001,
        "a shallow drag did not snap flat: {from:?} -> {to:?}"
    );
}

#[test]
fn a_new_shape_takes_the_current_style() {
    let mut state = state();
    state.set_stroke_color(Color::rgb(10, 200, 90));
    state.set_stroke_width(9.0);
    state.set_tool(Tool::Ellipse);
    drag(&mut state, at(20.0, 20.0), at(120.0, 100.0));

    let style = state.document().annotations()[0].style;
    assert_eq!(style.stroke, Color::rgb(10, 200, 90));
    assert!((style.stroke_width - 9.0).abs() < 0.001);
}

#[test]
fn highlight_and_redaction_ignore_the_palette() {
    for tool in [Tool::Highlight, Tool::Redact] {
        let mut state = state();
        state.set_stroke_color(Color::rgb(255, 0, 0));
        state.set_tool(tool);
        drag(&mut state, at(20.0, 20.0), at(120.0, 100.0));

        let style = state.document().annotations()[0].style;
        assert_ne!(
            style.stroke,
            Color::rgb(255, 0, 0),
            "{tool:?} adopted the stroke colour, which it has no use for"
        );
    }
}

#[test]
fn the_one_redact_tool_creates_a_secure_mosaic() {
    let mut state = state();
    state.set_tool(Tool::Redact);
    drag(&mut state, at(20.0, 20.0), at(120.0, 100.0));

    let object = &state.document().annotations()[0];
    assert!(matches!(
        object.annotation,
        Annotation::Redact {
            style: RedactStyle::Pixelate,
            ..
        }
    ));
    assert_eq!(
        object.style.effective_redact_intensity(),
        Some(scrozz_annotate::REDACT_INTENSITY_DEFAULT)
    );
}

#[test]
fn redact_intensity_is_bounded_inherited_and_selected_edits_coalesce() {
    let mut state = state();
    state.set_tool(Tool::Redact);
    state.set_redact_intensity(0.2);
    drag(&mut state, at(20.0, 20.0), at(120.0, 100.0));
    let id = state.selection().expect("new redaction selected");
    assert_eq!(
        state
            .document()
            .get(id)
            .unwrap()
            .style
            .effective_redact_intensity(),
        Some(0.2)
    );
    let depth = state.undo_depth();

    state.set_redact_intensity(0.5);
    state.set_redact_intensity(0.9);
    assert_eq!(state.undo_depth(), depth + 1);
    assert_eq!(
        state
            .document()
            .get(id)
            .unwrap()
            .style
            .effective_redact_intensity(),
        Some(0.9)
    );

    let revision = state.revision();
    state.set_redact_intensity(0.9);
    assert_eq!(
        state.revision(),
        revision,
        "an unchanged slider value edited content"
    );

    state.command(Command::Undo).expect("undo intensity");
    assert_eq!(
        state
            .document()
            .get(id)
            .unwrap()
            .style
            .effective_redact_intensity(),
        Some(0.2)
    );

    state.set_redact_intensity(f32::NAN);
    assert_eq!(
        state.redact_intensity(),
        scrozz_annotate::REDACT_INTENSITY_DEFAULT
    );
    state.set_redact_intensity(99.0);
    assert_eq!(state.redact_intensity(), 1.0);

    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(160.0, 20.0), at(220.0, 80.0));
    assert_eq!(
        state.document().annotations()[1]
            .style
            .effective_redact_intensity(),
        None,
        "secure-render metadata leaked onto an ordinary shape"
    );
}

#[test]
fn changing_a_legacy_redaction_intensity_migrates_only_that_object_to_secure_rendering() {
    let mut document = Document::new(capture());
    let id = document.add(
        Annotation::Redact {
            area: LogicalRect::new(at(20.0, 20.0), LogicalSize::new(100.0, 80.0)),
            style: RedactStyle::Blur,
        },
        Style::redaction(),
    );
    let mut state = EditorState::new(document);
    state.select(Some(id));
    assert_eq!(
        state
            .document()
            .get(id)
            .unwrap()
            .style
            .effective_redact_intensity(),
        None
    );

    state.set_redact_intensity(0.75);

    let object = state.document().get(id).unwrap();
    assert!(matches!(
        object.annotation,
        Annotation::Redact {
            style: RedactStyle::Blur,
            ..
        }
    ));
    assert_eq!(object.style.effective_redact_intensity(), Some(0.75));
}

// ---------------------------------------------------------------------------
// Selection, move, resize, delete
// ---------------------------------------------------------------------------

#[test]
fn clicking_a_shape_selects_it_and_empty_space_clears_it() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    let id = state.document().annotations()[0].id;

    state.set_tool(Tool::Select);
    state.select(None);
    // A hollow rectangle is grabbed by its outline, not its empty middle.
    state.pointer_pressed(at(70.0, 40.0));
    state.pointer_released();
    assert_eq!(state.selection(), Some(id));

    state.pointer_pressed(at(340.0, 260.0));
    state.pointer_released();
    assert_eq!(state.selection(), None);
}

#[test]
fn clicking_overlapping_shapes_selects_the_topmost() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(200.0, 180.0));
    state.set_tool(Tool::Ellipse);
    drag(&mut state, at(60.0, 60.0), at(180.0, 160.0));
    let top = state.document().annotations()[1].id;

    state.set_tool(Tool::Select);
    state.select(None);
    // On the ellipse's outline, which also lies inside the rectangle.
    state.pointer_pressed(at(120.0, 60.0));
    state.pointer_released();
    assert_eq!(state.selection(), Some(top));
}

#[test]
fn dragging_a_selected_shape_moves_it_without_resizing() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    let before = state.selection_bounds().expect("bounds");

    state.set_tool(Tool::Select);
    drag(&mut state, at(70.0, 40.0), at(100.0, 60.0));
    let after = state.selection_bounds().expect("bounds");

    assert!((after.origin.x - before.origin.x - 30.0).abs() < 0.5);
    assert!((after.origin.y - before.origin.y - 20.0).abs() < 0.5);
    assert!((after.size.width - before.size.width).abs() < 0.001);
    assert!((after.size.height - before.size.height).abs() < 0.001);
}

#[test]
fn every_handle_is_reachable_on_a_selected_shape() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(200.0, 180.0));
    let bounds = state.selection_bounds().expect("bounds");
    state.set_tool(Tool::Select);

    for handle in Handle::ALL {
        let point = handle.position(&bounds);
        assert_eq!(
            state.handle_at(point),
            Some(handle),
            "{handle:?} was not grabbable at its own position"
        );
    }
}

#[test]
fn every_handle_resizes_the_edge_it_names() {
    let start = LogicalRect::new(LogicalPoint::new(50.0, 50.0), LogicalSize::new(100.0, 80.0));
    for handle in Handle::ALL {
        let moved = handle.resize(&start, 12.0, 9.0);
        let left_moved = (moved.origin.x - start.origin.x).abs() > 0.001;
        let top_moved = (moved.origin.y - start.origin.y).abs() > 0.001;
        let right_moved =
            ((moved.origin.x + moved.size.width) - (start.origin.x + start.size.width)).abs()
                > 0.001;
        let bottom_moved =
            ((moved.origin.y + moved.size.height) - (start.origin.y + start.size.height)).abs()
                > 0.001;

        assert_eq!(left_moved, handle.moves_left(), "{handle:?} left edge");
        assert_eq!(right_moved, handle.moves_right(), "{handle:?} right edge");
        assert_eq!(top_moved, handle.moves_top(), "{handle:?} top edge");
        assert_eq!(
            bottom_moved,
            handle.moves_bottom(),
            "{handle:?} bottom edge"
        );
    }
}

#[test]
fn a_handle_dragged_past_its_opposite_flips_instead_of_inverting() {
    let start = LogicalRect::new(LogicalPoint::new(50.0, 50.0), LogicalSize::new(100.0, 80.0));
    let flipped = Handle::TopLeft.resize(&start, 400.0, 400.0);
    assert!(flipped.size.width > 0.0, "width went negative");
    assert!(flipped.size.height > 0.0, "height went negative");
}

#[test]
fn dragging_a_handle_resizes_the_selection() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(200.0, 180.0));
    let before = state.selection_bounds().expect("bounds");
    state.set_tool(Tool::Select);

    let grab = Handle::BottomRight.position(&before);
    drag(&mut state, grab, at(grab.x + 40.0, grab.y + 30.0));
    let after = state.selection_bounds().expect("bounds");

    assert!(after.size.width > before.size.width);
    assert!(after.size.height > before.size.height);
    assert!(
        (after.origin.x - before.origin.x).abs() < 0.5,
        "origin moved"
    );
}

#[test]
fn delete_removes_the_selection() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));

    state.command(Command::Delete).expect("delete");
    assert!(state.document().annotations().is_empty());
    assert_eq!(state.selection(), None);
}

#[test]
fn delete_with_nothing_selected_clears_the_crop() {
    let mut state = state();
    draft_crop(&mut state, 40.0, 40.0, 200.0, 180.0);
    state.command(Command::ApplyCrop).expect("crop");
    assert!(state.document().crop().is_some());

    state.select(None);
    state.command(Command::Delete).expect("delete");
    assert_eq!(state.document().crop(), None, "the crop survived delete");
}

#[test]
fn nudging_moves_the_selection_by_the_amount_asked_for() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    let before = state.selection_bounds().expect("bounds");

    state
        .command(Command::Nudge { dx: 4.0, dy: -3.0 })
        .expect("nudge");
    let after = state.selection_bounds().expect("bounds");
    assert!((after.origin.x - before.origin.x - 4.0).abs() < 0.001);
    assert!((after.origin.y - before.origin.y + 3.0).abs() < 0.001);
}

#[test]
fn nudging_nothing_changes_nothing() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    state.select(None);
    let before = state.document().annotations()[0].annotation.bounds();

    state
        .command(Command::Nudge { dx: 9.0, dy: 9.0 })
        .expect("nudge");
    assert_eq!(
        state.document().annotations()[0].annotation.bounds(),
        before
    );
}

#[test]
fn z_order_commands_move_the_selection_to_the_ends() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(20.0, 20.0), at(100.0, 90.0));
    let first = state.document().annotations()[0].id;
    drag(&mut state, at(120.0, 20.0), at(200.0, 90.0));

    state.select(Some(first));
    state.command(Command::BringToFront).expect("front");
    assert_eq!(
        state.document().annotations().last().expect("last").id,
        first
    );

    state.command(Command::SendToBack).expect("back");
    assert_eq!(state.document().annotations()[0].id, first);
}

// ---------------------------------------------------------------------------
// Undo and redo
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_editor_has_nothing_to_undo() {
    let state = state();
    assert!(!state.can_undo());
    assert!(!state.can_redo());
    assert!(!state.is_dirty());
}

#[test]
fn undo_removes_a_drawn_shape_and_redo_restores_it() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    assert!(state.can_undo());

    state.command(Command::Undo).expect("undo");
    assert!(state.document().annotations().is_empty());
    assert!(state.can_redo());

    state.command(Command::Redo).expect("redo");
    assert_eq!(state.document().annotations().len(), 1);
}

#[test]
fn a_whole_drag_costs_exactly_one_undo() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    state.pointer_pressed(at(40.0, 40.0));
    for step in 1..=10 {
        state.pointer_dragged(
            at(40.0 + f64::from(step) * 12.0, 40.0 + f64::from(step) * 9.0),
            false,
        );
    }
    state.pointer_released();

    state.command(Command::Undo).expect("undo");
    assert!(
        state.document().annotations().is_empty(),
        "one undo did not unwind the whole drag"
    );
    assert!(!state.can_undo(), "the drag left extra history behind");
}

#[test]
fn a_move_and_the_draw_before_it_are_separate_steps() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    let drawn = state.selection_bounds().expect("bounds");

    state.set_tool(Tool::Select);
    drag(&mut state, at(70.0, 40.0), at(110.0, 70.0));
    assert_ne!(state.selection_bounds().expect("bounds"), drawn);

    state.command(Command::Undo).expect("undo");
    assert_eq!(state.document().annotations().len(), 1, "undo went too far");
    assert_eq!(state.selection_bounds().expect("bounds"), drawn);
}

#[test]
fn a_new_edit_discards_the_redo_branch() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    state.command(Command::Undo).expect("undo");
    assert!(state.can_redo());

    state.set_tool(Tool::Ellipse);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    assert!(!state.can_redo(), "an abandoned future survived a new edit");
}

#[test]
fn undo_during_a_drag_abandons_the_drag() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));

    state.set_tool(Tool::Ellipse);
    state.pointer_pressed(at(200.0, 40.0));
    state.pointer_dragged(at(300.0, 140.0), false);
    assert!(state.is_dragging());

    state.command(Command::Undo).expect("undo");
    assert!(!state.is_dragging());
    assert!(state.document().annotations().is_empty());
}

#[test]
fn consecutive_nudges_coalesce_into_one_step() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    let drawn = state.selection_bounds().expect("bounds");

    for _ in 0..5 {
        state
            .command(Command::Nudge { dx: 1.0, dy: 0.0 })
            .expect("nudge");
    }
    state.command(Command::Undo).expect("undo");
    assert_eq!(
        state.selection_bounds().expect("bounds"),
        drawn,
        "one undo did not unwind a run of nudges"
    );
}

#[test]
fn undo_restores_a_deleted_shape() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    state.command(Command::Delete).expect("delete");
    state.command(Command::Undo).expect("undo");
    assert_eq!(state.document().annotations().len(), 1);
}

#[test]
fn undo_reverses_a_crop() {
    let mut state = state();
    draft_crop(&mut state, 40.0, 40.0, 200.0, 180.0);
    state.command(Command::ApplyCrop).expect("crop");
    assert!(state.document().crop().is_some());

    state.command(Command::Undo).expect("undo");
    assert_eq!(state.document().crop(), None);
}

// ---------------------------------------------------------------------------
// Crop
// ---------------------------------------------------------------------------

#[test]
fn a_crop_shrinks_the_content_without_touching_the_source() {
    let mut state = state();
    let full = state.document().content_size();
    draft_crop(&mut state, 40.0, 40.0, 200.0, 180.0);
    state.command(Command::ApplyCrop).expect("crop");

    let cropped = state.document().content_size();
    assert!(cropped.width < full.width);
    assert!(cropped.height < full.height);
    assert_eq!(
        state.document().source().frame.width(),
        capture().frame.width(),
        "cropping resized the source pixels"
    );
}

#[test]
fn applying_a_crop_returns_to_the_pointer() {
    let mut state = state();
    draft_crop(&mut state, 40.0, 40.0, 200.0, 180.0);
    state.command(Command::ApplyCrop).expect("crop");
    assert_eq!(state.tool(), Tool::Select);
}

#[test]
fn apply_crop_without_a_pending_rect_does_nothing() {
    let mut state = state();
    state.command(Command::ApplyCrop).expect("crop");
    assert_eq!(state.document().crop(), None);
    assert!(!state.can_undo());
}

#[test]
fn a_pending_crop_is_visible_before_it_is_committed() {
    let mut state = state();
    state.set_tool(Tool::Crop);
    assert_eq!(
        state.pending_crop(),
        Some(state.document().logical_bounds()),
        "crop mode must begin at the whole current image"
    );
    state.set_crop_width(200.0);
    assert!(state.pending_crop().unwrap().size.width < 400.0);
    assert_eq!(
        state.document().crop(),
        None,
        "the crop was applied before it was committed"
    );
}

#[test]
fn crop_mode_starts_at_the_entire_current_image_without_editing_it() {
    let mut state = state();
    let revision = state.revision();
    state.set_tool(Tool::Crop);

    assert_eq!(
        state.pending_crop(),
        Some(state.document().logical_bounds())
    );
    assert_eq!(state.document().crop(), None);
    assert_eq!(state.revision(), revision);
    assert!(!state.can_undo());
}

#[test]
fn crop_edges_corners_and_inside_move_the_draft_directly() {
    let mut state = state();
    draft_crop(&mut state, 40.0, 30.0, 260.0, 210.0);
    let narrowed = state.pending_crop().unwrap();
    assert_eq!(narrowed.origin, at(40.0, 30.0));
    assert_eq!(narrowed.size, LogicalSize::new(220.0, 180.0));

    let centre = LogicalPoint::new(
        narrowed.origin.x + narrowed.size.width / 2.0,
        narrowed.origin.y + narrowed.size.height / 2.0,
    );
    drag(&mut state, centre, at(centre.x + 20.0, centre.y + 15.0));
    let moved = state.pending_crop().unwrap();
    assert_eq!(moved.origin, at(60.0, 45.0));
    assert_eq!(moved.size, narrowed.size);
    assert_eq!(state.document().crop(), None);
}

#[test]
fn crop_hit_testing_covers_complete_edges_with_corner_priority() {
    let mut state = state();
    draft_crop(&mut state, 40.0, 30.0, 260.0, 210.0);
    let crop = state.pending_crop().unwrap();
    let tolerance = state.handle_tolerance();

    assert_eq!(state.crop_handle_at(at(150.0, 30.0)), Some(Handle::Top));
    assert_eq!(state.crop_handle_at(at(260.0, 120.0)), Some(Handle::Right));
    assert_eq!(state.crop_handle_at(at(150.0, 210.0)), Some(Handle::Bottom));
    assert_eq!(state.crop_handle_at(at(40.0, 120.0)), Some(Handle::Left));
    assert_eq!(
        state.crop_handle_at(at(crop.origin.x + 2.0, crop.origin.y + 2.0)),
        Some(Handle::TopLeft)
    );
    assert_eq!(state.crop_handle_at(at(150.0, 120.0)), None);
    assert_eq!(
        state.crop_handle_at(at(150.0, crop.origin.y + tolerance + 0.5)),
        None
    );
}

#[test]
fn crop_resize_cursors_match_each_edge_and_corner() {
    for handle in [Handle::Left, Handle::Right] {
        assert_eq!(crop_cursor(handle), CursorIcon::ResizeHorizontal);
    }
    for handle in [Handle::Top, Handle::Bottom] {
        assert_eq!(crop_cursor(handle), CursorIcon::ResizeVertical);
    }
    for handle in [Handle::TopLeft, Handle::BottomRight] {
        assert_eq!(crop_cursor(handle), CursorIcon::ResizeNwSe);
    }
    for handle in [Handle::TopRight, Handle::BottomLeft] {
        assert_eq!(crop_cursor(handle), CursorIcon::ResizeNeSw);
    }
}

#[test]
fn crop_aspect_dimensions_and_swap_stay_bounded_in_source_space() {
    let mut state = state();
    state.set_tool(Tool::Crop);
    state.set_crop_aspect(CropAspect::Square);
    let square = state.pending_crop().unwrap();
    assert!((square.size.width - square.size.height).abs() < 0.001);

    state.set_crop_width(120.0);
    let resized = state.pending_crop().unwrap();
    assert_eq!(resized.size, LogicalSize::new(120.0, 120.0));

    state.set_crop_aspect(CropAspect::Landscape16x9);
    let landscape = state.pending_crop().unwrap();
    assert!((landscape.size.width / landscape.size.height - 16.0 / 9.0).abs() < 0.001);

    state.swap_crop_dimensions();
    let portrait = state.pending_crop().unwrap();
    assert_eq!(state.crop_aspect(), Some(CropAspect::Portrait9x16));
    assert!((portrait.size.width / portrait.size.height - 9.0 / 16.0).abs() < 0.001);

    let Some(((width_px, height_px), (800, 600))) = state.crop_pixel_sizes() else {
        panic!("crop pixel readout");
    };
    assert_eq!(width_px, (portrait.size.width * 2.0).round() as u32);
    assert_eq!(height_px, (portrait.size.height * 2.0).round() as u32);
}

#[test]
fn crop_handle_drag_preserves_the_selected_aspect_ratio() {
    let mut state = state();
    state.set_tool(Tool::Crop);
    state.set_crop_aspect(CropAspect::Landscape16x9);
    let before = state.pending_crop().unwrap();
    let right = Handle::Right.position(&before);
    drag(&mut state, right, at(right.x - 80.0, right.y));
    let after = state.pending_crop().unwrap();
    assert!((after.size.width / after.size.height - 16.0 / 9.0).abs() < 0.001);
    assert_eq!(after.origin.x, before.origin.x);
}

#[test]
fn malformed_crop_dimensions_are_sanitised_and_bounded() {
    let mut state = state();
    state.set_tool(Tool::Crop);
    state.set_crop_width(f64::NAN);
    state.set_crop_height(f64::INFINITY);
    let crop = state.pending_crop().unwrap();
    let bounds = state.document().logical_bounds();
    assert!(crop.size.width.is_finite() && crop.size.height.is_finite());
    assert!(crop.size.width >= MIN_SIZE && crop.size.height >= MIN_SIZE);
    assert!(crop.size.width <= bounds.size.width);
    assert!(crop.size.height <= bounds.size.height);
}

#[test]
fn crop_snap_can_be_temporarily_disabled_without_leaving_source_bounds() {
    let mut state = state();
    state.set_tool(Tool::Crop);
    state.set_crop_width(100.0);
    state.set_crop_height(80.0);
    let start = state.pending_crop().unwrap();
    let centre = LogicalPoint::new(
        start.origin.x + start.size.width / 2.0,
        start.origin.y + start.size.height / 2.0,
    );

    state.pointer_pressed(centre);
    state.pointer_dragged_with_snap(at(53.0, centre.y), false, false);
    state.pointer_released();
    assert_eq!(state.pending_crop().unwrap().origin.x, 0.0);

    let snapped = state.pending_crop().unwrap();
    let centre = LogicalPoint::new(
        snapped.origin.x + snapped.size.width / 2.0,
        snapped.origin.y + snapped.size.height / 2.0,
    );
    state.pointer_pressed(centre);
    state.pointer_dragged_with_snap(at(centre.x + 3.0, centre.y), false, true);
    state.pointer_released();
    assert_eq!(state.pending_crop().unwrap().origin.x, 3.0);
}

#[test]
fn structural_snap_has_zoom_independent_entry_release_hysteresis_and_modifier_bypass() {
    let mut capture = capture();
    for y in 0..capture.frame.height() as usize {
        for x in 0..capture.frame.width() as usize {
            let value = if x < 400 { 32 } else { 224 };
            let offset = y * capture.frame.stride + x * 4;
            capture.frame.data[offset..offset + 4].copy_from_slice(&[value, value, value, 255]);
        }
    }
    let index =
        StructuralBoundaryIndex::analyze(&capture.frame, &AnalysisCancellation::default()).unwrap();
    assert!(
        index
            .segments()
            .iter()
            .any(|segment| (segment.position - 200.0).abs() <= 1.0)
    );

    let mut state = EditorState::new(Document::new(capture));
    state.set_tool(Tool::Crop);
    state.set_crop_width(100.0);
    state.set_crop_height(80.0);
    state.set_crop_boundaries(Arc::new(index));
    let crop = state.pending_crop().unwrap();
    let right = Handle::Right.position(&crop);
    state.pointer_pressed(right);

    state.pointer_dragged_with_snap(at(204.0, right.y), false, false);
    assert_eq!(
        state.pending_crop().unwrap().origin.x + state.pending_crop().unwrap().size.width,
        200.0
    );
    assert_eq!(state.active_crop_snap_segments().len(), 1);

    state.pointer_dragged_with_snap(at(210.0, right.y), false, false);
    assert_eq!(
        state.pending_crop().unwrap().origin.x + state.pending_crop().unwrap().size.width,
        200.0,
        "the 12-point release radius must hold after the 6-point entry"
    );

    state.pointer_dragged_with_snap(at(213.0, right.y), false, false);
    assert_eq!(
        state.pending_crop().unwrap().origin.x + state.pending_crop().unwrap().size.width,
        213.0
    );

    state.pointer_dragged_with_snap(at(202.0, right.y), false, true);
    assert_eq!(
        state.pending_crop().unwrap().origin.x + state.pending_crop().unwrap().size.width,
        202.0
    );
    assert!(state.active_crop_snap_segments().is_empty());
    state.pointer_released();
}

#[test]
fn crop_rotation_and_flip_are_transactional_and_share_one_undo_step() {
    let mut state = state();
    draft_crop(&mut state, 40.0, 30.0, 260.0, 210.0);
    state.command(Command::RotateCropRight).unwrap();
    assert_eq!(state.display_orientation(), ImageOrientation::RotateRight);
    assert_eq!(state.document().orientation(), ImageOrientation::Identity);
    assert_eq!(state.visual_crop_handle(Handle::Right), Handle::Bottom);
    assert_eq!(
        state.display_to_source(state.source_to_display(at(73.0, 91.0))),
        at(73.0, 91.0)
    );

    state.command(Command::CancelCrop).unwrap();
    assert_eq!(state.document().orientation(), ImageOrientation::Identity);
    assert_eq!(state.document().crop(), None);
    assert!(!state.can_undo());

    draft_crop(&mut state, 40.0, 30.0, 260.0, 210.0);
    state.command(Command::RotateCropRight).unwrap();
    state.command(Command::FlipCropHorizontal).unwrap();
    let expected = ImageOrientation::RotateRight.then(ImageOrientation::FlipHorizontal);
    state.command(Command::ApplyCrop).unwrap();
    assert_eq!(state.document().orientation(), expected);
    assert!(state.document().crop().is_some());
    assert_eq!(state.undo_depth(), 1);

    state.command(Command::Undo).unwrap();
    assert_eq!(state.document().orientation(), ImageOrientation::Identity);
    assert_eq!(state.document().crop(), None);
}

#[test]
fn manual_crop_is_available_for_native_window_captures() {
    let mut capture = capture();
    capture.provenance = Provenance::Window;
    capture.target = CaptureTarget::Window(scrozz_core::WindowId("native".to_owned()));
    let mut state = EditorState::new(Document::new(capture));

    state.set_tool(Tool::Crop);
    state.set_crop_width(120.0);
    state.set_crop_height(90.0);
    state.command(Command::ApplyCrop).unwrap();

    assert_eq!(
        state.document().crop().unwrap().size,
        LogicalSize::new(120.0, 90.0)
    );
}

#[test]
fn crop_cancel_is_a_no_op_apply_is_one_step_and_revert_is_undoable() {
    let mut state = state();
    draft_crop(&mut state, 40.0, 30.0, 260.0, 210.0);
    state.command(Command::CancelCrop).expect("cancel crop");
    assert_eq!(state.document().crop(), None);
    assert!(!state.can_undo());
    assert_eq!(state.tool(), Tool::Select);

    draft_crop(&mut state, 40.0, 30.0, 260.0, 210.0);
    let depth = state.undo_depth();
    state.command(Command::ApplyCrop).expect("apply crop");
    let applied = state.document().crop().expect("committed crop");
    assert_eq!(state.undo_depth(), depth + 1);

    state.set_tool(Tool::Crop);
    state.command(Command::RevertCrop).expect("revert crop");
    assert_eq!(state.document().crop(), None);
    state.command(Command::Undo).expect("undo revert");
    assert_eq!(state.document().crop(), Some(applied));
}

#[test]
fn applying_the_initial_full_crop_is_a_no_op() {
    let mut state = state();
    state.set_tool(Tool::Crop);
    state.command(Command::ApplyCrop).expect("apply full crop");
    assert_eq!(state.document().crop(), None);
    assert!(!state.can_undo());
    assert_eq!(state.tool(), Tool::Select);
}

#[test]
fn reopening_crop_starts_from_the_committed_crop_but_can_revert_to_full_source() {
    let mut state = state();
    draft_crop(&mut state, 40.0, 30.0, 260.0, 210.0);
    state.command(Command::ApplyCrop).expect("apply crop");
    let committed = state.document().crop().unwrap();

    state.set_tool(Tool::Crop);
    assert_eq!(state.pending_crop(), Some(committed));
    assert_eq!(
        state.document().logical_bounds().size,
        LogicalSize::new(400.0, 300.0),
        "the complete source must remain available behind the crop draft"
    );
    state.command(Command::RevertCrop).expect("revert");
    assert_eq!(state.document().crop(), None);
}

#[test]
fn arrow_keys_nudge_the_crop_without_committing_it() {
    let mut state = state();
    state.set_tool(Tool::Crop);
    state.set_crop_width(100.0);
    state.set_crop_height(80.0);
    let before = state.pending_crop().unwrap();
    let revision = state.revision();

    state
        .command(Command::Nudge { dx: 7.0, dy: -4.0 })
        .expect("nudge crop");

    let after = state.pending_crop().unwrap();
    assert_eq!(after.origin.x, before.origin.x + 7.0);
    assert_eq!(after.origin.y, before.origin.y - 4.0);
    assert_eq!(state.revision(), revision);
    assert!(!state.can_undo());
}

// ---------------------------------------------------------------------------
// Escape
// ---------------------------------------------------------------------------

#[test]
fn escape_unwinds_one_layer_at_a_time() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    assert!(state.selection().is_some());

    // 1: the selection.
    assert_eq!(
        state.command(Command::Escape).expect("escape"),
        Intent::None
    );
    assert_eq!(state.selection(), None);

    // 2: the tool.
    assert_eq!(
        state.command(Command::Escape).expect("escape"),
        Intent::None
    );
    assert_eq!(state.tool(), Tool::Select);

    // 3: the window.
    assert_eq!(
        state.command(Command::Escape).expect("escape"),
        Intent::Close
    );
}

#[test]
fn escape_cancels_a_drag_before_anything_else() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    state.pointer_pressed(at(40.0, 40.0));
    state.pointer_dragged(at(160.0, 140.0), false);
    assert!(state.is_dragging());

    state.command(Command::Escape).expect("escape");
    assert!(!state.is_dragging());
    assert!(
        state.document().annotations().is_empty(),
        "the abandoned shape was kept"
    );
    assert_eq!(
        state.tool(),
        Tool::Rectangle,
        "escape also dropped the tool"
    );
}

#[test]
fn escape_unwinds_crop_adjustment_then_crop_mode_without_committing() {
    let mut state = state();
    state.set_tool(Tool::Crop);
    state.set_crop_width(200.0);
    let before = state.pending_crop().unwrap();
    let right = Handle::Right.position(&before);
    state.pointer_pressed(right);
    state.pointer_dragged(at(right.x - 50.0, right.y), false);
    assert_ne!(state.pending_crop(), Some(before));

    state.command(Command::Escape).expect("cancel adjustment");
    assert_eq!(state.pending_crop(), Some(before));
    assert!(state.crop_mode());

    state.command(Command::Escape).expect("leave crop mode");
    assert!(!state.crop_mode());
    assert_eq!(state.document().crop(), None);
    assert!(!state.can_undo());
}

#[test]
fn escape_on_an_empty_editor_closes_it_immediately() {
    let mut state = state();
    assert_eq!(
        state.command(Command::Escape).expect("escape"),
        Intent::Close
    );
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

#[test]
fn clicking_with_the_text_tool_opens_an_editable_annotation() {
    let mut state = state();
    state.set_tool(Tool::Text);
    state.pointer_pressed(at(80.0, 80.0));
    state.pointer_released();

    let id = state.editing_text().expect("text entry did not open");
    assert_eq!(state.selection(), Some(id));
    assert_eq!(state.text_buffer(), Some(""));
}

#[test]
fn typing_into_a_text_annotation_reaches_the_document() {
    let mut state = state();
    state.set_tool(Tool::Text);
    state.pointer_pressed(at(80.0, 80.0));
    state.pointer_released();
    state.set_text_buffer("Ship it");

    let Annotation::Text { content, .. } = &state.document().annotations()[0].annotation else {
        panic!("the text tool drew something else");
    };
    assert_eq!(content, "Ship it");
}

#[test]
fn an_empty_text_annotation_is_discarded_when_it_loses_focus() {
    let mut state = state();
    state.set_tool(Tool::Text);
    state.pointer_pressed(at(80.0, 80.0));
    state.pointer_released();
    state.finish_text();

    assert!(
        state.document().annotations().is_empty(),
        "an empty label was left behind"
    );
    assert_eq!(state.editing_text(), None);
}

#[test]
fn a_text_annotation_with_content_survives_losing_focus() {
    let mut state = state();
    state.set_tool(Tool::Text);
    state.pointer_pressed(at(80.0, 80.0));
    state.pointer_released();
    state.set_text_buffer("Keep me");
    state.finish_text();

    assert_eq!(state.document().annotations().len(), 1);
    assert_eq!(state.editing_text(), None);
}

#[test]
fn escape_leaves_text_entry_before_it_clears_the_selection() {
    let mut state = state();
    state.set_tool(Tool::Text);
    state.pointer_pressed(at(80.0, 80.0));
    state.pointer_released();
    state.set_text_buffer("Note");

    state.command(Command::Escape).expect("escape");
    assert_eq!(state.editing_text(), None);
    assert!(
        state.selection().is_some(),
        "escape unwound two layers at once"
    );
}

// ---------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------

#[test]
fn restyling_updates_the_selection_in_place() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));

    state.set_stroke_color(Color::rgb(1, 2, 3));
    assert_eq!(
        state.document().annotations()[0].style.stroke,
        Color::rgb(1, 2, 3)
    );
}

#[test]
fn recoloring_a_highlight_preserves_its_translucency() {
    let mut state = state();
    state.set_tool(Tool::Highlight);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    let before = state.document().annotations()[0].style;

    state.set_stroke_color(Color::rgb(1, 2, 3));

    let after = state.document().annotations()[0].style;
    assert_eq!((after.stroke.r, after.stroke.g, after.stroke.b), (1, 2, 3));
    assert_eq!(after.stroke.a, before.stroke.a);
    assert_eq!(
        after.fill.map(|color| color.a),
        before.fill.map(|color| color.a)
    );
    assert!(after.stroke.a < 255, "the highlighter became opaque");
}

#[test]
fn restyling_a_redaction_does_not_recolour_it() {
    let mut state = state();
    state.set_tool(Tool::Redact);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    let before = state.document().annotations()[0].style;

    state.set_stroke_color(Color::rgb(255, 0, 0));
    assert_eq!(state.document().annotations()[0].style, before);
}

#[test]
fn the_stroke_width_is_clamped_to_something_drawable() {
    let mut state = state();
    state.set_stroke_width(0.0);
    assert!(state.stroke_width() > 0.0);
    state.set_stroke_width(1e6);
    assert!(state.stroke_width() < 1e6);
}

#[test]
fn setting_a_whole_style_replaces_the_defaults() {
    let mut state = state();
    let style = Style {
        stroke: Color::rgb(9, 9, 9),
        fill: Some(Color::rgba(1, 2, 3, 40)),
        stroke_width: 6.0,
        opacity: 0.8,
        font_size: 30.0,
        ..Style::default()
    };
    state.set_style(style);
    assert_eq!(state.style().stroke, Color::rgb(9, 9, 9));
    assert!((state.style().font_size - 30.0).abs() < 0.001);
}

// ---------------------------------------------------------------------------
// Zoom and pan
// ---------------------------------------------------------------------------

#[test]
fn zoom_steps_in_and_out_and_is_clamped_at_both_ends() {
    let mut state = state();
    let start = state.zoom();

    state.command(Command::ZoomIn).expect("zoom");
    assert!(state.zoom() > start);
    state.command(Command::ZoomOut).expect("zoom");
    assert!((state.zoom() - start).abs() < 0.001);

    for _ in 0..40 {
        state.command(Command::ZoomIn).expect("zoom");
    }
    assert!(state.zoom() <= MAX_ZOOM + 0.001);

    for _ in 0..80 {
        state.command(Command::ZoomOut).expect("zoom");
    }
    assert!(state.zoom() >= MIN_ZOOM - 0.001);
}

#[test]
fn zoom_reset_restores_both_zoom_and_pan() {
    let mut state = state();
    state.command(Command::ZoomIn).expect("zoom");
    state.set_pan((40.0, -25.0));

    state.command(Command::ZoomReset).expect("reset");
    assert!((state.zoom() - 1.0).abs() < 0.001);
    assert_eq!(state.pan(), (0.0, 0.0));
}

#[test]
fn gesture_zoom_keeps_the_pointer_anchor_and_key_zoom_keeps_viewport_center() {
    let mut state = state();
    state.set_pan((30.0, -20.0));
    state.zoom_about(2.0, (100.0, 50.0));
    assert_eq!(state.zoom(), 2.0);
    assert_eq!(state.pan(), (-40.0, -90.0));

    state.command(Command::ZoomIn).expect("key zoom");
    assert_eq!(state.pan(), (-50.0, -112.5));
}

#[test]
fn invalid_zoom_input_cannot_poison_the_view_transform() {
    let mut state = state();
    state.zoom_about(f32::NAN, (10.0, 20.0));
    assert_eq!(state.zoom(), 1.0);
    assert_eq!(state.pan(), (0.0, 0.0));
}

#[test]
fn panning_tracks_the_pointer_from_where_it_grabbed() {
    let mut state = state();
    state.set_pan((10.0, 10.0));
    state.begin_pan((100.0, 100.0));
    state.pan_to((130.0, 80.0));
    assert_eq!(state.pan(), (40.0, -10.0));
}

#[test]
fn zoom_and_pan_are_not_document_edits() {
    let mut state = state();
    state.command(Command::ZoomIn).expect("zoom");
    state.set_pan((25.0, 25.0));
    assert!(!state.can_undo(), "a view change entered the undo history");
    assert!(!state.is_dirty(), "a view change marked the document dirty");
}

// ---------------------------------------------------------------------------
// Host intents
// ---------------------------------------------------------------------------

#[test]
fn copy_and_save_ask_the_host_and_change_nothing() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    let revision = state.revision();

    assert_eq!(state.command(Command::Copy).expect("copy"), Intent::Copy);
    assert_eq!(state.command(Command::Save).expect("save"), Intent::Save);
    assert_eq!(state.document().annotations().len(), 1);
    assert_eq!(
        state.revision(),
        revision,
        "asking to export bumped the revision and would rebuild the preview"
    );
}

#[test]
fn select_all_picks_the_topmost_annotation() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(20.0, 20.0), at(100.0, 90.0));
    drag(&mut state, at(120.0, 20.0), at(200.0, 90.0));
    let top = state.document().annotations().last().expect("last").id;

    state.select(None);
    state.command(Command::SelectAll).expect("select all");
    assert_eq!(state.selection(), Some(top));
}

#[test]
fn select_all_on_an_empty_document_selects_nothing() {
    let mut state = state();
    state.command(Command::SelectAll).expect("select all");
    assert_eq!(state.selection(), None);
}

// ---------------------------------------------------------------------------
// Revision
// ---------------------------------------------------------------------------

#[test]
fn the_revision_advances_when_the_document_changes() {
    let mut state = state();
    let before = state.revision();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    assert!(
        state.revision() > before,
        "drawing did not invalidate the preview"
    );
}

#[test]
fn editor_chrome_changes_do_not_invalidate_rendered_content() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    let revision = state.revision();
    let id = state.document().annotations()[0].id;

    state.select(None);
    state.select(Some(id));
    state.set_tool(Tool::Crop);
    state.set_stroke_color(Color::rgb(1, 2, 3));
    state.set_crop_width(200.0);

    assert_eq!(
        state.revision(),
        revision,
        "selection, tool, unselected style, or a pending crop rerendered the document"
    );
}

#[test]
fn crop_minimum_is_enforced_after_clamping_to_the_source() {
    let mut state = state();
    state.set_tool(Tool::Crop);
    let full = state.pending_crop().unwrap();
    drag(
        &mut state,
        Handle::Left.position(&full),
        at(full.size.width - 1.0, full.size.height / 2.0),
    );

    assert!(
        state.pending_crop().unwrap().size.width >= MIN_SIZE,
        "a crop edge collapsed below the minimum"
    );
    assert_eq!(state.document().crop(), None);
}

#[test]
fn an_edited_document_is_dirty_and_a_view_change_is_not() {
    let mut state = state();
    assert!(!state.is_dirty());
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(40.0, 40.0), at(160.0, 140.0));
    assert!(state.is_dirty());
}

// ---------------------------------------------------------------------------
// Non-destructiveness
// ---------------------------------------------------------------------------

#[test]
fn nothing_the_editor_does_touches_the_source_pixels() {
    let original = capture();
    let mut state = EditorState::new(Document::new(original.clone()));

    for tool in Tool::ALL {
        state.set_tool(tool);
        drag(&mut state, at(30.0, 30.0), at(150.0, 120.0));
    }
    state.command(Command::ApplyCrop).ok();

    let source = &state.document().source().frame;
    assert_eq!(source.width(), original.frame.width());
    assert_eq!(source.height(), original.frame.height());
    assert_eq!(
        source.data, original.frame.data,
        "an edit reached the captured pixels"
    );
}

#[test]
fn a_size_the_editor_reports_matches_what_it_would_render() {
    let mut state = state();
    let logical = LogicalSize::new(
        f64::from(capture().frame.width()) / 2.0,
        f64::from(capture().frame.height()) / 2.0,
    );
    assert!((state.document().content_size().width - logical.width).abs() < 0.001);

    state.set_tool(Tool::Crop);
    draft_crop(&mut state, 40.0, 40.0, 200.0, 180.0);
    state.command(Command::ApplyCrop).expect("crop");
    let cropped = state.document().content_size();
    assert!((cropped.width - 160.0).abs() < 1.0, "{cropped:?}");
    assert!((cropped.height - 140.0).abs() < 1.0, "{cropped:?}");
}

// ---------------------------------------------------------------------------
// Text entry
// ---------------------------------------------------------------------------

/// Opens a text annotation and returns the state with the caret ready.
fn typing() -> EditorState {
    let mut state = state();
    state.set_tool(Tool::Text);
    state.pointer_pressed(at(80.0, 80.0));
    state.pointer_released();
    state
}

/// The content of the one text annotation, as the document holds it.
fn content(state: &EditorState) -> String {
    match &state.document().annotations()[0].annotation {
        Annotation::Text { content, .. } => content.clone(),
        other => panic!("expected text, found {other:?}"),
    }
}

fn type_str(state: &mut EditorState, text: &str) {
    for ch in text.chars() {
        state.text_edit(&TextEdit::Insert(ch.to_string()));
    }
}

#[test]
fn typing_characters_appends_them_in_order() {
    let mut state = typing();
    type_str(&mut state, "Ship");

    assert_eq!(content(&state), "Ship");
    assert_eq!(state.text_caret(), 4);
}

#[test]
fn backspace_removes_the_character_before_the_caret() {
    let mut state = typing();
    type_str(&mut state, "Shipp");
    state.text_edit(&TextEdit::Backspace);

    assert_eq!(content(&state), "Ship");
    assert_eq!(state.text_caret(), 4);
}

#[test]
fn backspace_on_an_empty_field_is_harmless() {
    let mut state = typing();
    state.text_edit(&TextEdit::Backspace);

    assert_eq!(content(&state), "");
    assert_eq!(state.text_caret(), 0);
}

#[test]
fn typing_inserts_at_the_caret_rather_than_the_end() {
    let mut state = typing();
    type_str(&mut state, "Shp");
    state.text_edit(&TextEdit::Caret(Caret::Left));
    state.text_edit(&TextEdit::Insert("i".to_owned()));

    assert_eq!(content(&state), "Ship");
    assert_eq!(state.text_caret(), 3);
}

#[test]
fn backspace_steps_over_a_whole_multibyte_character() {
    let mut state = typing();
    type_str(&mut state, "aé");
    assert_eq!(state.text_caret(), 3, "é is two bytes");
    state.text_edit(&TextEdit::Backspace);

    assert_eq!(content(&state), "a");
    assert_eq!(state.text_caret(), 1);
}

#[test]
fn forward_delete_removes_the_character_after_the_caret() {
    let mut state = typing();
    type_str(&mut state, "Ship");
    state.text_edit(&TextEdit::Caret(Caret::LineStart));
    state.text_edit(&TextEdit::DeleteForward);

    assert_eq!(content(&state), "hip");
    assert_eq!(state.text_caret(), 0);
}

#[test]
fn the_caret_stops_at_both_ends_rather_than_wrapping() {
    let mut state = typing();
    type_str(&mut state, "ab");
    state.text_edit(&TextEdit::Caret(Caret::Right));
    assert_eq!(state.text_caret(), 2);
    state.text_edit(&TextEdit::Caret(Caret::Left));
    state.text_edit(&TextEdit::Caret(Caret::Left));
    state.text_edit(&TextEdit::Caret(Caret::Left));
    assert_eq!(state.text_caret(), 0);
}

#[test]
fn home_and_end_work_on_the_current_line_only() {
    let mut state = typing();
    type_str(&mut state, "one\ntwo");
    state.text_edit(&TextEdit::Caret(Caret::LineStart));
    assert_eq!(state.text_caret(), 4, "home left the second line");
    state.text_edit(&TextEdit::Caret(Caret::LineEnd));
    assert_eq!(state.text_caret(), 7);
}

#[test]
fn moving_up_a_line_clamps_to_the_shorter_line() {
    let mut state = typing();
    type_str(&mut state, "ab\nlonger");
    state.text_edit(&TextEdit::Caret(Caret::Up));

    assert_eq!(
        state.text_caret_cell(),
        Some((0, 2)),
        "overshot the short line"
    );
}

#[test]
fn the_caret_cell_tracks_rows_and_columns_for_drawing() {
    let mut state = typing();
    type_str(&mut state, "ab\ncd");

    assert_eq!(state.text_caret_cell(), Some((1, 2)));
    state.text_edit(&TextEdit::Caret(Caret::Up));
    assert_eq!(state.text_caret_cell(), Some((0, 2)));
}

#[test]
fn an_ime_composition_shows_in_the_document_before_it_is_committed() {
    let mut state = typing();
    state.text_edit(&TextEdit::Preedit("に".to_owned()));

    assert_eq!(content(&state), "に", "composition must be visible");
    assert!(state.preedit().is_some(), "composition was not tracked");
}

#[test]
fn a_longer_composition_replaces_the_shorter_one() {
    let mut state = typing();
    state.text_edit(&TextEdit::Preedit("に".to_owned()));
    state.text_edit(&TextEdit::Preedit("にほん".to_owned()));

    assert_eq!(content(&state), "にほん", "compositions accumulated");
}

#[test]
fn committing_a_composition_leaves_the_text_and_clears_the_range() {
    let mut state = typing();
    state.text_edit(&TextEdit::Preedit("にほん".to_owned()));
    state.text_edit(&TextEdit::Insert("日本".to_owned()));

    assert_eq!(content(&state), "日本");
    assert_eq!(state.preedit(), None);
    assert_eq!(state.text_caret(), "日本".len());
}

#[test]
fn a_dismissed_composition_removes_its_own_glyphs() {
    let mut state = typing();
    type_str(&mut state, "a");
    state.text_edit(&TextEdit::Preedit("にほん".to_owned()));
    state.text_edit(&TextEdit::Preedit(String::new()));

    assert_eq!(content(&state), "a");
    assert_eq!(state.preedit(), None);
}

#[test]
fn typing_around_a_composition_keeps_it_anchored() {
    let mut state = typing();
    type_str(&mut state, "ab");
    state.text_edit(&TextEdit::Caret(Caret::Left));
    state.text_edit(&TextEdit::Preedit("ん".to_owned()));

    assert_eq!(
        content(&state),
        "aんb",
        "composition landed at the wrong end"
    );
}

#[test]
fn leaving_the_field_keeps_a_composition_the_user_can_see() {
    let mut state = typing();
    state.text_edit(&TextEdit::Preedit("にほん".to_owned()));
    state.finish_text();

    assert_eq!(state.document().annotations().len(), 1);
    assert_eq!(content(&state), "にほん");
    assert_eq!(state.preedit(), None);
}

#[test]
fn a_stray_keystroke_after_committing_does_not_reopen_the_label() {
    let mut state = typing();
    type_str(&mut state, "done");
    state.finish_text();
    state.text_edit(&TextEdit::Insert("x".to_owned()));

    assert_eq!(
        content(&state),
        "done",
        "text arrived after the field closed"
    );
    assert_eq!(state.editing_text(), None);
}

#[test]
fn typing_then_undo_restores_the_text_in_one_step() {
    let mut state = typing();
    type_str(&mut state, "Ship it");
    state.finish_text();
    state.command(Command::Undo).expect("undo");

    assert!(
        state.document().annotations().is_empty(),
        "undo left half a label behind"
    );
}

#[test]
fn replacing_the_whole_buffer_leaves_the_caret_somewhere_valid() {
    let mut state = typing();
    type_str(&mut state, "a long label");
    state.set_text_buffer("hi");

    assert!(state.text_caret() <= 2, "caret dangled past the end");
    assert_eq!(state.text_caret_cell(), Some((0, state.text_caret())));
}

// ---------------------------------------------------------------------------
// Zoom-invariant hit tolerance
// ---------------------------------------------------------------------------

#[test]
fn the_grab_radius_is_the_same_on_screen_at_every_zoom() {
    let mut state = state();
    let fit = {
        state.set_view_scale(0.25);
        state.handle_tolerance()
    };
    state.set_view_scale(4.0);
    let close = state.handle_tolerance();

    // A tolerance measured on screen is a document distance divided by the
    // scale, so the product is what must stay constant.
    assert!(
        (fit * 0.25 - close * 4.0).abs() < 1e-9,
        "grab radius changed with zoom: {fit} vs {close}"
    );
}

#[test]
fn a_handle_is_grabbable_when_the_capture_is_zoomed_far_out() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(100.0, 100.0), at(200.0, 180.0));
    // Fitting a large capture into a small window: one document unit is a
    // quarter of a point, so a click 20 units away is 5pt away on screen.
    state.set_view_scale(0.25);

    assert_eq!(
        state.handle_at(at(220.0, 180.0)),
        Some(Handle::BottomRight),
        "the corner was unreachable at fit zoom"
    );
}

#[test]
fn a_handle_does_not_swallow_distant_clicks_when_zoomed_in() {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(100.0, 100.0), at(200.0, 180.0));
    state.set_view_scale(4.0);

    // At 4x, 5 document units is 20pt away: well outside any sane handle.
    assert_eq!(
        state.handle_at(at(205.0, 180.0)),
        None,
        "the handle grabbed a click 20pt away"
    );
    assert_eq!(
        state.handle_at(at(201.0, 180.0)),
        Some(Handle::BottomRight),
        "the handle became unusably small when zoomed in"
    );
}

#[test]
fn an_impossible_view_scale_is_ignored_rather_than_dividing_by_zero() {
    let mut state = state();
    state.set_view_scale(2.0);
    state.set_view_scale(0.0);
    state.set_view_scale(f64::NAN);

    assert!((state.view_scale() - 2.0).abs() < f64::EPSILON);
    assert!(state.handle_tolerance().is_finite());
}

// ---------------------------------------------------------------------------
// Render revision versus viewport revision
// ---------------------------------------------------------------------------

#[test]
fn panning_does_not_invalidate_the_rendered_preview() {
    let mut state = state();
    let render = state.revision();
    let view = state.view_revision();

    state.set_pan((40.0, 12.0));

    assert_eq!(
        state.revision(),
        render,
        "a pan re-rasterised the whole capture"
    );
    assert!(state.view_revision() > view, "the pan was not observed");
}

#[test]
fn zooming_does_not_invalidate_the_rendered_preview() {
    let mut state = state();
    let render = state.revision();

    state.set_zoom(2.0);

    assert_eq!(state.revision(), render, "a zoom re-rasterised the capture");
    assert!(state.view_revision() > 0);
}

#[test]
fn panning_to_where_it_already_is_changes_nothing() {
    let mut state = state();
    state.set_pan((10.0, 10.0));
    let view = state.view_revision();
    state.set_pan((10.0, 10.0));

    assert_eq!(
        state.view_revision(),
        view,
        "an idle drag woke the viewport every frame"
    );
}

#[test]
fn editing_the_document_does_invalidate_the_rendered_preview() {
    let mut state = state();
    let render = state.revision();
    let view = state.view_revision();

    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(10.0, 10.0), at(90.0, 90.0));

    assert!(state.revision() > render, "a new shape did not redraw");
    assert_eq!(
        state.view_revision(),
        view,
        "drawing moved the viewport as a side effect"
    );
}

// ---------------------------------------------------------------------------
// Caret and composition hygiene across history
// ---------------------------------------------------------------------------

/// Places a label and types `text` into it, leaving the field open.
fn typed(text: &str) -> EditorState {
    let mut state = typing();
    state.text_edit(&TextEdit::Insert(text.to_owned()));
    state
}

#[test]
fn undoing_past_a_multibyte_character_leaves_the_caret_on_a_boundary() {
    // "é" is two bytes, so a caret at 3 after "aéb" is mid-character once the
    // history restores a shorter string. Slicing there would panic.
    let mut state = typed("aéb");
    assert_eq!(state.text_caret(), 4);

    state.command(Command::Undo).expect("undo");

    let len = state.text_buffer().map_or(0, str::len);
    assert!(state.text_caret() <= len, "the caret outran the text");
    if let Some(text) = state.text_buffer() {
        assert!(
            text.is_char_boundary(state.text_caret()),
            "caret {} is inside a code point of {text:?}",
            state.text_caret()
        );
    }
}

#[test]
fn typing_after_an_undo_into_multibyte_text_does_not_panic() {
    let mut state = typed("日本語");
    state.command(Command::Undo).expect("undo");
    // Whatever the caret was clamped to, an insert must be well defined.
    state.text_edit(&TextEdit::Insert("x".to_owned()));
    state.text_edit(&TextEdit::Backspace);
}

#[test]
fn deleting_after_an_undo_into_multibyte_text_does_not_panic() {
    let mut state = typed("🙂🙂🙂");
    state.command(Command::Undo).expect("undo");
    state.text_edit(&TextEdit::Backspace);
    state.text_edit(&TextEdit::DeleteForward);
}

#[test]
fn redoing_into_multibyte_text_also_lands_on_a_boundary() {
    let mut state = typed("aé");
    state.command(Command::Undo).expect("undo");
    state.command(Command::Redo).expect("redo");
    if let Some(text) = state.text_buffer() {
        assert!(text.is_char_boundary(state.text_caret()));
    }
}

#[test]
fn history_ends_a_composition_rather_than_leaving_it_dangling() {
    let mut state = typed("ab");
    state.text_edit(&TextEdit::Preedit("か".to_owned()));
    assert!(state.preedit().is_some());

    state.command(Command::Undo).expect("undo");

    assert!(
        state.preedit().is_none(),
        "a composition survived the undo that removed the text it spanned"
    );
}

#[test]
fn a_composition_abandoned_by_undo_cannot_corrupt_later_typing() {
    let mut state = typed("aé");
    state.text_edit(&TextEdit::Preedit("한".to_owned()));
    state.command(Command::Undo).expect("undo");
    assert!(state.preedit().is_none());
    // Whether the label survived the undo or not, the next keystroke must be
    // well defined rather than addressing a range that no longer exists.
    state.text_edit(&TextEdit::Insert("z".to_owned()));
    if let Some(text) = state.text_buffer() {
        assert!(text.contains('z'), "typing after the undo went nowhere");
    }
}

// ---------------------------------------------------------------------------
// Abandoning an empty label
// ---------------------------------------------------------------------------

#[test]
fn placing_a_label_and_typing_nothing_leaves_the_undo_stack_alone() {
    let mut state = state();
    let before = state.undo_depth();

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(40.0, 40.0));
    state.pointer_released();
    state.command(Command::Escape).expect("escape");

    assert_eq!(
        state.undo_depth(),
        before,
        "an abandoned empty label left an undoable step behind"
    );
    assert_eq!(state.document().annotations().len(), 0);
}

#[test]
fn undoing_after_abandoning_an_empty_label_does_not_resurrect_it() {
    let mut state = state();
    // One real annotation first, so there is something to undo *past*.
    state.set_tool(Tool::Rectangle);
    state.pointer_pressed(at(10.0, 10.0));
    state.pointer_dragged(at(90.0, 90.0), false);
    state.pointer_released();
    assert_eq!(state.document().annotations().len(), 1);

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(200.0, 200.0));
    state.pointer_released();
    state.command(Command::Escape).expect("escape");

    state.command(Command::Undo).expect("undo");

    // One undo must remove the rectangle, not bring back an invisible label.
    assert_eq!(
        state.document().annotations().len(),
        0,
        "undo restored something the user never made"
    );
}

#[test]
fn abandoning_an_empty_label_leaves_nothing_to_redo() {
    let mut state = state();
    state.set_tool(Tool::Text);
    state.pointer_pressed(at(40.0, 40.0));
    state.pointer_released();
    state.command(Command::Escape).expect("escape");
    assert_eq!(state.redo_depth(), 0);
}

#[test]
fn emptying_a_label_that_already_existed_is_still_undoable() {
    let mut state = typed("hello");
    state.command(Command::Escape).expect("escape");
    let with_text = state.undo_depth();
    assert_eq!(state.document().annotations().len(), 1);

    // Re-enter it and delete everything: a real edit to real content.
    // Click inside the label's own box, which starts at its origin and is as
    // wide as the glyphs it holds.
    let box_of = state.document().annotations()[0].bounds();
    let middle = at(
        box_of.origin.x + box_of.size.width / 2.0,
        box_of.origin.y + box_of.size.height / 2.0,
    );
    state.set_tool(Tool::Select);
    state.pointer_pressed(middle);
    state.pointer_released();
    assert!(
        state.editing_text().is_some(),
        "clicking the label did not re-enter it"
    );
    for _ in 0..5 {
        state.text_edit(&TextEdit::Backspace);
    }
    state.command(Command::Escape).expect("escape");
    assert_eq!(state.document().annotations().len(), 0);

    assert!(
        state.undo_depth() > with_text,
        "deleting text the user really typed was not recorded"
    );
    state.command(Command::Undo).expect("undo");
    assert_eq!(
        state.document().annotations().len(),
        1,
        "undo did not bring the deleted label back"
    );
}

#[test]
fn a_label_with_real_text_is_kept_and_recorded() {
    let mut state = typed("note");
    let before = state.undo_depth();
    state.command(Command::Escape).expect("escape");
    assert_eq!(state.document().annotations().len(), 1);
    assert!(state.undo_depth() >= before);
}

// ---------------------------------------------------------------------------
// Panning is a view change, not a document change
// ---------------------------------------------------------------------------

#[test]
fn gesture_panning_does_not_spend_a_document_revision() {
    let mut state = state();
    let content = state.revision();
    let view = state.view_revision();

    state.begin_pan((100.0, 100.0));
    state.pan_to((140.0, 130.0));
    state.pan_to((180.0, 160.0));

    assert_eq!(
        state.revision(),
        content,
        "panning rerasterised the preview"
    );
    assert!(
        state.view_revision() > view,
        "panning did not register as a view change"
    );
}

#[test]
fn gesture_panning_still_moves_the_picture() {
    let mut state = state();
    state.begin_pan((100.0, 100.0));
    state.pan_to((150.0, 120.0));
    assert_eq!(state.pan(), (50.0, 20.0));
}

#[test]
fn a_pan_that_goes_nowhere_costs_nothing() {
    let mut state = state();
    state.begin_pan((100.0, 100.0));
    let view = state.view_revision();
    state.pan_to((100.0, 100.0));
    assert_eq!(state.view_revision(), view);
}

#[test]
fn abandoning_an_empty_label_does_not_cost_a_pending_redo() {
    // The user drew something, took it back, then thought about a label and
    // thought better of that too. The rectangle is still theirs to bring back:
    // a click they cancelled cannot spend an undo they had banked.
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    state.pointer_pressed(at(10.0, 10.0));
    state.pointer_dragged(at(90.0, 90.0), false);
    state.pointer_released();

    state.command(Command::Undo).expect("undo");
    assert_eq!(state.document().annotations().len(), 0);
    assert!(state.can_redo(), "the rectangle should be redoable");

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(200.0, 200.0));
    state.pointer_released();
    state.command(Command::Escape).expect("escape");

    assert!(
        state.can_redo(),
        "abandoning a label the user never typed into destroyed their redo"
    );
    state.command(Command::Redo).expect("redo");
    assert_eq!(
        state.document().annotations().len(),
        1,
        "the rectangle did not come back"
    );
}

#[test]
fn abandoning_an_empty_label_after_changing_its_colour_leaves_nothing() {
    // Placing a label and then picking a colour for it is still a label the
    // user never typed into. The colour change is not an edit they made to the
    // document; it is part of the same unfinished thought.
    let mut state = state();
    let before = state.undo_depth();

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(40.0, 40.0));
    state.pointer_released();
    state.set_stroke_color(Color::rgb(200, 30, 30));
    state.command(Command::Escape).expect("escape");

    assert_eq!(
        state.document().annotations().len(),
        0,
        "a ghost label survived, invisible and selectable"
    );
    assert_eq!(
        state.undo_depth(),
        before,
        "the colour change left an undoable step for something that does not exist"
    );
}

#[test]
fn abandoning_an_empty_label_after_changing_its_width_leaves_nothing() {
    let mut state = state();
    let before = state.undo_depth();

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(40.0, 40.0));
    state.pointer_released();
    state.set_stroke_width(9.0);
    state.command(Command::Escape).expect("escape");

    assert_eq!(state.document().annotations().len(), 0);
    assert_eq!(state.undo_depth(), before);
}

#[test]
fn a_style_change_while_a_fresh_label_is_open_does_not_cost_a_pending_redo() {
    // The two failures compounded: a style edit before abandoning used to both
    // strand the label and take the redo branch with it.
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    state.pointer_pressed(at(10.0, 10.0));
    state.pointer_dragged(at(90.0, 90.0), false);
    state.pointer_released();
    state.command(Command::Undo).expect("undo");

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(200.0, 200.0));
    state.pointer_released();
    state.set_stroke_color(Color::rgb(10, 200, 10));
    state.set_stroke_width(7.0);
    state.command(Command::Escape).expect("escape");

    assert_eq!(state.document().annotations().len(), 0);
    assert!(state.can_redo(), "the rectangle is no longer redoable");
    state.command(Command::Redo).expect("redo");
    assert_eq!(state.document().annotations().len(), 1);
}

#[test]
fn a_label_the_user_did_type_into_is_a_real_edit_that_ends_the_redo_branch() {
    // The mirror of the tests above: once there are glyphs in it, the label is
    // a genuine edit, and a genuine edit is what redo is supposed to lose.
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    state.pointer_pressed(at(10.0, 10.0));
    state.pointer_dragged(at(90.0, 90.0), false);
    state.pointer_released();
    state.command(Command::Undo).expect("undo");
    assert!(state.can_redo());

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(200.0, 200.0));
    state.pointer_released();
    state.text_edit(&TextEdit::Insert("hello".to_owned()));
    state.command(Command::Escape).expect("escape");

    assert_eq!(state.document().annotations().len(), 1);
    assert!(
        !state.can_redo(),
        "a real edit must retire the redo branch it replaced"
    );
}

// ---------------------------------------------------------------------------
// Panning is a view change, from the first press to the last release
// ---------------------------------------------------------------------------

#[test]
fn a_whole_pan_gesture_never_touches_the_content_revision() {
    // The preview is rasterised at 2400px and uploaded to the GPU whenever the
    // content revision moves. A pan must cost none of that: the picture has not
    // changed, only where it sits.
    let mut state = state();
    let content = state.revision();
    let view = state.view_revision();

    state.begin_pan((100.0, 100.0));
    state.pan_to((140.0, 90.0));
    state.pan_to((180.0, 70.0));
    state.pointer_released();

    assert_eq!(
        state.revision(),
        content,
        "panning rerasterised the preview"
    );
    assert!(
        state.view_revision() > view,
        "the viewport moved but nothing was asked to redraw"
    );
}

#[test]
fn releasing_a_pan_leaves_the_pan_where_the_gesture_put_it() {
    let mut state = state();
    state.begin_pan((100.0, 100.0));
    state.pan_to((160.0, 120.0));
    let panned = state.pan();
    state.pointer_released();

    assert_eq!(state.pan(), panned, "the release snapped the view back");
}

#[test]
fn a_pan_fed_through_the_ordinary_drag_path_is_still_view_only() {
    // Some hosts route every pointer move through `pointer_dragged` rather than
    // calling `pan_to`. That must not turn a pan into a document edit.
    let mut state = state();
    let content = state.revision();

    state.begin_pan((100.0, 100.0));
    state.pointer_dragged(at(140.0, 90.0), false);
    state.pointer_released();

    assert_eq!(state.revision(), content);
}

#[test]
fn a_real_edit_still_spends_a_content_revision() {
    // The guard above must not have made every gesture free.
    let mut state = state();
    let content = state.revision();

    state.set_tool(Tool::Rectangle);
    state.pointer_pressed(at(10.0, 10.0));
    state.pointer_dragged(at(90.0, 90.0), false);
    state.pointer_released();

    assert!(
        state.revision() > content,
        "drawing a rectangle did not ask for a redraw"
    );
}

// ---------------------------------------------------------------------------
// History restores the caret that belonged to the state
// ---------------------------------------------------------------------------

/// A label holding `text`, closed, then clicked back into with the caret at
/// the end — the state a user is in when they edit something they made earlier.
///
/// Placing a label and typing into it is deliberately one undo step, so a
/// second visit is what it takes to have history *inside* a label at all.
/// `text` needs at least two characters: a narrower box is entirely covered by
/// its own resize handles, so a click lands on a handle rather than the glyphs.
fn reentered(text: &str) -> EditorState {
    let mut state = typed(text);
    state.command(Command::Escape).expect("escape");

    let box_of = state.document().annotations()[0].bounds();
    state.set_tool(Tool::Select);
    state.pointer_pressed(at(
        box_of.origin.x + box_of.size.width / 2.0,
        box_of.origin.y + box_of.size.height / 2.0,
    ));
    state.pointer_released();
    assert!(
        state.editing_text().is_some(),
        "clicking a label should reopen it"
    );
    state
}

#[test]
fn redo_puts_the_caret_after_the_character_it_brought_back() {
    // The review's case: append "é" to a label, undo, redo, then type "x".
    // The caret has to come back after the "é" — clamping the old byte offset
    // would leave it at 2 and the "x" would land inside the word.
    let mut state = reentered("ha");
    state.text_edit(&TextEdit::Insert("é".to_owned()));
    assert_eq!(state.text_buffer(), Some("haé"));

    state.command(Command::Undo).expect("undo");
    assert_eq!(state.text_buffer(), Some("ha"));
    assert_eq!(state.text_caret(), 2, "the caret should be after the 'a'");

    state.command(Command::Redo).expect("redo");
    assert_eq!(state.text_buffer(), Some("haé"));
    assert_eq!(
        state.text_caret(),
        4,
        "the caret did not come back with the character"
    );

    state.text_edit(&TextEdit::Insert("x".to_owned()));
    assert_eq!(state.text_buffer(), Some("haéx"));
}

#[test]
fn undo_puts_the_caret_where_it_was_when_that_state_was_on_screen() {
    let mut state = reentered("ab");
    state.text_edit(&TextEdit::Insert("cd".to_owned()));

    state.command(Command::Undo).expect("undo");

    assert_eq!(state.text_buffer(), Some("ab"));
    assert_eq!(state.text_caret(), 2);
    state.text_edit(&TextEdit::Insert("!".to_owned()));
    assert_eq!(state.text_buffer(), Some("ab!"));
}

#[test]
fn a_middle_insertion_survives_a_round_trip_through_the_history() {
    // Typing into the middle, taking it back, and putting it back again must
    // leave the caret at the site of the edit — not at the end of the string.
    let mut state = reentered("aé");
    state.text_edit(&TextEdit::Caret(Caret::LineStart));
    state.text_edit(&TextEdit::Insert("z".to_owned()));
    assert_eq!(state.text_buffer(), Some("zaé"));
    assert_eq!(state.text_caret(), 1);

    state.command(Command::Undo).expect("undo");
    assert_eq!(state.text_buffer(), Some("aé"));
    assert_eq!(state.text_caret(), 0, "the caret left the site of the edit");

    state.command(Command::Redo).expect("redo");
    assert_eq!(state.text_buffer(), Some("zaé"));
    assert_eq!(state.text_caret(), 1);

    state.text_edit(&TextEdit::Insert("y".to_owned()));
    assert_eq!(state.text_buffer(), Some("zyaé"));
}

#[test]
fn a_composition_interrupted_by_undo_does_not_resurrect_on_redo() {
    // An IME is mid-composition when the user undoes. The preedit belongs to
    // keystrokes that no longer exist, and redo must not bring it back as if it
    // had been committed.
    let mut state = reentered("ab");
    state.text_edit(&TextEdit::Insert("cd".to_owned()));
    state.text_edit(&TextEdit::Preedit("にほ".to_owned()));

    state.command(Command::Undo).expect("undo");
    assert!(state.take_ime_interrupt(), "the IME was not told to stop");
    assert_eq!(state.text_buffer(), Some("ab"));

    // Composition is composited into the document so it draws, so redo does
    // restore those glyphs — but as ordinary text, exactly as committing them
    // would have. What must not survive is the *range*: if the editor still
    // believed a composition were open there, the next keystroke would replace
    // the glyphs instead of following them.
    state.command(Command::Redo).expect("redo");
    state.text_edit(&TextEdit::Insert("!".to_owned()));
    assert_eq!(
        state.text_buffer(),
        Some("abcdにほ!"),
        "the restored text was still treated as an open composition"
    );
}

#[test]
fn a_restored_caret_is_still_clamped_to_a_character_boundary() {
    // The marker is recorded against one string and restored into another, so
    // it can still point inside a code point. Slicing there would panic.
    let mut state = typed("aébc");
    state.text_edit(&TextEdit::Caret(Caret::Left));
    state.text_edit(&TextEdit::Caret(Caret::Left));
    state.text_edit(&TextEdit::Caret(Caret::Left));

    for _ in 0..4 {
        state.command(Command::Undo).expect("undo");
        let len = state.text_buffer().map_or(0, str::len);
        assert!(state.text_caret() <= len);
        if let Some(text) = state.text_buffer() {
            assert!(text.is_char_boundary(state.text_caret()));
        }
    }
    // And the editor is still usable afterwards.
    state.text_edit(&TextEdit::Insert("ü".to_owned()));
}

// ---------------------------------------------------------------------------
// Interrupting the platform's composition
// ---------------------------------------------------------------------------

#[test]
fn undo_asks_the_platform_to_drop_a_composition_in_flight() {
    let mut state = typed("hello");
    assert!(
        !state.ime_interrupt_pending(),
        "nothing has moved the text yet"
    );

    state.command(Command::Undo).expect("undo");

    assert!(
        state.ime_interrupt_pending(),
        "the IME was left composing against text the undo replaced"
    );
}

#[test]
fn redo_asks_the_platform_to_drop_a_composition_in_flight() {
    let mut state = typed("hello");
    state.command(Command::Undo).expect("undo");
    let _ = state.take_ime_interrupt();

    state.command(Command::Redo).expect("redo");

    assert!(state.ime_interrupt_pending());
}

#[test]
fn the_interruption_is_sent_once_and_then_forgotten() {
    // Left set, it would cancel every composition the user starts afterwards,
    // which looks exactly like a broken IME.
    let mut state = typed("hello");
    state.command(Command::Undo).expect("undo");

    assert!(state.take_ime_interrupt(), "the first frame must carry it");
    assert!(
        !state.take_ime_interrupt(),
        "the instruction was still queued a frame later"
    );
    assert!(!state.ime_interrupt_pending());
}

#[test]
fn ordinary_typing_never_interrupts_a_composition() {
    let mut state = typing();
    state.text_edit(&TextEdit::Preedit("にほ".to_owned()));
    state.text_edit(&TextEdit::Insert("日本".to_owned()));
    state.text_edit(&TextEdit::Caret(Caret::Left));

    assert!(
        !state.ime_interrupt_pending(),
        "the user's own typing cancelled their own composition"
    );
}

#[test]
fn an_undo_that_removes_the_label_still_interrupts() {
    // The composition has to be cancelled even though there is no longer a
    // caret to draw, or the IME commits into a label that no longer exists.
    let mut state = typing();
    state.text_edit(&TextEdit::Insert("hi".to_owned()));
    state.command(Command::Escape).expect("escape");
    state.command(Command::Undo).expect("undo");
    state.command(Command::Undo).expect("undo");

    assert!(state.ime_interrupt_pending());
}

// ---------------------------------------------------------------------------
// Cancelling a label the user never filled in
// ---------------------------------------------------------------------------

/// A finished rectangle, so there is something on the undo stack worth losing.
fn with_a_rectangle() -> EditorState {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(20.0, 20.0), at(120.0, 90.0));
    state
}

#[test]
fn escaping_a_fresh_label_after_undoing_its_colour_leaves_nothing_behind() {
    // The whole sequence in one place: place a label, restyle it, take the
    // restyle back, then think better of the label. Every one of those is
    // something a user does without thinking, and the end state has to be the
    // one they started from.
    let mut state = typing();
    state.set_stroke_color(Color::rgb(10, 200, 40));
    state.command(Command::Undo).expect("undo the colour");
    state.command(Command::Escape).expect("escape");

    assert!(
        state.document().annotations().is_empty(),
        "an empty label the user backed out of must not survive"
    );
    state.command(Command::Undo).expect("undo");
    assert!(
        state.document().annotations().is_empty(),
        "and undo must not resurrect it either"
    );
}

#[test]
fn escaping_a_fresh_label_after_undoing_its_width_leaves_nothing_behind() {
    // The same, through a different style property, because a fix that keys off
    // which tag the history happens to be carrying would only cover one.
    let mut state = typing();
    state.set_stroke_width(9.0);
    state.command(Command::Undo).expect("undo the width");
    state.command(Command::Escape).expect("escape");

    assert!(state.document().annotations().is_empty());
    state.command(Command::Undo).expect("undo");
    assert!(state.document().annotations().is_empty());
}

#[test]
fn undoing_a_fresh_labels_style_then_escaping_keeps_the_earlier_redo() {
    // A click the user takes back must not cost them a step they can redo. The
    // rectangle is undone *before* the label is placed, so its redo branch is
    // what `begin` put into safekeeping.
    let mut state = with_a_rectangle();
    state.command(Command::Undo).expect("undo the rectangle");
    assert!(state.document().annotations().is_empty());

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(200.0, 200.0));
    state.pointer_released();
    state.set_stroke_color(Color::rgb(10, 200, 40));
    state.command(Command::Undo).expect("undo the colour");
    state.command(Command::Escape).expect("escape");

    assert!(
        state.document().annotations().is_empty(),
        "the abandoned label must be gone"
    );
    state.command(Command::Redo).expect("redo");
    assert_eq!(
        state.document().annotations().len(),
        1,
        "the rectangle was redoable before the label was placed, and still is"
    );
    assert_eq!(
        state.document().annotations()[0].annotation.kind(),
        AnnotationKind::Rectangle
    );
}

#[test]
fn escaping_a_fresh_label_after_undoing_its_style_leaves_no_selectable_ghost() {
    // An empty label is invisible but hit-testable, so a ghost shows up as a
    // click that selects nothing the user can see.
    let mut state = typing();
    state.set_stroke_width(9.0);
    state.command(Command::Undo).expect("undo the width");
    state.command(Command::Escape).expect("escape");
    // The ghost only appears once the removal that was committed is taken back.
    state.command(Command::Undo).expect("undo");

    state.set_tool(Tool::Select);
    state.pointer_pressed(at(80.0, 80.0));
    state.pointer_released();
    assert!(
        state.selection().is_none(),
        "there is nothing there to select"
    );
    assert!(state.document().annotations().is_empty());
}

// ---------------------------------------------------------------------------
// Undo and redo that do not move
// ---------------------------------------------------------------------------

#[test]
fn a_redo_with_no_future_leaves_a_composition_alone() {
    // Nothing moved, so nothing about the composition is stale. Clearing it and
    // telling the platform to cancel would throw away glyphs the user is
    // looking at over a keystroke that did nothing.
    let mut state = reentered("ab");
    state.text_edit(&TextEdit::Preedit("か".to_owned()));
    let before = content(&state);
    let composing = state.preedit().expect("the composition was tracked");
    let caret = state.text_caret();
    let _ = state.take_ime_interrupt();

    state.command(Command::Redo).expect("redo");

    assert_eq!(content(&state), before, "the document must not have moved");
    assert_eq!(
        state.preedit(),
        Some(composing),
        "the composition is still live"
    );
    assert_eq!(state.text_caret(), caret);
    assert!(
        !state.ime_interrupt_pending(),
        "a redo that did nothing must not cancel the IME"
    );
}

/// A label that was already in the document when the editor opened — the
/// history's origin — reopened for editing. Reaching an undo with *nothing*
/// behind it needs a label the user did not just place.
fn pre_existing_label() -> EditorState {
    let mut document = Document::new(capture());
    document.add(
        Annotation::Text {
            at: at(80.0, 80.0),
            content: "ab".to_owned(),
        },
        Style::stroked(),
    );
    let mut state = EditorState::new(document);
    let box_of = state.document().annotations()[0].bounds();
    state.set_tool(Tool::Select);
    state.pointer_pressed(at(
        box_of.origin.x + box_of.size.width / 2.0,
        box_of.origin.y + box_of.size.height / 2.0,
    ));
    state.pointer_released();
    assert!(state.editing_text().is_some(), "the label must reopen");
    state
}

#[test]
fn an_undo_with_no_past_changes_nothing_and_interrupts_nothing() {
    let mut state = pre_existing_label();
    let before = content(&state);
    let caret = state.text_caret();
    let _ = state.take_ime_interrupt();

    state.command(Command::Undo).expect("undo");

    assert_eq!(content(&state), before, "there was nothing behind this");
    assert_eq!(state.preedit(), None);
    assert_eq!(state.text_caret(), caret, "the caret must not be reset");
    assert!(
        !state.ime_interrupt_pending(),
        "an undo with nothing behind it must not cancel the IME"
    );
}

#[test]
fn a_second_undo_at_the_origin_does_not_interrupt_again() {
    // The realistic route into a no-op undo: compose, undo — which really does
    // move and really should interrupt — then press ⌘Z once more out of habit.
    let mut state = pre_existing_label();
    state.text_edit(&TextEdit::Preedit("か".to_owned()));
    state.command(Command::Undo).expect("the real one");
    assert!(
        state.take_ime_interrupt(),
        "the undo that moved has to interrupt"
    );
    let before = content(&state);
    let caret = state.text_caret();

    state.command(Command::Undo).expect("the one that cannot");

    assert_eq!(content(&state), before);
    assert_eq!(state.preedit(), None);
    assert_eq!(state.text_caret(), caret);
    assert!(
        !state.ime_interrupt_pending(),
        "the second ⌘Z did nothing, so it must ask for nothing"
    );
}

#[test]
fn a_real_undo_still_interrupts_the_composition_exactly_once() {
    let mut state = reentered("ab");
    state.text_edit(&TextEdit::Insert("c".to_owned()));
    state.text_edit(&TextEdit::Preedit("か".to_owned()));
    let _ = state.take_ime_interrupt();

    state.command(Command::Undo).expect("undo");

    assert_eq!(content(&state), "ab", "the history did move");
    assert_eq!(state.preedit(), None);
    assert!(
        state.take_ime_interrupt(),
        "a real undo has to tell the platform to drop its composition"
    );
    assert!(
        !state.ime_interrupt_pending(),
        "and it must say so exactly once"
    );
}

// Undoing and redoing the click that placed a label.

#[test]
fn redoing_a_fresh_labels_creation_puts_the_user_back_in_it() {
    // Place a label, take the click back, then change your mind again. The
    // label that comes back is empty and invisible, so if nobody is editing it
    // there is no way to reach it: typing goes nowhere and Escape has nothing
    // to cancel, and it sits in the document for good.
    let mut state = typing();
    let id = state
        .editing_text()
        .expect("placing a label starts editing it");

    state.command(Command::Undo).expect("undo the placement");
    assert!(
        state.document().annotations().is_empty(),
        "the label is gone"
    );
    assert_eq!(state.editing_text(), None, "so nothing is being edited");

    state.command(Command::Redo).expect("redo the placement");
    assert_eq!(
        state.document().annotations().len(),
        1,
        "the label is back in the document"
    );
    assert_eq!(
        state.editing_text(),
        Some(id),
        "and the user is back inside it, not staring at an unreachable ghost"
    );
}

#[test]
fn typing_works_again_after_redoing_a_labels_creation() {
    let mut state = typing();
    state.command(Command::Undo).expect("undo the placement");
    state.command(Command::Redo).expect("redo the placement");

    type_str(&mut state, "hi");
    assert_eq!(
        content(&state),
        "hi",
        "the keystrokes have somewhere to land"
    );
}

#[test]
fn escaping_after_redoing_a_labels_creation_still_leaves_nothing_behind() {
    // The redone label is still one the user never filled in, so escaping it
    // has to take the placement back with it.
    let mut state = typing();
    state.command(Command::Undo).expect("undo the placement");
    state.command(Command::Redo).expect("redo the placement");

    state.command(Command::Escape).expect("escape");
    assert!(
        state.document().annotations().is_empty(),
        "an empty label the user walked away from leaves nothing behind"
    );

    state.command(Command::Undo).expect("undo after escaping");
    assert!(
        state.document().annotations().is_empty(),
        "and there is no invisible label one undo away"
    );
}

#[test]
fn redoing_the_creation_of_a_label_that_was_typed_into_does_not_reopen_it() {
    // A label with text in it was closed deliberately. Undoing and redoing that
    // work must not drop the user back into a field they had finished with.
    let mut state = typed("hello");
    state.command(Command::Escape).expect("leave the label");
    assert_eq!(state.editing_text(), None);

    state.command(Command::Undo).expect("undo the label");
    state.command(Command::Redo).expect("redo the label");
    assert_eq!(
        state.editing_text(),
        None,
        "redo restores the document, not a text cursor the user had dismissed"
    );
    assert_eq!(content(&state), "hello");
}

#[test]
fn escaping_a_label_whose_placement_was_undone_takes_the_placement_back() {
    // Place a label, undo the click that made it, then walk away. Nobody is
    // editing anything at that point — the label is not even in the document —
    // but the placement is still unfinished and still has to be cancelled.
    let mut state = typing();
    state.command(Command::Undo).expect("undo the placement");
    assert!(state.document().annotations().is_empty());

    state.command(Command::Escape).expect("escape");

    state.command(Command::Redo).expect("redo after escaping");
    assert!(
        state.document().annotations().is_empty(),
        "the placement was taken back, so there is nothing left to redo it into"
    );
    assert_eq!(
        state.editing_text(),
        None,
        "and nothing invisible is being edited"
    );
}

#[test]
fn switching_tools_after_undoing_a_placement_takes_the_placement_back() {
    // The same cancellation, reached the other way the editor offers: picking a
    // different tool ends whatever the last one had open.
    let mut state = typing();
    state.command(Command::Undo).expect("undo the placement");

    state.set_tool(Tool::Rectangle);

    state.command(Command::Redo).expect("redo after switching");
    assert!(
        state.document().annotations().is_empty(),
        "switching away from an unfinished label cancels it too"
    );
}

#[test]
fn a_label_cancelled_from_the_undone_state_leaves_no_ghost_to_click_on() {
    // The failure this guards against was not just an extra undo step: the
    // redone annotation was in the document, empty, so it could be hit-tested
    // and selected while being impossible to see or type into.
    let mut state = typing();
    state.command(Command::Undo).expect("undo the placement");
    state.command(Command::Escape).expect("escape");
    state.command(Command::Redo).expect("redo after escaping");

    state.set_tool(Tool::Select);
    state.pointer_pressed(at(80.0, 80.0));
    state.pointer_released();
    assert_eq!(
        state.selection(),
        None,
        "there is nothing at the click, because there is nothing there"
    );
    assert!(state.document().annotations().is_empty());
}

#[test]
fn cancelling_a_placement_from_the_undone_state_keeps_earlier_work() {
    // The cancellation must cost the user only the click they took back.
    let mut state = with_a_rectangle();
    let before = state.document().annotations().len();
    state.set_tool(Tool::Text);
    state.pointer_pressed(at(120.0, 120.0));
    state.pointer_released();

    state.command(Command::Undo).expect("undo the placement");
    state.command(Command::Escape).expect("escape");

    assert_eq!(
        state.document().annotations().len(),
        before,
        "the rectangle drawn beforehand is untouched"
    );
    state.command(Command::Undo).expect("undo the rectangle");
    assert!(state.document().annotations().is_empty());
    state.command(Command::Redo).expect("redo the rectangle");
    assert_eq!(
        state.document().annotations().len(),
        before,
        "and it is still redoable afterwards"
    );
}

#[test]
fn escaping_a_pre_existing_label_does_not_destroy_its_redo() {
    // The set-aside path must not be turned into a cancellation for a label the
    // user did not place. Escape leaves such a label alone, so the typing it
    // still had on the redo stack has to come back afterwards — an escape that
    // quietly committed, or that rolled the placement back, would eat it.
    let mut state = pre_existing_label();
    type_str(&mut state, "c");
    assert_eq!(content(&state), "abc");
    state.command(Command::Undo).expect("undo the typing");
    assert_eq!(content(&state), "ab");

    state.command(Command::Escape).expect("escape");
    assert_eq!(
        content(&state),
        "ab",
        "a label that was there before this session is still there"
    );

    state.command(Command::Redo).expect("redo the typing");
    assert_eq!(
        content(&state),
        "abc",
        "and escaping never cost it the redo it was holding"
    );
}

/// Where a rectangle in the document sits, so a test can tell one drawing apart
/// from another that replaced it.
fn rect_at(state: &EditorState, index: usize) -> LogicalRect {
    match &state.document().annotations()[index].annotation {
        Annotation::Rectangle(rect) => *rect,
        other => panic!("expected a rectangle, found {other:?}"),
    }
}

#[test]
fn drawing_after_undoing_a_placement_is_not_swallowed_by_escaping_later() {
    // The sequence that lost work: draw something, place a label, undo the
    // label, undo the drawing, draw something else, then press Escape. Escape
    // cancels the label — but the place it wanted to roll back to now holds the
    // second drawing, and rolling back to it deleted that drawing outright with
    // nothing left to redo.
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(20.0, 20.0), at(120.0, 90.0)); // A

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(200.0, 200.0));
    state.pointer_released();

    state.command(Command::Undo).expect("undo the label");
    state.command(Command::Undo).expect("undo the rectangle");
    assert!(state.document().annotations().is_empty());

    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(150.0, 150.0), at(260.0, 240.0)); // B
    let b = rect_at(&state, 0);

    state.command(Command::Escape).expect("escape");

    assert_eq!(
        state.document().annotations().len(),
        1,
        "the second drawing is still there"
    );
    assert_eq!(rect_at(&state, 0), b, "and it is still the second drawing");

    state.command(Command::Undo).expect("undo B");
    assert!(state.document().annotations().is_empty());
    state.command(Command::Redo).expect("redo B");
    assert_eq!(rect_at(&state, 0), b, "B never stopped being redoable");
}

#[test]
fn placing_a_second_label_settles_the_one_left_set_aside() {
    // Place a label, undo the click, then click somewhere else. Starting the
    // second placement replaces the first one's rollback point, and the first
    // one had swallowed the redo stack — so the click that was never taken back
    // came back as an empty label nobody could see, type into, or escape from.
    let mut state = state();
    state.set_tool(Tool::Text);
    state.pointer_pressed(at(80.0, 80.0));
    state.pointer_released();
    state
        .command(Command::Undo)
        .expect("undo the first placement");

    state.pointer_pressed(at(220.0, 200.0));
    state.pointer_released();
    state
        .command(Command::Escape)
        .expect("leave the second label");

    assert!(
        state.document().annotations().is_empty(),
        "both empty labels were cancelled, so the document is untouched"
    );
    state.command(Command::Redo).expect("redo");
    assert!(
        state.document().annotations().is_empty(),
        "and neither of them is waiting on the redo stack to come back invisible"
    );

    state.set_tool(Tool::Select);
    state.pointer_pressed(at(80.0, 80.0));
    state.pointer_released();
    assert_eq!(
        state.selection(),
        None,
        "nothing to click on at the first spot"
    );
}

#[test]
fn escape_still_works_when_a_placement_refuses_to_be_cancelled() {
    // The history can decline the rollback — the place it points at belongs to
    // work done since. Declining is right, but Escape has to keep meaning
    // something: retrying the same refusal forever left the key dead, so the
    // tool could not be reset and the window could not be closed.
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(20.0, 20.0), at(120.0, 90.0));

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(200.0, 200.0));
    state.pointer_released();
    state.command(Command::Undo).expect("undo the label");
    state.command(Command::Undo).expect("undo the rectangle");

    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(150.0, 150.0), at(260.0, 240.0));
    let b = rect_at(&state, 0);

    // However many layers of state there are to unwind, Escape has to unwind
    // them and then close. Before the fix every press was consumed by the
    // refusal, so the answer was always `None` and the editor could not be left.
    let mut intents = Vec::new();
    for _ in 0..4 {
        intents.push(state.command(Command::Escape).expect("escape"));
    }

    assert!(
        intents.contains(&Intent::Close),
        "Escape never got past the refused cancellation: {intents:?}"
    );
    assert_eq!(
        state.tool(),
        Tool::Select,
        "and it reset the tool on the way"
    );
    assert_eq!(
        rect_at(&state, 0),
        b,
        "none of those presses cost the user their drawing"
    );
    assert_eq!(
        state.document().annotations().len(),
        1,
        "and none of them left a ghost behind either"
    );
}

#[test]
fn finishing_a_label_that_history_removed_does_not_un_create_it() {
    // A label the user finished, reopened, and then undid away. Undo took it out
    // of the document, so nothing is being edited — but it is not an abandoned
    // click either, and treating it as one would delete work the redo stack is
    // still holding.
    let mut state = typed("hello");
    state.command(Command::Escape).expect("finish the label");

    let box_of = state.document().annotations()[0].bounds();
    state.set_tool(Tool::Select);
    state.pointer_pressed(at(
        box_of.origin.x + box_of.size.width / 2.0,
        box_of.origin.y + box_of.size.height / 2.0,
    ));
    state.pointer_released();
    assert!(state.editing_text().is_some(), "the label reopened");

    state.command(Command::Undo).expect("undo the label away");
    assert!(
        state.document().annotations().is_empty(),
        "the label really did leave the document"
    );
    assert_eq!(state.editing_text(), None, "so nothing is being edited");

    assert_eq!(
        state.command(Command::Escape).expect("escape"),
        Intent::None,
        "Escape settles the set-aside label rather than falling through"
    );

    state.command(Command::Redo).expect("redo");
    assert_eq!(
        content(&state),
        "hello",
        "the label came back with what the user wrote in it"
    );
    assert_eq!(
        state.editing_text(),
        None,
        "and it came back finished, not reopened"
    );
}

// ---------------------------------------------------------------------------
// Settling an unfinished label before unrelated work
// ---------------------------------------------------------------------------

/// Draws a rectangle, places an empty label, then sets the label aside with an
/// undo — leaving its placement holding an open rollback point.
fn a_drawing_and_a_set_aside_label() -> EditorState {
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(20.0, 20.0), at(120.0, 90.0));

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(200.0, 200.0));
    state.pointer_released();
    state.command(Command::Undo).expect("set the label aside");
    assert_eq!(
        state.document().annotations().len(),
        1,
        "the label is out of the document but not cancelled"
    );
    state
}

#[test]
fn nudging_while_a_label_is_set_aside_does_not_lose_the_nudge() {
    // The keyboard path had no gate at all. Selecting and nudging while a label
    // sat set aside put the move *above* the rollback point the label was still
    // holding, so the next Escape rolled the shape back to where it started —
    // and truncated the redo stack on the way, so the move was gone for good.
    let mut state = a_drawing_and_a_set_aside_label();
    let before = rect_at(&state, 0);

    state.command(Command::SelectAll).expect("select the shape");
    state
        .command(Command::Nudge { dx: 12.0, dy: 7.0 })
        .expect("nudge it");
    let moved = rect_at(&state, 0);
    assert_ne!(moved, before, "the nudge moved the shape");

    state.command(Command::Escape).expect("escape");
    assert_eq!(
        rect_at(&state, 0),
        moved,
        "escaping the label did not drag the shape back with it"
    );
    assert_eq!(
        state.document().annotations().len(),
        1,
        "and the empty label is gone rather than left behind"
    );

    state.command(Command::Undo).expect("undo the nudge");
    assert_eq!(rect_at(&state, 0), before, "the nudge is one undo deep");
    state.command(Command::Redo).expect("redo the nudge");
    assert_eq!(
        rect_at(&state, 0),
        moved,
        "and it never stopped being redoable"
    );
}

#[test]
fn deleting_while_a_label_is_set_aside_settles_it_first() {
    // Same gate, a different command: the deletion has to land above a closed
    // rollback point, not an open one.
    let mut state = a_drawing_and_a_set_aside_label();
    state.command(Command::SelectAll).expect("select the shape");
    state.command(Command::Delete).expect("delete it");
    assert!(
        state.document().annotations().is_empty(),
        "the shape is gone and the label was never committed"
    );

    state.command(Command::Escape).expect("escape");
    assert!(
        state.document().annotations().is_empty(),
        "escaping does not bring the deleted shape back"
    );
    state.command(Command::Undo).expect("undo the deletion");
    assert_eq!(
        state.document().annotations().len(),
        1,
        "the deletion is still undoable, and undoing it does not resurrect a label"
    );
}

#[test]
fn every_command_says_whether_it_settles_an_unfinished_label() {
    // The classification is a compile-time exhaustive match, so this only has to
    // pin the two commands the rule turns on: navigating the history is what a
    // set-aside label exists to survive, and escaping settles it itself.
    let mut state = a_drawing_and_a_set_aside_label();
    state.command(Command::Redo).expect("redo the placement");
    assert!(
        state.editing_text().is_some(),
        "redo picked the set-aside label back up rather than settling it away"
    );
}

#[test]
fn a_second_placement_is_refused_while_the_first_cannot_be_cancelled() {
    // Draw something, place a label, undo the label, undo the drawing. The
    // label's cancellation is now unreachable — the history is behind the point
    // it would roll back to — so clicking to place a *second* label has to do
    // nothing. Starting one closed the first label's edit and took the redo
    // branch it was stranded in into its own safekeeping; escaping the second
    // one handed that branch straight back, and two redos put an invisible,
    // unselectable, uncancellable label into the document.
    let mut state = a_drawing_and_a_set_aside_label();
    state.command(Command::Undo).expect("undo the drawing");
    assert!(state.document().annotations().is_empty());

    state.pointer_pressed(at(300.0, 240.0));
    state.pointer_released();
    assert!(
        state.document().annotations().is_empty(),
        "no second label was started on top of the first"
    );

    state.command(Command::Escape).expect("escape");
    state.command(Command::Redo).expect("redo the drawing");
    state.command(Command::Redo).expect("redo the placement");

    assert_eq!(
        state.document().annotations().len(),
        2,
        "the drawing and exactly one label came back"
    );
    assert!(
        state.editing_text().is_some(),
        "and the label that came back is the one being edited, not an orphan"
    );

    type_str(&mut state, "hi");
    match &state.document().annotations()[1].annotation {
        Annotation::Text { content, .. } => {
            assert_eq!(content, "hi", "the keystrokes reached it");
        }
        other => panic!("expected the label, found {other:?}"),
    }

    state.command(Command::Escape).expect("leave the label");
    assert_eq!(
        state.document().annotations().len(),
        2,
        "and escaping a label with text in it keeps it"
    );
}

#[test]
fn a_refused_placement_leaves_the_document_and_the_preview_alone() {
    let mut state = a_drawing_and_a_set_aside_label();
    state.command(Command::Undo).expect("undo the drawing");
    let revision = state.revision();
    let before = state.document().annotations().len();

    state.pointer_pressed(at(300.0, 240.0));
    assert_eq!(
        state.document().annotations().len(),
        before,
        "the press placed nothing"
    );
    assert_eq!(
        state.revision(),
        revision,
        "nothing changed, so nothing needs rerasterizing"
    );
    state.pointer_released();

    assert_eq!(state.document().annotations().len(), before);
}

#[test]
fn drawing_is_still_allowed_while_a_label_refuses_to_be_cancelled() {
    // Only a *second* label is refused. Anything else the user draws lands above
    // a rollback point the history has already ruled out, so it is safe — and
    // refusing it as well would leave the editor feeling broken.
    let mut state = a_drawing_and_a_set_aside_label();
    state.command(Command::Undo).expect("undo the drawing");

    state.set_tool(Tool::Ellipse);
    drag(&mut state, at(150.0, 150.0), at(260.0, 240.0));
    assert_eq!(
        state.document().annotations().len(),
        1,
        "the ellipse was drawn"
    );

    state.command(Command::Escape).expect("escape");
    assert_eq!(
        state.document().annotations().len(),
        1,
        "and escaping the pending label did not take it back"
    );
}

// ---------------------------------------------------------------------------
// Letting go of a pending label the history can no longer give back
// ---------------------------------------------------------------------------

/// The text of the only text annotation in the document, if there is one.
fn only_label(state: &EditorState) -> Option<String> {
    let mut found = None;
    for object in state.document().annotations() {
        if let Annotation::Text { content, .. } = &object.annotation {
            assert!(found.is_none(), "expected at most one label");
            found = Some(content.clone());
        }
    }
    found
}

#[test]
fn the_text_tool_works_again_once_a_pending_label_is_beyond_recovery() {
    // Refusing to cancel a set-aside label is a wait, and a wait needs an end.
    // Draw, place a label, undo the label, undo the drawing — the cancellation
    // is refused but still reachable. Then draw something else: committing it
    // discards the branch the label's placement was stranded in, so no redo can
    // ever bring the label back.
    //
    // The refusal used to stand anyway. `finish_suspended_text` could never
    // clear the pending label, so every later press of the text tool was
    // refused on its behalf and the tool was dead for the rest of the session.
    let mut state = a_drawing_and_a_set_aside_label();
    state.command(Command::Undo).expect("undo the drawing");

    state.set_tool(Tool::Ellipse);
    drag(&mut state, at(150.0, 150.0), at(260.0, 240.0));
    assert_eq!(
        state.document().annotations().len(),
        1,
        "the ellipse landed"
    );

    // Exactly the user's next move: choose the text tool and click.
    state.set_tool(Tool::Text);
    state.pointer_pressed(at(300.0, 240.0));
    state.pointer_released();
    assert_eq!(
        state.document().annotations().len(),
        2,
        "a new label was placed rather than refused on a dead one's behalf"
    );
    assert!(
        state.editing_text().is_some(),
        "and the user is in it, not stranded outside it"
    );

    type_str(&mut state, "ok");
    assert_eq!(
        only_label(&state).as_deref(),
        Some("ok"),
        "the keystrokes reached the new label"
    );

    state.command(Command::Escape).expect("leave the label");
    assert_eq!(
        state.document().annotations().len(),
        2,
        "a label with text in it is kept"
    );
    assert_eq!(only_label(&state).as_deref(), Some("ok"));

    // And letting go of the dead one resurrected nothing.
    state.command(Command::Redo).expect("redo");
    assert_eq!(
        state.document().annotations().len(),
        2,
        "there was no stranded placement left to redo into the document"
    );
}

#[test]
fn a_press_lets_go_of_a_pending_label_the_history_has_discarded() {
    // The same rule reached through the press itself rather than through
    // choosing a tool, so it holds even when the text tool never leaves the
    // user's hand. The stranding commit here is a keyboard nudge.
    let mut state = state();
    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(20.0, 20.0), at(120.0, 90.0));
    drag(&mut state, at(200.0, 20.0), at(300.0, 90.0));

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(60.0, 200.0));
    state.pointer_released();
    state.command(Command::Undo).expect("set the label aside");
    state.command(Command::Undo).expect("undo the second shape");
    assert_eq!(state.document().annotations().len(), 1);

    // A press right now is refused: the label is still coming back.
    state.pointer_pressed(at(300.0, 240.0));
    state.pointer_released();
    assert_eq!(
        state.document().annotations().len(),
        1,
        "no second label while the first can still return"
    );

    // The nudge commits, which discards the branch holding the placement.
    state.command(Command::SelectAll).expect("select the shape");
    state
        .command(Command::Nudge { dx: 9.0, dy: 4.0 })
        .expect("nudge it");

    state.pointer_pressed(at(300.0, 240.0));
    state.pointer_released();
    assert_eq!(
        state.document().annotations().len(),
        2,
        "now that the placement is gone for good, the press places a label"
    );
    assert!(state.editing_text().is_some());
    type_str(&mut state, "x");
    assert_eq!(only_label(&state).as_deref(), Some("x"));
}

// ---------------------------------------------------------------------------
// Not opening a second editing session over a pending label
// ---------------------------------------------------------------------------

/// A finished label, a rectangle drawn after it, and a second label placed and
/// then set aside by two undos — leaving its cancellation refused but still
/// reachable, with both it and the rectangle sitting on the redo branch.
fn a_finished_label_a_shape_and_a_pending_label() -> (EditorState, LogicalPoint) {
    let mut state = state();
    state.set_tool(Tool::Text);
    state.pointer_pressed(at(60.0, 60.0));
    state.pointer_released();
    type_str(&mut state, "hi");
    state.command(Command::Escape).expect("finish the label");

    let bounds = state.document().annotations()[0].bounds();
    let centre = at(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );

    state.set_tool(Tool::Rectangle);
    drag(&mut state, at(150.0, 150.0), at(260.0, 240.0));

    state.set_tool(Tool::Text);
    state.pointer_pressed(at(320.0, 60.0));
    state.pointer_released();
    state.command(Command::Undo).expect("set the label aside");
    state.command(Command::Undo).expect("undo the rectangle");

    assert_eq!(
        state.document().annotations().len(),
        1,
        "only the finished label is in the document"
    );
    (state, centre)
}

#[test]
fn clicking_an_existing_label_is_refused_while_another_one_is_pending() {
    // Re-entering a label that is already there starts an editing session just
    // as placing a new one does, and the pending label's id is the only thing
    // that will pick it back up when a redo returns it. Overwriting that id
    // left the placement on the redo stack with nobody holding it: two redos
    // put an empty label into the document while the editing session belonged
    // to a different one, and escaping then committed the orphan for good.
    let (mut state, existing) = a_finished_label_a_shape_and_a_pending_label();

    state.set_tool(Tool::Select);
    state.pointer_pressed(existing);
    state.pointer_released();
    assert!(
        state.editing_text().is_none(),
        "the click did not open a second editing session"
    );
    assert_eq!(
        state.document().annotations().len(),
        1,
        "and it changed nothing"
    );

    // The pending label is still the one the history will hand back.
    state.command(Command::Redo).expect("redo the rectangle");
    state.command(Command::Redo).expect("redo the placement");
    assert_eq!(state.document().annotations().len(), 3);
    assert!(
        state.editing_text().is_some(),
        "the label that came back is the one being edited"
    );

    state.command(Command::Escape).expect("cancel it");
    assert_eq!(
        state.document().annotations().len(),
        2,
        "escaping cancelled the empty label rather than committing an orphan"
    );
    assert_eq!(
        only_label(&state).as_deref(),
        Some("hi"),
        "and the label left standing is the one with text in it"
    );
}

#[test]
fn selecting_a_shape_that_is_not_a_label_is_still_allowed_while_one_is_pending() {
    // The refusal is about starting a second editing session, not about the
    // pointer. Selecting and moving an ordinary shape opens nothing, so it goes
    // ahead — refusing every click would leave the editor feeling broken.
    let (mut state, _) = a_finished_label_a_shape_and_a_pending_label();
    state.command(Command::Redo).expect("redo the rectangle");
    let shape = state.document().annotations()[1].bounds();
    // A hollow rectangle is grabbed by its outline, not its empty middle.
    let edge = at(shape.origin.x + shape.size.width / 2.0, shape.origin.y);

    state.set_tool(Tool::Select);
    state.pointer_pressed(edge);
    assert!(state.selection().is_some(), "the shape was selected");
    state.pointer_dragged(at(edge.x + 30.0, edge.y), false);
    state.pointer_released();
    assert_ne!(
        state.document().annotations()[1].bounds().origin.x,
        shape.origin.x,
        "and dragging it moved it"
    );
}

#[test]
fn a_resize_handle_still_works_while_a_label_is_pending() {
    // A press on a handle never reaches the hit test, so it can never open a
    // label — the refusal must not swallow it just because a label happens to
    // be sitting under the handle's corner.
    let (mut state, _) = a_finished_label_a_shape_and_a_pending_label();
    state.command(Command::Redo).expect("redo the rectangle");

    let shape = state.document().annotations()[1].bounds();
    let edge = at(shape.origin.x + shape.size.width / 2.0, shape.origin.y);
    state.set_tool(Tool::Select);
    state.pointer_pressed(edge);
    state.pointer_released();
    assert!(state.selection().is_some(), "the shape has handles now");

    let corner = at(shape.origin.x, shape.origin.y);
    state.pointer_pressed(corner);
    state.pointer_dragged(at(corner.x - 20.0, corner.y - 20.0), false);
    state.pointer_released();
    assert!(
        state.document().annotations()[1].bounds().size.width > shape.size.width,
        "the handle resized the shape"
    );
}
