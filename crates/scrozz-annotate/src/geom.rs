//! Small geometry helpers over `scrozz_core`'s logical types.
//!
//! These live here rather than in `scrozz-core` because they are only needed to
//! answer annotation questions — "is this click on that arrow?" — and the core
//! crate is deliberately kept to types and contracts.

use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};

/// Right edge of `rect`.
#[must_use]
pub fn max_x(rect: &LogicalRect) -> f64 {
    rect.origin.x + rect.size.width
}

/// Bottom edge of `rect`.
#[must_use]
pub fn max_y(rect: &LogicalRect) -> f64 {
    rect.origin.y + rect.size.height
}

/// Centre of `rect`.
#[must_use]
pub fn center(rect: &LogicalRect) -> LogicalPoint {
    LogicalPoint::new(
        rect.origin.x + rect.size.width / 2.0,
        rect.origin.y + rect.size.height / 2.0,
    )
}

/// A rectangle from two edges pairs, in any order.
#[must_use]
pub fn from_edges(left: f64, top: f64, right: f64, bottom: f64) -> LogicalRect {
    LogicalRect::from_corners(
        LogicalPoint::new(left, top),
        LogicalPoint::new(right, bottom),
    )
}

/// `rect` grown by `amount` on every side.
///
/// Shrinking past zero extent yields a zero-sized rectangle at the centre
/// rather than an inverted one, since [`LogicalSize`] clamps negatives away.
#[must_use]
pub fn inflate(rect: &LogicalRect, amount: f64) -> LogicalRect {
    let left = rect.origin.x - amount;
    let top = rect.origin.y - amount;
    let right = max_x(rect) + amount;
    let bottom = max_y(rect) + amount;
    if right < left || bottom < top {
        let c = center(rect);
        return LogicalRect::new(c, LogicalSize::new(0.0, 0.0));
    }
    from_edges(left, top, right, bottom)
}

/// Whether `point` lies inside `rect`, edges included.
#[must_use]
pub fn contains(rect: &LogicalRect, point: LogicalPoint) -> bool {
    point.x >= rect.origin.x
        && point.x <= max_x(rect)
        && point.y >= rect.origin.y
        && point.y <= max_y(rect)
}

/// Whether `point` lies inside the ellipse inscribed in `rect`.
#[must_use]
pub fn contains_ellipse(rect: &LogicalRect, point: LogicalPoint) -> bool {
    let rx = rect.size.width / 2.0;
    let ry = rect.size.height / 2.0;
    if rx <= 0.0 || ry <= 0.0 {
        return false;
    }
    let c = center(rect);
    let dx = (point.x - c.x) / rx;
    let dy = (point.y - c.y) / ry;
    dx.mul_add(dx, dy * dy) <= 1.0
}

/// Distance from `point` to the segment `a`–`b`.
#[must_use]
pub fn distance_to_segment(point: LogicalPoint, a: LogicalPoint, b: LogicalPoint) -> f64 {
    let vx = b.x - a.x;
    let vy = b.y - a.y;
    let len_sq = vx.mul_add(vx, vy * vy);
    if len_sq <= f64::EPSILON {
        return distance(point, a);
    }
    let t = (((point.x - a.x) * vx) + ((point.y - a.y) * vy)) / len_sq;
    let t = t.clamp(0.0, 1.0);
    distance(
        point,
        LogicalPoint::new(vx.mul_add(t, a.x), vy.mul_add(t, a.y)),
    )
}

/// Distance from `point` to the outline of `rect`.
///
/// Zero on the outline, growing in both directions — an outlined rectangle is
/// hit near its border, not across its whole interior.
#[must_use]
pub fn distance_to_rect_outline(point: LogicalPoint, rect: &LogicalRect) -> f64 {
    let corners = [
        LogicalPoint::new(rect.origin.x, rect.origin.y),
        LogicalPoint::new(max_x(rect), rect.origin.y),
        LogicalPoint::new(max_x(rect), max_y(rect)),
        LogicalPoint::new(rect.origin.x, max_y(rect)),
    ];
    let mut best = f64::MAX;
    for i in 0..4 {
        let d = distance_to_segment(point, corners[i], corners[(i + 1) % 4]);
        best = best.min(d);
    }
    best
}

/// Straight-line distance between two points.
#[must_use]
pub fn distance(a: LogicalPoint, b: LogicalPoint) -> f64 {
    (b.x - a.x).hypot(b.y - a.y)
}

/// The tight bounding box of a point list.
///
/// An empty list yields a zero rectangle at the origin.
#[must_use]
pub fn bounding_box(points: &[LogicalPoint]) -> LogicalRect {
    let Some(first) = points.first() else {
        return LogicalRect::default();
    };
    let (mut left, mut top) = (first.x, first.y);
    let (mut right, mut bottom) = (first.x, first.y);
    for p in &points[1..] {
        left = left.min(p.x);
        top = top.min(p.y);
        right = right.max(p.x);
        bottom = bottom.max(p.y);
    }
    from_edges(left, top, right, bottom)
}

/// Maps `point` from `from` to the corresponding position within `to`.
///
/// Used by resize: every annotation is resized by remapping its defining points
/// through this, so an arrow keeps its direction and freehand ink keeps its
/// shape. A degenerate source axis translates instead of dividing by zero.
#[must_use]
pub fn remap(point: LogicalPoint, from: &LogicalRect, to: &LogicalRect) -> LogicalPoint {
    let x = if from.size.width > f64::EPSILON {
        let t = (point.x - from.origin.x) / from.size.width;
        to.size.width.mul_add(t, to.origin.x)
    } else {
        point.x - from.origin.x + to.origin.x
    };
    let y = if from.size.height > f64::EPSILON {
        let t = (point.y - from.origin.y) / from.size.height;
        to.size.height.mul_add(t, to.origin.y)
    } else {
        point.y - from.origin.y + to.origin.y
    };
    LogicalPoint::new(x, y)
}
