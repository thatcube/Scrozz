//! Headless interaction contracts for the annotation colour popover.

#![allow(clippy::expect_used)]

use egui::accesskit::{Action, ActionData, ActionRequest, Node, NodeId, Toggled, TreeId};
use egui::{
    Context, CursorIcon, Event, Key, Modifiers, MouseWheelUnit, PointerButton, RawInput, Rect,
    TouchPhase, pos2, vec2,
};
use scrozz_annotate::{Annotation, ArrowStyle, Color, Document};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Provenance, ScaleFactor,
};
use scrozz_ui::editor::{
    Command, EditorUi, Intent, PALETTE, TextEdit, Tool, editor_layout, fit, rect_to_screen,
    toolbar::COLOR_CONTROL_ID,
};
use scrozz_ui::{Space, Theme, theme};

const SIZE: [f32; 2] = [1040.0, 720.0];

fn capture() -> Capture {
    let width = 160;
    let height = 120;
    Capture {
        frame: Frame {
            data: vec![200; width * height * 4],
            size: PhysicalSize::new(width as f64, height as f64),
            stride: width * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::IDENTITY,
        },
        provenance: Provenance::Region,
        target: CaptureTarget::Region(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(width as f64, height as f64),
        )),
    }
}

fn editor() -> EditorUi {
    EditorUi::new(Document::new(capture()))
}

struct Driver {
    ctx: Context,
    time: f64,
}

impl Driver {
    fn new(dark: bool, pixels_per_point: f32) -> Self {
        let ctx = Context::default();
        ctx.enable_accesskit();
        ctx.set_pixels_per_point(pixels_per_point);
        theme::install_fonts(&ctx);
        let theme = if dark { Theme::dark() } else { Theme::light() };
        theme::install_style(&ctx, &theme);
        Self { ctx, time: 0.0 }
    }

    fn frame(
        &mut self,
        editor: &mut EditorUi,
        size: [f32; 2],
        events: Vec<Event>,
    ) -> (Intent, egui::FullOutput) {
        self.time += 1.0 / 60.0;
        let mut intent = Intent::None;
        let mut output = self.ctx.run_ui(
            RawInput {
                time: Some(self.time),
                predicted_dt: 1.0 / 60.0,
                focused: true,
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(size[0], size[1]))),
                events,
                ..Default::default()
            },
            |ui| intent = editor.update(ui),
        );
        output.textures_delta.clear();
        (intent, output)
    }
}

fn access_node(
    output: &egui::FullOutput,
    predicate: impl Fn(&str) -> bool,
) -> (TreeId, NodeId, &Node) {
    let update = output
        .platform_output
        .accesskit_update
        .as_ref()
        .expect("the editor should expose an accessibility tree");
    let (id, node) = update
        .nodes
        .iter()
        .find(|(_, node)| node.label().is_some_and(&predicate))
        .expect("the requested accessible control should exist");
    (update.tree_id, *id, node)
}

fn activate(tree: TreeId, node: NodeId) -> Event {
    access_action(tree, node, Action::Click)
}

fn access_action(tree: TreeId, node: NodeId, action: Action) -> Event {
    Event::AccessKitActionRequest(ActionRequest {
        action,
        target_tree: tree,
        target_node: node,
        data: None,
    })
}

#[test]
fn toolbar_done_and_cancel_are_the_accessible_session_decisions() {
    for (label, expected) in [("Done", Intent::Commit), ("Cancel", Intent::Discard)] {
        let mut driver = Driver::new(false, 1.0);
        let mut editor = editor();
        let (_, output) = driver.frame(&mut editor, SIZE, Vec::new());
        let (tree, node, _) = access_node(&output, |candidate| candidate == label);
        let (intent, _) = driver.frame(&mut editor, SIZE, vec![activate(tree, node)]);
        assert_eq!(intent, expected, "{label}");
    }
}

fn set_numeric(tree: TreeId, node: NodeId, value: f64) -> Event {
    Event::AccessKitActionRequest(ActionRequest {
        action: Action::SetValue,
        target_tree: tree,
        target_node: node,
        data: Some(ActionData::NumericValue(value)),
    })
}

