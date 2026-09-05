//! Enumerating windows.
//!
//! ScreenCaptureKit remains the source of window identity and metadata because
//! capture needs the same `SCWindow` object. Its array is not ordered by visual
//! stacking, though, so window IDs are joined to CoreGraphics' authoritative
//! front-to-back list before the picker sees them.

use std::collections::HashMap;

use objc2_core_graphics::{CGWindowID, CGWindowListCreate, CGWindowListOption, kCGNullWindowID};
use objc2_screen_capture_kit::{SCShareableContent, SCWindow};
use scrozz_core::{Display, DisplayId, Error, LogicalRect, Result, Window, WindowId};

/// Windows the user could plausibly pick, in front-to-back order.
///
/// Only the normal window layer is listed. Everything above it — menu bar
/// extras, tooltips, the Dock, screen-saver windows — is furniture rather than
/// content, and offering it in a window picker is noise. Windows that are
/// currently off-screen (minimised, or on another Space) are kept and reported
/// as not visible, because "capture that minimised window" is a real request.
pub(crate) fn windows(content: &SCShareableContent, displays: &[Display]) -> Result<Vec<Window>> {
    // SAFETY: reading properties of the shareable content snapshot.
    let list = unsafe { content.windows() };
    let current_pid = i32::try_from(std::process::id()).ok();

    let mut windows: Vec<_> = list
        .iter()
        .filter(|window| {
            // SAFETY: immutable property reads from one content snapshot.
            let (layer, owner_pid) = unsafe {
                (
                    window.windowLayer(),
                    window
                        .owningApplication()
                        .map(|application| application.processID()),
                )
            };
            is_eligible_window(layer, owner_pid, current_pid)
        })
        .map(|window| to_window(&window, displays))
        .collect();

    order_front_to_back(&mut windows, &core_graphics_z_order()?);
    Ok(windows)
}

fn is_eligible_window(layer: isize, owner_pid: Option<i32>, current_pid: Option<i32>) -> bool {
    layer == 0 && current_pid.is_none_or(|current_pid| owner_pid != Some(current_pid))
}

#[allow(deprecated)]
fn core_graphics_z_order() -> Result<Vec<CGWindowID>> {
    let list = CGWindowListCreate(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    )
    .ok_or_else(|| Error::Platform("CoreGraphics could not enumerate window z-order".to_owned()))?;

    Ok((0..list.count())
        .filter_map(|index| {
            // SAFETY: `CGWindowListCreate` stores each `CGWindowID` directly
            // in an array pointer slot; the value is read without retaining or
            // dereferencing it.
            let slot = unsafe { list.value_at_index(index) };
            CGWindowID::try_from(slot.addr()).ok()
        })
        .collect())
}

/// Window-server identities in documented front-to-back order.
///
/// Scroll delivery needs the ordering alone — no ScreenCaptureKit snapshot, no
/// geometry — to decide which window a synthesised wheel event would actually
/// reach, so it reads the same CoreGraphics list the picker is sorted by.
pub(crate) fn on_screen_window_ids_front_to_back() -> Result<Vec<u32>> {
    core_graphics_z_order()
}

fn order_front_to_back(windows: &mut [Window], front_to_back: &[CGWindowID]) {
    let ranks: HashMap<_, _> = front_to_back
        .iter()
        .copied()
        .enumerate()
        .map(|(rank, id)| (id, rank))
        .collect();

    // Stable sorting keeps ScreenCaptureKit's relative order for minimised,
    // off-Space, desktop, and privacy-protected windows absent from the
    // on-screen CoreGraphics list. They remain non-hit-testable when the OS
    // reports them off screen.
    windows.sort_by_key(|window| {
        window
            .id
            .0
            .parse::<CGWindowID>()
            .ok()
            .and_then(|id| ranks.get(&id).copied())
            .unwrap_or(usize::MAX)
    });
}

/// Finds a specific window in a content snapshot.
///
/// Returning `None` here is the ordinary "the window closed while the user was
/// choosing" case, which the caller turns into `Error::TargetGone`.
pub(crate) fn find(
    content: &SCShareableContent,
    id: &WindowId,
) -> Option<objc2::rc::Retained<SCWindow>> {
    let wanted: u32 = id.0.parse().ok()?;
    // SAFETY: reading properties of the shareable content snapshot.
    unsafe {
        content
            .windows()
            .iter()
            .find(|window| window.windowID() == wanted)
    }
}

fn to_window(window: &SCWindow, displays: &[Display]) -> Window {
    // SAFETY: all plain property reads on a live `SCWindow`.
    let (id, frame, title, application, owner_pid, is_visible) = unsafe {
        (
            window.windowID(),
            window.frame(),
            window.title().map(|title| title.to_string()),
            window
                .owningApplication()
                .map(|app| app.applicationName().to_string()),
            window
                .owningApplication()
                .and_then(|app| u32::try_from(app.processID()).ok()),
            window.isOnScreen(),
        )
    };

    let bounds = super::display::from_cg_rect(frame);

    Window {
        id: WindowId(id.to_string()),
        // An empty title is the OS saying it has none, so report `None`
        // rather than a blank string a picker would render as a gap.
        title: title.filter(|title| !title.is_empty()),
        application: application.filter(|name| !name.is_empty()),
        owner_pid,
        bounds,
        display: containing_display(bounds, displays),
        is_visible,
    }
}

