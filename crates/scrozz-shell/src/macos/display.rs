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
//! - **`visibleFrame`** is AppKit's work-area baseline. It subtracts the menu bar
//!   and a permanently shown Dock. For an auto-hidden Dock AppKit gives that
//!   space back even while the Dock is revealed, so Scrozz reserves the current
//!   Dock tile plus its surround before producing [`Display::work_area`].
//!
//! Anchoring the capture stack to `frame` instead of `visibleFrame` puts the
//! whole stack behind the Dock — the cards are there, they are just not
//! visible, which reads as the app having silently failed. This is not a
//! hypothetical: the bottom-left corner of `frame` is exactly where the Dock
//! is on a default Mac.
//!
//! The result is not a fixed inset. It changes when the Dock moves to the left
//! or right edge, when the user resizes it, when auto-hide is toggled, and on
//! notched displays. Nothing here caches it.
//!
//! # Coordinates
//!
//! Both rectangles come out of AppKit bottom-left-origin and are flipped into
//! Scrozz's top-left [`LogicalRect`] by [`crate::overlay::appkit_to_logical`],
//! through the height of `NSScreen.screens[0].frame` — the screen that owns the
//! menu bar and therefore AppKit's global origin.

use std::{
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{NSApplicationDidChangeScreenParametersNotification, NSEvent, NSScreen};
use objc2_core_foundation::{
    CFNumber, CFPreferencesCopyAppValue, CFPreferencesGetAppBooleanValue, CFString,
};
use objc2_foundation::{MainThreadMarker, NSNotification, NSNotificationCenter, NSRect};
use scrozz_core::{Display, DisplayId, Error, LogicalPoint, LogicalRect, Result, ScaleFactor};

use crate::macos::main_thread;
use crate::overlay::{AppKitRect, appkit_to_logical};

/// Extra material around Dock tiles in the floating Dock background.
///
/// `tilesize` describes the icon, not the complete input-obscuring surface. The
/// public work-area API excludes a permanently visible Dock, but intentionally
/// excludes an auto-hidden Dock even while it is revealed. Reserving the tile
/// plus this chrome keeps capture cards clear in both states.
const AUTO_HIDE_DOCK_CHROME: f64 = 20.0;
const DEFAULT_DOCK_TILE_SIZE: f64 = 64.0;
static REFERENCE_HEIGHT_BITS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DockEdge {
    Bottom,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct AutoHideDock {
    edge: DockEdge,
    thickness: f64,
}

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
    crate::macos::activity::record_display_enumeration();
    let height = NSScreen::screens(mtm)
        .firstObject()
        .map_or(0.0, |screen| screen.frame().size.height);
    REFERENCE_HEIGHT_BITS.store(height.to_bits(), Ordering::Release);
    height
}

/// Converts one `NSScreen` into a Scrozz [`Display`].
fn to_display(screen: &NSScreen, reference_height: f64, is_primary: bool) -> Display {
    let bounds = appkit_to_logical(ns_rect(screen.frame()), reference_height);
    let work_area = reserve_auto_hidden_dock(
        bounds,
        appkit_to_logical(ns_rect(screen.visibleFrame()), reference_height),
        auto_hide_dock(),
    );

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

/// Reads the user's Dock placement without spawning `defaults`.
///
/// AppKit's `visibleFrame` deliberately gives auto-hidden Dock space back to
/// applications. That is correct for ordinary windows and wrong for a capture
/// card that remains visible while the user reveals the Dock.
fn auto_hide_dock() -> Option<AutoHideDock> {
    let domain = CFString::from_static_str("com.apple.dock");
    let autohide_key = CFString::from_static_str("autohide");
    let mut valid = 0_u8;
    // SAFETY: `valid` is a live one-byte Boolean output, as required by Core
    // Foundation.
    let autohide = unsafe { CFPreferencesGetAppBooleanValue(&autohide_key, &domain, &mut valid) };
    if valid == 0 || !autohide {
        return None;
    }

    let tile_key = CFString::from_static_str("tilesize");
    let tile = CFPreferencesCopyAppValue(&tile_key, &domain)
        .and_then(|value| value.downcast::<CFNumber>().ok())
        .and_then(|value| value.as_f64())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(DEFAULT_DOCK_TILE_SIZE);

    let orientation_key = CFString::from_static_str("orientation");
    let edge = CFPreferencesCopyAppValue(&orientation_key, &domain)
        .and_then(|value| value.downcast::<CFString>().ok())
        .map_or(DockEdge::Bottom, |value| match value.to_string().as_str() {
            "left" => DockEdge::Left,
            "right" => DockEdge::Right,
            _ => DockEdge::Bottom,
        });

    Some(AutoHideDock {
        edge,
        thickness: tile.clamp(16.0, 256.0) + AUTO_HIDE_DOCK_CHROME,
    })
}

fn reserve_auto_hidden_dock(
    bounds: LogicalRect,
    mut work_area: LogicalRect,
    dock: Option<AutoHideDock>,
) -> LogicalRect {
    let Some(dock) = dock else {
        return work_area;
    };
    let epsilon = 0.5;
    match dock.edge {
        DockEdge::Bottom => {
            let work_bottom = work_area.origin.y + work_area.size.height;
            let bounds_bottom = bounds.origin.y + bounds.size.height;
            if (work_bottom - bounds_bottom).abs() <= epsilon {
                work_area.size.height = (work_area.size.height - dock.thickness).max(1.0);
            }
        }
        DockEdge::Left => {
            if (work_area.origin.x - bounds.origin.x).abs() <= epsilon {
                work_area.origin.x += dock.thickness;
                work_area.size.width = (work_area.size.width - dock.thickness).max(1.0);
            }
        }
        DockEdge::Right => {
            let work_right = work_area.origin.x + work_area.size.width;
            let bounds_right = bounds.origin.x + bounds.size.width;
            if (work_right - bounds_right).abs() <= epsilon {
                work_area.size.width = (work_area.size.width - dock.thickness).max(1.0);
            }
        }
    }
    work_area
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
    crate::macos::activity::record_display_enumeration();
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
    crate::macos::activity::record_display_enumeration();
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
/// Returns [`Error::Platform`] off the main thread.
pub fn pointer_location() -> Result<LogicalPoint> {
    let mtm = main_thread("reading the pointer location")?;
    let cached = f64::from_bits(REFERENCE_HEIGHT_BITS.load(Ordering::Acquire));
    let reference = if cached.is_finite() && cached > 0.0 {
        cached
    } else {
        reference_height(mtm)
    };
    // The display refresh paths update the cached reference height. Sampling
    // the pointer therefore stays a pure NSEvent read instead of enumerating
    // NSScreen on every idle hover probe.
    let location = NSEvent::mouseLocation();
    Ok(LogicalPoint::new(location.x, reference - location.y))
}

/// The display containing the pointer.
///
/// # Errors
///
/// Returns [`Error::Platform`] off the main thread, and [`Error::TargetGone`]
/// if there are no displays.
pub fn active_display() -> Result<Display> {
    display_at(pointer_location()?)
}

/// Event-driven invalidation for cached display geometry.
///
/// AppKit owns the notification source. The returned monitor owns exactly one
/// observer token and unregisters it before release; reading [`Self::changed`]
/// is an atomic load and never enumerates displays.
pub struct DisplayChangeMonitor {
    center: Retained<NSNotificationCenter>,
    observer: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    generation: Arc<AtomicU64>,
    observed_generation: u64,
}

impl DisplayChangeMonitor {
    /// Installs one process-local AppKit screen-parameter observer.
    pub fn new() -> Result<Self> {
        let _mtm = main_thread("observing display changes")?;
        let center = NSNotificationCenter::defaultCenter();
        let generation = Arc::new(AtomicU64::new(0));
        let callback_generation = Arc::clone(&generation);
        let block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
            callback_generation.fetch_add(1, Ordering::Release);
        });
        // SAFETY: the notification name is an AppKit process-lifetime constant,
        // the copied block owns its Arc, and a nil queue delivers synchronously
        // on the posting thread (AppKit posts this notification on the main
        // thread).
        let observer = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSApplicationDidChangeScreenParametersNotification),
                None,
                None,
                &block,
            )
        };
        Ok(Self {
            center,
            observer,
            generation,
            observed_generation: 0,
        })
    }

    /// Returns true once for one or more notifications since the last read.
    pub fn changed(&mut self) -> bool {
        let generation = self.generation.load(Ordering::Acquire);
        if generation == self.observed_generation {
            return false;
        }
        self.observed_generation = generation;
        true
    }
}

