//! Compositing a document to pixels.

pub mod beautify;
pub mod raster;
pub mod redact;
pub mod shapes;

use scrozz_core::{
    ColorSpace, Error, Frame, LogicalPoint, LogicalRect, PhysicalPoint, PhysicalRect, PhysicalSize,
    PixelFormat, Result, ScaleFactor,
};
use scrozz_export::{RgbaImage, convert_to_srgb, to_straight_rgba8};
use tiny_skia::{
    BlendMode, FillRule, Mask, Paint, Path, PathBuilder, Pixmap, Rect, StrokeDash, Transform,
};

use crate::{
    annotation::{Annotation, AnnotationObject, RedactStyle},
    document::{Beautification, Canvas, Document, MAX_RASTER_PIXELS},
    style::{ArrowStyle, Color, TextPreset},
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

/// Final raster placement after the reversible inner canvas and outer
/// beautification have both been resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderGeometry {
    /// Actual final raster width.
    pub output_width: u32,
    /// Actual final raster height.
    pub output_height: u32,
    /// The transformed inner canvas inside the final raster.
    pub content_rect: PhysicalRect,
    /// Physical pixels per source-logical point.
    pub scale: ScaleFactor,
    /// Reversible crop, expansion, flip, and rotation mapping.
    pub canvas: crate::CanvasGeometry,
}

/// Effective outer spacing around the inner canvas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhysicalInsets {
    /// Pixels above the inner canvas.
    pub top: f64,
    /// Pixels to the right of the inner canvas.
    pub right: f64,
    /// Pixels below the inner canvas.
    pub bottom: f64,
    /// Pixels to the left of the inner canvas.
    pub left: f64,
}

impl RenderGeometry {
    /// Physical offset of the transformed inner canvas.
    #[must_use]
    pub const fn content_offset(self) -> PhysicalPoint {
        self.content_rect.origin
    }

    /// Final raster size in physical pixels.
    #[must_use]
    pub fn output_size(self) -> PhysicalSize {
        PhysicalSize::new(f64::from(self.output_width), f64::from(self.output_height))
    }

    /// Effective spacing on each side after integer raster rounding.
    #[must_use]
    pub fn effective_insets(self) -> PhysicalInsets {
        PhysicalInsets {
            top: self.content_rect.origin.y,
            right: f64::from(self.output_width)
                - self.content_rect.origin.x
                - self.content_rect.size.width,
            bottom: f64::from(self.output_height)
                - self.content_rect.origin.y
                - self.content_rect.size.height,
            left: self.content_rect.origin.x,
        }
    }

    /// Maps source-logical coordinates into final physical output pixels.
    #[must_use]
    pub fn source_to_output(self, source: LogicalPoint) -> PhysicalPoint {
        let inner = self.canvas.source_to_canvas(source);
        PhysicalPoint::new(
            self.content_rect.origin.x + inner.x * self.scale.get(),
            self.content_rect.origin.y + inner.y * self.scale.get(),
        )
    }

    /// Maps final physical output pixels back into source-logical coordinates.
    #[must_use]
    pub fn output_to_source(self, output: PhysicalPoint) -> LogicalPoint {
        self.canvas.canvas_to_source(LogicalPoint::new(
            (output.x - self.content_rect.origin.x) / self.scale.get(),
            (output.y - self.content_rect.origin.y) / self.scale.get(),
        ))
    }
}

