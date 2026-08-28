#![allow(missing_docs)]

use scrozz_core::selection::{
    AspectLock, CrosshairMode, SelectionCapabilities, SelectionMode, SelectionOptions,
    SizeConstraint,
};
use scrozz_core::{
    Display, DisplayId, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor, Window, WindowId,
};

use crate::{
    harness::{Fixture, Scenario, Scene, SceneCtx},
    theme::{self, Appearance, Theme},
};

use super::{FrozenDisplayFrame, SelectionUi, hud::HudNav};

#[derive(Debug, Default)]
pub struct SelectionScene;

impl Scene for SelectionScene {
    fn name(&self) -> &str {
        "selection-ui"
    }

    fn setup(&self, ctx: &egui::Context) {
        theme::install_fonts(ctx);
    }

    fn ui(&self, ui: &mut egui::Ui, ctx: &SceneCtx<'_>) {
        let theme = if ctx.theme == egui::Theme::Dark {
            Theme::for_appearance(Appearance::Dark)
        } else {
            Theme::for_appearance(Appearance::Light)
        };
        theme::install_style(ui.ctx(), &theme);
        let mut selector = build_selector(ctx.fixture, ctx.seed);
        prime_fixture(ctx.fixture.scenario, &mut selector);
        let _ = selector.update(ui);
    }
}

fn build_selector(fixture: &Fixture, seed: u64) -> SelectionUi {
    let (options, displays, windows) = scenario_data(fixture);
    let frozen = displays
        .iter()
        .enumerate()
        .map(|(index, display)| {
            FrozenDisplayFrame::synthetic(
                display.clone(),
                seed ^ ((index as u64 + 1) * 0x9E37_79B9),
            )
        })
        .collect();
    SelectionUi::new(options, displays, frozen)
        .with_windows(windows)
        .with_capabilities(selector_capabilities())
}

fn scenario_data(fixture: &Fixture) -> (SelectionOptions, Vec<Display>, Vec<Window>) {
    match fixture.scenario {
        Scenario::SelectorIdle | Scenario::SelectorDragging => (
            capture_area_options(),
            vec![display(
                "main",
                "Built-in Display",
                LogicalRect::new(
                    LogicalPoint::new(0.0, 0.0),
                    LogicalSize::new(fixture.size_pt.0 as f64, fixture.size_pt.1 as f64),
                ),
                2.0,
                true,
            )],
            Vec::new(),
        ),
        Scenario::SelectorMagnifier => (
            capture_area_options().with_crosshair_mode(CrosshairMode::Always),
            vec![display(
                "main",
                "Built-in Display",
                LogicalRect::new(
                    LogicalPoint::new(0.0, 0.0),
                    LogicalSize::new(fixture.size_pt.0 as f64, fixture.size_pt.1 as f64),
                ),
                2.0,
                true,
            )],
            Vec::new(),
        ),
        Scenario::SelectorRemembered => (
            SelectionOptions {
                remembered: Some(LogicalRect::new(
                    LogicalPoint::new(120.0, 84.0),
                    LogicalSize::new(250.0, 160.0),
                )),
                ..capture_area_options()
            },
            vec![display(
                "main",
                "Built-in Display",
                LogicalRect::new(
                    LogicalPoint::new(0.0, 0.0),
                    LogicalSize::new(fixture.size_pt.0 as f64, fixture.size_pt.1 as f64),
                ),
                2.0,
                true,
            )],
            Vec::new(),
        ),
        Scenario::SelectorExact => (
            SelectionOptions {
                constraint: SizeConstraint::free()
                    .with_exact(LogicalSize::new(320.0, 200.0))
                    .expect("valid exact size"),
                ..capture_area_options()
            },
            vec![display(
                "main",
                "Built-in Display",
                LogicalRect::new(
                    LogicalPoint::new(0.0, 0.0),
                    LogicalSize::new(fixture.size_pt.0 as f64, fixture.size_pt.1 as f64),
                ),
                2.0,
                true,
            )],
            Vec::new(),
        ),
        Scenario::SelectorAspect => (
            SelectionOptions {
                constraint: SizeConstraint::free()
                    .with_aspect(AspectLock::ratio(16.0, 9.0).expect("valid ratio")),
                ..capture_area_options()
            },
            vec![display(
                "main",
                "Built-in Display",
                LogicalRect::new(
                    LogicalPoint::new(0.0, 0.0),
                    LogicalSize::new(fixture.size_pt.0 as f64, fixture.size_pt.1 as f64),
                ),
                2.0,
                true,
            )],
            Vec::new(),
        ),
        Scenario::SelectorAllInOne => (
            SelectionOptions {
                hud: true,
                freeze: true,
                ..SelectionOptions::region()
            },
            vec![display(
                "main",
                "Built-in Display",
                LogicalRect::new(
                    LogicalPoint::new(0.0, 0.0),
                    LogicalSize::new(fixture.size_pt.0 as f64, fixture.size_pt.1 as f64),
                ),
                2.0,
                true,
            )],
            vec![
                window(
                    "notes",
                    Some("Meeting notes"),
                    Some("Notes"),
                    LogicalRect::new(
                        LogicalPoint::new(96.0, 88.0),
                        LogicalSize::new(248.0, 166.0),
                    ),
                    "main",
                    true,
                ),
                window(
                    "browser",
                    Some("Rust reference"),
                    Some("Safari"),
                    LogicalRect::new(
                        LogicalPoint::new(212.0, 116.0),
                        LogicalSize::new(284.0, 196.0),
                    ),
                    "main",
                    true,
                ),
            ],
        ),
        Scenario::SelectorMixedDpi => (
            SelectionOptions {
                mode: SelectionMode::Window,
                hud: true,
                freeze: true,
                ..SelectionOptions::region()
            },
            vec![
                display(
                    "left",
                    "Retina",
                    LogicalRect::new(
                        LogicalPoint::new(0.0, 0.0),
                        LogicalSize::new(420.0, fixture.size_pt.1 as f64),
                    ),
                    2.0,
                    true,
                ),
                display(
                    "right",
                    "External",
                    LogicalRect::new(
                        LogicalPoint::new(420.0, 24.0),
                        LogicalSize::new(
                            fixture.size_pt.0 as f64 - 420.0,
                            fixture.size_pt.1 as f64 - 24.0,
                        ),
                    ),
                    1.25,
                    false,
                ),
            ],
            vec![
                window(
                    "terminal",
                    Some("Logs"),
                    Some("Terminal"),
                    LogicalRect::new(
                        LogicalPoint::new(462.0, 74.0),
                        LogicalSize::new(202.0, 146.0),
                    ),
                    "right",
                    true,
                ),
                window(
                    "editor",
                    Some("Scene fixture"),
                    Some("Editor"),
                    LogicalRect::new(
                        LogicalPoint::new(498.0, 112.0),
                        LogicalSize::new(180.0, 132.0),
                    ),
                    "right",
                    false,
                ),
            ],
        ),
        _ => (
            capture_area_options(),
            vec![display(
                "main",
                "Built-in Display",
                LogicalRect::new(
                    LogicalPoint::new(0.0, 0.0),
                    LogicalSize::new(fixture.size_pt.0 as f64, fixture.size_pt.1 as f64),
                ),
                2.0,
                true,
            )],
            Vec::new(),
        ),
    }
}

