//! Compositing a document to pixels.

pub mod beautify;
pub mod raster;
pub mod redact;
pub mod shapes;

use scrozz_core::{
    ColorSpace, Error, Frame, LogicalRect, Result, ScaleFactor, Transform as ColorTransform,
};
use tiny_skia::{BlendMode, Pixmap, Transform};

use crate::{
    annotation::{Annotation, AnnotationObject, RedactStyle},
    document::Document,
    style::Color,
};

use shapes::Scaled;

/// Renders a document to pixels.
pub trait Renderer {
    /// Composites annotations and framing over the source.
    ///
    /// # Errors
    ///
    /// Returns an error if rendering failed.
    fn render(&self, document: &Document) -> Result<Frame>;
}

/// The `tiny-skia` renderer.
///
/// Stateless and cheap to construct; rendering allocates its own canvas each
/// time and never mutates the document or its source.
#[derive(Debug, Clone, Copy, Default)]
pub struct SkiaRenderer;

impl SkiaRenderer {
    /// A renderer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Composites the document at an explicit output scale.
    ///
    /// Annotations are authored in logical points, so the same document renders
    /// correctly at 1×, at 2×, and at any export size in between or beyond.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the source frame is malformed, if
    /// the output would be zero-sized or unallocatable, or if the document
    /// carries beautification for a window capture — see decision D9.
    pub fn render_at(&self, document: &Document, scale: ScaleFactor) -> Result<Frame> {
        // D9, second gate. `Document::set_beautification` refuses this too, but
        // a document can also arrive from persistence or from a future editing
        // path, and shipping a re-shadowed window capture must be impossible
        // rather than merely unlikely.
        if document.beautification().is_some() && !document.may_beautify() {
            return Err(Error::InvalidRequest(
                "beautification is not permitted for window captures (decision D9)".to_owned(),
            ));
        }

        let source = raster::to_pixmap(&document.source.frame)?;
        let content = document.content_bounds();
        let width = (content.size.width * scale.get()).round().max(1.0) as u32;
        let height = (content.size.height * scale.get()).round().max(1.0) as u32;

        let mut canvas = Pixmap::new(width, height).ok_or_else(|| {
            Error::InvalidRequest(format!("output size {width}x{height} is not renderable"))
        })?;
        // One working space for the whole composite: the capture's own. Source
        // pixels are already in it and are never resampled; only the
        // annotations, authored in sRGB, have to be converted on the way in.
        let into = ColorTransform::new(ColorSpace::Srgb, document.source.frame.color_space);
        let xf = Scaled::with_origin(scale.get(), content.origin);
        draw_source(&mut canvas, &source, document.logical_bounds(), xf);

        for object in document.annotations() {
            draw_object(&mut canvas, object, xf, into);
        }

        let canvas = match document.beautification() {
            Some(beautification) => beautify::apply(&canvas, beautification, scale.get(), into)?,
            None => canvas,
        };

        Ok(raster::from_pixmap(
            canvas,
            document.source.frame.color_space,
            scale,
        ))
    }

    /// Composites the document scaled so its output is `width` pixels wide.
    ///
    /// # Errors
    ///
    /// As [`Self::render_at`], plus [`Error::InvalidRequest`] if `width` is zero
    /// or the source has no width to scale from.
    pub fn render_to_width(&self, document: &Document, width: u32) -> Result<Frame> {
        if width == 0 {
            return Err(Error::InvalidRequest(
                "export width must be greater than zero".to_owned(),
            ));
        }
        let logical = document.content_size();
        if logical.width <= 0.0 {
            return Err(Error::InvalidRequest(
                "cannot scale a source with no width".to_owned(),
            ));
        }
        self.render_at(document, ScaleFactor::new(f64::from(width) / logical.width))
    }
}

impl Renderer for SkiaRenderer {
    /// Composites at the source's own scale, which is the lossless default.
    fn render(&self, document: &Document) -> Result<Frame> {
        self.render_at(document, document.source.frame.scale)
    }
}

/// Draws the source image into the canvas, positioned and scaled by `xf`.
///
/// `bounds` is the source's own logical rectangle; mapping it through the same
/// transform the annotations use is what makes a crop line up exactly with the
/// marks that were drawn over it.
fn draw_source(canvas: &mut Pixmap, source: &Pixmap, bounds: LogicalRect, xf: Scaled) {
    let target_w = f64::from(xf.length(bounds.size.width));
    let target_h = f64::from(xf.length(bounds.size.height));
    let sx = target_w / f64::from(source.width());
    let sy = target_h / f64::from(source.height());
    let quality = if (sx - 1.0).abs() < 1e-9 && (sy - 1.0).abs() < 1e-9 {
        // Exact 1:1 must be a byte-for-byte copy. Any filter here would soften
        // a native-resolution export, which is the common case.
        tiny_skia::FilterQuality::Nearest
    } else {
        tiny_skia::FilterQuality::Bilinear
    };
    let (dx, dy) = xf.point(bounds.origin);
    canvas.draw_pixmap(
        0,
        0,
        source.as_ref(),
        &tiny_skia::PixmapPaint {
            quality,
            ..tiny_skia::PixmapPaint::default()
        },
        Transform::from_scale(sx as f32, sy as f32).post_translate(dx, dy),
        None,
    );
}

