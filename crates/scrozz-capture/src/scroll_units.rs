//! Pure conversions shared by the native scroll-input drivers.

use scrozz_core::{LogicalPoint, LogicalRect, ScaleFactor, ScrollAxis};

/// Win32's conventional wheel delta.
const POINTS_PER_NOTCH: f64 = 120.0;

pub(crate) fn macos_deltas(axis: ScrollAxis, amount: f64) -> (i32, i32) {
    let delta = rounded_nonzero(amount);
    match axis {
        // Quartz reports both wheel axes from the content's perspective:
        // negative Y moves the viewport down and negative X moves it right.
        ScrollAxis::Vertical => (-delta, 0),
        ScrollAxis::Horizontal => (0, -delta),
    }
}

pub(crate) fn windows_delta(axis: ScrollAxis, amount: f64) -> i32 {
    let delta = rounded_nonzero(amount).signum() * POINTS_PER_NOTCH as i32;
    match axis {
        // A positive Win32 vertical delta means wheel-up, the inverse of the
        // core contract. HWHEEL is different: positive means tilt/scroll right.
        ScrollAxis::Vertical => -delta,
        ScrollAxis::Horizontal => delta,
    }
}

pub(crate) fn x11_button_and_notches(axis: ScrollAxis, amount: f64) -> (u8, u32) {
    let button = match (axis, amount.is_sign_positive()) {
        (ScrollAxis::Vertical, true) => 5,    // wheel down
        (ScrollAxis::Vertical, false) => 4,   // wheel up
        (ScrollAxis::Horizontal, true) => 7,  // wheel right
        (ScrollAxis::Horizontal, false) => 6, // wheel left
    };

    // XTEST exposes only discrete core-pointer buttons. A requested viewport
    // distance is not a safe notch count: applications apply their own line and
    // page settings, so send one conservative detent and adapt from measured
    // visual movement.
    (button, 1)
}

pub(crate) fn finite_point(point: LogicalPoint) -> bool {
    point.x.is_finite() && point.y.is_finite()
}

pub(crate) fn logical_to_device_point(
    point: LogicalPoint,
    bounds: LogicalRect,
    scale: ScaleFactor,
) -> Option<(i32, i32)> {
    if !finite_point(point)
        || point.x < bounds.origin.x
        || point.y < bounds.origin.y
        || point.x >= bounds.origin.x + bounds.size.width
        || point.y >= bounds.origin.y + bounds.size.height
    {
        return None;
    }

    Some((
        rounded_coordinate(point.x * scale.get())?,
        rounded_coordinate(point.y * scale.get())?,
    ))
}

fn rounded_nonzero(value: f64) -> i32 {
    if !value.is_finite() || value == 0.0 {
        return 0;
    }
    let limit = f64::from(i32::MAX);
    let rounded = value.clamp(-limit, limit).round() as i32;
    if rounded == 0 {
        if value.is_sign_negative() { -1 } else { 1 }
    } else {
        rounded
    }
}

fn rounded_coordinate(value: f64) -> Option<i32> {
    if !value.is_finite() || value < f64::from(i32::MIN) || value > f64::from(i32::MAX) {
        return None;
    }
    Some(value.round() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{LogicalSize, ScrollGesture};

    #[test]
    fn native_axis_signs_match_the_core_contract() {
        assert_eq!(macos_deltas(ScrollAxis::Vertical, 120.0), (-120, 0));
        assert_eq!(macos_deltas(ScrollAxis::Horizontal, 120.0), (0, -120));
        assert_eq!(windows_delta(ScrollAxis::Vertical, 120.0), -120);
        assert_eq!(windows_delta(ScrollAxis::Horizontal, 120.0), 120);
    }

    #[test]
    fn a_subpoint_nonzero_gesture_is_not_lost_to_integer_rounding() {
        assert_eq!(windows_delta(ScrollAxis::Vertical, 0.1), -120);
        assert_eq!(macos_deltas(ScrollAxis::Horizontal, -0.1), (0, 1));
    }

    #[test]
    fn x11_uses_conservative_bounded_notches_and_directional_buttons() {
        assert_eq!(x11_button_and_notches(ScrollAxis::Vertical, 1.0), (5, 1));
        assert_eq!(x11_button_and_notches(ScrollAxis::Vertical, -240.0), (4, 1));
        assert_eq!(
            x11_button_and_notches(ScrollAxis::Horizontal, 360.0),
            (7, 1)
        );
        assert_eq!(
            x11_button_and_notches(ScrollAxis::Horizontal, -100_000.0),
            (6, 1)
        );
    }

    #[test]
    fn logical_points_map_through_the_selected_monitor_scale() {
        let bounds = LogicalRect::new(
            LogicalPoint::new(-1280.0, 0.0),
            LogicalSize::new(1280.0, 720.0),
        );
        assert_eq!(
            logical_to_device_point(
                LogicalPoint::new(-1000.0, 200.0),
                bounds,
                ScaleFactor::new(1.5)
            ),
            Some((-1500, 300))
        );
        assert_eq!(
            logical_to_device_point(LogicalPoint::new(1.0, 200.0), bounds, ScaleFactor::new(1.5)),
            None
        );
    }

    #[test]
    fn noop_values_stay_zero_in_native_integer_units() {
        for amount in [0.0, f64::NAN, f64::INFINITY] {
            let gesture = ScrollGesture::down(LogicalPoint::new(0.0, 0.0), amount);
            assert!(gesture.is_noop());
            assert_eq!(macos_deltas(gesture.axis, gesture.amount), (0, 0));
            assert_eq!(windows_delta(gesture.axis, gesture.amount), 0);
        }
    }
}
