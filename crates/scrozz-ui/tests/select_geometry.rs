//! Selector-focused regression tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scrozz_core::{Display, DisplayId, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor};
use scrozz_ui::DisplayLayout;
use scrozz_ui::select::geom;

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

#[test]
fn point_hits_the_owning_display() {
    let layout = DisplayLayout::new(vec![
        display("left", 0.0, 0.0, 400.0, 300.0, 2.0, true),
        display("right", 400.0, 0.0, 500.0, 300.0, 1.25, false),
    ]);

    assert_eq!(
        layout
            .display_at_point(LogicalPoint::new(200.0, 120.0))
            .unwrap()
            .id
            .0,
        "left"
    );
    assert_eq!(
        layout
            .display_at_point(LogicalPoint::new(640.0, 120.0))
            .unwrap()
            .id
            .0,
        "right"
    );
}

#[test]
fn cross_display_rect_has_no_single_owner() {
    let layout = DisplayLayout::new(vec![
        display("left", 0.0, 0.0, 400.0, 300.0, 2.0, true),
        display("right", 400.0, 0.0, 500.0, 300.0, 1.25, false),
    ]);

    let spanning = LogicalRect::new(
        LogicalPoint::new(350.0, 40.0),
        LogicalSize::new(120.0, 80.0),
    );
    assert!(layout.display_owning_rect(spanning).is_none());
}

#[test]
fn rect_clamping_stays_within_one_display() {
    let layout = DisplayLayout::new(vec![display("main", 0.0, 0.0, 300.0, 200.0, 2.0, true)]);
    let rect = LogicalRect::new(
        LogicalPoint::new(260.0, 170.0),
        LogicalSize::new(80.0, 50.0),
    );

    let clamped = layout
        .clamp_rect_to_display(&DisplayId("main".to_owned()), rect)
        .unwrap();
    assert_eq!(clamped.origin, LogicalPoint::new(220.0, 150.0));
    assert_eq!(clamped.size, LogicalSize::new(80.0, 50.0));
}

#[test]
fn physical_conversion_uses_the_owning_display_scale() {
    let retina = display("retina", 100.0, 50.0, 400.0, 300.0, 2.0, true);
    let external = display("external", 500.0, 50.0, 300.0, 300.0, 1.25, false);

    assert_eq!(
        geom::logical_to_local_physical(&retina, LogicalPoint::new(150.5, 90.0)),
        (101, 80)
    );
    assert_eq!(
        geom::logical_to_local_physical(&external, LogicalPoint::new(540.0, 90.0)),
        (50, 50)
    );
}
