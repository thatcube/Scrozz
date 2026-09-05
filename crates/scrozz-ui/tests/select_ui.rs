//! Selector UI integration tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use egui::{Context, CursorIcon, Event, PointerButton, RawInput, Rect, pos2, vec2};
use scrozz_core::selection::{SelectionMode, SelectionOptions};
use scrozz_core::{
    CaptureTarget, Display, DisplayId, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor,
    SelectionOutcome, Window, WindowId,
};
use scrozz_ui::{FrozenDisplayFrame, SelectionDecision, SelectionUi, Theme, theme};

fn display(id: &str, x: f64, y: f64, w: f64, h: f64, scale: f64, primary: bool) -> Display {
    let bounds = LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(w, h));
    Display {
        id: DisplayId(id.to_owned()),
        name: id.to_owned(),
        bounds,
        work_area: bounds,
        scale: ScaleFactor::new(scale),
        is_primary: primary,
    }
}

fn window(
    id: &str,
    bounds: LogicalRect,
    display: &str,
    title: Option<&str>,
    application: Option<&str>,
) -> Window {
    Window {
        id: WindowId(id.to_owned()),
        title: title.map(str::to_owned),
        application: application.map(str::to_owned),
        bounds,
        display: DisplayId(display.to_owned()),
        is_visible: true,
    }
}

struct Driver {
    ctx: Context,
    time: f64,
}

impl Driver {
    fn new() -> Self {
        let ctx = Context::default();
        ctx.enable_accesskit();
        theme::install_fonts(&ctx);
        theme::install_style(&ctx, &Theme::dark());
        let mut full = ctx.run_ui(
            RawInput {
                focused: true,
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))),
                ..Default::default()
            },
            |_| {},
        );
        full.textures_delta.clear();
        Self { ctx, time: 0.0 }
    }

    fn step(&mut self, size: [f32; 2], events: Vec<Event>, f: impl FnMut(&mut egui::Ui)) {
        drop(self.step_output(size, events, f));
    }

    fn step_output(
        &mut self,
        size: [f32; 2],
        events: Vec<Event>,
        f: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
        self.time += 1.0 / 60.0;
        let mut full = self.ctx.run_ui(
            RawInput {
                time: Some(self.time),
                predicted_dt: (1.0 / 60.0) as f32,
                focused: true,
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(size[0], size[1]))),
                events,
                ..Default::default()
            },
            f,
        );
        full.textures_delta.clear();
        let _ = full
            .viewport_output
            .values()
            .map(|out| out.repaint_delay)
            .min()
            .unwrap_or(Duration::MAX);
        full
    }
}

fn selection_ui(mode: SelectionMode, displays: Vec<Display>) -> SelectionUi {
    let frames = displays
        .iter()
        .enumerate()
        .map(|(index, display)| FrozenDisplayFrame::synthetic(display.clone(), index as u64 + 1))
        .collect();
    SelectionUi::new(SelectionOptions::for_mode(mode), displays, frames)
}

#[test]
fn ordinary_region_selection_uses_the_native_crosshair_cursor() {
    let displays = vec![display("main", 0.0, 0.0, 360.0, 240.0, 2.0, true)];
    let mut selector = selection_ui(SelectionMode::Region, displays);
    let mut driver = Driver::new();

    let output = driver.step_output(
        [360.0, 240.0],
        vec![Event::PointerMoved(pos2(120.0, 150.0))],
        |ui| {
            let _ = selector.update(ui);
        },
    );

    assert_eq!(output.platform_output.cursor_icon, CursorIcon::Crosshair);
    assert!(selector.state().region().is_none());
}

