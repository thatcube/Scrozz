//! Compositing a document to pixels.

pub mod beautify;
pub mod raster;
pub mod redact;
pub mod shapes;

use scrozz_core::{ColorSpace, Error, Frame, PixelFormat, Result, ScaleFactor};
use scrozz_export::{RgbaImage, convert_to_srgb, to_straight_rgba8};
use tiny_skia::{BlendMode, Pixmap, Transform};

use crate::{
    annotation::{Annotation, AnnotationObject, RedactStyle},
    document::{Beautification, Document, MAX_RASTER_PIXELS},
    style::Color,
};

use shapes::Scaled;

const MAX_RENDER_WORKING_BYTES: u64 = 768 * 1024 * 1024;
const BYTES_PER_PIXEL: u64 = 4;

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
        self.render_at_with_width(document, scale, None)
    }

    fn render_at_with_width(
        &self,
        document: &Document,
        scale: ScaleFactor,
        target_width: Option<u32>,
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

        let edited = !document.annotations().is_empty()
            || document
                .beautification()
                .is_some_and(|beautification| !beautification.is_noop());
        let logical = document.logical_size();
        let width = checked_render_dimension(logical.width * scale.get(), "width")?;
        let height = checked_render_dimension(logical.height * scale.get(), "height")?;
        let converts_source = edited
            && matches!(
                document.source.frame.color_space,
                ColorSpace::DisplayP3 | ColorSpace::Rec2020
            );
        let retained_background_bytes = match document.beautification() {
            Some(beautification) => {
                beautification.validate()?;
                retained_background_bytes(beautification)?
            }
            None => 0,
        };
        preflight_render(
            &document.source.frame,
            width,
            height,
            converts_source,
            retained_background_bytes,
        )?;

        let converted_source = if converts_source {
            Some(frame_to_srgb(&document.source.frame)?)
        } else {
            None
        };
        let source_frame = converted_source.as_ref().unwrap_or(&document.source.frame);
        let source = raster::to_pixmap(source_frame)?;

        let mut canvas = Pixmap::new(width, height).ok_or_else(|| {
            Error::InvalidRequest(format!("output size {width}x{height} is not renderable"))
        })?;
        draw_source(&mut canvas, &source);

        let xf = Scaled::new(scale.get());
        for object in document.annotations() {
            draw_object(&mut canvas, object, xf);
        }
        drop(source);
        drop(converted_source);

        let retained_source_bytes =
            u64::try_from(document.source.frame.data.len()).map_err(|_| {
                Error::InvalidRequest("source buffer size is not addressable".to_owned())
            })?;
        let canvas = match document.beautification() {
            Some(beautification) if !beautification.is_noop() => {
                beautify::apply_with_retained_bytes(
                    &canvas,
                    beautification,
                    scale.get(),
                    retained_source_bytes,
                    target_width,
                )?
            }
            Some(_) | None => canvas,
        };
        if let Some(target_width) = target_width
            && canvas.width() != target_width
        {
            return Err(Error::InvalidRequest(format!(
                "rendered width is {}, expected {target_width}",
                canvas.width()
            )));
        }

        let color_space = if !edited {
            document.source.frame.color_space
        } else if document.source.frame.color_space == ColorSpace::Unknown
            || document.beautification().is_some_and(|beautification| {
                matches!(
                    &beautification.background,
                    crate::Background::Image(image)
                        if image.color_space() == ColorSpace::Unknown
                )
            })
        {
            ColorSpace::Unknown
        } else {
            ColorSpace::Srgb
        };

        Ok(raster::from_pixmap(canvas, color_space, scale))
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
        let logical = document.output_logical_size();
        if logical.width <= 0.0 {
            return Err(Error::InvalidRequest(
                "cannot scale a source with no width".to_owned(),
            ));
        }
        self.render_at_with_width(
            document,
            ScaleFactor::new(f64::from(width) / logical.width),
            Some(width),
        )
    }
}

impl Renderer for SkiaRenderer {
    /// Composites at the source's own scale, which is the lossless default.
    fn render(&self, document: &Document) -> Result<Frame> {
        self.render_at(document, document.source.frame.scale)
    }
}

fn checked_render_dimension(value: f64, name: &str) -> Result<u32> {
    if !value.is_finite() || value <= 0.0 || value > f64::from(u32::MAX) {
        return Err(Error::InvalidRequest(format!(
            "rendered {name} {value} is not addressable"
        )));
    }
    Ok(value.round().max(1.0) as u32)
}

