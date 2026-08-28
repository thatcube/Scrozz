//! Path construction and painting for each annotation kind.
//!
//! Everything here is built in **physical canvas pixels**: the caller multiplies
//! logical coordinates through [`Scaled`] first, and paths are then drawn with an
//! identity transform. Doing the scaling in path space rather than handing
//! `tiny-skia` a scaling transform is what makes rendering at 2× exactly twice
//! rendering at 1× — including stroke widths, arrowhead sizes and glyph weight,
//! all of which would otherwise be scaled by a different rule than the geometry.

use scrozz_core::{LogicalPoint, LogicalRect, Transform as ColorTransform};
use tiny_skia::{
    BlendMode, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Rect, Shader, Stroke,
    Transform,
};

use crate::{
    annotation::AnnotationObject,
    font, geom,
    style::{Color, Style},
};

/// Converts logical annotation coordinates into physical canvas pixels.
///
/// The origin is the logical point that lands on the canvas's top-left corner.
/// It is non-zero when the document is cropped, and carrying it here — rather
/// than pre-translating every annotation — is what keeps crop non-destructive:
/// the annotations still hold their original coordinates, and only the view of
/// them moves.
#[derive(Debug, Clone, Copy)]
pub struct Scaled {
    scale: f64,
    origin: LogicalPoint,
}

impl Scaled {
    /// A converter for a canvas rendered at `scale` physical pixels per point,
    /// showing the document from its own origin.
    #[must_use]
    pub fn new(scale: f64) -> Self {
        Self {
            scale,
            origin: LogicalPoint::new(0.0, 0.0),
        }
    }

    /// A converter for a canvas whose top-left corner is `origin`.
    #[must_use]
    pub const fn with_origin(scale: f64, origin: LogicalPoint) -> Self {
        Self { scale, origin }
    }

    /// The scale factor.
    #[must_use]
    pub const fn factor(self) -> f64 {
        self.scale
    }

    /// A horizontal coordinate, in canvas pixels.
    #[must_use]
    pub fn x(self, v: f64) -> f32 {
        ((v - self.origin.x) * self.scale) as f32
    }

    /// A vertical coordinate, in canvas pixels.
    #[must_use]
    pub fn y(self, v: f64) -> f32 {
        ((v - self.origin.y) * self.scale) as f32
    }

    /// A point, in canvas pixels.
    #[must_use]
    pub fn point(self, p: LogicalPoint) -> (f32, f32) {
        (self.x(p.x), self.y(p.y))
    }

    /// A length, in canvas pixels.
    ///
    /// Deliberately *not* origin-shifted: a stroke width is a distance, not a
    /// position, and translating one would make every stroke change thickness
    /// as soon as the document was cropped.
    #[must_use]
    pub fn length(self, v: f64) -> f32 {
        (v * self.scale) as f32
    }

    /// A rectangle, in canvas pixels, or `None` if it encloses no area.
    #[must_use]
    pub fn rect(self, r: &LogicalRect) -> Option<Rect> {
        Rect::from_ltrb(
            self.x(r.origin.x),
            self.y(r.origin.y),
            self.x(geom::max_x(r)),
            self.y(geom::max_y(r)),
        )
    }
}

/// An annotation colour moved into the working space.
///
/// Annotation colours are authored in sRGB — the swatch the user picked is an
/// sRGB triple — but the canvas is in the *capture's* space, which for a modern
/// Mac screenshot is Display P3. Writing sRGB bytes into a P3 buffer paints a
/// visibly more saturated colour than the one chosen, and makes the same
/// annotation look different depending on which display it was captured from.
/// The conversion is a no-op for an sRGB or unknown source, which is the common
/// case, and costs a handful of operations per *annotation* either way — this is
/// nowhere near the per-pixel path.
///
/// Everything that puts a user-chosen colour onto the canvas goes through here,
/// including the ones that bypass [`paint`] because they write pixels directly.
#[must_use]
pub fn working(color: Color, into: ColorTransform) -> Color {
    let [r, g, b] = into.convert_u8([color.r, color.g, color.b]);
    Color::rgba(r, g, b, color.a)
}

/// A solid-colour paint with `opacity` folded into its alpha.
///
/// `into` converts the colour into the working space before it is painted; see
/// [`working`].
#[must_use]
pub fn paint(
    color: Color,
    opacity: f32,
    blend_mode: BlendMode,
    into: ColorTransform,
) -> Paint<'static> {
    let c = working(color.scaled_alpha(opacity), into);
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

/// Fills a path.
pub fn fill_path(pixmap: &mut Pixmap, path: &Path, paint: &Paint<'_>) {
    pixmap.fill_path(path, paint, FillRule::Winding, Transform::identity(), None);
}

/// The shaft and head of an arrow, as two paths.
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
) -> Option<(Path, Path)> {
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

    let base_x = ux.mul_add(-head_length, x1);
    let base_y = uy.mul_add(-head_length, y1);
    let (px, py) = (-uy, ux);

    let mut shaft = PathBuilder::new();
    shaft.move_to(x0, y0);
    // Overlap the head's base slightly so no seam shows between them.
    shaft.line_to(
        ux.mul_add(head_length * 0.35, base_x),
        uy.mul_add(head_length * 0.35, base_y),
    );

    let mut head = PathBuilder::new();
    head.move_to(x1, y1);
    head.line_to(
        px.mul_add(half_width, base_x),
        py.mul_add(half_width, base_y),
    );
    head.line_to(
        px.mul_add(-half_width, base_x),
        py.mul_add(-half_width, base_y),
    );
    head.close();

    Some((shaft.finish()?, head.finish()?))
}

/// A straight line path between two points.
///
/// `None` for a zero-length line: a round-capped stroke of no length would
/// paint a dot, which is not what "I dragged nowhere" should leave behind.
#[must_use]
pub fn line(from: LogicalPoint, to: LogicalPoint, xf: Scaled) -> Option<Path> {
    let (x0, y0) = xf.point(from);
    let (x1, y1) = xf.point(to);
    if (x1 - x0).hypot(y1 - y0) <= f32::EPSILON {
        return None;
    }
    let mut builder = PathBuilder::new();
    builder.move_to(x0, y0);
    builder.line_to(x1, y1);
    builder.finish()
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

/// The stroked outlines for a block of text.
#[must_use]
pub fn text(content: &str, at: LogicalPoint, style: &Style, xf: Scaled) -> Option<Path> {
    let strokes = font::outline(content, at, style.effective_font_size());
    polylines(&strokes, xf)
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
    let glyphs = font::outline(&label, origin, font_size);
    Some((disc, polylines(&glyphs, xf)))
}

/// Combines a set of logical polylines into one path.
fn polylines(strokes: &[Vec<LogicalPoint>], xf: Scaled) -> Option<Path> {
    let mut b = PathBuilder::new();
    let mut any = false;
    for stroke in strokes {
        if stroke.len() < 2 {
            continue;
        }
        let (x, y) = xf.point(stroke[0]);
        b.move_to(x, y);
        for p in &stroke[1..] {
            let (x, y) = xf.point(*p);
            b.line_to(x, y);
        }
        any = true;
    }
    if any { b.finish() } else { None }
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