fn prime_fixture(scenario: Scenario, selector: &mut SelectionUi) {
    match scenario {
        Scenario::SelectorIdle => selector.state_mut().hover(LogicalPoint::new(220.0, 160.0)),
        Scenario::SelectorDragging => {
            selector
                .state_mut()
                .pointer_pressed(LogicalPoint::new(96.0, 92.0));
            selector
                .state_mut()
                .pointer_moved(LogicalPoint::new(342.0, 228.0));
        }
        Scenario::SelectorRemembered => selector.state_mut().hover(LogicalPoint::new(260.0, 180.0)),
        Scenario::SelectorExact => {
            selector
                .state_mut()
                .pointer_pressed(LogicalPoint::new(104.0, 120.0));
            selector
                .state_mut()
                .pointer_moved(LogicalPoint::new(188.0, 150.0));
            selector
                .state_mut()
                .pointer_released(LogicalPoint::new(188.0, 150.0));
        }
        Scenario::SelectorAspect => {
            selector
                .state_mut()
                .pointer_pressed(LogicalPoint::new(96.0, 88.0));
            selector
                .state_mut()
                .pointer_moved(LogicalPoint::new(356.0, 242.0));
        }
        Scenario::SelectorMagnifier => {
            selector
                .state_mut()
                .pointer_pressed(LogicalPoint::new(84.0, 78.0));
            selector
                .state_mut()
                .pointer_moved(LogicalPoint::new(276.0, 196.0));
            selector.state_mut().hover(LogicalPoint::new(314.0, 178.0));
        }
        Scenario::SelectorAllInOne => {
            let _ = selector.set_mode(SelectionMode::Display);
            selector.state_mut().hover(LogicalPoint::new(212.0, 144.0));
            selector.hud_mut().navigate(HudNav::Next);
        }
        Scenario::SelectorMixedDpi => selector.state_mut().hover(LogicalPoint::new(520.0, 164.0)),
        _ => {}
    }
}

fn display(id: &str, name: &str, bounds: LogicalRect, scale: f64, is_primary: bool) -> Display {
    Display {
        id: DisplayId(id.to_owned()),
        name: name.to_owned(),
        bounds,
        work_area: bounds,
        scale: ScaleFactor::new(scale),
        is_primary,
    }
}

fn capture_area_options() -> SelectionOptions {
    SelectionOptions {
        hud: false,
        freeze: true,
        ..SelectionOptions::region()
    }
}

fn selector_capabilities() -> SelectionCapabilities {
    SelectionCapabilities::CLIENT_OVERLAY
}

fn window(
    id: &str,
    title: Option<&str>,
    application: Option<&str>,
    bounds: LogicalRect,
    display: &str,
    is_visible: bool,
) -> Window {
    Window {
        id: WindowId(id.to_owned()),
        title: title.map(str::to_owned),
        application: application.map(str::to_owned),
        bounds,
        display: DisplayId(display.to_owned()),
        is_visible,
    }
}
