//! Pure mixed-DPI region resolution for Windows recording.

use scrozz_core::LogicalRect;

/// One monitor's identity and virtual-desktop geometry.
#[derive(Debug, Clone, Copy)]
pub struct MonitorGeometry<'a> {
    /// Stable `\\.\DISPLAYn` device name.
    pub id: &'a str,
    /// Physical left edge in the Windows virtual desktop.
    pub left: i32,
    /// Physical top edge in the Windows virtual desktop.
    pub top: i32,
    /// Physical right edge in the Windows virtual desktop.
    pub right: i32,
    /// Physical bottom edge in the Windows virtual desktop.
    pub bottom: i32,
    /// Physical pixels per logical point.
    pub scale: f64,
}

/// A crop relative to one captured monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionCrop {
    /// Index of the monitor whose identity resolved the request.
    pub monitor_index: usize,
    /// Left edge relative to that monitor in physical pixels.
    pub left: u32,
    /// Top edge relative to that monitor in physical pixels.
    pub top: u32,
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

/// Why a coordinate-only region cannot be mapped to one monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionError {
    /// The request itself or an enumerated monitor is malformed.
    InvalidGeometry,
    /// No connected monitor overlaps the request.
    NoDisplay,
    /// More than one monitor can interpret the same logical request.
    AmbiguousDisplays(Vec<String>),
    /// One monitor overlaps the request, but does not contain all of it.
    CrossesDisplay(String),
}

/// Resolves a region only when one monitor identity owns the entire transform.
///
/// Dividing each physical monitor origin by its own scale can create overlaps
/// in the coordinate-only logical desktop. Such a rectangle has no trustworthy
/// display identity, so it is rejected rather than silently captured from the
/// wrong monitor.
pub fn resolve_region(
    region: LogicalRect,
    monitors: &[MonitorGeometry<'_>],
) -> std::result::Result<RegionCrop, RegionError> {
    if !valid_region(region) {
        return Err(RegionError::InvalidGeometry);
    }

    let overlaps: Vec<usize> = monitors
        .iter()
        .enumerate()
        .filter_map(|(index, monitor)| {
            valid_monitor(monitor)
                .then(|| logical_bounds(monitor))
                .filter(|bounds| intersection(region, *bounds).is_some())
                .map(|_| index)
        })
        .collect();

    let [monitor_index] = overlaps.as_slice() else {
        return match overlaps.as_slice() {
            [] => Err(RegionError::NoDisplay),
            _ => Err(RegionError::AmbiguousDisplays(
                overlaps
                    .iter()
                    .map(|index| monitors[*index].id.to_owned())
                    .collect(),
            )),
        };
    };
    let monitor = &monitors[*monitor_index];
    let bounds = logical_bounds(monitor);
    if !contains(bounds, region) {
        return Err(RegionError::CrossesDisplay(monitor.id.to_owned()));
    }

    let left = relative_physical_edge(region.origin.x, monitor.scale, monitor.left).floor();
    let top = relative_physical_edge(region.origin.y, monitor.scale, monitor.top).floor();
    let right = relative_physical_edge(
        region.origin.x + region.size.width,
        monitor.scale,
        monitor.left,
    )
    .ceil();
    let bottom = relative_physical_edge(
        region.origin.y + region.size.height,
        monitor.scale,
        monitor.top,
    )
    .ceil();
    let left = nonnegative_u32(left);
    let top = nonnegative_u32(top);
    let right = nonnegative_u32(right);
    let bottom = nonnegative_u32(bottom);

    Ok(RegionCrop {
        monitor_index: *monitor_index,
        left: left.min(right),
        top: top.min(bottom),
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
    })
}

fn valid_region(region: LogicalRect) -> bool {
    [
        region.origin.x,
        region.origin.y,
        region.size.width,
        region.size.height,
    ]
    .into_iter()
    .all(f64::is_finite)
        && region.size.width > 0.0
        && region.size.height > 0.0
}

fn valid_monitor(monitor: &MonitorGeometry<'_>) -> bool {
    monitor.scale.is_finite()
        && monitor.scale > 0.0
        && monitor.right > monitor.left
        && monitor.bottom > monitor.top
}

fn logical_bounds(monitor: &MonitorGeometry<'_>) -> LogicalRect {
    LogicalRect {
        origin: scrozz_core::Point::new(
            f64::from(monitor.left) / monitor.scale,
            f64::from(monitor.top) / monitor.scale,
        ),
        size: scrozz_core::Size::new(
            (f64::from(monitor.right) - f64::from(monitor.left)) / monitor.scale,
            (f64::from(monitor.bottom) - f64::from(monitor.top)) / monitor.scale,
        ),
    }
}

fn contains(outer: LogicalRect, inner: LogicalRect) -> bool {
    greater_or_same(inner.origin.x, outer.origin.x)
        && greater_or_same(inner.origin.y, outer.origin.y)
        && less_or_same(
            inner.origin.x + inner.size.width,
            outer.origin.x + outer.size.width,
        )
        && less_or_same(
            inner.origin.y + inner.size.height,
            outer.origin.y + outer.size.height,
        )
}

fn intersection(a: LogicalRect, b: LogicalRect) -> Option<LogicalRect> {
    let left = a.origin.x.max(b.origin.x);
    let top = a.origin.y.max(b.origin.y);
    let right = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let bottom = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    (right > left && bottom > top).then(|| LogicalRect {
        origin: scrozz_core::Point::new(left, top),
        size: scrozz_core::Size::new(right - left, bottom - top),
    })
}

fn nonnegative_u32(value: f64) -> u32 {
    value.max(0.0) as u32
}

fn greater_or_same(value: f64, edge: f64) -> bool {
    value >= edge || same_edge(value, edge)
}

fn less_or_same(value: f64, edge: f64) -> bool {
    value <= edge || same_edge(value, edge)
}

fn same_edge(a: f64, b: f64) -> bool {
    let magnitude = a.abs().max(b.abs()).max(1.0);
    (a - b).abs() <= f64::EPSILON * magnitude * 8.0
}

fn relative_physical_edge(logical: f64, scale: f64, physical_origin: i32) -> f64 {
    let global = logical * scale;
    let relative = global - f64::from(physical_origin);
    let nearest = relative.round();
    let magnitude = global.abs().max(f64::from(physical_origin).abs()).max(1.0);
    if (relative - nearest).abs() <= f64::EPSILON * magnitude * 16.0 {
        nearest
    } else {
        relative
    }
}
