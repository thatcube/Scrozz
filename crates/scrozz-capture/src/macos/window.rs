//! Enumerating windows.
//!
//! ScreenCaptureKit supplies the capturable window objects. CoreGraphics supplies
//! the documented window-server ordering; ScreenCaptureKit does not promise that
//! its array order is front-to-back.

use std::collections::HashMap;

use objc2_core_graphics::{CGWindowListCreate, CGWindowListOption, kCGNullWindowID};
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
    let rank: HashMap<u32, usize> = on_screen_window_ids_front_to_back()?
        .into_iter()
        .enumerate()
        .map(|(rank, id)| (id, rank))
        .collect();
    let mut windows: Vec<(u32, Window)> = list
        .iter()
        .filter(|window| {
            // SAFETY: `windowLayer` is a plain property read.
            let layer = unsafe { window.windowLayer() };
            layer == 0
        })
        .map(|window| {
            // SAFETY: `windowID` is a plain property read on this snapshot.
            let id = unsafe { window.windowID() };
            (id, to_window(&window, displays))
        })
        .collect();
    windows.sort_by_key(|(id, _)| rank.get(id).copied().unwrap_or(usize::MAX));
    Ok(windows.into_iter().map(|(_, window)| window).collect())
}

/// Window-server identities in documented front-to-back order.
pub(crate) fn on_screen_window_ids_front_to_back() -> Result<Vec<u32>> {
    let options =
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let ids = CGWindowListCreate(options, kCGNullWindowID).ok_or_else(|| {
        Error::Platform("CGWindowListCreate returned no on-screen window list".into())
    })?;
    let mut ordered = Vec::with_capacity(ids.len());
    for index in 0..ids.len() {
        let index = isize::try_from(index)
            .map_err(|_| Error::Platform("the macOS window list is too large".into()))?;
        // SAFETY: `index` is in bounds and the immutable array stays alive.
        // CGWindowListCreate stores each CGWindowID directly in the CFArray's
        // pointer-sized value slot; these are not retained CFNumber objects.
        let raw = unsafe { ids.value_at_index(index) };
        if let Ok(id) = u32::try_from(raw.addr()) {
            ordered.push(id);
        }
    }
    Ok(ordered)
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
    let (id, frame, title, application, is_visible) = unsafe {
        (
            window.windowID(),
            window.frame(),
            window.title().map(|title| title.to_string()),
            window
                .owningApplication()
                .map(|app| app.applicationName().to_string()),
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
        bounds,
        display: containing_display(bounds, displays),
        is_visible,
    }
}

/// The display a window belongs to.
///
/// Decided by the window's centre rather than its origin: a window dragged
/// halfway across a boundary belongs to whichever display shows more of it, and
/// its origin can easily sit on the other one — or, for a window pushed
/// partly off the left edge, on no display at all.
fn containing_display(bounds: LogicalRect, displays: &[Display]) -> DisplayId {
    let centre = (
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );

    displays
        .iter()
        .find(|display| super::display::contains(display.bounds, centre))
        .or_else(|| displays.iter().find(|display| display.is_primary))
        .or_else(|| displays.first())
        .map(|display| display.id.clone())
        .unwrap_or_else(|| DisplayId(String::new()))
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
    fn no_displays_yields_an_empty_id_rather_than_a_panic() {
        assert_eq!(
            containing_display(window_at(0.0, 100.0), &[]),
            DisplayId(String::new())
        );
    }
}
