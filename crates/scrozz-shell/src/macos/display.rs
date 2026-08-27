//! Display enumeration, and the two rectangles every overlay depends on.
//!
//! # `frame` versus `visibleFrame`
//!
//! `NSScreen` exposes both, and the difference is the whole reason
//! [`scrozz_core::Display`] carries two rectangles:
//!
//! - **`frame`** is the display's full bounds, including the strip the menu bar
//!   occupies and the strip the Dock occupies. This becomes
//!   [`Display::bounds`], and it is what a *capture* covers.
//! - **`visibleFrame`** is the work area: AppKit has already subtracted the
//!   menu bar and the Dock, at the Dock's current edge, at its current size,
//!   and with auto-hide honoured. This becomes [`Display::work_area`], and it
//!   is what an *overlay* anchors to.
//!
//! Anchoring the capture stack to `frame` instead of `visibleFrame` puts the
//! whole stack behind the Dock — the cards are there, they are just not
//! visible, which reads as the app having silently failed. This is not a
//! hypothetical: the bottom-left corner of `frame` is exactly where the Dock
//! is on a default Mac.
//!
//! `visibleFrame` is also not a fixed inset. It changes when the Dock moves to
//! the left or right edge, when the user resizes it, when auto-hide is toggled,
//! and on notched displays. Nothing here caches it.
//!
//! # Coordinates
//!
//! Both rectangles come out of AppKit bottom-left-origin and are flipped into
//! Scrozz's top-left [`LogicalRect`] by [`crate::overlay::appkit_to_logical`],
//! through the height of `NSScreen.screens[0].frame` — the screen that owns the
//! menu bar and therefore AppKit's global origin.

use objc2_app_kit::{NSEvent, NSScreen};
use objc2_foundation::{MainThreadMarker, NSRect};
use scrozz_core::{Display, DisplayId, Error, LogicalPoint, LogicalRect, Result, ScaleFactor};

use crate::macos::main_thread;
use crate::overlay::{AppKitRect, appkit_to_logical};

/// Converts an `NSRect` in screen coordinates to [`AppKitRect`].
///
/// A field-for-field move; it exists so the flip arithmetic itself lives in a
/// platform-free module a headless test can reach.
const fn ns_rect(rect: NSRect) -> AppKitRect {
    AppKitRect::new(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    )
}

/// Height of the display AppKit's global coordinate origin sits on.
///
/// `NSScreen.screens[0]` is the screen carrying the menu bar, whose bottom-left
/// corner is AppKit's `(0, 0)`. Every flip in Scrozz goes through this one
/// number, and it is the **full** frame height, not the visible frame: the
/// origin is below the Dock, so flipping through the work-area height would
/// displace every overlay by the Dock's height.
///
/// Returns `0.0` if there are no screens at all — a Mac with every display
/// asleep — which makes downstream rectangles degenerate rather than panicking.
#[must_use]
pub fn reference_height(mtm: MainThreadMarker) -> f64 {
    NSScreen::screens(mtm)
        .firstObject()
        .map_or(0.0, |screen| screen.frame().size.height)
}

/// Converts one `NSScreen` into a Scrozz [`Display`].
fn to_display(screen: &NSScreen, reference_height: f64, is_primary: bool) -> Display {
    let bounds = appkit_to_logical(ns_rect(screen.frame()), reference_height);
    let work_area = appkit_to_logical(ns_rect(screen.visibleFrame()), reference_height);

    // `CGDirectDisplayID` is stable for as long as the display stays connected,
    // which is exactly the "stable for a session" contract `DisplayId` states,
    // and it is the same identifier the capture backends address displays by.
    let id = DisplayId(screen.CGDirectDisplayID().to_string());

    let scale = {
        let raw = screen.backingScaleFactor();
        if raw.is_finite() && raw > 0.0 {
            ScaleFactor::new(raw)
        } else {
            // `ScaleFactor::new` panics on a non-positive factor, and a screen
            // being reconfigured has been observed to report 0.0 momentarily.
            ScaleFactor::IDENTITY
        }
    };

    Display {
        id,
        name: screen.localizedName().to_string(),
        bounds,
        work_area,
        scale,
        is_primary,
    }
}