#[test]
fn toolbar_leads_with_crop_scene_and_add_image_before_annotations() {
    let mut driver = Driver::new(false, 1.0);
    let mut editor = editor();
    let (_, output) = driver.frame(&mut editor, SIZE, Vec::new());
    let update = output.platform_output.accesskit_update.as_ref().unwrap();
    let labels: Vec<_> = update
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label().map(str::to_owned))
        .collect();
    let position = |label: &str| {
        labels
            .iter()
            .position(|candidate| candidate == label)
            .unwrap_or_else(|| panic!("{label} is exposed to accessibility"))
    };
    assert!(position("Crop") < position("Scene"));
    assert!(position("Scene") < position("Add Image"));
    assert!(position("Add Image") < position("Select"));

    let (tree, add_image, _) = access_node(&output, |label| label == "Add Image");
    let (intent, _) = driver.frame(&mut editor, SIZE, vec![activate(tree, add_image)]);
    assert_eq!(intent, Intent::AddImage);
}

#[test]
fn disclosure_keyboard_activation_does_not_apply_the_editors_enter_command() {
    let mut driver = Driver::new(true, 1.0);
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Crop);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(20.0, 20.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(80.0, 70.0), false);
    let crop = editor.state().pending_crop().expect("pending crop");
    let _ = driver.frame(&mut editor, SIZE, Vec::new());
    driver
        .ctx
        .memory_mut(|memory| memory.request_focus(egui::Id::new(COLOR_CONTROL_ID)));

    let (intent, _) = driver.frame(
        &mut editor,
        SIZE,
        vec![Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
    );

    assert_eq!(intent, Intent::None);
    assert_eq!(editor.state().pending_crop(), Some(crop));
    assert!(editor.color_popover_is_open());
}

#[test]
fn disclosure_opens_a_complete_accessible_palette_without_reflowing() {
    let mut driver = Driver::new(true, 2.0);
    let mut editor = editor();
    let layout_before = editor_layout(Rect::from_min_size(pos2(0.0, 0.0), vec2(SIZE[0], SIZE[1])));
    let (_, closed) = driver.frame(&mut editor, SIZE, Vec::new());
    let (tree, control, _) = access_node(&closed, |label| label.starts_with("Colour:"));
    let revision = editor.state().revision();
    let history = editor.state().undo_depth();

    let (_, opened) = driver.frame(&mut editor, SIZE, vec![activate(tree, control)]);

    assert!(editor.color_popover_is_open());
    assert_eq!(editor.state().revision(), revision);
    assert_eq!(editor.state().undo_depth(), history);
    assert_eq!(
        editor_layout(Rect::from_min_size(pos2(0.0, 0.0), vec2(SIZE[0], SIZE[1]))),
        layout_before
    );
    for name in [
        "Black colour",
        "Red colour",
        "Orange colour",
        "Yellow colour",
        "Green colour",
        "Cyan colour",
        "Blue colour",
        "Purple colour",
        "Pink colour",
        "White colour",
        "Add custom colour",
    ] {
        let _ = access_node(&opened, |label| label == name);
    }
    let (_, _, selected) = access_node(&opened, |label| label == "Red colour");
    assert_eq!(selected.toggled(), Some(Toggled::True));
}

#[test]
fn same_colour_is_a_no_op_and_custom_colour_is_forwarded() {
    let mut driver = Driver::new(false, 1.0);
    let mut editor = editor();
    editor.open_color_popover();
    let (_, opened) = driver.frame(&mut editor, SIZE, Vec::new());
    let (tree, red, _) = access_node(&opened, |label| label == "Red colour");
    let revision = editor.state().revision();
    let history = editor.state().undo_depth();

    let (intent, _) = driver.frame(&mut editor, SIZE, vec![activate(tree, red)]);
    assert_eq!(intent, Intent::None);
    assert!(!editor.color_popover_is_open());
    assert_eq!(editor.state().revision(), revision);
    assert_eq!(editor.state().undo_depth(), history);

    editor.open_color_popover();
    let (_, reopened) = driver.frame(&mut editor, SIZE, Vec::new());
    let (tree, custom, _) = access_node(&reopened, |label| label == "Add custom colour");
    let (intent, _) = driver.frame(&mut editor, SIZE, vec![activate(tree, custom)]);
    assert_eq!(intent, Intent::CustomColor);
}

