//! The annotation editor's state machine, exercised without a window.
//!
//! [`EditorState`] is deliberately free of egui so that gestures, accelerators
//! and undo can be asserted as plain function calls. Everything the editor's
//! canvas does — press, drag, release — arrives here, so a test that drives
//! those three in sequence is testing the real path a mouse takes, not a
//! parallel one written for the test's convenience.

use scrozz_annotate::{Annotation, AnnotationKind, Color, Document, RedactStyle, Style};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Provenance, ScaleFactor,
};
use scrozz_ui::editor::{
    Caret, Command, EditorState, Handle, Intent, MAX_ZOOM, MIN_ZOOM, TextEdit, Tool,
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
    for tool in [Tool::Highlight, Tool::Pixelate, Tool::Blur] {
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
fn the_two_redaction_tools_pick_different_styles() {
    let mut pixelate = state();
    pixelate.set_tool(Tool::Pixelate);
    drag(&mut pixelate, at(20.0, 20.0), at(120.0, 100.0));

    let mut blur = state();
    blur.set_tool(Tool::Blur);
    drag(&mut blur, at(20.0, 20.0), at(120.0, 100.0));

    let style_of = |state: &EditorState| match state.document().annotations()[0].annotation {
        Annotation::Redact { style, .. } => style,
        _ => panic!("a redaction tool drew something else"),
    };
    assert_eq!(style_of(&pixelate), RedactStyle::Pixelate);
    assert_eq!(style_of(&blur), RedactStyle::Blur);
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
    state.set_tool(Tool::Crop);
    drag(&mut state, at(40.0, 40.0), at(200.0, 180.0));
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
    state.set_tool(Tool::Crop);
    drag(&mut state, at(40.0, 40.0), at(200.0, 180.0));
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
    state.set_tool(Tool::Crop);
    drag(&mut state, at(40.0, 40.0), at(200.0, 180.0));
    state.command(Command::ApplyCrop).expect("crop");

    let cropped = state.document().content_size();
    assert!(cropped.width < full.width);
    assert!(cropped.height < full.height);
    assert_eq!(
        state.document().source.frame.width(),
        capture().frame.width(),
        "cropping resized the source pixels"
    );
}

#[test]
fn applying_a_crop_returns_to_the_pointer() {
    let mut state = state();
    state.set_tool(Tool::Crop);
    drag(&mut state, at(40.0, 40.0), at(200.0, 180.0));
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
    state.pointer_pressed(at(40.0, 40.0));
    state.pointer_dragged(at(200.0, 180.0), false);
    assert!(state.pending_crop().is_some());
    assert_eq!(
        state.document().crop(),
        None,
        "the crop was applied before it was committed"
    );
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
fn restyling_a_redaction_does_not_recolour_it() {
    let mut state = state();
    state.set_tool(Tool::Pixelate);
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

    let source = &state.document().source.frame;
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
    drag(&mut state, at(40.0, 40.0), at(200.0, 180.0));
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
