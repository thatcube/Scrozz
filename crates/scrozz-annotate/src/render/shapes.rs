//! Path construction and painting for each annotation kind.
//!
//! Everything here is built in **physical canvas pixels**: the caller multiplies
//! logical coordinates through [`Scaled`] first, and paths are then drawn with an
//! identity transform. Doing the scaling in path space rather than handing
//! `tiny-skia` a scaling transform is what makes rendering at 2× exactly twice
//! rendering at 1× — including stroke widths, arrowhead sizes and glyph weight,
//! all of which would otherwise be scaled by a different rule than the geometry.

use scrozz_core::{LogicalPoint, LogicalRect};
use tiny_skia::{
    BlendMode, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Rect, Shader, Stroke,
    Transform,
};

use crate::{
    annotation::AnnotationObject,
    document::CanvasGeometry,
    font, geom,
    style::{ArrowStyle, Color, Style, TextPreset},
};

/// Converts logical annotation coordinates into physical canvas pixels.
#[derive(Debug, Clone, Copy)]
pub struct Scaled {
    scale: f64,
    geometry: Option<CanvasGeometry>,
}

impl Scaled {
    /// A converter for a canvas rendered at `scale` physical pixels per point.
    #[must_use]
    pub const fn new(scale: f64) -> Self {
        Self {
            scale,
            geometry: None,
        }
    }

    /// A converter that also applies a document's canvas transform.
    #[must_use]
    pub const fn for_canvas(scale: f64, geometry: CanvasGeometry) -> Self {
        Self {
            scale,
            geometry: Some(geometry),
        }
    }

    /// The scale factor.
    #[must_use]
    pub const fn factor(self) -> f64 {
        self.scale
    }

    /// A point, in canvas pixels.
    #[must_use]
    pub fn point(self, p: LogicalPoint) -> (f32, f32) {
        let p = self
            .geometry
            .map_or(p, |geometry| geometry.source_to_canvas(p));
        ((p.x * self.scale) as f32, (p.y * self.scale) as f32)
    }

    /// A length, in canvas pixels.
    #[must_use]
    pub fn length(self, v: f64) -> f32 {
        (v * self.scale) as f32
    }

    /// A rectangle, in canvas pixels, or `None` if it encloses no area.
    #[must_use]
    pub fn rect(self, r: &LogicalRect) -> Option<Rect> {
        let corners = [
            r.origin,
            LogicalPoint::new(geom::max_x(r), r.origin.y),
            LogicalPoint::new(geom::max_x(r), geom::max_y(r)),
            LogicalPoint::new(r.origin.x, geom::max_y(r)),
        ];
        let (first_x, first_y) = self.point(corners[0]);
        let mut left = first_x;
        let mut right = first_x;
        let mut top = first_y;
        let mut bottom = first_y;
        for corner in &corners[1..] {
            let (x, y) = self.point(*corner);
            left = left.min(x);
            right = right.max(x);
            top = top.min(y);
            bottom = bottom.max(y);
        }
        Rect::from_ltrb(left, top, right, bottom)
    }

    /// A tiny-skia transform from logical points into canvas pixels.
    #[must_use]
    pub fn transform(self) -> Transform {
        let origin = self.point(LogicalPoint::new(0.0, 0.0));
        let x_axis = self.point(LogicalPoint::new(1.0, 0.0));
        let y_axis = self.point(LogicalPoint::new(0.0, 1.0));
        Transform::from_row(
            x_axis.0 - origin.0,
            x_axis.1 - origin.1,
            y_axis.0 - origin.0,
            y_axis.1 - origin.1,
            origin.0,
            origin.1,
        )
    }
}

/// A solid-colour paint with `opacity` folded into its alpha.
#[must_use]
pub fn paint(color: Color, opacity: f32, blend_mode: BlendMode) -> Paint<'static> {
    let c = color.scaled_alpha(opacity);
    let mut paint = Paint {
        anti_alias: true,
        blend_mode,
        ..Paint::default()
    };
    paint.shader = Shader::SolidColor(tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a));
    paint
}