#[test]
fn zoom_keys_pinch_wheel_and_percentage_menu_are_view_only() {
    let mut driver = Driver::new(true, 1.0);
    let mut editor = editor();
    let revision = editor.state().revision();
    let command = Modifiers {
        command: true,
        ..Modifiers::NONE
    };
    let (_, initial) = driver.frame(&mut editor, SIZE, Vec::new());
    let (tree, zoom, _) = access_node(&initial, |label| label == "100%");

    let _ = driver.frame(&mut editor, SIZE, vec![activate(tree, zoom)]);
    let (_, menu) = driver.frame(&mut editor, SIZE, Vec::new());
    let (tree, two_hundred, _) = access_node(&menu, |label| label == "200%");
    let _ = driver.frame(&mut editor, SIZE, vec![activate(tree, two_hundred)]);
    assert_eq!(editor.state().zoom(), 2.0);

    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![Event::Key {
            key: Key::Num0,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: command,
        }],
    );
    assert_eq!(editor.state().zoom(), 1.0);
    assert!(editor.state().is_fit_zoom());
    assert_eq!(editor.state().pan(), (0.0, 0.0));

    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![Event::Key {
            key: Key::Plus,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: command,
        }],
    );
    let keyboard_zoom = editor.state().zoom();
    assert!(keyboard_zoom > 1.0);
    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![Event::Key {
            key: Key::Minus,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: command,
        }],
    );
    assert!(editor.state().zoom() < keyboard_zoom);

    let pointer = pos2(720.0, 420.0);
    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![Event::PointerMoved(pointer), Event::Zoom(1.5)],
    );
    assert!(editor.state().zoom() > 1.0);
    assert_ne!(
        editor.state().pan(),
        (0.0, 0.0),
        "pinch zoom did not preserve the off-centre pointer anchor"
    );
    let after_pinch = editor.state().zoom();

    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![
            Event::PointerMoved(pointer),
            Event::MouseWheel {
                unit: MouseWheelUnit::Point,
                delta: vec2(0.0, 40.0),
                phase: TouchPhase::Move,
                modifiers: command,
            },
        ],
    );
    assert_ne!(editor.state().zoom(), after_pinch);
    assert_eq!(editor.state().revision(), revision);
    assert!(!editor.state().is_dirty());
}

#[test]
fn middle_mouse_space_drag_and_trackpad_scroll_pan_a_zoomed_canvas() {
    let mut driver = Driver::new(true, 1.0);
    let mut editor = editor();
    editor.state_mut().set_zoom(2.0);
    let start = pos2(520.0, 380.0);
    let moved = pos2(560.0, 405.0);

    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![
            Event::PointerMoved(start),
            Event::PointerButton {
                pos: start,
                button: PointerButton::Middle,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
    );
    assert!(
        editor.state().is_panning(),
        "middle press did not begin pan"
    );
    let _ = driver.frame(&mut editor, SIZE, vec![Event::PointerMoved(moved)]);
    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![Event::PointerButton {
            pos: moved,
            button: PointerButton::Middle,
            pressed: false,
            modifiers: Modifiers::NONE,
        }],
    );
    let after_middle = editor.state().pan();
    assert_ne!(after_middle, (0.0, 0.0));

    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![
            Event::PointerMoved(start),
            Event::Key {
                key: Key::Space,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: start,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
    );
    let _ = driver.frame(&mut editor, SIZE, vec![Event::PointerMoved(moved)]);
    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![
            Event::PointerButton {
                pos: moved,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
            Event::Key {
                key: Key::Space,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
        ],
    );
    let after_space = editor.state().pan();
    assert_ne!(after_space, after_middle);

    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![
            Event::PointerMoved(start),
            Event::MouseWheel {
                unit: MouseWheelUnit::Point,
                delta: vec2(13.0, -9.0),
                phase: TouchPhase::Move,
                modifiers: Modifiers::NONE,
            },
        ],
    );
    assert_ne!(editor.state().pan(), after_space);
}

#[test]
fn crop_panel_exposes_dimensions_transforms_zoom_snap_and_transaction_actions() {
    let mut driver = Driver::new(false, 1.0);
    let mut editor = editor();
    let revision = editor.state().revision();
    editor.state_mut().set_tool(Tool::Crop);
    let (_, output) = driver.frame(&mut editor, SIZE, Vec::new());

    let _ = access_node(&output, |label| label == "Aspect ratio: Freeform");
    let _ = access_node(&output, |label| label.starts_with("Crop width:"));
    let _ = access_node(&output, |label| label.starts_with("Crop height:"));
    let update = output.platform_output.accesskit_update.as_ref().unwrap();
    let labels: Vec<_> = update
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label())
        .collect();
    for expected in [
        "Swap",
        "Rotate left",
        "Rotate right",
        "Flip horizontal",
        "Flip vertical",
        "Zoom -",
        "Zoom +",
        "Fit",
        "Snap to edges",
    ] {
        assert!(
            labels.contains(&expected),
            "missing {expected:?} from crop controls: {labels:?}"
        );
    }
    // Distinct from the toolbar's own session Cancel/Done, which are the only
    // controls that end an editing session.
    let (tree, cancel, _) = access_node(&output, |label| label == "Cancel Crop");
    let _ = access_node(&output, |label| label == "Apply Crop");

    let _ = driver.frame(&mut editor, SIZE, vec![activate(tree, cancel)]);
    assert!(!editor.state().crop_mode());
    assert_eq!(editor.document().crop(), None);
    assert_eq!(editor.state().revision(), revision);

    editor.state_mut().set_tool(Tool::Crop);
    editor.state_mut().set_crop_width(80.0);
    editor.state_mut().set_crop_height(60.0);
    editor
        .state_mut()
        .command(scrozz_ui::editor::Command::ApplyCrop)
        .unwrap();
    editor.state_mut().set_tool(Tool::Crop);
    let (_, output) = driver.frame(&mut editor, SIZE, Vec::new());
    let _ = access_node(&output, |label| label == "Revert to Original");
}