fn preflight_render(
    source: &Frame,
    output_width: u32,
    output_height: u32,
    converts_source: bool,
    retained_background_bytes: u64,
) -> Result<()> {
    let source_pixels = u64::from(source.width())
        .checked_mul(u64::from(source.height()))
        .ok_or_else(|| Error::InvalidRequest("source raster area overflowed".to_owned()))?;
    let output_pixels = u64::from(output_width)
        .checked_mul(u64::from(output_height))
        .ok_or_else(|| Error::InvalidRequest("output raster area overflowed".to_owned()))?;
    for (label, pixels) in [("source", source_pixels), ("output", output_pixels)] {
        if pixels > MAX_RASTER_PIXELS {
            return Err(Error::InvalidRequest(format!(
                "{label} raster has {pixels} pixels; the limit is {MAX_RASTER_PIXELS}"
            )));
        }
    }

    let source_raster = source_pixels
        .checked_mul(BYTES_PER_PIXEL)
        .ok_or_else(|| Error::InvalidRequest("source raster size overflowed".to_owned()))?;
    let output_raster = output_pixels
        .checked_mul(BYTES_PER_PIXEL)
        .ok_or_else(|| Error::InvalidRequest("output raster size overflowed".to_owned()))?;
    let retained = u64::try_from(source.data.len())
        .map_err(|_| Error::InvalidRequest("source buffer size is not addressable".to_owned()))?;
    let source_copies = if converts_source { 2 } else { 1 };
    let peak = source_raster
        .checked_mul(source_copies)
        .and_then(|bytes| bytes.checked_add(retained))
        .and_then(|bytes| bytes.checked_add(retained_background_bytes))
        .and_then(|bytes| bytes.checked_add(output_raster))
        .ok_or_else(|| Error::InvalidRequest("render working set overflowed".to_owned()))?;
    if peak > MAX_RENDER_WORKING_BYTES {
        return Err(Error::InvalidRequest(format!(
            "render needs about {} MiB of working memory; the limit is {} MiB",
            peak.div_ceil(1024 * 1024),
            MAX_RENDER_WORKING_BYTES / (1024 * 1024)
        )));
    }
    Ok(())
}

fn retained_background_bytes(beautification: &Beautification) -> Result<u64> {
    let crate::Background::Image(image) = &beautification.background else {
        return Ok(0);
    };
    u64::from(image.width())
        .checked_mul(u64::from(image.height()))
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .and_then(|bytes| bytes.checked_add(image.encoded_len() as u64))
        .ok_or_else(|| Error::InvalidRequest("background working set overflowed".to_owned()))
}

fn frame_to_srgb(frame: &Frame) -> Result<Frame> {
    let source = to_straight_rgba8(frame)?;
    let RgbaImage {
        width,
        height,
        data,
    } = convert_to_srgb(&source, frame.color_space)?;
    Ok(Frame {
        data,
        size: scrozz_core::PhysicalSize::new(f64::from(width), f64::from(height)),
        stride: width as usize * PixelFormat::Rgba8.bytes_per_pixel(),
        format: PixelFormat::Rgba8,
        color_space: ColorSpace::Srgb,
        scale: frame.scale,
    })
}

/// Draws the source image scaled to fill the canvas.
fn draw_source(canvas: &mut Pixmap, source: &Pixmap) {
    let sx = f64::from(canvas.width()) / f64::from(source.width());
    let sy = f64::from(canvas.height()) / f64::from(source.height());
    let quality = if (sx - 1.0).abs() < 1e-9 && (sy - 1.0).abs() < 1e-9 {
        // Exact 1:1 must be a byte-for-byte copy. Any filter here would soften
        // a native-resolution export, which is the common case.
        tiny_skia::FilterQuality::Nearest
    } else {
        tiny_skia::FilterQuality::Bilinear
    };
    canvas.draw_pixmap(
        0,
        0,
        source.as_ref(),
        &tiny_skia::PixmapPaint {
            quality,
            ..tiny_skia::PixmapPaint::default()
        },
        Transform::from_scale(sx as f32, sy as f32),
        None,
    );
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
            let Some((shaft, head)) = shapes::arrow(object, *from, *to, xf) else {
                return;
            };
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver);
            shapes::stroke_path(canvas, &shaft, &paint, width);
            shapes::fill_path(canvas, &head, &paint);
        }
        Annotation::Rectangle(rect) => {
            let Some(path) = shapes::rectangle(rect, xf) else {
                return;
            };
            fill_then_stroke(canvas, &path, object, opacity, width);
        }
        Annotation::Ellipse(rect) => {
            let Some(path) = shapes::ellipse(rect, xf) else {
                return;
            };
            fill_then_stroke(canvas, &path, object, opacity, width);
        }
        Annotation::Freehand(points) => {
            let Some(path) = shapes::freehand(points, xf) else {
                return;
            };
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver);
            shapes::stroke_path(canvas, &path, &paint, width);
        }
        Annotation::Text { at, content } => {
            let Some(path) = shapes::text(content, *at, &object.style, xf) else {
                return;
            };
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver);
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
            let disc_paint = shapes::paint(fill, opacity, BlendMode::SourceOver);
            shapes::fill_path(canvas, &disc, &disc_paint);
            if let Some(label) = label {
                // Pick black or white against the disc so the numeral stays
                // legible whatever colour the user chose.
                let ink = shapes::paint(fill.contrasting(), opacity, BlendMode::SourceOver);
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
            let paint = shapes::paint(color, opacity, BlendMode::Multiply);
            shapes::fill_path(canvas, &path, &paint);
        }
        Annotation::Redact { area, style } => {
            let Some(region) = redact::clip(
                canvas,
                xf.length(area.origin.x),
                xf.length(area.origin.y),
                xf.length(crate::geom::max_x(area)),
                xf.length(crate::geom::max_y(area)),
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
