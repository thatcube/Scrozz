//! Enumerating windows.
//!
//! ScreenCaptureKit is the source rather than `CGWindowListCopyWindowInfo`,
//! for one decisive reason: capturing a window needs the `SCWindow` object
//! itself, so enumerating through anything else would mean looking the window
//! up a second time and racing with the user closing it in between.

use objc2_screen_capture_kit::{SCShareableContent, SCWindow};
use scrozz_core::{Display, DisplayId, LogicalRect, Result, Window, WindowId};

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

    Ok(list
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
            layer == 0 && owner_pid != current_pid
        })
        .map(|window| to_window(&window, displays))
        .collect())
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