/// A round-capped, round-joined stroke of `width` canvas pixels.
///
/// Round caps and joins throughout: annotation strokes are freehand-ish marks,
/// and mitred joins on a hand-drawn polyline produce spikes at sharp corners.
#[must_use]
pub fn stroke(width: f32) -> Stroke {
    Stroke {
        width: width.max(0.1),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    }
}

/// Strokes a path, if it has one.
pub fn stroke_path(pixmap: &mut Pixmap, path: &Path, paint: &Paint<'_>, width: f32) {
    pixmap.stroke_path(path, paint, &stroke(width), Transform::identity(), None);
}

/// Strokes a path with explicit stroke properties and an optional transform.
pub fn stroke_path_with(
    pixmap: &mut Pixmap,
    path: &Path,
    paint: &Paint<'_>,
    stroke: &Stroke,
    transform: Transform,
) {
    pixmap.stroke_path(path, paint, stroke, transform, None);
}

/// Fills a path.
pub fn fill_path(pixmap: &mut Pixmap, path: &Path, paint: &Paint<'_>) {
    pixmap.fill_path(path, paint, FillRule::Winding, Transform::identity(), None);
}

/// Fills a path through an additional transform.
pub fn fill_path_with(pixmap: &mut Pixmap, path: &Path, paint: &Paint<'_>, transform: Transform) {
    pixmap.fill_path(path, paint, FillRule::Winding, transform, None);
}

/// The shaft and one or two heads of an arrow.
///
/// The head is a filled triangle whose length and width both scale with stroke
/// width, so a hairline arrow does not end in a pinpoint and a heavy one does
/// not end in a blob. The shaft stops short of the head's base rather than
/// running through it, which would show as a bump on a translucent arrow.
#[must_use]
pub fn arrow(
    object: &AnnotationObject,
    from: LogicalPoint,
    to: LogicalPoint,
    xf: Scaled,
) -> Option<(Path, Vec<Path>)> {
    let (x0, y0) = xf.point(from);
    let (x1, y1) = xf.point(to);
    let (dx, dy) = (x1 - x0, y1 - y0);
    let length = dx.hypot(dy);
    if length <= f32::EPSILON {
        return None;
    }
    let (ux, uy) = (dx / length, dy / length);

    // Never let the head eat the whole arrow: a very short arrow becomes mostly
    // head, but must still read as an arrow rather than as a stray triangle.
    let head_length = xf.length(object.arrow_head_length()).min(length * 0.6);
    let half_width = xf.length(object.arrow_head_half_width());

    let mut heads = Vec::with_capacity(2);
    let mut shaft = PathBuilder::new();

    match object.style.arrow_style {
        ArrowStyle::Curved => {
            let control = geom::curved_arrow_control(from, to);
            let (cx, cy) = xf.point(control);
            let (tdx, tdy) = (x1 - cx, y1 - cy);
            let tangent_length = tdx.hypot(tdy).max(f32::EPSILON);
            let (tux, tuy) = (tdx / tangent_length, tdy / tangent_length);
            let base_x = tux.mul_add(-head_length, x1);
            let base_y = tuy.mul_add(-head_length, y1);
            shaft.move_to(x0, y0);
            shaft.quad_to(
                cx,
                cy,
                tux.mul_add(head_length * 0.35, base_x),
                tuy.mul_add(head_length * 0.35, base_y),
            );
            heads.push(arrow_head(x1, y1, tux, tuy, head_length, half_width)?);
        }
        ArrowStyle::Straight | ArrowStyle::Dashed | ArrowStyle::DoubleEnded => {
            let end_x = ux.mul_add(-head_length * 0.65, x1);
            let end_y = uy.mul_add(-head_length * 0.65, y1);
            let (start_x, start_y) = if object.style.arrow_style == ArrowStyle::DoubleEnded {
                (
                    ux.mul_add(head_length * 0.65, x0),
                    uy.mul_add(head_length * 0.65, y0),
                )
            } else {
                (x0, y0)
            };
            shaft.move_to(start_x, start_y);
            shaft.line_to(end_x, end_y);
            heads.push(arrow_head(x1, y1, ux, uy, head_length, half_width)?);
            if object.style.arrow_style == ArrowStyle::DoubleEnded {
                heads.push(arrow_head(x0, y0, -ux, -uy, head_length, half_width)?);
            }
        }
    }

    Some((shaft.finish()?, heads))
}