#[test]
fn primary_click_commits_display_mode() {
    let displays = vec![display("main", 0.0, 0.0, 360.0, 240.0, 2.0, true)];
    let mut selector = selection_ui(SelectionMode::Display, displays);
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    driver.step(
        [360.0, 240.0],
        vec![
            Event::PointerMoved(pos2(120.0, 150.0)),
            Event::PointerButton {
                pos: pos2(120.0, 150.0),
                button: PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
        |ui| {
            decision = selector.update(ui);
        },
    );

    match decision {
        SelectionDecision::Selected(outcome) => {
            assert_eq!(outcome.mode, SelectionMode::Display);
            assert_eq!(
                outcome.target,
                CaptureTarget::Display(DisplayId("main".to_owned()))
            );
        }
        other => panic!("unexpected decision: {other:?}"),
    }
}

#[test]
fn primary_click_commits_all_displays_mode() {
    let displays = vec![
        display("left", 0.0, 0.0, 320.0, 240.0, 2.0, true),
        display("right", 320.0, 0.0, 280.0, 240.0, 1.25, false),
    ];
    let mut selector = selection_ui(SelectionMode::AllDisplays, displays);
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    driver.step(
        [600.0, 240.0],
        vec![
            Event::PointerMoved(pos2(420.0, 120.0)),
            Event::PointerButton {
                pos: pos2(420.0, 120.0),
                button: PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
        |ui| {
            decision = selector.update(ui);
        },
    );

    match decision {
        SelectionDecision::Selected(outcome) => {
            assert_eq!(outcome.mode, SelectionMode::AllDisplays);
            assert_eq!(outcome.target, CaptureTarget::AllDisplays);
        }
        other => panic!("unexpected decision: {other:?}"),
    }
}

#[test]
fn primary_click_commits_window_mode_when_windows_are_supplied() {
    let displays = vec![
        display("main", 0.0, 0.0, 400.0, 300.0, 2.0, true),
        display("right", 400.0, 0.0, 320.0, 300.0, 1.25, false),
    ];
    let mut selector = selection_ui(SelectionMode::Region, displays).with_windows(vec![window(
        "browser",
        LogicalRect::new(
            LogicalPoint::new(420.0, 52.0),
            LogicalSize::new(180.0, 140.0),
        ),
        "right",
        Some("Rust docs"),
        Some("Safari"),
    )]);
    assert!(selector.set_mode(SelectionMode::Window));
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    driver.step(
        [720.0, 300.0],
        vec![
            Event::PointerMoved(pos2(460.0, 120.0)),
            Event::PointerButton {
                pos: pos2(460.0, 120.0),
                button: PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
        |ui| {
            decision = selector.update(ui);
        },
    );

    match decision {
        SelectionDecision::Selected(outcome) => {
            assert_eq!(
                outcome.target,
                CaptureTarget::Window(WindowId("browser".to_owned()))
            );
            assert_eq!(outcome.display, Some(DisplayId("right".to_owned())));
            assert_eq!(outcome.scale, ScaleFactor::new(1.25));
        }
        other => panic!("unexpected decision: {other:?}"),
    }
}

#[test]
fn display_local_viewport_maps_pointer_input_to_its_own_display() {
    let displays = vec![
        display("primary", 0.0, 0.0, 320.0, 240.0, 1.0, true),
        display("hidpi", 200.0, 0.0, 280.0, 200.0, 1.5, false),
    ];
    let hidpi = DisplayId("hidpi".to_owned());
    let mut selector = selection_ui(SelectionMode::Region, displays);
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    driver.step(
        [280.0, 200.0],
        vec![Event::PointerButton {
            pos: pos2(40.0, 110.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| decision = selector.update_display(ui, &hidpi),
    );
    driver.step(
        [280.0, 200.0],
        vec![Event::PointerMoved(pos2(160.0, 190.0))],
        |ui| decision = selector.update_display(ui, &hidpi),
    );
    driver.step(
        [280.0, 200.0],
        vec![Event::PointerButton {
            pos: pos2(160.0, 190.0),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| decision = selector.update_display(ui, &hidpi),
    );

    match decision {
        SelectionDecision::Selected(outcome) => {
            assert_eq!(outcome.display, Some(hidpi));
            assert_eq!(outcome.scale, ScaleFactor::new(1.5));
            assert_eq!(
                outcome.rect.unwrap(),
                LogicalRect::new(
                    LogicalPoint::new(240.0, 110.0),
                    LogicalSize::new(120.0, 80.0),
                )
            );
        }
        other => panic!("unexpected decision: {other:?}"),
    }
}

#[test]
fn window_mode_is_only_available_after_windows_are_supplied() {
    let displays = vec![display("main", 0.0, 0.0, 320.0, 240.0, 2.0, true)];
    let mut without_windows = selection_ui(SelectionMode::Region, displays.clone());
    assert!(!without_windows.set_mode(SelectionMode::Window));

    let mut with_windows =
        selection_ui(SelectionMode::Region, displays).with_windows(vec![window(
            "editor",
            LogicalRect::new(
                LogicalPoint::new(40.0, 40.0),
                LogicalSize::new(160.0, 120.0),
            ),
            "main",
            Some("Draft"),
            Some("Editor"),
        )]);
    assert!(with_windows.set_mode(SelectionMode::Window));
}

#[test]
fn region_drag_commits_on_the_first_mouse_release() {
    let displays = vec![display("main", 0.0, 0.0, 360.0, 240.0, 2.0, true)];
    let mut selector = selection_ui(SelectionMode::Region, displays);
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    driver.step(
        [360.0, 240.0],
        vec![Event::PointerButton {
            pos: pos2(40.0, 120.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| decision = selector.update(ui),
    );
    driver.step(
        [360.0, 240.0],
        vec![Event::PointerMoved(pos2(200.0, 220.0))],
        |ui| decision = selector.update(ui),
    );
    driver.step(
        [360.0, 240.0],
        vec![Event::PointerButton {
            pos: pos2(200.0, 220.0),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| decision = selector.update(ui),
    );
    assert!(matches!(decision, SelectionDecision::Selected(_)));
}

#[test]
fn confirmed_region_can_be_moved_before_the_done_control_commits_it() {
    let displays = vec![display("main", 0.0, 0.0, 360.0, 240.0, 2.0, true)];
    let frames = displays
        .iter()
        .map(|display| FrozenDisplayFrame::synthetic(display.clone(), 1))
        .collect();
    let mut selector = SelectionUi::new(
        SelectionOptions {
            confirm_region: true,
            ..SelectionOptions::region()
        },
        displays,
        frames,
    );
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    for events in [
        vec![Event::PointerButton {
            pos: pos2(40.0, 120.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
        vec![Event::PointerMoved(pos2(200.0, 220.0))],
        vec![Event::PointerButton {
            pos: pos2(200.0, 220.0),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    ] {
        driver.step([360.0, 240.0], events, |ui| {
            decision = selector.update(ui);
        });
    }
    assert_eq!(decision, SelectionDecision::Pending);

    for events in [
        vec![Event::PointerButton {
            pos: pos2(100.0, 160.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
        vec![Event::PointerMoved(pos2(120.0, 170.0))],
        vec![Event::PointerButton {
            pos: pos2(120.0, 170.0),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    ] {
        driver.step([360.0, 240.0], events, |ui| {
            decision = selector.update(ui);
        });
    }
    assert_eq!(decision, SelectionDecision::Pending);
    assert_eq!(
        selector.state().region(),
        Some(LogicalRect::new(
            LogicalPoint::new(60.0, 130.0),
            LogicalSize::new(160.0, 100.0)
        ))
    );

    for events in [
        vec![Event::PointerButton {
            pos: pos2(220.0, 230.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
        vec![Event::PointerMoved(pos2(240.0, 235.0))],
        vec![Event::PointerButton {
            pos: pos2(240.0, 235.0),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    ] {
        driver.step([360.0, 240.0], events, |ui| {
            decision = selector.update(ui);
        });
    }
    assert_eq!(decision, SelectionDecision::Pending);
    assert_eq!(
        selector.state().region(),
        Some(LogicalRect::new(
            LogicalPoint::new(60.0, 130.0),
            LogicalSize::new(180.0, 105.0)
        ))
    );

    for pressed in [true, false] {
        driver.step(
            [360.0, 240.0],
            vec![
                Event::PointerMoved(pos2(278.0, 104.0)),
                Event::PointerButton {
                    pos: pos2(278.0, 104.0),
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            |ui| decision = selector.update(ui),
        );
    }
    assert!(matches!(decision, SelectionDecision::Selected(_)));
}

#[test]
fn scrolling_region_keeps_real_handles_until_its_nearby_start_button_commits() {
    let displays = vec![display("main", 0.0, 0.0, 800.0, 600.0, 2.0, true)];
    let frames = displays
        .iter()
        .map(|display| FrozenDisplayFrame::synthetic(display.clone(), 1))
        .collect();
    let mut selector = SelectionUi::new(
        SelectionOptions {
            confirm_region: true,
            scrolling_setup: true,
            ..SelectionOptions::region()
        },
        displays,
        frames,
    );
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    for events in [
        vec![Event::PointerButton {
            pos: pos2(100.0, 100.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
        vec![Event::PointerMoved(pos2(500.0, 350.0))],
        vec![Event::PointerButton {
            pos: pos2(500.0, 350.0),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    ] {
        driver.step([800.0, 600.0], events, |ui| {
            decision = selector.update(ui);
        });
    }
    assert_eq!(decision, SelectionDecision::Pending);

    let outside = driver.step_output(
        [800.0, 600.0],
        vec![Event::PointerMoved(pos2(720.0, 160.0))],
        |ui| decision = selector.update(ui),
    );
    assert_eq!(outside.platform_output.cursor_icon, CursorIcon::Default);
    let handle = driver.step_output(
        [800.0, 600.0],
        vec![Event::PointerMoved(pos2(500.0, 350.0))],
        |ui| decision = selector.update(ui),
    );
    assert_eq!(handle.platform_output.cursor_icon, CursorIcon::ResizeNwSe);
    let right_edge = driver.step_output(
        [800.0, 600.0],
        vec![Event::PointerMoved(pos2(500.0, 225.0))],
        |ui| decision = selector.update(ui),
    );
    assert_eq!(
        right_edge.platform_output.cursor_icon,
        CursorIcon::ResizeHorizontal
    );
    let top_edge = driver.step_output(
        [800.0, 600.0],
        vec![Event::PointerMoved(pos2(300.0, 100.0))],
        |ui| decision = selector.update(ui),
    );
    assert_eq!(
        top_edge.platform_output.cursor_icon,
        CursorIcon::ResizeVertical
    );
    let inside = driver.step_output(
        [800.0, 600.0],
        vec![Event::PointerMoved(pos2(300.0, 225.0))],
        |ui| decision = selector.update(ui),
    );
    assert_eq!(inside.platform_output.cursor_icon, CursorIcon::Grab);

    for pressed in [true, false] {
        driver.step(
            [800.0, 600.0],
            vec![
                Event::PointerMoved(pos2(450.0, 385.0)),
                Event::PointerButton {
                    pos: pos2(450.0, 385.0),
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            |ui| decision = selector.update(ui),
        );
    }
    let SelectionDecision::Selected(outcome) = decision else {
        panic!("Start capture did not commit the scrolling region");
    };
    assert_eq!(
        outcome.scrolling,
        Some(scrozz_core::ScrollSelection {
            control: scrozz_core::ScrollControl::Manual,
        })
    );
}

#[test]
fn scrolling_controls_render_only_on_the_selected_display() {
    let displays = vec![
        display("left", 0.0, 0.0, 400.0, 300.0, 1.0, true),
        display("right", 400.0, 0.0, 400.0, 300.0, 1.0, false),
    ];
    let frames = displays
        .iter()
        .map(|display| FrozenDisplayFrame::synthetic(display.clone(), 1))
        .collect();
    let mut selector = SelectionUi::new(
        SelectionOptions {
            confirm_region: true,
            scrolling_setup: true,
            ..SelectionOptions::region()
        },
        displays,
        frames,
    );
    let left = DisplayId("left".to_owned());
    let right = DisplayId("right".to_owned());
    let mut driver = Driver::new();

    for events in [
        vec![Event::PointerButton {
            pos: pos2(60.0, 60.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
        vec![Event::PointerMoved(pos2(320.0, 220.0))],
        vec![Event::PointerButton {
            pos: pos2(320.0, 220.0),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    ] {
        driver.step([400.0, 300.0], events, |ui| {
            let _ = selector.update_display(ui, &left);
        });
    }

    let right_output = driver.step_output([400.0, 300.0], Vec::new(), |ui| {
        let _ = selector.update_display(ui, &right);
    });
    let right_update = right_output
        .platform_output
        .accesskit_update
        .expect("right display accessibility tree");
    let right_labels: Vec<_> = right_update
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label())
        .collect();
    assert!(!right_labels.contains(&"Auto"));
    assert!(!right_labels.contains(&"Start capture"));

    let left_output = driver.step_output([400.0, 300.0], Vec::new(), |ui| {
        let _ = selector.update_display(ui, &left);
    });
    let left_update = left_output
        .platform_output
        .accesskit_update
        .expect("left display accessibility tree");
    let left_labels: Vec<_> = left_update
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label())
        .collect();
    assert!(left_labels.contains(&"Auto"));
    assert!(left_labels.contains(&"Start capture"));
}

#[test]
fn scrolling_requires_start_and_preserves_the_selected_control_mode() {
    use egui::accesskit::{Action, ActionRequest};

    let displays = vec![display("main", 0.0, 0.0, 800.0, 600.0, 2.0, true)];
    let frames = displays
        .iter()
        .map(|display| FrozenDisplayFrame::synthetic(display.clone(), 1))
        .collect();
    let mut selector = SelectionUi::new(
        SelectionOptions {
            confirm_region: true,
            scrolling_setup: true,
            ..SelectionOptions::region()
        },
        displays,
        frames,
    );
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    for events in [
        vec![Event::PointerButton {
            pos: pos2(100.0, 100.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
        vec![Event::PointerMoved(pos2(500.0, 350.0))],
        vec![Event::PointerButton {
            pos: pos2(500.0, 350.0),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    ] {
        driver.step([800.0, 600.0], events, |ui| {
            decision = selector.update(ui);
        });
    }
    let output = driver.step_output([800.0, 600.0], Vec::new(), |ui| {
        decision = selector.update(ui);
    });
    let update = output
        .platform_output
        .accesskit_update
        .expect("scrolling controls should expose an accessibility tree");
    let automatic = update
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some("Auto"))
        .map(|(id, _)| *id)
        .expect("Automatic scrolling control");
    let start = update
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some("Start capture"))
        .map(|(id, _)| *id)
        .expect("Start capture control");
    driver.step(
        [800.0, 600.0],
        vec![Event::AccessKitActionRequest(ActionRequest {
            action: Action::Click,
            target_tree: update.tree_id,
            target_node: automatic,
            data: None,
        })],
        |ui| decision = selector.update(ui),
    );
    assert_eq!(decision, SelectionDecision::Pending);

    driver.step(
        [800.0, 600.0],
        vec![Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| decision = selector.update(ui),
    );
    assert_eq!(
        decision,
        SelectionDecision::Pending,
        "Enter must not bypass the dedicated scrolling Start control"
    );
    driver.step(
        [800.0, 600.0],
        vec![Event::AccessKitActionRequest(ActionRequest {
            action: Action::Click,
            target_tree: update.tree_id,
            target_node: start,
            data: None,
        })],
        |ui| decision = selector.update(ui),
    );

    let SelectionDecision::Selected(outcome) = decision else {
        panic!("Start capture did not confirm the scrolling region");
    };
    assert_eq!(
        outcome.scrolling,
        Some(scrozz_core::ScrollSelection {
            control: scrozz_core::ScrollControl::Automatic,
        })
    );
}

#[test]
fn a_one_point_move_is_a_capture_not_a_click() {
    let displays = vec![display("main", 0.0, 0.0, 360.0, 240.0, 2.0, true)];
    let frames = displays
        .iter()
        .map(|display| FrozenDisplayFrame::synthetic(display.clone(), 1))
        .collect();
    let mut selector = SelectionUi::new(
        SelectionOptions {
            remembered: Some(LogicalRect::new(
                LogicalPoint::new(40.0, 80.0),
                LogicalSize::new(160.0, 120.0),
            )),
            ..SelectionOptions::region()
        },
        displays,
        frames,
    );
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    driver.step(
        [360.0, 240.0],
        vec![Event::PointerButton {
            pos: pos2(100.0, 140.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| decision = selector.update(ui),
    );
    driver.step(
        [360.0, 240.0],
        vec![Event::PointerMoved(pos2(101.0, 140.0))],
        |ui| decision = selector.update(ui),
    );
    driver.step(
        [360.0, 240.0],
        vec![Event::PointerButton {
            pos: pos2(101.0, 140.0),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| decision = selector.update(ui),
    );

    assert!(matches!(decision, SelectionDecision::Selected(_)));
}

#[test]
fn a_primary_click_without_movement_cancels_region_capture() {
    let displays = vec![display("main", 0.0, 0.0, 360.0, 240.0, 2.0, true)];
    let mut selector = selection_ui(SelectionMode::Region, displays);
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    driver.step(
        [360.0, 240.0],
        vec![Event::PointerButton {
            pos: pos2(100.0, 140.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| decision = selector.update(ui),
    );
    driver.step(
        [360.0, 240.0],
        vec![Event::PointerButton {
            pos: pos2(100.0, 140.0),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| decision = selector.update(ui),
    );

    assert_eq!(decision, SelectionDecision::Cancelled);
}

#[test]
fn right_click_cancels_region_capture() {
    let displays = vec![display("main", 0.0, 0.0, 360.0, 240.0, 2.0, true)];
    let mut selector = selection_ui(SelectionMode::Region, displays);
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    driver.step(
        [360.0, 240.0],
        vec![Event::PointerButton {
            pos: pos2(100.0, 140.0),
            button: PointerButton::Secondary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| decision = selector.update(ui),
    );

    assert_eq!(decision, SelectionDecision::Cancelled);
}

#[test]
fn escape_wins_when_enter_arrives_in_the_same_frame() {
    let displays = vec![display("main", 0.0, 0.0, 360.0, 240.0, 2.0, true)];
    let mut selector = SelectionUi::new(
        SelectionOptions {
            remembered: Some(LogicalRect::new(
                LogicalPoint::new(20.0, 30.0),
                LogicalSize::new(120.0, 80.0),
            )),
            ..SelectionOptions::region()
        },
        displays,
        Vec::new(),
    );
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    driver.step(
        [360.0, 240.0],
        vec![
            Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ],
        |ui| decision = selector.update(ui),
    );

    assert_eq!(decision, SelectionDecision::Cancelled);
}

#[test]
fn hud_keyboard_focus_survives_between_tab_and_space_frames() {
    let displays = vec![display("main", 0.0, 0.0, 360.0, 240.0, 2.0, true)];
    let frames = displays
        .iter()
        .map(|display| FrozenDisplayFrame::synthetic(display.clone(), 1))
        .collect();
    let mut selector = SelectionUi::new(SelectionOptions::default(), displays, frames);
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;

    driver.step(
        [360.0, 240.0],
        vec![Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| {
            decision = selector.update(ui);
        },
    );
    assert_eq!(decision, SelectionDecision::Pending);
    driver.step(
        [360.0, 240.0],
        vec![Event::Key {
            key: egui::Key::Space,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| {
            decision = selector.update(ui);
        },
    );

    assert_eq!(selector.state().mode(), SelectionMode::Display);
    assert_eq!(decision, SelectionDecision::Pending);
}

#[test]
fn arrow_keys_do_not_mutate_a_region_hidden_by_another_mode() {
    let original = LogicalRect::new(LogicalPoint::new(20.0, 30.0), LogicalSize::new(120.0, 80.0));
    let displays = vec![display("main", 0.0, 0.0, 360.0, 240.0, 2.0, true)];
    let frames = vec![FrozenDisplayFrame::synthetic(displays[0].clone(), 1)];
    let mut selector = SelectionUi::new(
        SelectionOptions {
            remembered: Some(original),
            ..SelectionOptions::region()
        },
        displays,
        frames,
    );
    assert!(selector.set_mode(SelectionMode::Display));
    let mut driver = Driver::new();

    driver.step(
        [360.0, 240.0],
        vec![Event::Key {
            key: egui::Key::ArrowRight,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
        |ui| {
            let _ = selector.update(ui);
        },
    );
    assert!(selector.set_mode(SelectionMode::Region));

    assert_eq!(selector.state().region(), Some(original));
}

#[test]
fn accessibility_activation_commits_the_current_non_region_target() {
    use egui::accesskit::{Action, ActionRequest};

    let displays = vec![display("main", 0.0, 0.0, 360.0, 240.0, 2.0, true)];
    let mut selector = selection_ui(SelectionMode::AllDisplays, displays);
    let mut driver = Driver::new();
    let mut decision = SelectionDecision::Pending;
    let output = driver.step_output([360.0, 240.0], Vec::new(), |ui| {
        decision = selector.update(ui);
    });
    let update = output
        .platform_output
        .accesskit_update
        .expect("the selector should expose an accessibility tree");
    let canvas = update
        .nodes
        .iter()
        .find(|(_, node)| {
            node.label()
                .is_some_and(|label| label.starts_with("Selection overlay, Capture All Displays"))
        })
        .map(|(id, _)| *id)
        .expect("the selector canvas should be an accessible control");

    driver.step(
        [360.0, 240.0],
        vec![Event::AccessKitActionRequest(ActionRequest {
            action: Action::Click,
            target_tree: update.tree_id,
            target_node: canvas,
            data: None,
        })],
        |ui| decision = selector.update(ui),
    );

    assert!(matches!(
        decision,
        SelectionDecision::Selected(SelectionOutcome {
            target: CaptureTarget::AllDisplays,
            ..
        })
    ));
}