impl Drop for DisplayChangeMonitor {
    fn drop(&mut self) {
        // SAFETY: this is the exact opaque token returned by this center.
        unsafe {
            self.center.removeObserver(self.observer.as_ref());
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::LogicalSize;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> LogicalRect {
        LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(width, height))
    }

    #[test]
    fn an_auto_hidden_bottom_dock_is_reserved_even_when_visible_frame_reaches_the_edge() {
        let bounds = rect(0.0, 0.0, 1728.0, 1117.0);
        let visible = rect(0.0, 33.0, 1728.0, 1084.0);
        let safe = reserve_auto_hidden_dock(
            bounds,
            visible,
            Some(AutoHideDock {
                edge: DockEdge::Bottom,
                thickness: 134.0,
            }),
        );
        assert_eq!(safe, rect(0.0, 33.0, 1728.0, 950.0));
    }

    #[test]
    fn an_already_excluded_dock_is_not_reserved_twice() {
        let bounds = rect(0.0, 0.0, 1728.0, 1117.0);
        let visible = rect(0.0, 33.0, 1728.0, 950.0);
        let safe = reserve_auto_hidden_dock(
            bounds,
            visible,
            Some(AutoHideDock {
                edge: DockEdge::Bottom,
                thickness: 134.0,
            }),
        );
        assert_eq!(safe, visible);
    }

    #[test]
    fn side_docks_move_the_safe_edge_instead_of_the_bottom() {
        let bounds = rect(0.0, 0.0, 1728.0, 1117.0);
        let visible = rect(0.0, 33.0, 1728.0, 1084.0);
        let safe = reserve_auto_hidden_dock(
            bounds,
            visible,
            Some(AutoHideDock {
                edge: DockEdge::Left,
                thickness: 80.0,
            }),
        );
        assert_eq!(safe, rect(80.0, 33.0, 1648.0, 1084.0));
    }
}