/// Every connected display, primary first.
///
/// `NSScreen.screens` is already ordered with the menu-bar screen at index 0,
/// which is the ordering [`Display::is_primary`] is derived from.
///
/// # Errors
///
/// Returns [`Error::Platform`] when called off the main thread.
///
/// # Examples
///
/// A doctest rather than a `#[test]` because `NSScreen` is main-thread-only and
/// libtest always runs tests on a spawned thread. Reads only — nothing is drawn.
///
/// ```
/// let displays = scrozz_shell::macos::display::displays()
///     .expect("doctests run on the main thread");
/// assert!(!displays.is_empty(), "a Mac always has at least one display");
///
/// // Exactly one display owns the menu bar, and it is the logical origin: the
/// // whole top-left coordinate space is defined relative to its top-left corner.
/// let primary: Vec<_> = displays.iter().filter(|screen| screen.is_primary).collect();
/// assert_eq!(primary.len(), 1);
/// assert_eq!(primary[0].bounds.origin.x, 0.0);
/// assert_eq!(primary[0].bounds.origin.y, 0.0);
///
/// for screen in &displays {
///     // `visibleFrame` excludes the menu bar and the Dock, so the work area is
///     // always inside the bounds and never above them. Anchoring to `bounds`
///     // instead is what puts the capture stack behind the Dock.
///     assert!(screen.work_area.size.width <= screen.bounds.size.width);
///     assert!(screen.work_area.size.height <= screen.bounds.size.height);
///     assert!(screen.work_area.origin.y >= screen.bounds.origin.y);
///     assert!(screen.work_area.origin.x >= screen.bounds.origin.x);
///     assert!(screen.scale.get() > 0.0, "a scale factor of zero would divide by zero");
/// }
///
/// // The menu bar is roughly 24pt tall and always present, so the primary
/// // display's work area must start below its bounds. A flipped conversion
/// // shows up here as the inset landing at the bottom instead.
/// assert!(
///     primary[0].work_area.origin.y > primary[0].bounds.origin.y,
///     "the menu bar inset is missing from the top of the primary work area"
/// );
/// ```
pub fn displays() -> Result<Vec<Display>> {
    let mtm = main_thread("enumerating displays")?;
    let reference = reference_height(mtm);
    Ok(NSScreen::screens(mtm)
        .iter()
        .enumerate()
        .map(|(index, screen)| to_display(&screen, reference, index == 0))
        .collect())
}

/// The display carrying the menu bar.
///
/// # Errors
///
/// Returns [`Error::Platform`] off the main thread, and
/// [`Error::TargetGone`] if the machine reports no screens.
pub fn primary_display() -> Result<Display> {
    let mtm = main_thread("reading the primary display")?;
    let reference = reference_height(mtm);
    NSScreen::screens(mtm)
        .firstObject()
        .map(|screen| to_display(&screen, reference, true))
        .ok_or_else(|| Error::TargetGone("no displays are connected".to_owned()))
}

/// The display containing a point in Scrozz's top-left logical space.
///
/// Falls back to the primary display when the point is in the dead space
/// between two non-rectangular arrangements of screens, which is a real
/// position the pointer can occupy on a stepped multi-monitor layout.
///
/// # Errors
///
/// Returns [`Error::Platform`] off the main thread, and [`Error::TargetGone`]
/// if there are no displays.
pub fn display_at(point: LogicalPoint) -> Result<Display> {
    let all = displays()?;
    all.iter()
        .find(|display| contains(display.bounds, point))
        .or_else(|| all.first())
        .cloned()
        .ok_or_else(|| Error::TargetGone("no displays are connected".to_owned()))
}

/// The display containing the pointer.
///
/// This is what decides where an overlay appears: per D27 a capture card shows
/// up next to the work the user is looking at, and on a multi-monitor desk the
/// pointer is the only honest signal of which screen that is.
///
/// # Errors
///
/// Returns [`Error::Platform`] off the main thread, and [`Error::TargetGone`]
/// if there are no displays.
pub fn active_display() -> Result<Display> {
    display_at(pointer_location()?)
}

/// The pointer in Scrozz's global top-left logical coordinate space.
///
/// This is suitable for a click-through overlay's out-of-band pointer probe:
/// unlike window events it remains available while the panel ignores mouse
/// events.
///
/// # Errors
///
/// Returns [`Error::Platform`] off the main thread.
pub fn pointer_location() -> Result<LogicalPoint> {
    let mtm = main_thread("reading the pointer's display")?;
    let reference = reference_height(mtm);
    // `NSEvent::mouseLocation` is a class method with no receiver state and is
    // safe in these bindings; it reports AppKit global coordinates, so it needs
    // the same flip as every screen rectangle.
    let location = NSEvent::mouseLocation();
    Ok(LogicalPoint::new(location.x, reference - location.y))
}

/// Whether a rectangle contains a point, half-open on the far edges.
///
/// Half-open matters on a multi-monitor desk: the right edge of one display is
/// the left edge of the next, and a closed test would report the pointer on
/// both.
fn contains(rect: LogicalRect, point: LogicalPoint) -> bool {
    point.x >= rect.origin.x
        && point.y >= rect.origin.y
        && point.x < rect.origin.x + rect.size.width
        && point.y < rect.origin.y + rect.size.height
}
