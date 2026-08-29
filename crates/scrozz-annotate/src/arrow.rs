//! Shared arrow geometry for rendering, bounds, and hit testing.

use scrozz_core::{LogicalPoint, LogicalRect};

use crate::{ArrowStyle, Style, geom};

const CURVE_SAMPLES: usize = 28;

pub(crate) fn outline(
    from: LogicalPoint,
    to: LogicalPoint,
    style: Style,
    seed: u64,
) -> Option<Vec<LogicalPoint>> {
    let chord_x = to.x - from.x;
    let chord_y = to.y - from.y;
    let chord = chord_x.hypot(chord_y);
    if chord <= f64::EPSILON {
        return None;
    }
    let width = style.effective_stroke_width();
    let double = style.arrow_style == ArrowStyle::Double;
    let desired_head = match style.arrow_style {
        ArrowStyle::Sketch => width.mul_add(4.0, 7.0).clamp(9.0, 31.0),
        _ => width.mul_add(3.6, 6.0).clamp(8.0, 28.0),
    };
    let head_length = desired_head.min(chord * if double { 0.28 } else { 0.42 });
    let head_half = match style.arrow_style {
        ArrowStyle::Sketch => width.mul_add(2.1, 2.5),
        _ => width.mul_add(1.8, 2.0),
    }
    .min(chord * 0.35);
    let start_t = if double { head_length / chord } else { 0.0 };
    let end_t = 1.0 - head_length / chord;
    let span = (end_t - start_t).max(0.0);
    let blend_t = (head_length * 0.30 / chord).min(span * 0.24);
    let shaft_start = (start_t + blend_t).min(end_t);
    let shaft_end = (end_t - blend_t).max(shaft_start);
    let body_half = |fraction| {
        shaft_half(style.arrow_style, width, fraction)
            .min(head_half * 0.72)
            .min(chord * 0.08)
    };

    let mut left = Vec::with_capacity(CURVE_SAMPLES + 4);
    let mut right = Vec::with_capacity(CURVE_SAMPLES + 4);
    for index in 0..=CURVE_SAMPLES {
        let fraction = index as f64 / CURVE_SAMPLES as f64;
        let t = shaft_start + (shaft_end - shaft_start) * fraction;
        let point = curve_point(from, to, style, seed, t);
        let (tx, ty) = curve_tangent(from, to, style, seed, t);
        let length = tx.hypot(ty).max(f64::EPSILON);
        let normal = (-ty / length, tx / length);
        let half = body_half(fraction);
        left.push(LogicalPoint::new(
            point.x + normal.0 * half,
            point.y + normal.1 * half,
        ));
        right.push(LogicalPoint::new(
            point.x - normal.0 * half,
            point.y - normal.1 * half,
        ));
    }

    let offset_point = |t: f64, half_width: f64, side: f64| {
        let point = curve_point(from, to, style, seed, t);
        let (tx, ty) = curve_tangent(from, to, style, seed, t);
        let length = tx.hypot(ty).max(f64::EPSILON);
        LogicalPoint::new(
            point.x + (-ty / length) * half_width * side,
            point.y + (tx / length) * half_width * side,
        )
    };
    let shoulder = |t: f64, side: f64| offset_point(t, head_half, side);
    let end_flank_t = end_t + (1.0 - end_t) * 0.72;
    let start_flank_t = start_t * 0.28;
    let mut polygon = Vec::with_capacity(left.len() + right.len() + 6);
    if double {
        polygon.push(from);
        polygon.push(offset_point(start_flank_t, head_half * 0.62, 1.0));
        polygon.push(shoulder(start_t, 1.0));
    } else {
        let point = curve_point(from, to, style, seed, 0.0);
        let (tx, ty) = curve_tangent(from, to, style, seed, 0.0);
        let length = tx.hypot(ty).max(f64::EPSILON);
        let half = body_half(0.0);
        polygon.push(LogicalPoint::new(
            point.x + (-ty / length) * half,
            point.y + (tx / length) * half,
        ));
    }
    polygon.extend(left);
    polygon.push(shoulder(end_t, 1.0));
    polygon.push(offset_point(end_flank_t, head_half * 0.62, 1.0));
    polygon.push(to);
    polygon.push(offset_point(end_flank_t, head_half * 0.62, -1.0));
    polygon.push(shoulder(end_t, -1.0));
    polygon.extend(right.into_iter().rev());
    if double {
        polygon.push(shoulder(start_t, -1.0));
        polygon.push(offset_point(start_flank_t, head_half * 0.62, -1.0));
    } else {
        let point = curve_point(from, to, style, seed, 0.0);
        let (tx, ty) = curve_tangent(from, to, style, seed, 0.0);
        let length = tx.hypot(ty).max(f64::EPSILON);
        let half = body_half(0.0);
        let tangent = (tx / length, ty / length);
        let normal = (-tangent.1, tangent.0);
        polygon.push(LogicalPoint::new(
            point.x - normal.0 * half,
            point.y - normal.1 * half,
        ));
        for index in 1..4 {
            let angle = -std::f64::consts::FRAC_PI_2 + std::f64::consts::PI * index as f64 / 4.0;
            polygon.push(LogicalPoint::new(
                point.x + normal.0 * angle.sin() * half - tangent.0 * angle.cos() * half,
                point.y + normal.1 * angle.sin() * half - tangent.1 * angle.cos() * half,
            ));
        }
    }
    Some(polygon)
}

