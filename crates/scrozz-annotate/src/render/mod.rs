//! Compositing a document to pixels.

pub mod beautify;
pub mod raster;
pub mod redact;
pub mod shapes;

use scrozz_core::{Error, Frame, LogicalRect, Result, ScaleFactor};
use tiny_skia::{
    BlendMode, FillRule, Mask, Paint, Path, PathBuilder, Pixmap, Rect, StrokeDash, Transform,
};

use crate::{
    annotation::{Annotation, AnnotationObject, RedactStyle},
    document::Document,
    style::{ArrowStyle, Color, TextPreset},
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
        self.render_geometry_at(document, document.canvas_geometry(), scale)
    }

    /// Composites through a temporary canvas without changing the document.
    ///
    /// An editor can use this to reveal the full source while a crop remains
    /// persisted for the final export.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::render_at`], plus canvas validation
    /// errors from [`Document::canvas_geometry_for`].
    pub fn render_canvas(&self, document: &Document, canvas: crate::Canvas) -> Result<Frame> {
        let geometry = document.canvas_geometry_for(canvas)?;
        self.render_geometry_at(document, geometry, document.source.frame.scale)
    }

    fn render_geometry_at(
        &self,
        document: &Document,
        geometry: crate::CanvasGeometry,
        scale: ScaleFactor,
    ) -> Result<Frame> {
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
        let logical = geometry.output_size();
        let width = (logical.width * scale.get()).ceil().max(1.0) as u32;
        let height = (logical.height * scale.get()).ceil().max(1.0) as u32;

        let mut canvas = Pixmap::new(width, height).ok_or_else(|| {
            Error::InvalidRequest(format!("output size {width}x{height} is not renderable"))
        })?;
        let xf = Scaled::for_canvas(scale.get(), geometry);
        draw_source(
            &mut canvas,
            &source,
            document.source.frame.scale,
            geometry.source_crop(),
            xf,
        )?;

        for object in document.annotations() {
            draw_object(&mut canvas, object, xf);
        }

        let canvas = match document.beautification() {
            Some(beautification) => beautify::apply(&canvas, beautification, scale.get())?,
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
        let logical = document.canvas_size();
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

/// Draws source pixels through the reversible canvas transform and crop mask.
fn draw_source(
    canvas: &mut Pixmap,
    source: &Pixmap,
    source_scale: ScaleFactor,
    crop: LogicalRect,
    xf: Scaled,
) -> Result<()> {
    let logical_to_output = xf.transform();
    let source_to_output = Transform::from_row(
        logical_to_output.sx / source_scale.get() as f32,
        logical_to_output.ky / source_scale.get() as f32,
        logical_to_output.kx / source_scale.get() as f32,
        logical_to_output.sy / source_scale.get() as f32,
        logical_to_output.tx,
        logical_to_output.ty,
    );
    let ratio = xf.factor() / source_scale.get();
    let quality = if (ratio - 1.0).abs() < 1e-9 {
        // Exact 1:1 must be a byte-for-byte copy. Any filter here would soften
        // a native-resolution export, which is the common case.
        tiny_skia::FilterQuality::Nearest
    } else {
        tiny_skia::FilterQuality::Bilinear
    };
    let mut mask = Mask::new(canvas.width(), canvas.height()).ok_or_else(|| {
        Error::InvalidRequest(format!(
            "crop mask {}x{} is not renderable",
            canvas.width(),
            canvas.height()
        ))
    })?;
    let crop_path = shapes::rectangle(&crop, xf).ok_or_else(|| {
        Error::InvalidRequest("canvas crop does not enclose any source pixels".to_owned())
    })?;
    mask.fill_path(&crop_path, FillRule::Winding, false, Transform::identity());

    canvas.draw_pixmap(
        0,
        0,
        source.as_ref(),
        &tiny_skia::PixmapPaint {
            quality,
            ..tiny_skia::PixmapPaint::default()
        },
        source_to_output,
        Some(&mask),
    );
    Ok(())
}

/// Draws one annotation onto the canvas.
///
/// Redactions are handled in z-order like everything else, and destroy whatever
/// has been composited beneath them — including earlier annotations. That is the
/// stricter reading and the safer one: a redaction always erases what it covers.
fn draw_object(canvas: &mut Pixmap, object: &AnnotationObject, xf: Scaled) {
    let opacity = object.style.effective_opacity();
    if opacity <= 0.0 {
        return;
    }
    let width = xf.length(object.style.effective_stroke_width());

    match &object.annotation {
        Annotation::Arrow { from, to } => {
            let Some((shaft, heads)) = shapes::arrow(object, *from, *to, xf) else {
                return;
            };
            let mut shaft_style = shapes::stroke(width);
            if object.style.arrow_style == ArrowStyle::Dashed {
                shaft_style.dash = StrokeDash::new(vec![width * 2.2, width * 1.6], 0.0);
            }
            if object.style.shadow {
                draw_path_shadow(canvas, &shaft, Some(&shaft_style), false, xf);
                for head in &heads {
                    draw_path_shadow(canvas, head, None, true, xf);
                }
            }
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver);
            shapes::stroke_path_with(canvas, &shaft, &paint, &shaft_style, Transform::identity());
            for head in &heads {
                shapes::fill_path(canvas, head, &paint);
            }
        }
        Annotation::Line { from, to } => {
            let Some(path) = shapes::line(*from, *to, xf) else {
                return;
            };
            if object.style.shadow {
                let stroke = shapes::stroke(width);
                draw_path_shadow(canvas, &path, Some(&stroke), false, xf);
            }
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver);
            shapes::stroke_path(canvas, &path, &paint, width);
        }
        Annotation::Rectangle(rect) => {
            let Some(path) = shapes::rectangle(rect, xf) else {
                return;
            };
            draw_shape_shadow(canvas, &path, object, width, xf);
            fill_then_stroke(canvas, &path, object, opacity, width);
        }
        Annotation::Ellipse(rect) => {
            let Some(path) = shapes::ellipse(rect, xf) else {
                return;
            };
            draw_shape_shadow(canvas, &path, object, width, xf);
            fill_then_stroke(canvas, &path, object, opacity, width);
        }
        Annotation::Freehand(points) => {
            let Some(path) = shapes::freehand(points, xf) else {
                return;
            };
            if object.style.shadow {
                let stroke = shapes::stroke(width);
                draw_path_shadow(canvas, &path, Some(&stroke), false, xf);
            }
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver);
            shapes::stroke_path(canvas, &path, &paint, width);
        }
        Annotation::Text { at, content } => {
            let Some(path) = shapes::text(content, *at, &object.style, xf) else {
                return;
            };
            let boxed = text_box(object, xf);
            if object.style.shadow {
                if let Some(boxed) = &boxed {
                    draw_path_shadow(canvas, boxed, None, true, xf);
                } else {
                    draw_path_shadow(canvas, &path, None, true, xf);
                }
            }

            let ink = if let Some(boxed) = boxed {
                let background = object.style.fill.unwrap_or(object.style.stroke);
                let paint = shapes::paint(background, opacity, BlendMode::SourceOver);
                shapes::fill_path(canvas, &boxed, &paint);
                background.contrasting()
            } else {
                object.style.stroke
            };
            let paint = shapes::paint(ink, opacity, BlendMode::SourceOver);
            if object.style.text_preset == TextPreset::Outlined {
                let outline = shapes::paint(
                    object.style.stroke.contrasting(),
                    opacity,
                    BlendMode::SourceOver,
                );
                shapes::stroke_path(
                    canvas,
                    &path,
                    &outline,
                    xf.length(object.style.effective_font_size() * 0.16)
                        .max(1.0),
                );
            } else if object.style.text_preset == TextPreset::Rounded {
                shapes::stroke_path(
                    canvas,
                    &path,
                    &paint,
                    xf.length(object.style.effective_font_size() * 0.055),
                );
            }
            shapes::fill_path(canvas, &path, &paint);
        }
        Annotation::Counter { at, index } => {
            let Some((disc, label)) = shapes::counter(object, *at, *index, xf) else {
                return;
            };
            if object.style.shadow {
                draw_path_shadow(canvas, &disc, None, true, xf);
            }
            let fill = object.style.fill.unwrap_or(object.style.stroke);
            let disc_paint = shapes::paint(fill, opacity, BlendMode::SourceOver);
            shapes::fill_path(canvas, &disc, &disc_paint);
            if let Some(label) = label {
                // Pick black or white against the disc so the numeral stays
                // legible whatever colour the user chose.
                let ink = shapes::paint(fill.contrasting(), opacity, BlendMode::SourceOver);
                shapes::fill_path(canvas, &label, &ink);
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
            let paint = shapes::paint(color, opacity, BlendMode::Multiply);
            shapes::fill_path(canvas, &path, &paint);
        }
        Annotation::Spotlight(area) => {
            let Some(hole) = xf.rect(area) else {
                return;
            };
            let color = object.style.fill.unwrap_or(object.style.stroke);
            let paint = shapes::paint(color, opacity, BlendMode::SourceOver);
            draw_spotlight(canvas, hole, &paint);
        }
        Annotation::Redact { area, style } => {
            let Some(rect) = xf.rect(area) else {
                return;
            };
            let Some(region) =
                redact::clip(canvas, rect.left(), rect.top(), rect.right(), rect.bottom())
            else {
                return;
            };
            let strength = object.style.effective_redact_strength();
            match style {
                RedactStyle::Blur => redact::blur_with_strength(canvas, region, strength),
                RedactStyle::Pixelate => {
                    redact::pixelate_with_strength(canvas, region, strength);
                }
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

fn draw_shape_shadow(
    canvas: &mut Pixmap,
    path: &Path,
    object: &AnnotationObject,
    width: f32,
    xf: Scaled,
) {
    if !object.style.shadow {
        return;
    }
    let fill = object.style.fill.is_some_and(|color| !color.is_invisible());
    let stroke = (object.style.stroke_width > 0.0).then(|| shapes::stroke(width));
    draw_path_shadow(canvas, path, stroke.as_ref(), fill, xf);
}

fn draw_path_shadow(
    canvas: &mut Pixmap,
    path: &Path,
    stroke: Option<&tiny_skia::Stroke>,
    fill: bool,
    xf: Scaled,
) {
    let paint = shapes::paint(Color::BLACK, 0.28, BlendMode::SourceOver);
    let transform = Transform::from_translate(xf.length(2.5), xf.length(3.5));
    if fill {
        shapes::fill_path_with(canvas, path, &paint, transform);
    }
    if let Some(stroke) = stroke {
        shapes::stroke_path_with(canvas, path, &paint, stroke, transform);
    }
}

fn text_box(object: &AnnotationObject, xf: Scaled) -> Option<Path> {
    if !object.style.text_preset.is_boxed() {
        return None;
    }
    let padding = object.style.effective_font_size() * 0.28;
    let bounds = crate::geom::inflate(&object.bounds(), padding);
    let rect = xf.rect(&bounds)?;
    let radius = match object.style.text_preset {
        TextPreset::RoundedBoxed => xf.length(object.style.effective_font_size() * 0.45),
        TextPreset::Boxed | TextPreset::MonospacedBoxed => xf.length(3.0),
        _ => 0.0,
    };
    shapes::rounded_rect(rect, radius)
}

fn draw_spotlight(canvas: &mut Pixmap, hole: Rect, paint: &Paint<'_>) {
    let width = canvas.width() as f32;
    let height = canvas.height() as f32;
    let left = hole.left().clamp(0.0, width);
    let top = hole.top().clamp(0.0, height);
    let right = hole.right().clamp(0.0, width);
    let bottom = hole.bottom().clamp(0.0, height);
    for rect in [
        Rect::from_ltrb(0.0, 0.0, width, top),
        Rect::from_ltrb(0.0, bottom, width, height),
        Rect::from_ltrb(0.0, top, left, bottom),
        Rect::from_ltrb(right, top, width, bottom),
    ]
    .into_iter()
    .flatten()
    {
        let mut path = PathBuilder::new();
        path.push_rect(rect);
        if let Some(path) = path.finish() {
            shapes::fill_path(canvas, &path, paint);
        }
    }
}

fn fill_then_stroke(
    canvas: &mut Pixmap,
    path: &tiny_skia::Path,
    object: &AnnotationObject,
    opacity: f32,
    width: f32,
) {
    if let Some(fill) = object.style.fill.filter(|c| !c.is_invisible()) {
        let paint = shapes::paint(fill, opacity, BlendMode::SourceOver);
        shapes::fill_path(canvas, path, &paint);
    }
    if !object.style.stroke.is_invisible() && object.style.stroke_width > 0.0 {
        let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver);
        shapes::stroke_path(canvas, path, &paint, width);
    }
}
