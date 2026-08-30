//! Compositing a document to pixels.

pub mod beautify;
pub mod raster;
pub mod redact;
pub mod shapes;

use scrozz_core::{
    ColorSpace, Error, Frame, LogicalRect, PhysicalPoint, PhysicalRect, PhysicalSize, Result,
    ScaleFactor, Transform as ColorTransform,
};
use tiny_skia::{BlendMode, IntRect, IntSize, Pixmap, Transform};

use crate::{
    annotation::{Annotation, AnnotationObject, RedactStyle},
    document::{
        Beautification, Document, MAX_RASTER_EDGE, MAX_RASTER_PIXELS, quantized_crop_edges,
    },
    style::Color,
};

use shapes::Scaled;

const MAX_RENDER_WORKING_BYTES: u64 = 768 * 1024 * 1024;
const MAX_PIXMAP_BYTES: u64 = 256 * 1024 * 1024;
const BYTES_PER_PIXEL: u64 = 4;

/// Renders a document to pixels.
pub trait Renderer {
    /// Composites annotations and Scene over the source.
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

/// Physical geometry of a rendered Scene preview.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneRenderLayout {
    /// Width of the complete Scene canvas.
    pub width: u32,
    /// Height of the complete Scene canvas.
    pub height: u32,
    /// Rectangle occupied by the untouched capture pixels.
    pub subject: PhysicalRect,
}

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
    /// the output would be zero-sized or unallocatable, or if window framing
    /// would change native subject pixels — see decision D9.
    pub fn render_at(&self, document: &Document, scale: ScaleFactor) -> Result<Frame> {
        self.render_at_with_width(document, scale, None)
    }

    fn render_at_with_width(
        &self,
        document: &Document,
        scale: ScaleFactor,
        target_width: Option<u32>,
    ) -> Result<Frame> {
        self.render_at_with_width_and_layout(document, scale, target_width)
            .map(|(frame, _)| frame)
    }

    fn render_at_with_width_and_layout(
        &self,
        document: &Document,
        scale: ScaleFactor,
        target_width: Option<u32>,
    ) -> Result<(Frame, Option<SceneRenderLayout>)> {
        if document.source().provenance.forbids_compositing()
            && document
                .beautification()
                .is_some_and(|beautification| !beautification.preserves_subject_pixels())
        {
            return Err(Error::InvalidRequest(
                "window Scene may add only an outer canvas; native pixels must remain untouched (decision D9)"
                    .to_owned(),
            ));
        }

        let logical = document.display_size();
        let width = checked_render_dimension(logical.width * scale.get(), "width")?;
        let height = checked_render_dimension(logical.height * scale.get(), "height")?;
        let retained_background_bytes = match document.beautification() {
            Some(beautification) => {
                beautification.validate()?;
                retained_background_bytes(beautification)?
            }
            None => 0,
        };
        preflight_render(
            &document.source().frame,
            width,
            height,
            false,
            retained_background_bytes,
        )?;

        let source = raster::to_pixmap(&document.source().frame)?;
        let bounds = document.logical_bounds();
        let content = document.content_bounds();
        let source_size = scaled_size(bounds, scale)?;
        let crop = crop_rect(bounds, content, scale, source_size)?;

        let mut canvas = new_pixmap(source_size.0, source_size.1, "source-sized output")?;
        let into = ColorTransform::new(ColorSpace::Srgb, document.source().frame.color_space);
        let xf = Scaled::with_origin(scale.get(), bounds.origin);
        draw_source(&mut canvas, &source, bounds, xf);

        for object in document.annotations() {
            draw_object(&mut canvas, object, xf, into)?;
        }
        drop(source);

        let canvas = if content == bounds {
            canvas
        } else {
            crop_canvas(canvas, crop)?
        };
        let canvas = orient_canvas(canvas, document.orientation())?;
        let retained_source_bytes =
            u64::try_from(document.source().frame.data.len()).map_err(|_| {
                Error::InvalidRequest("source buffer size is not addressable".to_owned())
            })?;
        let (canvas, scene_layout) = match document.beautification() {
            Some(beautification) if !beautification.is_noop() => {
                let (canvas, layout) = beautify::apply_with_retained_bytes_and_layout(
                    &canvas,
                    beautification,
                    beautify::ApplyOptions {
                        scale: scale.get(),
                        source_scale: document.source().frame.scale.get(),
                        target_color_space: document.source().frame.color_space,
                        preserve_source_pixels: document.source().provenance.forbids_compositing(),
                        retained_source_bytes,
                        target_width,
                    },
                )?;
                let subject = PhysicalRect::new(
                    PhysicalPoint::new(
                        f64::from(layout.content.left()),
                        f64::from(layout.content.top()),
                    ),
                    PhysicalSize::new(
                        f64::from(layout.content.width()),
                        f64::from(layout.content.height()),
                    ),
                );
                (
                    canvas,
                    Some(SceneRenderLayout {
                        width: layout.width,
                        height: layout.height,
                        subject,
                    }),
                )
            }
            Some(_) | None => (canvas, None),
        };
        if let Some(target_width) = target_width
            && canvas.width() != target_width
        {
            return Err(Error::InvalidRequest(format!(
                "rendered width is {}, expected {target_width}",
                canvas.width()
            )));
        }

        let color_space = if document.source().frame.color_space == ColorSpace::Unknown
            || document.beautification().is_some_and(|beautification| {
                matches!(
                    &beautification.background,
                    crate::Background::Image(image) | crate::Background::Desktop(image)
                        if image.color_space() == ColorSpace::Unknown
                )
            }) {
            ColorSpace::Unknown
        } else {
            document.source().frame.color_space
        };

        Ok((
            raster::from_pixmap(canvas, color_space, scale),
            scene_layout,
        ))
    }

    /// Composites the document scaled so its output is `width` pixels wide.
    ///
    /// # Errors
    ///
    /// As [`Self::render_at`], plus [`Error::InvalidRequest`] if `width` is zero
    /// or the source has no width to scale from.
    pub fn render_to_width(&self, document: &Document, width: u32) -> Result<Frame> {
        self.render_to_width_with_layout(document, width)
            .map(|(frame, _)| frame)
    }

    /// Composites a preview and returns the Scene's canonical subject geometry.
    ///
    /// The geometry is expressed in the returned frame's physical pixel space.
    /// `None` means the document has no visible Scene and the full frame is the
    /// editable capture subject.
    ///
    /// # Errors
    ///
    /// As [`Self::render_to_width`].
    pub fn render_to_width_with_layout(
        &self,
        document: &Document,
        width: u32,
    ) -> Result<(Frame, Option<SceneRenderLayout>)> {
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
        self.render_at_with_width_and_layout(
            document,
            ScaleFactor::new(f64::from(width) / logical.width),
            Some(width),
        )
    }
}

