//! Coordinate and DPI arithmetic for the Windows backend.
//!
//! Deliberately free of every `windows` crate type so it compiles — and is
//! therefore unit-tested — on macOS and Linux as well. `tests/windows.rs`
//! `#[path]`-includes this file so the maths below is exercised by
//! `cargo test` on the developer's Mac, which is the only place these rules
//! get checked before real hardware exists.
//!
//! # The coordinate model, stated once
//!
//! A per-monitor-DPI-aware-v2 process sees the Windows virtual desktop in
//! **real device pixels**. `EnumDisplayMonitors`, `GetMonitorInfoW` and
//! `GetWindowRect` all report that space, and it has two properties that break
//! naive code:
//!
//! 1. **The origin is not (0, 0).** The primary monitor's top-left is the
//!    origin, so a monitor placed to the left of or above it has **negative**
//!    coordinates. Every rectangle here is `i32`, never `u32`, for that reason.
//! 2. **There is no single scale factor.** A 150% laptop panel next to a 100%
//!    external monitor is routine, so scale is a property of a *monitor*.
//!
//! Scrozz's [`scrozz_core::Display`] wants logical coordinates. With mixed DPI
//! there is no canonical global logical desktop — laying monitors out logically
//! while preserving adjacency is ambiguous — so this backend uses the
//! convention that makes **capture** correct:
//!
//! > Each monitor's logical rectangle is its device rectangle divided by *its
//! > own* scale, origin included.
//!
//! The invariant that buys is exact round-tripping:
//! `logical.to_physical(display.scale)` reproduces the true device rectangle,
//! origin and all, so a logical region hit-tested onto a display converts back
//! to the right crop. The cost is that logical coordinates may leave gaps
//! between monitors of different scale; nothing anchors across that gap, since
//! overlays are positioned per-display from [`scrozz_core::Display::work_area`].

use scrozz_core::{
    LogicalRect, PhysicalPoint, PhysicalRect, PhysicalSize, Point, ScaleFactor, Size,
};

/// The DPI Windows calls 100%.
pub const USER_DEFAULT_SCREEN_DPI: u32 = 96;

/// A rectangle in raw virtual-desktop device pixels.
///
/// Mirrors Win32 `RECT`: `right` and `bottom` are exclusive, and any field may
/// be negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceRect {
    /// Left edge, inclusive.
    pub left: i32,
    /// Top edge, inclusive.
    pub top: i32,
    /// Right edge, exclusive.
    pub right: i32,
    /// Bottom edge, exclusive.
    pub bottom: i32,
}

impl DeviceRect {
    /// Creates a rectangle from Win32 `RECT` edges.
    #[must_use]
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Creates a rectangle from an origin and a size.
    #[must_use]
    pub const fn from_origin_size(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            left,
            top,
            right: left.saturating_add(width),
            bottom: top.saturating_add(height),
        }
    }

    /// Width in device pixels, clamped at zero for inverted rectangles.
    #[must_use]
    pub const fn width(self) -> i32 {
        let w = self.right - self.left;
        if w < 0 { 0 } else { w }
    }

    /// Height in device pixels, clamped at zero for inverted rectangles.
    #[must_use]
    pub const fn height(self) -> i32 {
        let h = self.bottom - self.top;
        if h < 0 { 0 } else { h }
    }

    /// Whether the rectangle encloses no pixels.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width() == 0 || self.height() == 0
    }

    /// Area in device pixels, saturating rather than overflowing.
    ///
    /// `i64` because a 4-monitor 4K desktop already exceeds `i32` when squared
    /// during intersection comparisons.
    #[must_use]
    pub const fn area(self) -> i64 {
        self.width() as i64 * self.height() as i64
    }

    /// The overlapping region of two rectangles, or `None` if they are disjoint.
    #[must_use]
    pub fn intersection(self, other: Self) -> Option<Self> {
        let r = Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };
        (!r.is_empty()).then_some(r)
    }

    /// The smallest rectangle containing both.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// Whether a point lies inside, treating right and bottom as exclusive.
    #[must_use]
    pub const fn contains(self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// This rectangle expressed relative to `origin`'s top-left.
    ///
    /// Used to turn a virtual-desktop crop into an offset within one captured
    /// display's buffer.
    #[must_use]
    pub const fn offset_from(self, origin: Self) -> Self {
        Self {
            left: self.left - origin.left,
            top: self.top - origin.top,
            right: self.right - origin.left,
            bottom: self.bottom - origin.top,
        }
    }

    /// As a [`PhysicalRect`], for handing to core geometry.
    #[must_use]
    pub fn to_physical(self) -> PhysicalRect {
        PhysicalRect::new(
            PhysicalPoint::new(f64::from(self.left), f64::from(self.top)),
            PhysicalSize::new(f64::from(self.width()), f64::from(self.height())),
        )
    }
}