/// The display a window belongs to.
///
/// Decided by overlap area rather than origin or centre. Irregular monitor
/// layouts can put the centre in a gap, and an asymmetrical window can have its
/// centre on the display that contains less of it.
fn containing_display(bounds: LogicalRect, displays: &[Display]) -> DisplayId {
    displays
        .iter()
        .filter_map(|display| {
            let area = overlap_area(bounds, display.bounds);
            (area > 0.0).then_some((area, display))
        })
        .max_by(|(left, _), (right, _)| {
            left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(_, display)| display)
        .or_else(|| displays.iter().find(|display| display.is_primary))
        .or_else(|| displays.first())
        .map(|display| display.id.clone())
        .unwrap_or_else(|| DisplayId(String::new()))
}

fn overlap_area(a: LogicalRect, b: LogicalRect) -> f64 {
    let left = a.origin.x.max(b.origin.x);
    let top = a.origin.y.max(b.origin.y);
    let right = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let bottom = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    ((right - left).max(0.0)) * ((bottom - top).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{LogicalPoint, LogicalSize, ScaleFactor};

    fn display(id: &str, x: f64, is_primary: bool) -> Display {
        let bounds = LogicalRect::new(LogicalPoint::new(x, 0.0), LogicalSize::new(1000.0, 1000.0));
        Display {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds,
            work_area: bounds,
            scale: ScaleFactor::new(2.0),
            is_primary,
        }
    }

    fn window_at(x: f64, width: f64) -> LogicalRect {
        LogicalRect::new(LogicalPoint::new(x, 100.0), LogicalSize::new(width, 200.0))
    }

    fn overlapping_window(id: CGWindowID) -> Window {
        Window {
            id: WindowId(id.to_string()),
            title: Some(format!("window-{id}")),
            application: Some("Test".to_owned()),
            owner_pid: None,
            bounds: window_at(100.0, 500.0),
            display: DisplayId("main".to_owned()),
            is_visible: true,
        }
    }

    #[test]
    fn each_native_snapshot_uses_its_current_core_graphics_order() {
        let front = 48_457;
        let back = 48_110;
        let unmatched = 99_999;
        let source = vec![
            overlapping_window(back),
            overlapping_window(unmatched),
            overlapping_window(front),
        ];
        let mut first = source.clone();
        let mut second = source;

        order_front_to_back(&mut first, &[front, back]);
        order_front_to_back(&mut second, &[back, front]);

        assert_eq!(
            first
                .iter()
                .map(|window| window.id.0.as_str())
                .collect::<Vec<_>>(),
            ["48457", "48110", "99999"]
        );
        assert_eq!(
            second
                .iter()
                .map(|window| window.id.0.as_str())
                .collect::<Vec<_>>(),
            ["48110", "48457", "99999"]
        );
    }

    #[test]
    fn scrozz_and_non_window_layers_are_never_picker_candidates() {
        assert!(!is_eligible_window(0, Some(42), Some(42)));
        assert!(!is_eligible_window(1, Some(7), Some(42)));
        assert!(is_eligible_window(0, Some(7), Some(42)));
        assert!(is_eligible_window(0, None, Some(42)));
    }

    #[test]
    fn a_window_belongs_to_the_display_showing_most_of_it() {
        let displays = [display("left", 0.0, true), display("right", 1000.0, false)];

        // Origin is on the left display, but two-thirds of the window is right.
        let straddling = window_at(900.0, 300.0);
        assert_eq!(
            containing_display(straddling, &displays),
            DisplayId("right".to_owned())
        );
    }

    #[test]
    fn a_window_dragged_off_the_edge_falls_back_to_the_primary_display() {
        let displays = [display("left", 0.0, true), display("right", 1000.0, false)];
        let offscreen = window_at(-5000.0, 100.0);
        assert_eq!(
            containing_display(offscreen, &displays),
            DisplayId("left".to_owned())
        );
    }

    #[test]
    fn irregular_layout_uses_the_display_with_the_largest_overlap() {
        let bounds = |id: &str, x, y, width, height, is_primary| Display {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds: LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(width, height)),
            work_area: LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(width, height)),
            scale: ScaleFactor::new(2.0),
            is_primary,
        };
        let displays = [
            bounds("small", 0.0, 0.0, 1000.0, 1000.0, true),
            bounds("large", 1000.0, 700.0, 1600.0, 900.0, false),
        ];
        let window = LogicalRect::new(
            LogicalPoint::new(900.0, 400.0),
            LogicalSize::new(600.0, 500.0),
        );

        assert_eq!(
            containing_display(window, &displays),
            DisplayId("large".to_owned())
        );
    }

    #[test]
    fn no_displays_yields_an_empty_id_rather_than_a_panic() {
        assert_eq!(
            containing_display(window_at(0.0, 100.0), &[]),
            DisplayId(String::new())
        );
    }
}