/// Draws one annotation onto the canvas.
///
/// Redactions are handled in z-order like everything else, and destroy whatever
/// has been composited beneath them — including earlier annotations. That is the
/// stricter reading and the safer one: a redaction always erases what it covers.
fn draw_object(canvas: &mut Pixmap, object: &AnnotationObject, xf: Scaled, into: ColorTransform) {
    let opacity = object.style.effective_opacity();
    if opacity <= 0.0 {
        return;
    }
    let width = xf.length(object.style.effective_stroke_width());

    match &object.annotation {
        Annotation::Arrow { from, to } => {
            let Some((shaft, head)) = shapes::arrow(object, *from, *to, xf) else {
                return;
            };
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver, into);
            shapes::stroke_path(canvas, &shaft, &paint, width);
            shapes::fill_path(canvas, &head, &paint);
        }
        Annotation::Line { from, to } => {
            let Some(path) = shapes::line(*from, *to, xf) else {
                return;
            };
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver, into);
            shapes::stroke_path(canvas, &path, &paint, width);
        }
        Annotation::Rectangle(rect) => {
            let Some(path) = shapes::rectangle(rect, xf) else {
                return;
            };
            fill_then_stroke(canvas, &path, object, opacity, width, into);
        }
        Annotation::Ellipse(rect) => {
            let Some(path) = shapes::ellipse(rect, xf) else {
                return;
            };
            fill_then_stroke(canvas, &path, object, opacity, width, into);
        }
        Annotation::Freehand(points) => {
            let Some(path) = shapes::freehand(points, xf) else {
                return;
            };
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver, into);
            shapes::stroke_path(canvas, &path, &paint, width);
        }
        Annotation::Text { at, content } => {
            let Some(path) = shapes::text(content, *at, &object.style, xf) else {
                return;
            };
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver, into);
            // Type weight is a fraction of cap height, not the shape stroke
            // width: an 18pt label drawn with a 12pt shape stroke is a blob.
            let weight = xf
                .length(object.style.effective_font_size() * 0.12)
                .max(1.0);
            shapes::stroke_path(canvas, &path, &paint, weight);
        }
        Annotation::Counter { at, index } => {
            let Some((disc, label)) = shapes::counter(object, *at, *index, xf) else {
                return;
            };
            let fill = object.style.fill.unwrap_or(object.style.stroke);
            let disc_paint = shapes::paint(fill, opacity, BlendMode::SourceOver, into);
            shapes::fill_path(canvas, &disc, &disc_paint);
            if let Some(label) = label {
                // Pick black or white against the disc so the numeral stays
                // legible whatever colour the user chose.
                let ink = shapes::paint(fill.contrasting(), opacity, BlendMode::SourceOver, into);
                let weight = xf.length(object.counter_radius() * 0.2).max(1.0);
                shapes::stroke_path(canvas, &label, &ink, weight);
            }
        }
        Annotation::Highlight(rect) => {
            let Some(path) = shapes::highlight(rect, xf) else {
                return;
            };
            // Multiply, not source-over: a highlighter darkens what is under it
            // and leaves dark text readable, where a translucent overlay would
            // wash it out towards the highlight colour.
            let color = object.style.fill.unwrap_or(object.style.stroke);
            let paint = shapes::paint(color, opacity, BlendMode::Multiply, into);
            shapes::fill_path(canvas, &path, &paint);
        }
        Annotation::Redact { area, style } => {
            let Some(region) = redact::clip(
                canvas,
                xf.x(area.origin.x),
                xf.y(area.origin.y),
                xf.x(crate::geom::max_x(area)),
                xf.y(crate::geom::max_y(area)),
            ) else {
                return;
            };
            match style {
                RedactStyle::Blur => redact::blur(canvas, region),
                RedactStyle::Pixelate => redact::pixelate(canvas, region),
                RedactStyle::Solid => {
                    let color = object
                        .style
                        .fill
                        .or(Some(object.style.stroke))
                        .filter(|c| !c.is_invisible())
                        .unwrap_or(Color::BLACK);
                    redact::solid(canvas, region, color);
                }
            }
        }
    }
}

fn fill_then_stroke(
    canvas: &mut Pixmap,
    path: &tiny_skia::Path,
    object: &AnnotationObject,
    opacity: f32,
    width: f32,
    into: ColorTransform,
) {
    if let Some(fill) = object.style.fill.filter(|c| !c.is_invisible()) {
        let paint = shapes::paint(fill, opacity, BlendMode::SourceOver, into);
        shapes::fill_path(canvas, path, &paint);
    }
    if !object.style.stroke.is_invisible() && object.style.stroke_width > 0.0 {
        let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver, into);
        shapes::stroke_path(canvas, path, &paint, width);
    }
}
