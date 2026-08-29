//! Headless interaction contracts for the annotation colour popover.

#![allow(clippy::expect_used)]

use egui::accesskit::{Action, ActionRequest, Node, NodeId, Toggled, TreeId};
use egui::{Context, Event, Key, Modifiers, PointerButton, RawInput, Rect, pos2, vec2};
use scrozz_annotate::{Annotation, Color, Document};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Provenance, ScaleFactor,
};
use scrozz_ui::editor::{
    EditorUi, Intent, PALETTE, Tool, editor_layout, toolbar::COLOR_CONTROL_ID,
};
use scrozz_ui::{Theme, theme};

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
        "Choose custom colour",
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
    let (tree, custom, _) = access_node(&reopened, |label| label == "Choose custom colour");
    let (intent, _) = driver.frame(&mut editor, SIZE, vec![activate(tree, custom)]);
    assert_eq!(intent, Intent::CustomColor);
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