/// Converts a Windows DPI value to a [`ScaleFactor`].
///
/// Windows reports 96 for 100%, 120 for 125%, 144 for 150% and so on, and
/// fractional scaling means the result genuinely is not an integer — hence
/// `f64` all the way down. A zero or absurd DPI (seen when `GetDpiForMonitor`
/// fails and the out-parameter is left untouched) falls back to 1.0 rather than
/// panicking inside [`ScaleFactor::new`].
#[must_use]
pub fn scale_from_dpi(dpi: u32) -> ScaleFactor {
    if dpi == 0 {
        return ScaleFactor::IDENTITY;
    }
    let factor = f64::from(dpi) / f64::from(USER_DEFAULT_SCREEN_DPI);
    if factor.is_finite() && factor > 0.0 {
        ScaleFactor::new(factor)
    } else {
        ScaleFactor::IDENTITY
    }
}

/// A device rectangle in the logical desktop, per the module's convention.
#[must_use]
pub fn logical_from_device(rect: DeviceRect, scale: ScaleFactor) -> LogicalRect {
    rect.to_physical().to_logical(scale)
}

/// A logical rectangle back in device pixels, rounding outwards.
///
/// Outward rounding matches [`LogicalRect::to_physical`]: a user's selection
/// must never lose an edge pixel it visibly contained.
#[must_use]
pub fn device_from_logical(rect: LogicalRect, scale: ScaleFactor) -> DeviceRect {
    let p = rect.to_physical(scale);
    let left = p.origin.x.round() as i32;
    let top = p.origin.y.round() as i32;
    DeviceRect::from_origin_size(
        left,
        top,
        p.size.width.round() as i32,
        p.size.height.round() as i32,
    )
}

/// Index of the monitor a rectangle sits on predominantly.
///
/// A window straddling two monitors belongs to whichever shows more of it, and
/// a window entirely off-screen (minimised windows report
/// `-32000, -32000`) belongs to none. Falls back to the nearest monitor by
/// centre distance so an off-screen window still gets a plausible display id
/// rather than being dropped from enumeration.
#[must_use]
pub fn dominant_monitor(rect: DeviceRect, monitors: &[DeviceRect]) -> Option<usize> {
    if monitors.is_empty() {
        return None;
    }

    let mut best: Option<(usize, i64)> = None;
    for (i, m) in monitors.iter().enumerate() {
        if let Some(overlap) = rect.intersection(*m) {
            let area = overlap.area();
            if best.is_none_or(|(_, a)| area > a) {
                best = Some((i, area));
            }
        }
    }
    if let Some((i, _)) = best {
        return Some(i);
    }

    // No overlap at all: nearest centre wins.
    let cx = i64::from(rect.left) + i64::from(rect.width()) / 2;
    let cy = i64::from(rect.top) + i64::from(rect.height()) / 2;
    monitors
        .iter()
        .enumerate()
        .min_by_key(|(_, m)| {
            let mx = i64::from(m.left) + i64::from(m.width()) / 2;
            let my = i64::from(m.top) + i64::from(m.height()) / 2;
            (cx - mx).pow(2) + (cy - my).pow(2)
        })
        .map(|(i, _)| i)
}

/// The bounding box of every monitor, in device pixels.
///
/// May have a negative origin; that is the whole point of computing it rather
/// than assuming `(0, 0)`.
#[must_use]
pub fn virtual_desktop_bounds(monitors: &[DeviceRect]) -> DeviceRect {
    monitors
        .iter()
        .copied()
        .reduce(DeviceRect::union)
        .unwrap_or_default()
}

/// Whether every monitor shares one scale factor.
///
/// All-displays capture composites monitors into a single image, which only has
/// an unambiguous pixel grid when the scales agree; when they do not, the
/// backend resamples to the largest scale. Comparing `f64` exactly is correct
/// here because these values come from the same `dpi / 96` arithmetic, not from
/// accumulated float maths.
#[must_use]
pub fn uniform_scale(scales: &[ScaleFactor]) -> bool {
    match scales.split_first() {
        None => true,
        Some((first, rest)) => rest
            .iter()
            .all(|s| (s.get() - first.get()).abs() < f64::EPSILON),
    }
}

