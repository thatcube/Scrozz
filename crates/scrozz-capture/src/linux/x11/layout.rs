//! Geometry that relates monitors, work areas, windows and the pointer.
//!
//! X11 reports these things in four different, partly-overlapping coordinate
//! systems, and reconciling them is arithmetic — so it lives here, away from the
//! connection, where it can be tested exhaustively without a display server.
//!
//! The one genuinely subtle piece is the work area. `_NET_WORKAREA` is defined
//! per *desktop*, not per *monitor*: on a dual-head setup a window manager
//! reports a single rectangle spanning both screens, already shrunk by whatever
//! panels exist. Attaching that rectangle to each monitor unmodified gives every
//! monitor a work area larger than itself. Intersecting it with the monitor is
//! what actually produces the answer an overlay needs.

use scrozz_core::{Display, DisplayId, LogicalPoint, LogicalRect, LogicalSize};

use super::ewmh::WireRect;

/// A rectangle in device pixels on the X root window, as X reports everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width; zero means empty.
    pub width: u32,
    /// Height; zero means empty.
    pub height: u32,
}

impl PixelRect {
    /// Creates a rectangle.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The exclusive right edge.
    #[must_use]
    pub const fn right(&self) -> i64 {
        self.x as i64 + self.width as i64
    }

    /// The exclusive bottom edge.
    #[must_use]
    pub const fn bottom(&self) -> i64 {
        self.y as i64 + self.height as i64
    }

    /// Whether the rectangle encloses any area.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Whether a root-relative point falls inside.
    #[must_use]
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        (x as i64) >= self.x as i64
            && (x as i64) < self.right()
            && (y as i64) >= self.y as i64
            && (y as i64) < self.bottom()
    }

    /// The overlapping region of two rectangles, if they overlap at all.
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Option<Self> {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= i64::from(left) || bottom <= i64::from(top) {
            return None;
        }
        Some(Self {
            x: left,
            y: top,
            width: u32::try_from(right - i64::from(left)).ok()?,
            height: u32::try_from(bottom - i64::from(top)).ok()?,
        })
    }

    /// The area of the overlap, used to attribute a window to a monitor.
    #[must_use]
    pub fn overlap_area(&self, other: &Self) -> u64 {
        self.intersection(other)
            .map_or(0, |r| u64::from(r.width) * u64::from(r.height))
    }

    /// Converts to logical coordinates at the given scale.
    #[must_use]
    pub fn to_logical(self, scale: f64) -> LogicalRect {
        LogicalRect::new(
            LogicalPoint::new(f64::from(self.x) / scale, f64::from(self.y) / scale),
            LogicalSize::new(
                f64::from(self.width) / scale,
                f64::from(self.height) / scale,
            ),
        )
    }
}

impl From<WireRect> for PixelRect {
    fn from(r: WireRect) -> Self {
        Self {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

/// Narrows a desktop-wide `_NET_WORKAREA` down to one monitor.
///
/// Falls back to the monitor's own bounds when the two do not overlap. That
/// happens for real — a monitor hot-plugged since the window manager last
/// published the property is simply outside it — and an empty work area would
/// leave an overlay with nowhere to go.
#[must_use]
pub fn work_area_for(monitor: PixelRect, desktop_work_area: Option<PixelRect>) -> PixelRect {
    desktop_work_area
        .and_then(|desktop| monitor.intersection(&desktop))
        .unwrap_or(monitor)
}

/// Picks the display containing a point, for deciding where an overlay appears.
///
/// Falls back to the primary display, then to the first, so a pointer parked in
/// the dead space of an L-shaped multi-monitor arrangement still yields an
/// answer rather than an error.
#[must_use]
pub fn display_containing(
    x: i32,
    y: i32,
    displays: &[(DisplayId, PixelRect, bool)],
) -> Option<DisplayId> {
    displays
        .iter()
        .find(|(_, rect, _)| rect.contains(x, y))
        .or_else(|| displays.iter().find(|(_, _, primary)| *primary))
        .or_else(|| displays.first())
        .map(|(id, _, _)| id.clone())
}

/// Attributes a window to the display it sits on most.
///
/// "Predominantly on", as [`scrozz_core::Window::display`] requires, means
/// largest overlap area — not nearest centre and not whichever monitor contains
/// the top-left corner. A window dragged so that only its title bar is on the
/// second monitor belongs to the first, and corner tests get that backwards.
#[must_use]
pub fn display_for_window(
    window: PixelRect,
    displays: &[(DisplayId, PixelRect, bool)],
) -> Option<DisplayId> {
    displays
        .iter()
        .map(|(id, rect, _)| (id, window.overlap_area(rect)))
        .filter(|(_, area)| *area > 0)
        .max_by_key(|(_, area)| *area)
        .map(|(id, _)| id.clone())
        .or_else(|| {
            displays
                .iter()
                .find(|(_, _, primary)| *primary)
                .or_else(|| displays.first())
                .map(|(id, _, _)| id.clone())
        })
}

/// The smallest rectangle containing every monitor.
///
/// This is the capture region for [`scrozz_core::CaptureTarget::AllDisplays`].
/// It is the union rather than the root window's own geometry because the two
/// disagree whenever monitors are arranged with gaps or negative offsets, and
/// the union is the one that is never larger than it should be.
#[must_use]
pub fn bounding_box(rects: &[PixelRect]) -> Option<PixelRect> {
    let mut iter = rects.iter().filter(|r| !r.is_empty());
    let first = iter.next()?;
    let (mut left, mut top) = (i64::from(first.x), i64::from(first.y));
    let (mut right, mut bottom) = (first.right(), first.bottom());

    for rect in iter {
        left = left.min(i64::from(rect.x));
        top = top.min(i64::from(rect.y));
        right = right.max(rect.right());
        bottom = bottom.max(rect.bottom());
    }

    Some(PixelRect {
        x: i32::try_from(left).ok()?,
        y: i32::try_from(top).ok()?,
        width: u32::try_from(right - left).ok()?,
        height: u32::try_from(bottom - top).ok()?,
    })
}

/// Converts a logical capture region into the root-relative pixels to fetch.
///
/// Clamps to the root window, because `GetImage` outside the drawable is a
/// `BadMatch` and a region dragged one pixel past the screen edge is an entirely
/// ordinary thing for a user to do.
#[must_use]
pub fn region_to_pixels(region: LogicalRect, scale: f64, root: PixelRect) -> Option<PixelRect> {
    let left = (region.origin.x * scale).floor();
    let top = (region.origin.y * scale).floor();
    let right = ((region.origin.x + region.size.width) * scale).ceil();
    let bottom = ((region.origin.y + region.size.height) * scale).ceil();

    if !(left.is_finite() && top.is_finite() && right.is_finite() && bottom.is_finite()) {
        return None;
    }

    let requested = PixelRect {
        x: left as i32,
        y: top as i32,
        width: u32::try_from((right - left).max(0.0) as i64).ok()?,
        height: u32::try_from((bottom - top).max(0.0) as i64).ok()?,
    };

    requested.intersection(&root)
}

/// Builds the core [`Display`] value from the pieces X supplies separately.
#[must_use]
pub fn to_display(
    id: DisplayId,
    name: String,
    bounds: PixelRect,
    work_area: PixelRect,
    scale: scrozz_core::ScaleFactor,
    is_primary: bool,
) -> Display {
    Display {
        id,
        name,
        bounds: bounds.to_logical(scale.get()),
        work_area: work_area.to_logical(scale.get()),
        scale,
        is_primary,
    }
}