fn arrow_head(
    tip_x: f32,
    tip_y: f32,
    ux: f32,
    uy: f32,
    length: f32,
    half_width: f32,
) -> Option<Path> {
    let base_x = ux.mul_add(-length, tip_x);
    let base_y = uy.mul_add(-length, tip_y);
    let (px, py) = (-uy, ux);
    let mut head = PathBuilder::new();
    head.move_to(tip_x, tip_y);
    head.line_to(
        px.mul_add(half_width, base_x),
        py.mul_add(half_width, base_y),
    );
    head.line_to(
        px.mul_add(-half_width, base_x),
        py.mul_add(-half_width, base_y),
    );
    head.close();
    head.finish()
}

/// A straight line.
#[must_use]
pub fn line(from: LogicalPoint, to: LogicalPoint, xf: Scaled) -> Option<Path> {
    let (x0, y0) = xf.point(from);
    let (x1, y1) = xf.point(to);
    if (x1 - x0).hypot(y1 - y0) <= f32::EPSILON {
        return None;
    }
    let mut path = PathBuilder::new();
    path.move_to(x0, y0);
    path.line_to(x1, y1);
    path.finish()
}

/// A rectangle path.
#[must_use]
pub fn rectangle(rect: &LogicalRect, xf: Scaled) -> Option<Path> {
    let r = xf.rect(rect)?;
    let mut builder = PathBuilder::new();
    builder.push_rect(r);
    builder.finish()
}

/// A rounded-rectangle path, with the radius clamped to what fits.
#[must_use]
pub fn rounded_rect(rect: Rect, radius: f32) -> Option<Path> {
    let radius = radius
        .max(0.0)
        .min(rect.width() / 2.0)
        .min(rect.height() / 2.0);
    if radius <= 0.0 {
        let mut builder = PathBuilder::new();
        builder.push_rect(rect);
        return builder.finish();
    }
    // Circular-arc approximation constant for a cubic Bezier: the control
    // points sit `kappa * radius` from each tangent point towards the corner.
    const KAPPA: f32 = 0.552_284_8;
    let k = radius * (1.0 - KAPPA);
    let (l, t, r, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());

    let mut p = PathBuilder::new();
    p.move_to(l + radius, t);
    p.line_to(r - radius, t);
    p.cubic_to(r - k, t, r, t + k, r, t + radius);
    p.line_to(r, b - radius);
    p.cubic_to(r, b - k, r - k, b, r - radius, b);
    p.line_to(l + radius, b);
    p.cubic_to(l + k, b, l, b - k, l, b - radius);
    p.line_to(l, t + radius);
    p.cubic_to(l, t + k, l + k, t, l + radius, t);
    p.close();
    p.finish()
}

/// An ellipse path inscribed in `rect`.
#[must_use]
pub fn ellipse(rect: &LogicalRect, xf: Scaled) -> Option<Path> {
    let r = xf.rect(rect)?;
    let mut builder = PathBuilder::new();
    builder.push_oval(r);
    builder.finish()
}