impl Renderer for SkiaRenderer {
    /// Composites at the source's own scale, which is the lossless default.
    fn render(&self, document: &Document) -> Result<Frame> {
        self.render_at(document, document.source().frame.scale)
    }
}

fn checked_render_dimension(value: f64, name: &str) -> Result<u32> {
    if !value.is_finite() || value <= 0.0 || value > f64::from(MAX_RASTER_EDGE) {
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
    let (crate::Background::Image(image) | crate::Background::Desktop(image)) =
        &beautification.background
    else {
        return Ok(0);
    };
    u64::from(image.width())
        .checked_mul(u64::from(image.height()))
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .and_then(|bytes| bytes.checked_add(image.encoded_len() as u64))
        .ok_or_else(|| Error::InvalidRequest("background working set overflowed".to_owned()))
}

fn scaled_size(bounds: LogicalRect, scale: ScaleFactor) -> Result<(u32, u32)> {
    let width = scaled_length(bounds.size.width, scale)?;
    let height = scaled_length(bounds.size.height, scale)?;
    Ok((width, height))
}

fn scaled_length(length: f64, scale: ScaleFactor) -> Result<u32> {
    let pixels = (length * scale.get()).round().max(1.0);
    if !pixels.is_finite() || pixels > f64::from(u32::MAX) {
        return Err(Error::InvalidRequest(
            "scaled output dimension is not renderable".to_owned(),
        ));
    }
    Ok(pixels as u32)
}

fn crop_rect(
    bounds: LogicalRect,
    content: LogicalRect,
    scale: ScaleFactor,
    canvas: (u32, u32),
) -> Result<IntRect> {
    let (x0, y0, x1, y1) = quantized_crop_edges(bounds, content, scale, canvas);
    let x = i32::try_from(x0)
        .map_err(|_| Error::InvalidRequest("crop origin is not renderable".to_owned()))?;
    let y = i32::try_from(y0)
        .map_err(|_| Error::InvalidRequest("crop origin is not renderable".to_owned()))?;
    IntRect::from_xywh(x, y, x1 - x0, y1 - y0)
        .ok_or_else(|| Error::InvalidRequest("crop rectangle is not renderable".to_owned()))
}

fn crop_canvas(canvas: Pixmap, rect: IntRect) -> Result<Pixmap> {
    let mut cropped = new_pixmap(rect.width(), rect.height(), "cropped output")?;
    let source_width = canvas.width() as usize;
    let cropped_width = rect.width() as usize;
    for row in 0..rect.height() as usize {
        let source = ((rect.y() as usize + row) * source_width + rect.x() as usize) * 4;
        let destination = row * cropped_width * 4;
        let count = cropped_width * 4;
        cropped.data_mut()[destination..destination + count]
            .copy_from_slice(&canvas.data()[source..source + count]);
    }
    Ok(cropped)
}

fn orient_canvas(canvas: Pixmap, orientation: crate::ImageOrientation) -> Result<Pixmap> {
    use crate::ImageOrientation;

    if orientation == ImageOrientation::Identity {
        return Ok(canvas);
    }
    let (source_width, source_height) = (canvas.width(), canvas.height());
    let (width, height) = if orientation.swaps_axes() {
        (source_height, source_width)
    } else {
        (source_width, source_height)
    };
    let mut oriented = new_pixmap(width, height, "oriented output")?;
    for y in 0..source_height {
        for x in 0..source_width {
            let (dx, dy) = match orientation {
                ImageOrientation::Identity => (x, y),
                ImageOrientation::RotateRight => (source_height - 1 - y, x),
                ImageOrientation::Rotate180 => (source_width - 1 - x, source_height - 1 - y),
                ImageOrientation::RotateLeft => (y, source_width - 1 - x),
                ImageOrientation::FlipHorizontal => (source_width - 1 - x, y),
                ImageOrientation::FlipVertical => (x, source_height - 1 - y),
                ImageOrientation::Transpose => (y, x),
                ImageOrientation::Transverse => (source_height - 1 - y, source_width - 1 - x),
            };
            let source = (y as usize * source_width as usize + x as usize) * 4;
            let destination = (dy as usize * width as usize + dx as usize) * 4;
            oriented.data_mut()[destination..destination + 4]
                .copy_from_slice(&canvas.data()[source..source + 4]);
        }
    }
    Ok(oriented)
}

fn new_pixmap(width: u32, height: u32, label: &str) -> Result<Pixmap> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| Error::InvalidRequest(format!("{label} size overflows")))?;
    if bytes > MAX_PIXMAP_BYTES {
        return Err(Error::InvalidRequest(format!(
            "{label} {width}x{height} exceeds the {} MiB render limit",
            MAX_PIXMAP_BYTES / (1024 * 1024)
        )));
    }
    let bytes = usize::try_from(bytes)
        .map_err(|_| Error::InvalidRequest(format!("{label} size is not addressable")))?;
    let mut data = Vec::new();
    data.try_reserve_exact(bytes).map_err(|error| {
        Error::InvalidRequest(format!(
            "{label} {width}x{height} is not allocatable: {error}"
        ))
    })?;
    data.resize(bytes, 0);
    let size = IntSize::from_wh(width, height).ok_or_else(|| {
        Error::InvalidRequest(format!("{label} {width}x{height} is not renderable"))
    })?;
    Pixmap::from_vec(data, size).ok_or_else(|| {
        Error::InvalidRequest(format!("{label} {width}x{height} has an invalid buffer"))
    })
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
fn draw_object(
    canvas: &mut Pixmap,
    object: &AnnotationObject,
    xf: Scaled,
    into: ColorTransform,
) -> Result<()> {
    let opacity = object.style.effective_opacity();
    let secure_redaction = matches!(object.annotation, Annotation::Redact { .. })
        .then(|| object.style.effective_redact_intensity())
        .flatten();
    if opacity <= 0.0 && secure_redaction.is_none() {
        return Ok(());
    }
    let width = xf.length(object.style.effective_stroke_width());

    match &object.annotation {
        Annotation::Arrow { from, to } => {
            let Some(arrow) = shapes::arrow(object, *from, *to, xf) else {
                return Ok(());
            };
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver, into);
            shapes::fill_path(canvas, &arrow, &paint);
        }
        Annotation::Line { from, to } => {
            let Some(path) = shapes::line(*from, *to, xf) else {
                return Ok(());
            };
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver, into);
            shapes::stroke_path(canvas, &path, &paint, width);
        }
        Annotation::Rectangle(rect) => {
            let Some(path) = shapes::rectangle(rect, xf) else {
                return Ok(());
            };
            fill_then_stroke(canvas, &path, object, opacity, width, into);
        }
        Annotation::Ellipse(rect) => {
            let Some(path) = shapes::ellipse(rect, xf) else {
                return Ok(());
            };
            fill_then_stroke(canvas, &path, object, opacity, width, into);
        }
        Annotation::Freehand(points) => {
            let Some(path) = shapes::freehand(points, xf) else {
                return Ok(());
            };
            let paint = shapes::paint(object.style.stroke, opacity, BlendMode::SourceOver, into);
            shapes::stroke_path(canvas, &path, &paint, width);
        }
        Annotation::Text { at, content } => {
            let Some(path) = shapes::text(content, *at, &object.style, xf) else {
                return Ok(());
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
                return Ok(());
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
                return Ok(());
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
                return Ok(());
            };
            if let Some(intensity) = secure_redaction {
                redact::secure_mosaic(canvas, region, intensity, object.id.0);
            } else {
                match style {
                    RedactStyle::Blur => redact::blur(canvas, region)?,
                    RedactStyle::Pixelate => redact::pixelate(canvas, region),
                    RedactStyle::Solid => {
                        // A solid redaction writes its pixels directly rather than
                        // going through a `Paint`, so it has to be moved into the
                        // working space by hand — otherwise a custom redaction
                        // colour is the one annotation colour that comes out wrong
                        // on a wide-gamut capture.
                        let color = object
                            .style
                            .fill
                            .or(Some(object.style.stroke))
                            .filter(|c| !c.is_invisible())
                            .unwrap_or(Color::BLACK);
                        redact::solid(canvas, region, shapes::working(color, into));
                    }
                }
            }
        }
    }
    Ok(())
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
