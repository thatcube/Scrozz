//! Pure conversions shared by the native scroll-input drivers.

use scrozz_core::{LogicalPoint, LogicalRect, ScaleFactor, ScrollAxis};

/// Win32's conventional wheel delta, also used as the cross-platform estimate
/// for one discrete X11 wheel notch.
const POINTS_PER_NOTCH: f64 = 120.0;
const MAX_X11_NOTCHES: u32 = 32;

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
    let delta = rounded_nonzero(amount);
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

    // XTEST exposes only discrete core-pointer buttons. Approximate 120 logical
    // points as one notch, always preserve a non-zero request, and cap a single
    // gesture so malformed input cannot flood the X server.
    let notches = (amount.abs() / POINTS_PER_NOTCH)
        .round()
        .clamp(1.0, f64::from(MAX_X11_NOTCHES)) as u32;
    (button, notches)
}

pub(crate) fn portal_deltas(axis: ScrollAxis, amount: f64) -> (f64, f64) {
    match axis {
        // RemoteDesktop follows Wayland/libinput axis direction: positive Y is
        // wheel-down and positive X is wheel-right, matching the core contract.
        ScrollAxis::Vertical => (0.0, amount),
        ScrollAxis::Horizontal => (amount, 0.0),
    }
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

pub(crate) fn normalized_absolute_coordinate(value: i32, origin: i32, extent: i32) -> Option<i32> {
    if extent <= 1 {
        return None;
    }
    let maximum = i64::from(extent - 1);
    let offset = (i64::from(value) - i64::from(origin)).clamp(0, maximum);
    i32::try_from(offset * 65_535 / maximum).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PortalStreamGeometry {
    pub node_id: u32,
    pub position: (i32, i32),
    pub size: (i32, i32),
}

pub(crate) fn portal_stream_at(
    streams: &[PortalStreamGeometry],
    point: LogicalPoint,
) -> Option<(u32, f64, f64)> {
    if !finite_point(point) {
        return None;
    }

    streams.iter().find_map(|stream| {
        let (x, y) = stream.position;
        let (width, height) = stream.size;
        if width <= 0
            || height <= 0
            || point.x < f64::from(x)
            || point.y < f64::from(y)
            || point.x >= f64::from(x) + f64::from(width)
            || point.y >= f64::from(y) + f64::from(height)
        {
            return None;
        }
        Some((
            stream.node_id,
            point.x - f64::from(x),
            point.y - f64::from(y),
        ))
    })
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
        assert_eq!(portal_deltas(ScrollAxis::Vertical, 120.0), (0.0, 120.0));
        assert_eq!(portal_deltas(ScrollAxis::Horizontal, 120.0), (120.0, 0.0));
    }

    #[test]
    fn a_subpoint_nonzero_gesture_is_not_lost_to_integer_rounding() {
        assert_eq!(windows_delta(ScrollAxis::Vertical, 0.1), -1);
        assert_eq!(macos_deltas(ScrollAxis::Horizontal, -0.1), (0, 1));
    }

    #[test]
    fn x11_uses_conservative_bounded_notches_and_directional_buttons() {
        assert_eq!(x11_button_and_notches(ScrollAxis::Vertical, 1.0), (5, 1));
        assert_eq!(x11_button_and_notches(ScrollAxis::Vertical, -240.0), (4, 2));
        assert_eq!(
            x11_button_and_notches(ScrollAxis::Horizontal, 360.0),
            (7, 3)
        );
        assert_eq!(
            x11_button_and_notches(ScrollAxis::Horizontal, -100_000.0),
            (6, MAX_X11_NOTCHES)
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
    fn windows_absolute_coordinates_cover_a_negative_origin_desktop() {
        assert_eq!(normalized_absolute_coordinate(-1920, -1920, 3840), Some(0));
        assert_eq!(
            normalized_absolute_coordinate(1919, -1920, 3840),
            Some(65_535)
        );
        assert_eq!(normalized_absolute_coordinate(0, -1920, 3840), Some(32_776));
    }

    #[test]
    fn portal_stream_selection_rebases_global_points() {
        let streams = [
            PortalStreamGeometry {
                node_id: 10,
                position: (-1920, 0),
                size: (1920, 1080),
            },
            PortalStreamGeometry {
                node_id: 20,
                position: (0, 0),
                size: (2560, 1440),
            },
        ];
        assert_eq!(
            portal_stream_at(&streams, LogicalPoint::new(-100.0, 50.0)),
            Some((10, 1820.0, 50.0))
        );
        assert_eq!(
            portal_stream_at(&streams, LogicalPoint::new(0.0, 50.0)),
            Some((20, 0.0, 50.0))
        );
        assert_eq!(
            portal_stream_at(&streams, LogicalPoint::new(2560.0, 50.0)),
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