/// A smoothed path through a freehand point list.
///
/// Raw pointer samples are jagged and unevenly spaced, and joining them with
/// straight segments looks like a seismograph rather than ink. This fits a
/// Catmull-Rom spline through the samples and converts each span to a cubic
/// Bezier, which passes exactly through every captured point while curving
/// smoothly between them.
#[must_use]
pub fn freehand(points: &[LogicalPoint], xf: Scaled) -> Option<Path> {
    let pts: Vec<(f32, f32)> = dedupe(points, xf);
    match pts.len() {
        0 => return None,
        1 => {
            // A single tap still has to leave a mark, or the user's dot vanishes.
            let (x, y) = pts[0];
            let mut b = PathBuilder::new();
            b.move_to(x, y);
            b.line_to(x + 0.01, y);
            return b.finish();
        }
        2 => {
            let mut b = PathBuilder::new();
            b.move_to(pts[0].0, pts[0].1);
            b.line_to(pts[1].0, pts[1].1);
            return b.finish();
        }
        _ => {}
    }

    // Midpoint quadratic smoothing: each interior sample becomes the control
    // point of a quadratic that runs between the midpoints of its two adjacent
    // chords. Because every control point is either a sample or the midpoint of
    // two samples, each curve segment lies inside the convex hull of the points
    // the user actually drew, so the ink can never bulge outside the stroke.
    //
    // An interpolating spline (Catmull-Rom) was tried first and rejected: to stay
    // C1 continuous its tangent at a corner has to average both directions, which
    // forces the curve to overshoot the straight run leading into the corner. That
    // is correct for control points and wrong for pen samples, which are noisy and
    // want approximation rather than interpolation.
    let mut b = PathBuilder::new();
    b.move_to(pts[0].0, pts[0].1);
    for i in 1..pts.len() - 1 {
        let ctrl = pts[i];
        let next = pts[i + 1];
        let mid = (f32::midpoint(ctrl.0, next.0), f32::midpoint(ctrl.1, next.1));
        b.quad_to(ctrl.0, ctrl.1, mid.0, mid.1);
    }
    // The pen lifted at the last sample, so the stroke has to end exactly there.
    let last = pts[pts.len() - 1];
    b.line_to(last.0, last.1);
    b.finish()
}

/// A highlight is a plain rectangle; it is the blend mode that makes it read as
/// a marker rather than as paint.
#[must_use]
pub fn highlight(rect: &LogicalRect, xf: Scaled) -> Option<Path> {
    rectangle(rect, xf)
}

/// The filled Inter outlines for a block of text.
#[must_use]
pub fn text(content: &str, at: LogicalPoint, style: &Style, xf: Scaled) -> Option<Path> {
    font::outline(content, at, style.effective_font_size(), style.text_preset)?
        .transform(xf.transform())
}

/// The circle and the numeral of a counter marker.
#[must_use]
pub fn counter(
    object: &AnnotationObject,
    at: LogicalPoint,
    index: u32,
    xf: Scaled,
) -> Option<(Path, Option<Path>)> {
    let (cx, cy) = xf.point(at);
    let radius = xf.length(object.counter_radius());
    let disc = PathBuilder::from_circle(cx, cy, radius)?;

    // The numeral is sized from the disc, not from the style's font size, so a
    // two-digit marker still fits inside its circle.
    let label = index.to_string();
    let font_size = object.counter_radius() * 1.15 / (1.0 + 0.45 * (label.len() as f64 - 1.0));
    let size = font::measure(&label, font_size);
    let origin = LogicalPoint::new(at.x - size.width / 2.0, at.y - font_size / 2.0);
    let glyphs = font::outline(&label, origin, font_size, TextPreset::Standard)
        .and_then(|path| path.transform(xf.transform()));
    Some((disc, glyphs))
}

/// Drops samples that land on the same canvas pixel as their predecessor.
///
/// A stationary pointer emits a burst of identical samples; left in, they give
/// the spline zero-length chords and produce visible kinks.
fn dedupe(points: &[LogicalPoint], xf: Scaled) -> Vec<(f32, f32)> {
    let mut out: Vec<(f32, f32)> = Vec::with_capacity(points.len());
    for p in points {
        let q = xf.point(*p);
        if out
            .last()
            .is_some_and(|last| (last.0 - q.0).abs() < 0.01 && (last.1 - q.1).abs() < 0.01)
        {
            continue;
        }
        out.push(q);
    }
    out
}