#[test]
fn crop_canvas_uses_default_inside_and_resize_cursors_only_on_edges() {
    let mut driver = Driver::new(false, 1.0);
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Crop);
    editor.state_mut().set_crop_width(80.0);
    editor.state_mut().set_crop_height(60.0);

    let full = Rect::from_min_size(pos2(0.0, 0.0), vec2(SIZE[0], SIZE[1]));
    let (_, canvas) = editor_layout(full);
    let content = editor.state().display_content_bounds();
    let image = fit(content, canvas.shrink(Space::SM), 1.0, (0.0, 0.0));
    let crop = rect_to_screen(
        editor.state().pending_crop_display().unwrap(),
        image,
        content,
    );

    let (_, inside) = driver.frame(&mut editor, SIZE, vec![Event::PointerMoved(crop.center())]);
    assert_eq!(inside.platform_output.cursor_icon, CursorIcon::Default);

    let (_, edge) = driver.frame(
        &mut editor,
        SIZE,
        vec![Event::PointerMoved(crop.left_center())],
    );
    assert_eq!(
        edge.platform_output.cursor_icon,
        CursorIcon::ResizeHorizontal
    );

    let (_, corner) = driver.frame(
        &mut editor,
        SIZE,
        vec![Event::PointerMoved(crop.left_top())],
    );
    assert_eq!(corner.platform_output.cursor_icon, CursorIcon::ResizeNwSe);
}

#[test]
fn transformed_text_caret_has_a_visible_segment_and_valid_ime_bounds() {
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Text);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(20.0, 20.0));
    editor.state_mut().pointer_released();
    editor
        .state_mut()
        .text_edit(&TextEdit::Insert("note".to_owned()));
    editor.state_mut().command(Command::Escape).unwrap();

    editor.state_mut().set_tool(Tool::Crop);
    editor
        .state_mut()
        .command(Command::RotateCropRight)
        .unwrap();
    editor.state_mut().command(Command::ApplyCrop).unwrap();

    let text = editor.document().annotations()[0].bounds();
    let inside = LogicalPoint::new(
        text.origin.x + text.size.width / 2.0,
        text.origin.y + text.size.height / 2.0,
    );
    editor.state_mut().pointer_pressed(inside);
    editor.state_mut().pointer_released();
    assert!(editor.state().editing_text().is_some());

    let mut driver = Driver::new(false, 1.0);
    let (_, output) = driver.frame(&mut editor, SIZE, Vec::new());
    let ime = output
        .platform_output
        .ime
        .expect("editing transformed text should publish IME geometry");
    for rect in [ime.rect, ime.cursor_rect] {
        assert!(rect.width() > 0.0);
        assert!(rect.height() > 0.0);
        assert!(
            [rect.min.x, rect.min.y, rect.max.x, rect.max.y]
                .into_iter()
                .all(f32::is_finite)
        );
    }
    assert!(
        ime.cursor_rect.width() > ime.cursor_rect.height(),
        "a 90-degree rotation should turn the caret into a horizontal segment"
    );
}