/// A rendered frame paired with the exact geometry used to produce it.
#[derive(Debug, Clone)]
pub struct RenderedFrame {
    /// Flattened output pixels.
    pub frame: Frame,
    /// Placement and reversible source mapping for those pixels.
    pub geometry: RenderGeometry,
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
        Ok(self.render_at_with_geometry(document, scale)?.frame)
    }

    /// Renders at `scale` and returns the exact final raster placement.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::render_at`].
    pub fn render_at_with_geometry(
        &self,
        document: &Document,
        scale: ScaleFactor,
    ) -> Result<RenderedFrame> {
        self.render_resolved_at(
            document,
            *document.canvas(),
            document.canvas_geometry(),
            scale,
            None,
        )
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
        Ok(self.render_canvas_with_geometry(document, canvas)?.frame)
    }

    /// Renders a temporary canvas and returns its exact final placement.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::render_canvas`].
    pub fn render_canvas_with_geometry(
        &self,
        document: &Document,
        canvas: crate::Canvas,
    ) -> Result<RenderedFrame> {
        self.render_canvas_at_with_geometry(document, canvas, document.source.frame.scale)
    }

    /// Renders a temporary canvas at an explicit scale with exact placement.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::render_at`], plus canvas validation
    /// errors from [`Document::canvas_geometry_for`].
    pub fn render_canvas_at_with_geometry(
        &self,
        document: &Document,
        canvas: crate::Canvas,
        scale: ScaleFactor,
    ) -> Result<RenderedFrame> {
        let geometry = document.canvas_geometry_for(canvas)?;
        self.render_resolved_at(document, canvas, geometry, scale, None)
    }

    fn render_resolved_at(
        &self,
        document: &Document,
        canvas: Canvas,
        canvas_geometry: crate::CanvasGeometry,
        scale: ScaleFactor,
        target_width: Option<u32>,
    ) -> Result<RenderedFrame> {
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
            || canvas != Canvas::default()
            || document
                .beautification()
                .is_some_and(|beautification| !beautification.is_noop());
        let converts_source = edited
            && matches!(
                document.source.frame.color_space,
                ColorSpace::DisplayP3 | ColorSpace::Rec2020
            );
        let logical = canvas_geometry.output_size();
        let mut width = checked_render_dimension(logical.width * scale.get(), "width")?;
        let height = checked_render_dimension(logical.height * scale.get(), "height")?;
        if document
            .beautification()
            .is_none_or(Beautification::is_noop)
            && let Some(target_width) = target_width
        {
            width = target_width;
        }
        let retained_background_bytes = document
            .beautification()
            .map_or(Ok(0), retained_background_bytes)?;
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
        let mut source = raster::to_pixmap(source_frame)?;
        sanitize_redacted_source(
            &mut source,
            document.annotations(),
            document.source.frame.scale,
        );

        let mut canvas = Pixmap::new(width, height).ok_or_else(|| {
            Error::InvalidRequest(format!("output size {width}x{height} is not renderable"))
        })?;
        let xf = Scaled::for_canvas(scale.get(), canvas_geometry);
        draw_source(
            &mut canvas,
            &source,
            document.source.frame.scale,
            canvas_geometry.source_crop(),
            xf,
        )?;

        for object in document.annotations() {
            draw_object(&mut canvas, object, xf, document.redaction_seed());
        }

        drop(source);
        drop(converted_source);
        let retained_source_bytes =
            u64::try_from(document.source.frame.data.len()).map_err(|_| {
                Error::InvalidRequest("source buffer size is not addressable".to_owned())
            })?;
        let (canvas, layout) = match document.beautification() {
            Some(beautification) => beautify::apply_with_layout(
                &canvas,
                logical,
                beautification,
                scale.get(),
                retained_source_bytes,
                target_width,
            )?,
            None => (
                canvas,
                beautify::ResolvedLayout {
                    width,
                    height,
                    content: PhysicalRect::new(
                        PhysicalPoint::new(0.0, 0.0),
                        PhysicalSize::new(f64::from(width), f64::from(height)),
                    ),
                },
            ),
        };
        if canvas.width() != layout.width || canvas.height() != layout.height {
            return Err(Error::InvalidRequest(format!(
                "resolved output {}x{} did not match rendered output {}x{}",
                layout.width,
                layout.height,
                canvas.width(),
                canvas.height()
            )));
        }
        if let Some(target_width) = target_width
            && canvas.width() != target_width
        {
            return Err(Error::InvalidRequest(format!(
                "rendered width is {}, expected {target_width}",
                canvas.width()
            )));
        }
        let geometry = RenderGeometry {
            output_width: layout.width,
            output_height: layout.height,
            content_rect: layout.content,
            scale,
            canvas: canvas_geometry,
        };
        let color_space = output_color_space(document, edited);

        Ok(RenderedFrame {
            frame: raster::from_pixmap(canvas, color_space, scale),
            geometry,
        })
    }

    /// Composites the document scaled so its output is `width` pixels wide.
    ///
    /// # Errors
    ///
    /// As [`Self::render_at`], plus [`Error::InvalidRequest`] if `width` is zero
    /// or the source has no width to scale from.
    pub fn render_to_width(&self, document: &Document, width: u32) -> Result<Frame> {
        Ok(self.render_to_width_with_geometry(document, width)?.frame)
    }

    /// Scales to an exact final width and returns final raster placement.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::render_to_width`].
    pub fn render_to_width_with_geometry(
        &self,
        document: &Document,
        width: u32,
    ) -> Result<RenderedFrame> {
        if width == 0 {
            return Err(Error::InvalidRequest(
                "export width must be greater than zero".to_owned(),
            ));
        }
        let canvas = document.canvas_geometry();
        let logical_width = document.output_logical_size().width;
        if logical_width <= 0.0 {
            return Err(Error::InvalidRequest(
                "cannot scale a source with no width".to_owned(),
            ));
        }
        let rendered = self.render_resolved_at(
            document,
            *document.canvas(),
            canvas,
            ScaleFactor::new(f64::from(width) / logical_width),
            Some(width),
        )?;
        if rendered.geometry.output_width != width {
            return Err(Error::InvalidRequest(format!(
                "requested {width}px output, but raster rounding produced {}px",
                rendered.geometry.output_width
            )));
        }
        Ok(rendered)
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
    Ok(value.ceil().max(1.0) as u32)
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
    let stride = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(PixelFormat::Rgba8.bytes_per_pixel()))
        .ok_or_else(|| Error::InvalidRequest("converted source stride overflowed".to_owned()))?;
    Ok(Frame {
        data,
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride,
        format: PixelFormat::Rgba8,
        color_space: ColorSpace::Srgb,
        scale: frame.scale,
    })
}