pub(crate) fn visual_bounds(
    from: LogicalPoint,
    to: LogicalPoint,
    style: Style,
    seed: u64,
) -> LogicalRect {
    outline(from, to, style, seed)
        .map(|points| {
            let mut left = f64::INFINITY;
            let mut top = f64::INFINITY;
            let mut right = f64::NEG_INFINITY;
            let mut bottom = f64::NEG_INFINITY;
            for point in points {
                left = left.min(point.x);
                top = top.min(point.y);
                right = right.max(point.x);
                bottom = bottom.max(point.y);
            }
            geom::from_edges(left, top, right, bottom)
        })
        .unwrap_or_else(|| LogicalRect::from_corners(from, to))
}

pub(crate) fn hit(
    point: LogicalPoint,
    from: LogicalPoint,
    to: LogicalPoint,
    style: Style,
    seed: u64,
    tolerance: f64,
) -> bool {
    let Some(points) = outline(from, to, style, seed) else {
        return false;
    };
    point_in_polygon(point, &points)
        || points
            .iter()
            .zip(points.iter().cycle().skip(1))
            .take(points.len())
            .any(|(a, b)| geom::distance_to_segment(point, *a, *b) <= tolerance)
}

pub(crate) fn bend_handle(from: LogicalPoint, to: LogicalPoint, style: Style) -> LogicalPoint {
    let midpoint = LogicalPoint::new((from.x + to.x) * 0.5, (from.y + to.y) * 0.5);
    let bend = style.effective_arrow_bend() * 0.5;
    LogicalPoint::new(
        midpoint.x - (to.y - from.y) * bend,
        midpoint.y + (to.x - from.x) * bend,
    )
}

fn shaft_half(style: ArrowStyle, width: f64, fraction: f64) -> f64 {
    match style {
        ArrowStyle::Bold | ArrowStyle::Curved | ArrowStyle::Double => {
            width * (0.32 + 0.23 * fraction)
        }
        ArrowStyle::Sketch => width * (0.30 + 0.24 * fraction),
    }
}

fn curve_point(
    from: LogicalPoint,
    to: LogicalPoint,
    style: Style,
    seed: u64,
    t: f64,
) -> LogicalPoint {
    let mut point = if style.effective_arrow_bend().abs() <= f64::EPSILON {
        LogicalPoint::new(
            (to.x - from.x).mul_add(t, from.x),
            (to.y - from.y).mul_add(t, from.y),
        )
    } else {
        let control = curve_control(from, to, style.effective_arrow_bend());
        let one = 1.0 - t;
        LogicalPoint::new(
            one * one * from.x + 2.0 * one * t * control.x + t * t * to.x,
            one * one * from.y + 2.0 * one * t * control.y + t * t * to.y,
        )
    };
    if style.arrow_style == ArrowStyle::Sketch {
        let dx = to.x - from.x;
        let dy = to.y - from.y;
        let length = dx.hypot(dy).max(f64::EPSILON);
        let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17);
        let phase = (mixed as f64 / u64::MAX as f64) * std::f64::consts::TAU;
        let jitter = (std::f64::consts::PI * t).sin()
            * (std::f64::consts::TAU.mul_add(t * 2.0, phase)).sin()
            * style.effective_stroke_width().min(length * 0.08)
            * 0.18;
        point.x += -dy / length * jitter;
        point.y += dx / length * jitter;
    }
    point
}