#[test]
fn redact_is_the_only_privacy_tool_and_intensity_is_accessible_and_view_only_until_changed() {
    let mut driver = Driver::new(true, 1.0);
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Redact);
    let revision = editor.state().revision();
    let (_, output) = driver.frame(&mut editor, SIZE, Vec::new());
    let update = output.platform_output.accesskit_update.as_ref().unwrap();
    let labels: Vec<_> = update
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label())
        .collect();
    assert!(labels.contains(&"Redact"));
    assert!(!labels.contains(&"Blur"));
    assert!(!labels.contains(&"Pixelate"));
    assert!(!labels.iter().any(|label| label.starts_with("Colour:")));
    assert!(!labels.contains(&"Stroke width"));

    let (tree, intensity, node) = access_node(&output, |label| label.starts_with("Intensity"));
    assert_eq!(node.min_numeric_value(), Some(0.0));
    assert_eq!(node.max_numeric_value(), Some(100.0));
    assert_eq!(editor.state().revision(), revision);

    let _ = driver.frame(&mut editor, SIZE, vec![set_numeric(tree, intensity, 20.0)]);
    assert!((editor.state().redact_intensity() - 0.2).abs() < 0.001);
    assert_eq!(editor.state().revision(), revision);

    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(20.0, 20.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(100.0, 80.0), false);
    editor.state_mut().pointer_released();
    let depth = editor.state().undo_depth();
    let (_, output) = driver.frame(&mut editor, SIZE, Vec::new());
    let (tree, intensity, _) = access_node(&output, |label| label.starts_with("Intensity"));
    let _ = driver.frame(&mut editor, SIZE, vec![set_numeric(tree, intensity, 90.0)]);
    assert!((editor.state().redact_intensity() - 0.9).abs() < 0.001);
    assert_eq!(editor.state().undo_depth(), depth + 1);
}

#[test]
fn preset_updates_the_selection_and_the_next_arrow_default_in_one_history_step() {
    let mut driver = Driver::new(true, 1.0);
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Arrow);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(20.0, 20.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(80.0, 70.0), false);
    editor.state_mut().pointer_released();
    let selected = editor
        .state()
        .selection()
        .expect("the arrow should be selected");
    let history = editor.state().undo_depth();
    editor.open_color_popover();
    let (_, opened) = driver.frame(&mut editor, SIZE, Vec::new());
    let (tree, orange, _) = access_node(&opened, |label| label == "Orange colour");

    let _ = driver.frame(&mut editor, SIZE, vec![activate(tree, orange)]);

    let selected_style = editor
        .document()
        .get(selected)
        .expect("selected arrow")
        .style;
    assert_eq!(selected_style.stroke, PALETTE[2]);
    assert_eq!(editor.state().stroke_color(), PALETTE[2]);
    assert_eq!(editor.state().undo_depth(), history + 1);

    editor.state_mut().select(None);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(90.0, 20.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(130.0, 80.0), false);
    editor.state_mut().pointer_released();
    assert_eq!(
        editor.document().annotations()[1].style.stroke,
        PALETTE[2],
        "the active Arrow tool lost the new default"
    );
}

