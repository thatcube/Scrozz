//! Selector-focused regression tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scrozz_core::selection::{
    AspectLock, SelectionCapabilities, SelectionMode, SelectionOptions, SizeConstraint,
};
use scrozz_core::{
    CaptureTarget, Display, DisplayId, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor, Window,
    WindowId,
};
use scrozz_ui::{AxisDirection, DisplayLayout, SelectionState};

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

fn state(options: SelectionOptions) -> SelectionState {
    SelectionState::new(
        options,
        DisplayLayout::new(vec![display("main", 0.0, 0.0, 400.0, 300.0, 2.0, true)]),
        SelectionCapabilities {
            window_picking: false,
            ..SelectionCapabilities::CLIENT_OVERLAY
        },
    )
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

fn state_with_windows(
    options: SelectionOptions,
    displays: Vec<Display>,
    windows: Vec<Window>,
) -> SelectionState {
    SelectionState::new_with_windows(
        options,
        DisplayLayout::new(displays),
        SelectionCapabilities::CLIENT_OVERLAY,
        windows,
    )
}

#[test]
fn dragging_up_and_left_still_produces_a_valid_rect() {
    let mut state = state(SelectionOptions::region());

    state.pointer_pressed(LogicalPoint::new(240.0, 180.0));
    state.pointer_moved(LogicalPoint::new(120.0, 90.0));
    state.pointer_released(LogicalPoint::new(120.0, 90.0));

    assert_eq!(
        state.region().unwrap(),
        LogicalRect::new(
            LogicalPoint::new(120.0, 90.0),
            LogicalSize::new(120.0, 90.0)
        )
    );
}

#[test]
fn remembered_region_can_be_moved() {
    let mut state = state(SelectionOptions {
        remembered: Some(LogicalRect::new(
            LogicalPoint::new(50.0, 60.0),
            LogicalSize::new(100.0, 80.0),
        )),
        ..SelectionOptions::region()
    });

    state.pointer_pressed(LogicalPoint::new(70.0, 80.0));
    state.pointer_moved(LogicalPoint::new(150.0, 170.0));
    state.pointer_released(LogicalPoint::new(150.0, 170.0));

    assert_eq!(
        state.region().unwrap().origin,
        LogicalPoint::new(130.0, 150.0)
    );
}

#[test]
fn aspect_lock_holds_the_ratio() {
    let mut state = state(SelectionOptions {
        constraint: SizeConstraint::free().with_aspect(AspectLock::ratio(16.0, 9.0).unwrap()),
        ..SelectionOptions::region()
    });

    state.pointer_pressed(LogicalPoint::new(20.0, 20.0));
    state.pointer_moved(LogicalPoint::new(220.0, 200.0));
    state.pointer_released(LogicalPoint::new(220.0, 200.0));

    let rect = state.region().unwrap();
    let ratio = rect.size.width / rect.size.height;
    assert!((ratio - 16.0 / 9.0).abs() < 1e-9, "ratio was {ratio}");
}

#[test]
fn exact_size_drag_only_positions_the_region() {
    let mut state = state(SelectionOptions {
        constraint: SizeConstraint::free()
            .with_exact(LogicalSize::new(120.0, 80.0))
            .unwrap(),
        ..SelectionOptions::region()
    });

    state.pointer_pressed(LogicalPoint::new(10.0, 10.0));
    state.pointer_moved(LogicalPoint::new(60.0, 70.0));
    state.pointer_released(LogicalPoint::new(60.0, 70.0));

    assert_eq!(
        state.region().unwrap(),
        LogicalRect::new(LogicalPoint::new(60.0, 70.0), LogicalSize::new(120.0, 80.0))
    );
}

#[test]
fn keyboard_nudge_and_resize_use_one_or_ten_points() {
    let mut state = state(SelectionOptions {
        remembered: Some(LogicalRect::new(
            LogicalPoint::new(50.0, 60.0),
            LogicalSize::new(100.0, 80.0),
        )),
        ..SelectionOptions::region()
    });

    state.keyboard_nudge(AxisDirection::Right, false);
    state.keyboard_nudge(AxisDirection::Down, true);
    state.keyboard_resize(AxisDirection::Right, false);
    state.keyboard_resize(AxisDirection::Down, true);

    let rect = state.region().unwrap();
    assert_eq!(rect.origin, LogicalPoint::new(51.0, 70.0));
    assert_eq!(rect.size, LogicalSize::new(101.0, 90.0));
}

#[test]
fn enter_commits_display_mode_and_escape_announces_cancellation() {
    let mut state = state(SelectionOptions::for_mode(SelectionMode::Display));
    state.hover(LogicalPoint::new(120.0, 80.0));
    let outcome = state.commit().unwrap();

    assert_eq!(outcome.mode, SelectionMode::Display);
    assert!(state.cancel());
    assert_eq!(state.take_announcement().unwrap().0, "Selection cancelled");
}

#[test]
fn frontmost_visible_window_wins_hit_testing() {
    let mut state = state_with_windows(
        SelectionOptions::for_mode(SelectionMode::Window),
        vec![display("main", 0.0, 0.0, 500.0, 360.0, 2.0, true)],
        vec![
            window(
                "front",
                Some("Inbox"),
                Some("Mail"),
                LogicalRect::new(
                    LogicalPoint::new(80.0, 60.0),
                    LogicalSize::new(220.0, 180.0),
                ),
                "main",
                true,
            ),
            window(
                "hidden-top",
                Some("Hidden"),
                Some("Overlay"),
                LogicalRect::new(
                    LogicalPoint::new(96.0, 76.0),
                    LogicalSize::new(180.0, 120.0),
                ),
                "main",
                false,
            ),
            window(
                "back",
                Some("Browser"),
                Some("Safari"),
                LogicalRect::new(
                    LogicalPoint::new(100.0, 80.0),
                    LogicalSize::new(260.0, 200.0),
                ),
                "main",
                true,
            ),
        ],
    );

    state.hover(LogicalPoint::new(140.0, 120.0));
    assert_eq!(
        state.take_announcement().unwrap().0,
        "Window target Inbox — Mail"
    );

    let outcome = state.commit().unwrap();
    assert_eq!(outcome.mode, SelectionMode::Window);
    assert_eq!(
        outcome.target,
        CaptureTarget::Window(WindowId("front".to_owned()))
    );
    assert_eq!(outcome.scale, ScaleFactor::new(2.0));
}

#[test]
fn window_commit_uses_the_owning_display_scale() {
    let mut state = state_with_windows(
        SelectionOptions::for_mode(SelectionMode::Window),
        vec![
            display("left", 0.0, 0.0, 420.0, 320.0, 2.0, true),
            display("right", 420.0, 24.0, 360.0, 296.0, 1.25, false),
        ],
        vec![window(
            "terminal",
            Some("Logs"),
            Some("Terminal"),
            LogicalRect::new(
                LogicalPoint::new(468.0, 72.0),
                LogicalSize::new(180.0, 140.0),
            ),
            "right",
            true,
        )],
    );

    state.hover(LogicalPoint::new(520.0, 140.0));
    let outcome = state.commit().unwrap();
    assert_eq!(outcome.display, Some(DisplayId("right".to_owned())));
    assert_eq!(outcome.scale, ScaleFactor::new(1.25));
    assert_eq!(
        state.take_announcement().unwrap().0,
        "Window selected Logs — Terminal"
    );
}

#[test]
fn window_mode_enter_selects_the_hovered_window() {
    let mut state = state_with_windows(
        SelectionOptions::for_mode(SelectionMode::Window),
        vec![display("main", 0.0, 0.0, 400.0, 300.0, 2.0, true)],
        vec![window(
            "editor",
            Some("SelectionScene"),
            Some("Editor"),
            LogicalRect::new(
                LogicalPoint::new(60.0, 48.0),
                LogicalSize::new(240.0, 170.0),
            ),
            "main",
            true,
        )],
    );

    assert!(state.set_mode(SelectionMode::Window));
    assert_eq!(
        state.take_announcement().unwrap().0,
        "Window mode. Point at a window to choose it"
    );
    state.hover(LogicalPoint::new(144.0, 120.0));

    let outcome = state.commit().unwrap();
    assert_eq!(
        outcome.target,
        CaptureTarget::Window(WindowId("editor".to_owned()))
    );
}

#[test]
fn too_small_a_drag_is_rejected() {
    let mut state = state(SelectionOptions::region());

    state.pointer_pressed(LogicalPoint::new(10.0, 10.0));
    state.pointer_moved(LogicalPoint::new(14.0, 15.0));
    state.pointer_released(LogicalPoint::new(14.0, 15.0));

    assert!(state.region().is_none());
    assert_eq!(
        state.take_announcement().unwrap().0,
        "Selection too small; minimum is 8 by 8 points"
    );
}

#[test]
fn crossing_a_dpi_boundary_keeps_the_gesture_display_and_scale() {
    let mut state = state_with_windows(
        SelectionOptions::region(),
        vec![
            display("left", 0.0, 0.0, 400.0, 300.0, 2.0, true),
            display("right", 400.0, 0.0, 320.0, 300.0, 1.25, false),
        ],
        Vec::new(),
    );

    state.pointer_pressed(LogicalPoint::new(350.0, 80.0));
    state.pointer_moved(LogicalPoint::new(520.0, 180.0));
    state.pointer_released(LogicalPoint::new(520.0, 180.0));

    let outcome = state.commit().expect("the clamped region should commit");
    assert_eq!(outcome.display, Some(DisplayId("left".to_owned())));
    assert_eq!(outcome.scale, ScaleFactor::new(2.0));
    assert_eq!(
        outcome.rect.unwrap(),
        LogicalRect::new(
            LogicalPoint::new(350.0, 80.0),
            LogicalSize::new(50.0, 100.0)
        )
    );
}

#[test]
fn an_exact_size_that_does_not_fit_is_rejected_instead_of_shrunk() {
    let mut state = state(SelectionOptions {
        constraint: SizeConstraint::free()
            .with_exact(LogicalSize::new(480.0, 270.0))
            .unwrap(),
        ..SelectionOptions::region()
    });

    state.pointer_pressed(LogicalPoint::new(20.0, 20.0));

    assert!(state.region().is_none());
    assert!(
        state
            .take_announcement()
            .expect("the refusal should be announced")
            .0
            .contains("does not fit")
    );
    assert!(state.commit().is_none());
}

#[test]
fn exact_size_regions_move_but_do_not_resize() {
    let exact = LogicalSize::new(120.0, 80.0);
    let mut state = state(SelectionOptions {
        remembered: Some(LogicalRect::new(LogicalPoint::new(40.0, 50.0), exact)),
        constraint: SizeConstraint::free().with_exact(exact).unwrap(),
        ..SelectionOptions::region()
    });

    state.keyboard_resize(AxisDirection::Right, true);
    assert_eq!(state.region().unwrap().size, exact);
    assert_eq!(state.handle_at_point(LogicalPoint::new(160.0, 90.0)), None);

    state.keyboard_nudge(AxisDirection::Right, true);
    assert_eq!(
        state.region().unwrap().origin,
        LogicalPoint::new(50.0, 50.0)
    );
    assert!(state.commit().is_some());
}

#[test]
fn remembered_regions_are_normalized_to_the_requested_exact_size() {
    let exact = LogicalSize::new(120.0, 60.0);
    let mut state = state(SelectionOptions {
        remembered: Some(LogicalRect::new(
            LogicalPoint::new(320.0, 250.0),
            LogicalSize::new(40.0, 30.0),
        )),
        constraint: SizeConstraint::free().with_exact(exact).unwrap(),
        ..SelectionOptions::region()
    });

    let rect = state
        .region()
        .expect("the remembered region should be resized");
    assert_eq!(rect.size, exact);
    assert_eq!(rect.origin, LogicalPoint::new(280.0, 240.0));
    assert!(state.commit().is_some());
}

#[test]
fn aspect_locked_regions_keep_their_ratio_at_display_edges() {
    let ratio = AspectLock::ratio(16.0, 9.0).unwrap();
    let mut state = state(SelectionOptions {
        constraint: SizeConstraint::free().with_aspect(ratio),
        ..SelectionOptions::region()
    });

    state.pointer_pressed(LogicalPoint::new(0.0, 0.0));
    state.pointer_moved(LogicalPoint::new(400.0, 300.0));
    state.pointer_released(LogicalPoint::new(400.0, 300.0));

    let rect = state.region().unwrap();
    assert!((rect.size.width / rect.size.height - 16.0 / 9.0).abs() < 1e-9);
    assert_eq!(rect.size, LogicalSize::new(400.0, 225.0));
    assert!(state.commit().is_some());
}

#[test]
fn explicit_viewport_ownership_disambiguates_overlapping_logical_displays() {
    let displays = vec![
        display("primary", 0.0, 0.0, 1920.0, 1080.0, 1.0, true),
        display("hidpi", 1280.0, 0.0, 2560.0, 1440.0, 1.5, false),
    ];
    let mut state = state_with_windows(SelectionOptions::region(), displays, Vec::new());
    let hidpi = DisplayId("hidpi".to_owned());

    state.pointer_pressed_on(&hidpi, LogicalPoint::new(1400.0, 100.0));
    state.pointer_moved_on(&hidpi, LogicalPoint::new(1500.0, 180.0));
    state.pointer_released_on(&hidpi, LogicalPoint::new(1500.0, 180.0));
    state.keyboard_nudge(AxisDirection::Right, false);

    let outcome = state.commit().expect("the region should commit");
    assert_eq!(outcome.display, Some(hidpi));
    assert_eq!(outcome.scale, ScaleFactor::new(1.5));
    assert_eq!(
        outcome.rect.unwrap().origin,
        LogicalPoint::new(1401.0, 100.0)
    );
}

#[test]
fn remembered_display_identity_survives_ambiguous_mixed_dpi_geometry() {
    let remembered = LogicalRect::new(
        LogicalPoint::new(1400.0, 100.0),
        LogicalSize::new(100.0, 80.0),
    );
    let hidpi = DisplayId("hidpi".to_owned());
    let state = state_with_windows(
        SelectionOptions {
            remembered: Some(remembered),
            remembered_display: Some(hidpi.clone()),
            reuse_immediately: true,
            ..SelectionOptions::region()
        },
        vec![
            display("primary", 0.0, 0.0, 1920.0, 1080.0, 1.0, true),
            display("hidpi", 1280.0, 0.0, 2560.0, 1440.0, 1.5, false),
        ],
        Vec::new(),
    );

    let outcome = state
        .immediate_reuse()
        .expect("the remembered region should commit");
    assert_eq!(outcome.display, Some(hidpi));
    assert_eq!(outcome.scale, ScaleFactor::new(1.5));
    assert_eq!(outcome.rect, Some(remembered));
}

#[test]
fn region_ownership_survives_hovering_an_overlapping_display_in_another_mode() {
    let displays = vec![
        display("primary", 0.0, 0.0, 1920.0, 1080.0, 1.0, true),
        display("hidpi", 1280.0, 0.0, 2560.0, 1440.0, 1.5, false),
    ];
    let primary = DisplayId("primary".to_owned());
    let hidpi = DisplayId("hidpi".to_owned());
    let mut state = state_with_windows(SelectionOptions::region(), displays, Vec::new());

    state.pointer_pressed_on(&hidpi, LogicalPoint::new(1400.0, 100.0));
    state.pointer_moved_on(&hidpi, LogicalPoint::new(1500.0, 180.0));
    state.pointer_released_on(&hidpi, LogicalPoint::new(1500.0, 180.0));
    assert!(state.set_mode(SelectionMode::Display));
    state.hover_on(&primary, LogicalPoint::new(1450.0, 130.0));
    assert!(state.set_mode(SelectionMode::Region));

    let outcome = state.commit().expect("the existing region should commit");
    assert_eq!(outcome.display, Some(hidpi));
    assert_eq!(outcome.scale, ScaleFactor::new(1.5));
}

#[test]
fn an_empty_overlapping_viewport_cannot_select_another_displays_window() {
    let primary = DisplayId("primary".to_owned());
    let hidpi = DisplayId("hidpi".to_owned());
    let point = LogicalPoint::new(1400.0, 100.0);
    let mut state = state_with_windows(
        SelectionOptions::for_mode(SelectionMode::Window),
        vec![
            display("primary", 0.0, 0.0, 1920.0, 1080.0, 1.0, true),
            display("hidpi", 1280.0, 0.0, 2560.0, 1440.0, 1.5, false),
        ],
        vec![window(
            "primary-window",
            Some("Primary"),
            Some("Editor"),
            LogicalRect::new(point, LogicalSize::new(200.0, 160.0)),
            &primary.0,
            true,
        )],
    );

    state.hover_on(&hidpi, LogicalPoint::new(1450.0, 130.0));

    assert!(state.commit().is_none());
}

#[test]
fn resizing_at_a_display_edge_keeps_the_opposite_edge_fixed() {
    let mut state = state(SelectionOptions {
        remembered: Some(LogicalRect::new(
            LogicalPoint::new(100.0, 80.0),
            LogicalSize::new(100.0, 80.0),
        )),
        ..SelectionOptions::region()
    });

    state.pointer_pressed(LogicalPoint::new(200.0, 120.0));
    state.pointer_moved(LogicalPoint::new(500.0, 120.0));
    state.pointer_released(LogicalPoint::new(500.0, 120.0));

    assert_eq!(
        state.region().unwrap(),
        LogicalRect::new(
            LogicalPoint::new(100.0, 80.0),
            LogicalSize::new(300.0, 80.0)
        )
    );
}

#[test]
fn minimum_corner_resize_keeps_the_opposite_corner_fixed() {
    let mut state = state(SelectionOptions {
        remembered: Some(LogicalRect::new(
            LogicalPoint::new(50.0, 50.0),
            LogicalSize::new(100.0, 80.0),
        )),
        ..SelectionOptions::region()
    });

    state.pointer_pressed(LogicalPoint::new(50.0, 50.0));
    state.pointer_moved(LogicalPoint::new(149.0, 129.0));
    state.pointer_released(LogicalPoint::new(149.0, 129.0));

    assert_eq!(
        state.region().unwrap(),
        LogicalRect::new(LogicalPoint::new(142.0, 122.0), LogicalSize::new(8.0, 8.0))
    );
}

#[test]
fn corner_resize_does_not_reverse_after_crossing_the_fixed_corner() {
    let mut state = state(SelectionOptions {
        remembered: Some(LogicalRect::new(
            LogicalPoint::new(50.0, 50.0),
            LogicalSize::new(100.0, 80.0),
        )),
        ..SelectionOptions::region()
    });

    state.pointer_pressed(LogicalPoint::new(50.0, 50.0));
    state.pointer_moved(LogicalPoint::new(250.0, 250.0));
    state.pointer_released(LogicalPoint::new(250.0, 250.0));

    assert_eq!(
        state.region().unwrap(),
        LogicalRect::new(LogicalPoint::new(142.0, 122.0), LogicalSize::new(8.0, 8.0))
    );
}

#[test]
fn aspect_locked_side_resize_saturates_at_the_nearest_display_edge() {
    let mut state = state(SelectionOptions {
        remembered: Some(LogicalRect::new(
            LogicalPoint::new(100.0, 10.0),
            LogicalSize::new(100.0, 100.0),
        )),
        constraint: SizeConstraint::free().with_aspect(AspectLock::ratio(1.0, 1.0).unwrap()),
        ..SelectionOptions::region()
    });

    state.pointer_pressed(LogicalPoint::new(200.0, 60.0));
    state.pointer_moved(LogicalPoint::new(400.0, 60.0));
    state.pointer_released(LogicalPoint::new(400.0, 60.0));

    assert_eq!(
        state.region().unwrap(),
        LogicalRect::new(
            LogicalPoint::new(100.0, 0.0),
            LogicalSize::new(120.0, 120.0)
        )
    );
}
