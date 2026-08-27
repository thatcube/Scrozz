//! Tests for the logical/physical pixel boundary.
//!
//! These cover the arithmetic that silently corrupts captures when it is wrong:
//! Retina scaling, fractional scaling, and selections dragged in any direction.

use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize, PhysicalRect, ScaleFactor};

fn rect(x: f64, y: f64, w: f64, h: f64) -> LogicalRect {
    LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(w, h))
}

#[test]
fn retina_doubles_every_dimension() {
    let physical = rect(0.0, 0.0, 1920.0, 1080.0).to_physical(ScaleFactor::new(2.0));
    assert_eq!(physical.pixel_width(), 3840);
    assert_eq!(physical.pixel_height(), 2160);
}

#[test]
fn identity_scale_is_lossless() {
    let logical = rect(10.0, 20.0, 300.0, 400.0);
    let physical = logical.to_physical(ScaleFactor::IDENTITY);
    assert_eq!(physical.pixel_width(), 300);
    assert_eq!(physical.pixel_height(), 400);
    assert_eq!(physical.to_logical(ScaleFactor::IDENTITY), logical);
}

#[test]
fn fractional_scaling_rounds_outward_never_inward() {
    // Windows and Wayland both permit 1.5x. A selection at a fractional scale
    // must never lose an edge pixel the user deliberately included: cropping one
    // pixel short is visible, including one extra is not.
    let physical = rect(10.5, 10.5, 100.0, 100.0).to_physical(ScaleFactor::new(1.5));

    assert_eq!(physical.origin.x, 15.0, "origin floors: 10.5 * 1.5 = 15.75 -> 15");
    assert_eq!(physical.origin.y, 15.0, "origin floors");
    // Far edge is 110.5 * 1.5 = 165.75, which must reach 166 rather than
    // truncate to 165. Width is therefore 166 - 15 = 151, one pixel wider than
    // the naive 100 * 1.5 = 150.
    assert_eq!(physical.pixel_width(), 151);
    assert_eq!(physical.pixel_height(), 151);
}

#[test]
fn outward_rounding_never_shrinks_the_selection() {
    // Property: across a spread of awkward scales and sub-pixel origins, the
    // physical rectangle always covers at least the requested logical area.
    for scale in [1.0, 1.25, 1.5, 1.75, 2.0, 3.0] {
        for offset in [0.0, 0.1, 0.5, 0.9] {
            let logical = rect(offset, offset, 100.0, 50.0);
            let physical = logical.to_physical(ScaleFactor::new(scale));
            let covered = physical.to_logical(ScaleFactor::new(scale));

            assert!(
                covered.origin.x <= logical.origin.x + f64::EPSILON,
                "scale {scale} offset {offset}: left edge lost"
            );
            assert!(
                covered.origin.x + covered.size.width
                    >= logical.origin.x + logical.size.width - f64::EPSILON,
                "scale {scale} offset {offset}: right edge lost"
            );
        }
    }
}

#[test]
fn selection_dragged_any_direction_yields_the_same_rect() {
    // A drag-to-select gesture runs in all four directions and every one of them
    // must produce a valid, identical rectangle.
    let a = LogicalPoint::new(100.0, 100.0);
    let b = LogicalPoint::new(300.0, 250.0);

    let down_right = LogicalRect::from_corners(a, b);
    let up_left = LogicalRect::from_corners(b, a);
    let down_left = LogicalRect::from_corners(
        LogicalPoint::new(300.0, 100.0),
        LogicalPoint::new(100.0, 250.0),
    );

    assert_eq!(down_right, up_left);
    assert_eq!(down_right, down_left);
    assert_eq!(down_right.origin, a);
    assert_eq!(down_right.size, LogicalSize::new(200.0, 150.0));
}

#[test]
fn degenerate_sizes_clamp_rather_than_invert() {
    let zero_drag =
        LogicalRect::from_corners(LogicalPoint::new(50.0, 50.0), LogicalPoint::new(50.0, 50.0));
    assert!(zero_drag.is_empty());

    assert!(LogicalSize::new(-10.0, -10.0).is_empty());
}

#[test]
fn physical_round_trips_back_to_logical() {
    let physical = PhysicalRect::new(
        scrozz_core::PhysicalPoint::new(200.0, 400.0),
        scrozz_core::PhysicalSize::new(800.0, 600.0),
    );
    let logical = physical.to_logical(ScaleFactor::new(2.0));

    assert_eq!(logical.origin.x, 100.0);
    assert_eq!(logical.size.width, 400.0);
}

#[test]
#[should_panic(expected = "must be finite and positive")]
fn zero_scale_is_rejected_loudly() {
    // Silently clamping would push a wrong capture size downstream, where it is
    // far harder to trace back to its cause.
    let _ = ScaleFactor::new(0.0);
}

#[test]
#[should_panic(expected = "must be finite and positive")]
fn nan_scale_is_rejected_loudly() {
    let _ = ScaleFactor::new(f64::NAN);
}