#[test]
fn escape_and_outside_click_close_only_the_popover() {
    let mut driver = Driver::new(true, 1.0);
    let mut editor = editor();
    editor.open_color_popover();
    let _ = driver.frame(&mut editor, SIZE, Vec::new());

    let (intent, _) = driver.frame(
        &mut editor,
        SIZE,
        vec![Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
    );
    assert_eq!(
        intent,
        Intent::None,
        "Escape leaked through and closed the editor"
    );
    assert!(!editor.color_popover_is_open());

    editor.open_color_popover();
    let _ = driver.frame(&mut editor, SIZE, Vec::new());
    let outside = pos2(SIZE[0] - 20.0, SIZE[1] - 20.0);
    let revision = editor.state().revision();
    let history = editor.state().undo_depth();
    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![
            Event::PointerMoved(outside),
            Event::PointerButton {
                pos: outside,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: outside,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
    );
    assert!(!editor.color_popover_is_open());
    assert_eq!(editor.state().revision(), revision);
    assert_eq!(editor.state().undo_depth(), history);
}

#[test]
fn keyboard_navigation_selects_a_preset_and_edge_placement_stays_on_screen() {
    for pixels_per_point in [1.0, 2.0] {
        let mut driver = Driver::new(true, pixels_per_point);
        let mut editor = editor();
        editor.open_color_popover();
        let _ = driver.frame(&mut editor, [560.0, 400.0], Vec::new());
        let _ = driver.frame(
            &mut editor,
            [560.0, 400.0],
            vec![Event::Key {
                key: Key::ArrowDown,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        let _ = driver.frame(
            &mut editor,
            [560.0, 400.0],
            vec![Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );

        assert_eq!(editor.state().stroke_color(), PALETTE[2]);
        assert!(!editor.color_popover_is_open());
        let popup = editor.color_popover_rect().expect("resolved popup bounds");
        assert!(popup.left() >= 0.0 && popup.top() >= 0.0, "{popup:?}");
        assert!(
            popup.right() <= 560.0 && popup.bottom() <= 400.0,
            "{popup:?}"
        );
    }
}

#[test]
fn external_custom_colours_preserve_alpha_coalesce_and_no_op_cleanly() {
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Rectangle);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(20.0, 20.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(80.0, 70.0), false);
    editor.state_mut().pointer_released();
    let selected = editor.state().selection().expect("selected rectangle");
    let history = editor.state().undo_depth();
    let custom = Color::rgba(12, 130, 240, 96);

    editor.apply_external_color(Color::rgba(10, 120, 230, 128));
    editor.apply_external_color(custom);

    assert_eq!(editor.state().undo_depth(), history + 1);
    assert_eq!(
        editor
            .document()
            .get(selected)
            .expect("rectangle")
            .style
            .stroke,
        custom
    );
    let revision = editor.state().revision();
    editor.apply_external_color(custom);
    assert_eq!(editor.state().revision(), revision);
    assert_eq!(editor.state().undo_depth(), history + 1);
    assert!(matches!(
        editor
            .document()
            .get(selected)
            .expect("rectangle")
            .annotation,
        Annotation::Rectangle(_)
    ));
}

#[test]
fn fallback_exposes_keyboard_adjustable_rgba_channels() {
    let mut driver = Driver::new(true, 1.0);
    let mut editor = editor();
    editor.open_custom_color_fallback();
    let (_, output) = driver.frame(&mut editor, SIZE, Vec::new());
    for label in ["Red", "Green", "Blue", "Opacity"] {
        let _ = access_node(&output, |candidate| candidate.starts_with(label));
    }
    let (tree, green, _) = access_node(&output, |label| label.starts_with("Green"));
    let before = editor.state().stroke_color();

    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![access_action(tree, green, Action::Increment)],
    );

    assert_ne!(editor.state().stroke_color(), before);
}

#[test]
fn egui_colours_round_trip_as_straight_alpha() {
    let translucent = egui::Color32::from_rgba_unmultiplied(255, 0, 0, 128);
    assert_eq!(
        scrozz_ui::editor::toolbar::from_egui(translucent),
        Color::rgba(255, 0, 0, 128)
    );
}

#[test]
fn custom_swatches_are_bounded_mru_and_accessibly_replaceable() {
    let mut driver = Driver::new(true, 1.0);
    let mut editor = editor();
    let colors: Vec<_> = (0..10)
        .map(|value| Color::rgba(value, value + 1, value + 2, 128))
        .collect();
    editor.set_custom_swatches(colors.clone());
    assert_eq!(editor.custom_swatches().len(), 8);
    editor.state_mut().set_stroke_color(colors[2]);
    editor.open_color_popover();
    let (_, output) = driver.frame(&mut editor, SIZE, Vec::new());
    let _ = access_node(&output, |label| label.starts_with("Custom #"));
    let _ = access_node(&output, |label| label == "Remove selected custom colour");
    let (tree, replace, _) =
        access_node(&output, |label| label == "Replace selected custom colour");

    let (intent, _) = driver.frame(&mut editor, SIZE, vec![activate(tree, replace)]);
    assert_eq!(intent, Intent::CustomColor);
    let replacement = Color::rgba(240, 30, 80, 96);
    editor.remember_custom_color(replacement);
    let changed = editor
        .take_custom_swatches_change()
        .expect("palette persistence update");
    assert_eq!(changed[0], replacement);
    assert_eq!(changed.len(), 8);
    assert!(!changed.contains(&colors[2]));
}

#[test]
fn an_open_popover_owns_editor_keyboard_and_canvas_input() {
    let mut driver = Driver::new(true, 1.0);
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Arrow);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(20.0, 20.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(80.0, 70.0), false);
    editor.state_mut().pointer_released();
    let arrow = editor.document().annotations()[0].annotation.clone();
    let history = editor.state().undo_depth();
    editor.open_color_popover();
    let _ = driver.frame(&mut editor, SIZE, Vec::new());

    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![
            Event::Key {
                key: Key::ArrowDown,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            Event::Key {
                key: Key::Delete,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            Event::Text("r".to_owned()),
        ],
    );
    assert_eq!(editor.document().annotations()[0].annotation, arrow);
    assert_eq!(editor.state().tool(), Tool::Arrow);
    assert_eq!(editor.state().undo_depth(), history);

    editor.state_mut().set_tool(Tool::Counter);
    editor.open_color_popover();
    let _ = driver.frame(&mut editor, SIZE, Vec::new());
    let count = editor.document().len();
    let outside = pos2(SIZE[0] - 20.0, SIZE[1] - 20.0);
    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![
            Event::PointerMoved(outside),
            Event::PointerButton {
                pos: outside,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: outside,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
    );
    assert_eq!(
        editor.document().len(),
        count,
        "the dismissing click reached the canvas"
    );
}

#[test]
fn arrow_inspector_exposes_styles_bend_and_named_thickness_accessibly() {
    let mut driver = Driver::new(true, 1.0);
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Arrow);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(20.0, 20.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(100.0, 80.0), false);
    editor.state_mut().pointer_released();
    let selected = editor.state().selection().expect("selected arrow");
    let history = editor.state().undo_depth();
    editor.open_arrow_popover();
    let (_, output) = driver.frame(&mut editor, SIZE, Vec::new());
    for label in ["Bold arrow", "Curved arrow", "Sketch arrow", "Double arrow"] {
        let _ = access_node(&output, |candidate| candidate == label);
    }
    for label in [
        "Thin thickness",
        "Regular thickness",
        "Bold thickness",
        "Heavy thickness",
    ] {
        let _ = access_node(&output, |candidate| candidate.starts_with(label));
    }
    let _ = access_node(&output, |candidate| candidate.starts_with("Bend"));
    let (tree, curved, _) = access_node(&output, |label| label == "Curved arrow");

    let _ = driver.frame(&mut editor, SIZE, vec![activate(tree, curved)]);

    let object = editor.document().get(selected).expect("arrow");
    assert_eq!(object.style.arrow_style, ArrowStyle::Curved);
    assert!((object.style.arrow_bend - 0.28).abs() < 0.01);
    assert_eq!(editor.state().undo_depth(), history + 1);
    assert!(editor.arrow_popover_is_open());
}

#[test]
fn stroke_control_keyboard_steps_through_named_source_unit_widths() {
    let mut driver = Driver::new(true, 2.0);
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Arrow);
    let (_, output) = driver.frame(&mut editor, SIZE, Vec::new());
    let (tree, stroke, _) = access_node(&output, |label| label == "Stroke width");
    assert_eq!(editor.state().stroke_width(), 4.0);

    let _ = driver.frame(
        &mut editor,
        SIZE,
        vec![access_action(tree, stroke, Action::Increment)],
    );

    assert_eq!(editor.state().stroke_width(), 8.0);
}
