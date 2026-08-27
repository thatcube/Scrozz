//! Enumerating windows.
//!
//! ScreenCaptureKit remains the source of window identity and metadata because
//! capture needs the same `SCWindow` object. Its array is not ordered by visual
//! stacking, though, so window IDs are joined to CoreGraphics' authoritative
//! front-to-back list before the picker sees them.

use std::collections::HashMap;

use objc2_core_graphics::{CGWindowID, CGWindowListCreate, CGWindowListOption, kCGNullWindowID};
use objc2_screen_capture_kit::{SCShareableContent, SCWindow};
use scrozz_core::{Display, DisplayId, Error, LogicalRect, Result, SourceApp, Window, WindowId};

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

    let mut windows: Vec<_> = list
        .iter()
        .filter(|window| {
            // SAFETY: `windowLayer` is a plain property read.
            let layer = unsafe { window.windowLayer() };
            layer == 0
        })
        .map(|window| to_window(&window, displays))
        .collect();

    order_front_to_back(&mut windows, &core_graphics_z_order()?);
    Ok(windows)
}

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

fn order_front_to_back(windows: &mut [Window], front_to_back: &[CGWindowID]) {
    let ranks: HashMap<_, _> = front_to_back
        .iter()
        .copied()
        .enumerate()
        .map(|(rank, id)| (id, rank))
        .collect();

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
    let (id, frame, is_visible) =
        unsafe { (window.windowID(), window.frame(), window.isOnScreen()) };
    let metadata = WindowMetadata::from_window(window);

    let bounds = super::display::from_cg_rect(frame);

    Window {
        id: WindowId(id.to_string()),
        title: metadata.title,
        application: metadata.application,
        application_id: metadata.application_id,
        bounds,
        display: containing_display(bounds, displays),
        is_visible,
    }
}

/// Captures owner metadata from the same current `SCWindow` used for capture.
pub(crate) fn source_app(window: &SCWindow) -> SourceApp {
    WindowMetadata::from_window(window).into_source_app()
}

#[derive(Debug, Default, PartialEq, Eq)]
struct WindowMetadata {
    title: Option<String>,
    application: Option<String>,
    application_id: Option<String>,
}

impl WindowMetadata {
    fn from_window(window: &SCWindow) -> Self {
        // SAFETY: all plain property reads on a live `SCWindow`. Keeping the
        // owner retained while reading both fields makes them one snapshot.
        let (title, application, application_id) = unsafe {
            let owner = window.owningApplication();
            (
                window.title().map(|title| title.to_string()),
                owner.as_ref().map(|app| app.applicationName().to_string()),
                owner.as_ref().map(|app| app.bundleIdentifier().to_string()),
            )
        };

        Self::new(title, application, application_id)
    }

    fn new(
        title: Option<String>,
        application: Option<String>,
        application_id: Option<String>,
    ) -> Self {
        Self {
            title: non_empty(title),
            application: non_empty(application),
            application_id: non_empty(application_id),
        }
    }

    fn into_source_app(self) -> SourceApp {
        SourceApp {
            name: self.application,
            identifier: self.application_id,
            window_title: self.title,
        }
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
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
        display_rect(id, x, 0.0, 1000.0, 1000.0, is_primary)
    }

    fn display_rect(
        id: &str,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        is_primary: bool,
    ) -> Display {
        let bounds = LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(width, height));
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
            application_id: Some("com.thatcube.test".to_owned()),
            bounds: window_at(100.0, 500.0),
            display: DisplayId("main".to_owned()),
            is_visible: true,
        }
    }

    #[test]
    fn core_graphics_order_puts_the_visible_front_window_first_for_hit_testing() {
        // This is the ordering contradiction captured by the native macOS lab:
        // SCK put Outlook first while CoreGraphics showed the target above it.
        let outlook = 48_110;
        let target = 48_457;
        let finder = 48_188;
        let unmatched = 99_999;
        let mut windows = vec![
            overlapping_window(outlook),
            overlapping_window(finder),
            overlapping_window(target),
            overlapping_window(unmatched),
        ];

        order_front_to_back(&mut windows, &[target, outlook, finder]);

        assert_eq!(
            windows
                .iter()
                .map(|window| window.id.0.as_str())
                .collect::<Vec<_>>(),
            ["48457", "48110", "48188", "99999"]
        );
        assert_eq!(
            windows.first().map(|window| &window.id),
            Some(&WindowId(target.to_string())),
            "the picker uses the first overlapping candidate as the visible hit"
        );
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
        let displays = [
            display_rect("small", 0.0, 0.0, 1000.0, 1000.0, true),
            display_rect("large", 1000.0, 700.0, 1600.0, 900.0, false),
        ];
        // The centre (1200, 650) is in neither display. The centre-point rule
        // would fall back to primary even though most of the window is on large.
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

    #[test]
    fn bundle_identifier_and_display_name_map_to_their_distinct_fields() {
        let metadata = WindowMetadata::new(
            Some("Roadmap".to_owned()),
            Some("Safari".to_owned()),
            Some("com.apple.Safari".to_owned()),
        );

        assert_eq!(metadata.application.as_deref(), Some("Safari"));
        assert_eq!(metadata.application_id.as_deref(), Some("com.apple.Safari"));

        let source = metadata.into_source_app();
        assert_eq!(source.name.as_deref(), Some("Safari"));
        assert_eq!(source.identifier.as_deref(), Some("com.apple.Safari"));
        assert_eq!(source.window_title.as_deref(), Some("Roadmap"));
    }

    #[test]
    fn empty_screen_capture_kit_metadata_remains_unknown() {
        let metadata = WindowMetadata::new(
            Some(String::new()),
            Some(String::new()),
            Some(String::new()),
        );
        assert_eq!(metadata, WindowMetadata::default());
        assert!(!metadata.into_source_app().is_known());
    }
}
