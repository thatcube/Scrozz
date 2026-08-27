//! The two things AppKit knows that CoreGraphics does not.
//!
//! A display's human-readable name and its work area — the region left after
//! the menu bar and the Dock take their share — exist only on `NSScreen`, and
//! `NSScreen` is main-thread-only. That is not a constraint a capture backend
//! can impose on its callers, so everything here is strictly best-effort: it
//! enriches the CoreGraphics-derived display list when it happens to be called
//! from the main thread, and returns nothing otherwise.
//!
//! Mouse location is the exception. It comes from CoreGraphics' event system
//! rather than `NSEvent`, so it works from any thread.

use std::collections::HashMap;

use objc2_core_graphics::{CGDirectDisplayID, CGEvent};
use objc2_foundation::MainThreadMarker;
use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};

/// A display's name and work area, where AppKit could supply them.
pub(crate) type DisplayDetail = (Option<String>, Option<LogicalRect>);

/// Names and work areas for every screen AppKit can see, keyed by display ID.
///
/// Empty when called off the main thread, which is the common case for a
/// library call. Callers must treat every entry as optional.
pub(crate) fn display_names_and_work_areas() -> HashMap<CGDirectDisplayID, DisplayDetail> {
    let Some(mtm) = MainThreadMarker::new() else {
        return HashMap::new();
    };

    let screens = objc2_app_kit::NSScreen::screens(mtm);

    // AppKit measures from the bottom-left of the *primary* screen with the
    // y-axis pointing up; CoreGraphics measures from its top-left with the
    // y-axis pointing down. Converting between them needs the primary screen's
    // height, and the primary screen is the one whose frame origin is (0, 0) —
    // not necessarily the first in the array.
    let flip_height = screens
        .iter()
        .find(|screen| {
            let frame = screen.frame();
            frame.origin.x == 0.0 && frame.origin.y == 0.0
        })
        .map(|screen| screen.frame().size.height);

    screens
        .iter()
        .map(|screen| {
            let id = screen.CGDirectDisplayID();
            let name = Some(screen.localizedName().to_string()).filter(|it| !it.is_empty());
            let work_area = flip_height.map(|height| flip(screen.visibleFrame(), height));
            (id, (name, work_area))
        })
        .collect()
}

/// The pointer's position in global, top-left-origin display coordinates.
///
/// Uses a synthesised event rather than `NSEvent.mouseLocation` because that
/// requires the main thread and returns AppKit's flipped coordinates. This
/// agrees with `CGDisplayBounds` directly.
pub(crate) fn mouse_location() -> Option<(f64, f64)> {
    let event = CGEvent::new(None)?;
    let point = CGEvent::location(Some(&event));
    Some((point.x, point.y))
}

/// Converts an AppKit rect to the top-left-origin space `LogicalRect` uses.
fn flip(rect: objc2_foundation::NSRect, primary_height: f64) -> LogicalRect {
    LogicalRect::new(
        LogicalPoint::new(
            rect.origin.x,
            primary_height - (rect.origin.y + rect.size.height),
        ),
        LogicalSize::new(rect.size.width, rect.size.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }

    /// A full-height screen flips to the origin, not below it.
    #[test]
    fn a_full_screen_rect_flips_to_the_origin() {
        let flipped = flip(rect(0.0, 0.0, 1512.0, 982.0), 982.0);
        assert_eq!(flipped.origin.x, 0.0);
        assert_eq!(flipped.origin.y, 0.0);
    }

    /// The visible frame sits above the Dock, so in AppKit's upward y-axis its
    /// origin is raised; flipped, that becomes a top inset for the menu bar.
    #[test]
    fn a_visible_frame_flips_to_a_menu_bar_inset() {
        // 982pt screen; Dock takes 80pt at the bottom, menu bar 37pt at the top.
        let visible = rect(0.0, 80.0, 1512.0, 982.0 - 80.0 - 37.0);
        let flipped = flip(visible, 982.0);

        assert_eq!(flipped.origin.y, 37.0, "menu bar inset");
        assert_eq!(flipped.size.height, 865.0);
        assert_eq!(
            flipped.origin.y + flipped.size.height,
            982.0 - 80.0,
            "top of the Dock"
        );
    }

    /// A second display sitting to the left keeps its negative x untouched.
    #[test]
    fn horizontal_placement_is_unchanged_by_the_flip() {
        let flipped = flip(rect(-1920.0, 0.0, 1920.0, 1080.0), 982.0);
        assert_eq!(flipped.origin.x, -1920.0);
    }
}