fn curve_tangent(
    from: LogicalPoint,
    to: LogicalPoint,
    style: Style,
    seed: u64,
    t: f64,
) -> (f64, f64) {
    let before = curve_point(from, to, style, seed, (t - 0.001).max(0.0));
    let after = curve_point(from, to, style, seed, (t + 0.001).min(1.0));
    (after.x - before.x, after.y - before.y)
}

fn curve_control(from: LogicalPoint, to: LogicalPoint, bend: f64) -> LogicalPoint {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    LogicalPoint::new(
        (from.x + to.x) * 0.5 - dy * bend,
        (from.y + to.y) * 0.5 + dx * bend,
    )
}

fn point_in_polygon(point: LogicalPoint, polygon: &[LogicalPoint]) -> bool {
    let mut inside = false;
    let mut previous = polygon[polygon.len() - 1];
    for &current in polygon {
        let crosses = (current.y > point.y) != (previous.y > point.y)
            && point.x
                < (previous.x - current.x) * (point.y - current.y) / (previous.y - current.y)
                    + current.x;
        inside ^= crosses;
        previous = current;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_thick_and_malformed_arrows_stay_finite_bounded_and_simple() {
        for length in [0.01, 0.1, 1.0, 3.0, 8.0, 40.0, 200.0] {
            for width in [0.5, 2.0, 14.0, 24.0, 1.0e100] {
                for arrow_style in [
                    ArrowStyle::Bold,
                    ArrowStyle::Curved,
                    ArrowStyle::Sketch,
                    ArrowStyle::Double,
                ] {
                    for bend in [-0.75, 0.0, 0.75] {
                        let from = LogicalPoint::new(0.0, 0.0);
                        let to = LogicalPoint::new(length, 0.0);
                        let style = Style::stroked()
                            .with_stroke_width(width)
                            .with_arrow_style(arrow_style)
                            .with_arrow_bend(bend);
                        let points = outline(from, to, style, 42).expect("non-zero arrow");
                        assert!(
                            points
                                .iter()
                                .all(|point| point.x.is_finite() && point.y.is_finite()),
                            "{arrow_style:?}, length={length}, width={width}, bend={bend}"
                        );
                        let bounds = visual_bounds(from, to, style, 42);
                        assert!(
                            bounds.size.width <= length * 3.0 + 0.1
                                && bounds.size.height <= length * 3.0 + 0.1,
                            "unbounded {arrow_style:?} geometry: {bounds:?}"
                        );
                        assert!(
                            !has_strict_crossing(&points),
                            "self-intersecting {arrow_style:?}, length={length}, width={width}, bend={bend}"
                        );
                    }
                }
            }
        }
    }

    fn has_strict_crossing(points: &[LogicalPoint]) -> bool {
        let edge_count = points.len();
        for first in 0..edge_count {
            let a = points[first];
            let b = points[(first + 1) % edge_count];
            for second in first + 1..edge_count {
                if second == first
                    || second == (first + 1) % edge_count
                    || first == (second + 1) % edge_count
                {
                    continue;
                }
                let c = points[second];
                let d = points[(second + 1) % edge_count];
                if strict_intersection(a, b, c, d) {
                    return true;
                }
            }
        }
        false
    }

    fn strict_intersection(
        a: LogicalPoint,
        b: LogicalPoint,
        c: LogicalPoint,
        d: LogicalPoint,
    ) -> bool {
        let cross = |p: LogicalPoint, q: LogicalPoint, r: LogicalPoint| {
            (q.x - p.x).mul_add(r.y - p.y, -(q.y - p.y) * (r.x - p.x))
        };
        let ab_c = cross(a, b, c);
        let ab_d = cross(a, b, d);
        let cd_a = cross(c, d, a);
        let cd_b = cross(c, d, b);
        ab_c * ab_d < -f64::EPSILON && cd_a * cd_b < -f64::EPSILON
    }
}