/// The largest scale across a set of displays, or 1.0 for an empty set.
#[must_use]
pub fn max_scale(scales: &[ScaleFactor]) -> ScaleFactor {
    scales
        .iter()
        .copied()
        .fold(ScaleFactor::IDENTITY, |acc, s| {
            if s.get() > acc.get() { s } else { acc }
        })
}

/// Places `monitor` inside a composited virtual-desktop image drawn at
/// `target_scale`.
///
/// Both the position and the size are taken through logical space, so a 100%
/// monitor sitting beside a 150% one lands at the right place in a 150% canvas
/// and is resampled up to fill it.
#[must_use]
pub fn placement_in_composite(
    monitor: DeviceRect,
    monitor_scale: ScaleFactor,
    desktop_origin_logical: (f64, f64),
    target_scale: ScaleFactor,
) -> DeviceRect {
    let logical = logical_from_device(monitor, monitor_scale);
    let x = (logical.origin.x - desktop_origin_logical.0) * target_scale.get();
    let y = (logical.origin.y - desktop_origin_logical.1) * target_scale.get();
    let w = logical.size.width * target_scale.get();
    let h = logical.size.height * target_scale.get();
    DeviceRect::from_origin_size(
        x.round() as i32,
        y.round() as i32,
        w.round() as i32,
        h.round() as i32,
    )
}

/// The bounding box of a set of logical rectangles.
///
/// Returns `None` for an empty set rather than a zero rectangle, so a caller
/// that has somehow ended up with no displays gets an error instead of a
/// zero-sized image.
#[must_use]
pub fn logical_desktop_bounds(monitors: &[LogicalRect]) -> Option<LogicalRect> {
    let mut iter = monitors.iter();
    let first = *iter.next()?;
    let mut left = first.origin.x;
    let mut top = first.origin.y;
    let mut right = first.origin.x + first.size.width;
    let mut bottom = first.origin.y + first.size.height;

    for r in iter {
        left = left.min(r.origin.x);
        top = top.min(r.origin.y);
        right = right.max(r.origin.x + r.size.width);
        bottom = bottom.max(r.origin.y + r.size.height);
    }

    Some(LogicalRect {
        origin: Point::new(left, top),
        size: Size::new(right - left, bottom - top),
    })
}

/// Which monitor a logical rectangle sits on predominantly.
///
/// Logical space is the only space a caller-supplied region can be expressed
/// in: a selection dragged across a 100% and a 150% monitor has no single pixel
/// interpretation, but it does have an unambiguous logical one.
#[must_use]
pub fn dominant_monitor_logical(rect: LogicalRect, monitors: &[LogicalRect]) -> Option<usize> {
    let grid = |r: LogicalRect| {
        DeviceRect::new(
            r.origin.x.floor() as i32,
            r.origin.y.floor() as i32,
            (r.origin.x + r.size.width).ceil() as i32,
            (r.origin.y + r.size.height).ceil() as i32,
        )
    };
    let cells: Vec<DeviceRect> = monitors.iter().copied().map(grid).collect();
    dominant_monitor(grid(rect), &cells)
}

/// Where a logical region lands inside a captured monitor image.
///
/// Takes the region into the monitor's own pixel grid: clipped to the monitor,
/// made relative to its top-left, then multiplied by that monitor's scale. This
/// is why a region capture has to know which monitor it is on — the same
/// logical rectangle is 100 pixels wide on one screen and 150 on the next.
#[must_use]
pub fn region_within_monitor(
    region: LogicalRect,
    monitor: LogicalRect,
    scale: ScaleFactor,
) -> DeviceRect {
    let left = region.origin.x.max(monitor.origin.x);
    let top = region.origin.y.max(monitor.origin.y);
    let right = (region.origin.x + region.size.width).min(monitor.origin.x + monitor.size.width);
    let bottom = (region.origin.y + region.size.height).min(monitor.origin.y + monitor.size.height);

    if right <= left || bottom <= top {
        return DeviceRect::new(0, 0, 0, 0);
    }

    let s = scale.get();
    DeviceRect::new(
        ((left - monitor.origin.x) * s).floor() as i32,
        ((top - monitor.origin.y) * s).floor() as i32,
        ((right - monitor.origin.x) * s).ceil() as i32,
        ((bottom - monitor.origin.y) * s).ceil() as i32,
    )
}