fn output_color_space(document: &Document, edited: bool) -> ColorSpace {
    if !edited {
        return document.source.frame.color_space;
    }
    if document.source.frame.color_space == ColorSpace::Unknown
        || document.beautification().is_some_and(|beautification| {
            matches!(
                &beautification.background,
                crate::Background::Image(image) if image.color_space() == ColorSpace::Unknown
            )
        })
    {
        ColorSpace::Unknown
    } else {
        ColorSpace::Srgb
    }
}

/// Removes every covered source sample before filtered canvas transforms.
///
/// Redactions are still rendered in z-order below. This first pass protects the
/// edge of a redaction: bilinear scaling can otherwise move a covered source
/// sample into an adjacent destination pixel that the later redaction does not
/// quite cover.
fn sanitize_redacted_source(
    source: &mut Pixmap,
    annotations: &[AnnotationObject],
    source_scale: ScaleFactor,
) {
    let scale = source_scale.get() as f32;
    for object in annotations {
        let Annotation::Redact { area, style } = &object.annotation else {
            continue;
        };
        if *style == RedactStyle::SmoothBlur {
            continue;
        }
        let left = area.origin.x as f32 * scale;
        let top = area.origin.y as f32 * scale;
        let right = (area.origin.x + area.size.width) as f32 * scale;
        let bottom = (area.origin.y + area.size.height) as f32 * scale;
        if let Some(region) = redact::clip(source, left, top, right, bottom) {
            redact::solid(source, region, Color::BLACK);
        }
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
fn draw_object(canvas: &mut Pixmap, object: &AnnotationObject, xf: Scaled, redaction_seed: u64) {
    let opacity = object.style.effective_opacity();
    if opacity <= 0.0 && !object.annotation.is_destructive() {
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
            let path = shapes::text(content, *at, &object.style, xf);
            let boxed = text_box(object, xf);
            if path.is_none() && boxed.is_none() {
                return;
            }
            if object.style.shadow {
                if let Some(boxed) = &boxed {
                    draw_path_shadow(canvas, boxed, None, true, xf);
                } else if let Some(path) = &path {
                    draw_path_shadow(canvas, path, None, true, xf);
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
            let Some(path) = path else {
                return;
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
                RedactStyle::Blur => redact::blur_with_strength_and_seed(
                    canvas,
                    region,
                    strength,
                    mix_redaction_seed(redaction_seed, object.id.0),
                ),
                RedactStyle::SmoothBlur => {
                    redact::smooth_blur_with_strength(canvas, region, strength);
                }
                RedactStyle::Pixelate => {
                    redact::pixelate_with_strength_and_seed(
                        canvas,
                        region,
                        strength,
                        mix_redaction_seed(redaction_seed, object.id.0),
                    );
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

fn mix_redaction_seed(document_seed: u64, annotation_id: u64) -> u64 {
    splitmix64(document_seed ^ annotation_id.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
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
