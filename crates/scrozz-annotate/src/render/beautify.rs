//! Deterministic screenshot framing.
//!
//! The source pixmap is never modified. A new canvas is resolved from the
//! authoring model, its background is painted, and the annotated source is
//! composited into that canvas once.
//!
//! Decision D9 is enforced by [`crate::Document`] and again by the renderer that
//! calls this module: a window may be translated onto an outer canvas, but its
//! native pixels cannot be cropped, rounded, bordered, or re-shadowed.

use scrozz_core::{
    ColorSpace, Error, LogicalPoint, LogicalSize, Result, Transform as ColorTransform,
};
use scrozz_export::{RgbaImage, convert_color_space, convert_srgb_color};
use tiny_skia::{
    BlendMode, FillRule, GradientStop, IntRect, LinearGradient, Paint, Pattern, Pixmap,
    PixmapPaint, Point, PremultipliedColorU8, RadialGradient, Rect, SpreadMode, Stroke, Transform,
};

use crate::{
    document::{
        AutomaticBackground, Background, BackgroundImage, Beautification, BuiltInBackground,
        GeneratedTemplate, MAX_RASTER_EDGE, MAX_RASTER_PIXELS,
    },
    render::shapes::{self, Scaled},
    style::{Color, Style},
};

/// How far the shadow falls below the capture, relative to blur depth.
const SHADOW_DROP: f32 = 0.32;
/// Maximum fraction of free space consumed by the subtle optical correction.
const MAX_BALANCE_SHIFT: f64 = 0.12;
/// Conservative ceiling for all buffers live during one render.
const MAX_WORKING_BYTES: u64 = 768 * 1024 * 1024;
const BYTES_PER_PIXEL: u64 = 4;

/// The geometry selected for one render.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedLayout {
    /// Output width in physical pixels.
    pub width: u32,
    /// Output height in physical pixels.
    pub height: u32,
    /// Source rectangle sampled from the rendered current revision.
    pub source: IntRect,
    /// Capture bounds inside the output.
    pub content: Rect,
}

pub(crate) struct ApplyOptions {
    pub scale: f64,
    pub source_scale: f64,
    pub target_color_space: ColorSpace,
    pub preserve_source_pixels: bool,
    pub retained_source_bytes: u64,
    pub target_width: Option<u32>,
}

/// Frames `content` according to `beautification`.
///
/// `scale` is physical pixels per logical point, so padding, radii, border, and
/// shadow scale with the export exactly as annotation geometry does.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] for malformed settings, a malformed custom
/// background, or a canvas too large to allocate.
pub fn apply(content: &Pixmap, beautification: &Beautification, scale: f64) -> Result<Pixmap> {
    apply_with_retained_bytes(
        content,
        beautification,
        ApplyOptions {
            scale,
            source_scale: scale,
            target_color_space: ColorSpace::Srgb,
            preserve_source_pixels: false,
            retained_source_bytes: 0,
            target_width: None,
        },
    )
}

pub(crate) fn apply_with_retained_bytes(
    content: &Pixmap,
    beautification: &Beautification,
    options: ApplyOptions,
) -> Result<Pixmap> {
    let ApplyOptions {
        scale,
        source_scale,
        target_color_space,
        preserve_source_pixels,
        retained_source_bytes,
        target_width,
    } = options;
    beautification.validate()?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(Error::InvalidRequest(format!(
            "Scene scale must be finite and positive, got {scale}"
        )));
    }

    let layout = resolve_layout_with_width(
        content,
        beautification,
        scale,
        source_scale,
        target_width,
        preserve_source_pixels,
    )?;
    preflight_working_set(content, beautification, layout, retained_source_bytes)?;
    if beautification.is_noop() {
        return Ok(content.clone());
    }
    let mut canvas = Pixmap::new(layout.width, layout.height).ok_or_else(|| {
        Error::InvalidRequest(format!(
            "Scene canvas {}x{} is not allocatable",
            layout.width, layout.height
        ))
    })?;

    paint_background(
        &mut canvas,
        content,
        &beautification.background,
        target_color_space,
    )?;

    let radius = (beautification.corner_radius * scale) as f32;
    let radius = radius.min(layout.content.width().min(layout.content.height()) / 2.0);
    let shadow = (beautification.shadow * scale) as f32;
    if shadow > 0.0 {
        draw_shadow(&mut canvas, layout.content, radius, shadow)?;
    }
    draw_content(
        &mut canvas,
        content,
        layout.source,
        layout.content,
        radius,
        preserve_source_pixels,
    );

    let border = (beautification.border_width * scale) as f32;
    if border > 0.0 && !beautification.border_color.is_invisible() {
        draw_border(
            &mut canvas,
            layout.content,
            radius,
            border,
            converted_color(beautification.border_color, target_color_space)?,
        );
    }
    if let Some(watermark) = beautification
        .watermark
        .as_ref()
        .filter(|watermark| !watermark.text.trim().is_empty())
    {
        draw_watermark(
            &mut canvas,
            layout.content,
            watermark,
            scale,
            target_color_space,
        )?;
    }
    Ok(canvas)
}

/// Resolves canvas and content geometry without allocating the canvas.
///
/// Visual auto-balance uses an integer salience centroid, so identical source
/// bytes always produce identical placement across runs.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] if the output dimensions are not finite or
/// addressable by `tiny-skia`.
pub fn resolve_layout(
    content: &Pixmap,
    beautification: &Beautification,
    scale: f64,
) -> Result<ResolvedLayout> {
    resolve_layout_with_width(content, beautification, scale, scale, None, false)
}

fn resolve_layout_with_width(
    content: &Pixmap,
    beautification: &Beautification,
    scale: f64,
    source_scale: f64,
    target_width: Option<u32>,
    preserve_source_pixels: bool,
) -> Result<ResolvedLayout> {
    beautification.validate()?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(Error::InvalidRequest(format!(
            "Scene scale must be finite and positive, got {scale}"
        )));
    }

    if !source_scale.is_finite() || source_scale <= 0.0 {
        return Err(Error::InvalidRequest(format!(
            "source scale must be finite and positive, got {source_scale}"
        )));
    }
    let source = inset_source_rect(content, beautification, scale)?;
    let logical_content = LogicalSize::new(
        f64::from(content.width()) / scale,
        f64::from(content.height()) / scale,
    );
    let output = beautification.output_size_at_scale(logical_content, source_scale);
    let (width, height) = if let Some(width) = target_width {
        (
            width,
            checked_dimension(f64::from(width) / (output.width / output.height), "height")?,
        )
    } else if beautification.output_size.is_some() {
        exact_canvas(output, scale)?
    } else {
        (
            checked_dimension(output.width * scale, "width")?,
            checked_dimension(output.height * scale, "height")?,
        )
    };
    let pixel_count = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| Error::InvalidRequest("beautified canvas area overflowed".to_owned()))?;
    if pixel_count > MAX_RASTER_PIXELS {
        return Err(Error::InvalidRequest(format!(
            "Scene canvas {width}x{height} has {pixel_count} pixels; the limit is \
             {MAX_RASTER_PIXELS}"
        )));
    }
    let padding = beautification.resolved_padding();
    let left = padding.left * scale;
    let top = padding.top * scale;
    let right = padding.right * scale;
    let bottom = padding.bottom * scale;
    let content_width = f64::from(source.width());
    let content_height = f64::from(source.height());
    if content_width + left + right > f64::from(width) + 0.5
        || content_height + top + bottom > f64::from(height) + 0.5
    {
        return Err(Error::InvalidRequest(
            "Scene output is too small to retain the complete source and requested padding"
                .to_owned(),
        ));
    }
    let available_x = (f64::from(width) - content_width).max(0.0);
    let available_y = (f64::from(height) - content_height).max(0.0);

    let (x, y) = if beautification.auto_balance {
        if let Some(focus) = resolved_focus(source, beautification) {
            (
                balanced_position(
                    f64::from(width),
                    content_width,
                    available_x,
                    left,
                    right,
                    focus.0,
                ),
                balanced_position(
                    f64::from(height),
                    content_height,
                    available_y,
                    top,
                    bottom,
                    focus.1,
                ),
            )
        } else {
            (available_x / 2.0, available_y / 2.0)
        }
    } else {
        (
            aligned_position(
                available_x,
                left,
                right,
                beautification.alignment.horizontal(),
            ),
            aligned_position(
                available_y,
                top,
                bottom,
                beautification.alignment.vertical(),
            ),
        )
    };

    let (x, y) = if preserve_source_pixels {
        (x.round(), y.round())
    } else {
        (x, y)
    };
    let content = Rect::from_xywh(
        x as f32,
        y as f32,
        content_width as f32,
        content_height as f32,
    )
    .ok_or_else(|| Error::InvalidRequest("resolved content rectangle is empty".to_owned()))?;

    Ok(ResolvedLayout {
        width,
        height,
        source,
        content,
    })
}

fn exact_canvas(output: LogicalSize, scale: f64) -> Result<(u32, u32)> {
    Ok((
        checked_dimension(output.width * scale, "width")?,
        checked_dimension(output.height * scale, "height")?,
    ))
}

fn inset_source_rect(
    content: &Pixmap,
    beautification: &Beautification,
    scale: f64,
) -> Result<IntRect> {
    let inset = beautification.inset;
    let left = checked_inset(inset.left * scale, content.width(), "left")?;
    let top = checked_inset(inset.top * scale, content.height(), "top")?;
    let right = checked_inset(inset.right * scale, content.width(), "right")?;
    let bottom = checked_inset(inset.bottom * scale, content.height(), "bottom")?;
    let width = content
        .width()
        .checked_sub(left)
        .and_then(|value| value.checked_sub(right))
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            Error::InvalidRequest("horizontal inset removes the entire source".into())
        })?;
    let height = content
        .height()
        .checked_sub(top)
        .and_then(|value| value.checked_sub(bottom))
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::InvalidRequest("vertical inset removes the entire source".into()))?;
    IntRect::from_xywh(left as i32, top as i32, width, height)
        .ok_or_else(|| Error::InvalidRequest("source inset is not renderable".into()))
}

fn checked_inset(value: f64, extent: u32, edge: &str) -> Result<u32> {
    if !value.is_finite() || value < 0.0 || value > f64::from(extent) {
        return Err(Error::InvalidRequest(format!(
            "{edge} source inset {value} exceeds the {extent}px source"
        )));
    }
    Ok(value.round() as u32)
}

fn resolved_focus(source: IntRect, beautification: &Beautification) -> Option<(f64, f64)> {
    if let Some(metadata) = &beautification.smart_frame
        && metadata.focus.confidence >= 55
    {
        return Some((
            metadata.focus.x_in(f64::from(source.width())),
            metadata.focus.y_in(f64::from(source.height())),
        ));
    }
    None
}

fn checked_dimension(value: f64, name: &str) -> Result<u32> {
    if !value.is_finite() || value <= 0.0 || value > f64::from(MAX_RASTER_EDGE) {
        return Err(Error::InvalidRequest(format!(
            "beautified canvas {name} {value} is not renderable"
        )));
    }
    Ok(value.round().max(1.0) as u32)
}

fn preflight_working_set(
    content: &Pixmap,
    beautification: &Beautification,
    layout: ResolvedLayout,
    retained_source_bytes: u64,
) -> Result<()> {
    let content_bytes = raster_bytes(content.width(), content.height())?;
    let canvas_bytes = raster_bytes(layout.width, layout.height)?;
    let base = retained_source_bytes
        .checked_add(content_bytes)
        .and_then(|bytes| bytes.checked_add(canvas_bytes))
        .ok_or_else(|| Error::InvalidRequest("beautification working set overflowed".to_owned()))?;

    let (background_retained, background_scratch) = match &beautification.background {
        Background::Image(image) | Background::Desktop(image) => {
            let image_bytes = raster_bytes(image.width(), image.height())?;
            let encoded_bytes = image.encoded_len() as u64;
            // Raw and compressed forms remain in the document. Painting needs
            // one source pixmap; conversion briefly needs two raw buffers.
            let scratch_copies = if matches!(
                image.color_space(),
                ColorSpace::DisplayP3 | ColorSpace::Rec2020
            ) {
                2
            } else {
                1
            };
            (
                image_bytes.checked_add(encoded_bytes),
                image_bytes.checked_mul(scratch_copies),
            )
        }
        Background::BlurredSource { .. } => (Some(0), Some(canvas_bytes)),
        _ => (Some(0), Some(0)),
    };
    let background_retained = background_retained
        .ok_or_else(|| Error::InvalidRequest("background working set overflowed".to_owned()))?;
    let background_scratch = background_scratch
        .ok_or_else(|| Error::InvalidRequest("background working set overflowed".to_owned()))?;
    let base = base
        .checked_add(background_retained)
        .ok_or_else(|| Error::InvalidRequest("background working set overflowed".to_owned()))?;
    let background_peak = base
        .checked_add(background_scratch)
        .ok_or_else(|| Error::InvalidRequest("background working set overflowed".to_owned()))?;

    let shadow_peak = if beautification.shadow > 0.0 {
        // The bounded shadow layer and its box-blur scratch buffer can each be
        // no larger than the canvas.
        base.checked_add(
            canvas_bytes
                .checked_mul(2)
                .ok_or_else(|| Error::InvalidRequest("shadow working set overflowed".to_owned()))?,
        )
        .ok_or_else(|| Error::InvalidRequest("shadow working set overflowed".to_owned()))?
    } else {
        base
    };
    let peak = background_peak.max(shadow_peak);
    if peak > MAX_WORKING_BYTES {
        return Err(Error::InvalidRequest(format!(
            "beautification needs about {} MiB of working memory; the limit is {} MiB",
            peak.div_ceil(1024 * 1024),
            MAX_WORKING_BYTES / (1024 * 1024)
        )));
    }
    Ok(())
}

fn raster_bytes(width: u32, height: u32) -> Result<u64> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| Error::InvalidRequest(format!("raster {width}x{height} is too large")))
}

fn aligned_position(available: f64, leading: f64, trailing: f64, alignment: f64) -> f64 {
    let leading = leading.min(available).max(0.0);
    let trailing = trailing.min(available - leading).max(0.0);
    let movable = (available - leading - trailing).max(0.0);
    leading + movable * alignment
}

fn balanced_position(
    canvas: f64,
    content: f64,
    available: f64,
    leading: f64,
    trailing: f64,
    focus: f64,
) -> f64 {
    let center = available / 2.0;
    let desired = canvas / 2.0 - focus;
    let max_shift = (available * MAX_BALANCE_SHIFT).min((leading + trailing) * 0.18);
    let subtle = desired.clamp(center - max_shift, center + max_shift);
    subtle.clamp(leading, (canvas - content - trailing).max(leading))
}

fn paint_background(
    canvas: &mut Pixmap,
    content: &Pixmap,
    background: &Background,
    target_color_space: ColorSpace,
) -> Result<()> {
    match background {
        Background::Transparent => {}
        Background::Solid(color) => {
            canvas.fill(sk_color(converted_color(*color, target_color_space)?));
        }
        Background::Gradient { start, end } => {
            paint_gradient(
                canvas,
                converted_color(*start, target_color_space)?,
                converted_color(*end, target_color_space)?,
                false,
            );
        }
        Background::BuiltIn(background) => {
            let (start, end, diagonal) = built_in_colors(*background);
            paint_gradient(
                canvas,
                converted_color(start, target_color_space)?,
                converted_color(end, target_color_space)?,
                diagonal,
            );
        }
        Background::Automatic(background) => {
            paint_generated(canvas, background, target_color_space)?;
        }
        Background::Image(image) | Background::Desktop(image) => {
            paint_image_background(canvas, image, target_color_space)?;
        }
        Background::BlurredSource { blur_radius, tint } => paint_blurred_source(
            canvas,
            content,
            *blur_radius,
            converted_color(*tint, target_color_space)?,
        )?,
    }
    Ok(())
}

fn built_in_colors(background: BuiltInBackground) -> (Color, Color, bool) {
    match background {
        BuiltInBackground::Mist => (Color::rgb(235, 239, 247), Color::rgb(205, 214, 231), true),
        BuiltInBackground::Iris => (Color::rgb(105, 113, 247), Color::rgb(105, 62, 177), true),
        BuiltInBackground::Midnight => (Color::rgb(16, 26, 58), Color::rgb(29, 91, 112), true),
        BuiltInBackground::Sunrise => (Color::rgb(255, 181, 142), Color::rgb(203, 90, 135), true),
        BuiltInBackground::Lagoon => (Color::rgb(32, 159, 153), Color::rgb(33, 83, 139), true),
        BuiltInBackground::Sand => (Color::rgb(242, 235, 222), Color::rgb(216, 201, 179), false),
    }
}

fn paint_gradient(canvas: &mut Pixmap, start: Color, end: Color, diagonal: bool) {
    let Some(rect) = Rect::from_xywh(0.0, 0.0, canvas.width() as f32, canvas.height() as f32)
    else {
        return;
    };
    let finish = if diagonal {
        Point::from_xy(canvas.width() as f32, canvas.height() as f32)
    } else {
        Point::from_xy(0.0, canvas.height() as f32)
    };
    let Some(shader) = LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        finish,
        vec![
            GradientStop::new(0.0, sk_color(start)),
            GradientStop::new(1.0, sk_color(end)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) else {
        return;
    };
    let paint = Paint {
        shader,
        anti_alias: false,
        ..Paint::default()
    };
    canvas.fill_rect(rect, &paint, Transform::identity(), None);
}

fn paint_generated(
    canvas: &mut Pixmap,
    background: &AutomaticBackground,
    target_color_space: ColorSpace,
) -> Result<()> {
    let palette = background.resolved_palette();
    let converted = palette
        .into_iter()
        .map(|color| converted_color(color, target_color_space))
        .collect::<Result<Vec<_>>>()?;
    let start = converted[0];
    let end = converted[1];
    paint_gradient(canvas, start, end, true);

    match background.template {
        GeneratedTemplate::SmoothGradient => {}
        GeneratedTemplate::TonalStudio => {
            paint_linear_overlay(canvas, converted.as_slice());
        }
        GeneratedTemplate::SoftMesh => {
            for index in 0..3 {
                let color = converted[(index + 1) % converted.len()];
                let x = seeded_unit(background.seed, index * 2) * canvas.width() as f32;
                let y = seeded_unit(background.seed, index * 2 + 1) * canvas.height() as f32;
                let radius = canvas.width().max(canvas.height()) as f32
                    * (0.62 + seeded_unit(background.seed ^ 0xa5a5_a5a5, index) * 0.22);
                paint_radial_overlay(canvas, Point::from_xy(x, y), radius, color, 118);
            }
        }
    }
    Ok(())
}

fn paint_linear_overlay(canvas: &mut Pixmap, palette: &[Color]) {
    let Some(rect) = Rect::from_xywh(0.0, 0.0, canvas.width() as f32, canvas.height() as f32)
    else {
        return;
    };
    let stops = palette
        .iter()
        .enumerate()
        .map(|(index, color)| {
            let offset = index as f32 / (palette.len().saturating_sub(1).max(1)) as f32;
            GradientStop::new(offset, sk_color(with_alpha(*color, 112)))
        })
        .collect();
    let Some(shader) = LinearGradient::new(
        Point::from_xy(canvas.width() as f32, 0.0),
        Point::from_xy(0.0, canvas.height() as f32),
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    ) else {
        return;
    };
    let paint = Paint {
        shader,
        anti_alias: false,
        blend_mode: BlendMode::SourceOver,
        ..Paint::default()
    };
    canvas.fill_rect(rect, &paint, Transform::identity(), None);
}

fn paint_radial_overlay(canvas: &mut Pixmap, center: Point, radius: f32, color: Color, alpha: u8) {
    let Some(rect) = Rect::from_xywh(0.0, 0.0, canvas.width() as f32, canvas.height() as f32)
    else {
        return;
    };
    let Some(shader) = RadialGradient::new(
        center,
        0.0,
        center,
        radius,
        vec![
            GradientStop::new(0.0, sk_color(with_alpha(color, alpha))),
            GradientStop::new(1.0, sk_color(with_alpha(color, 0))),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) else {
        return;
    };
    let paint = Paint {
        shader,
        anti_alias: false,
        blend_mode: BlendMode::SourceOver,
        ..Paint::default()
    };
    canvas.fill_rect(rect, &paint, Transform::identity(), None);
}

fn seeded_unit(seed: u64, lane: usize) -> f32 {
    let mixed = seed
        .rotate_left(((lane * 11) % 64) as u32)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (mixed & 0xffff) as f32 / 65_535.0
}

const fn with_alpha(color: Color, alpha: u8) -> Color {
    Color::rgba(color.r, color.g, color.b, alpha)
}

fn paint_blurred_source(
    canvas: &mut Pixmap,
    content: &Pixmap,
    blur_radius: u16,
    tint: Color,
) -> Result<()> {
    let scale = (canvas.width() as f32 / content.width() as f32)
        .max(canvas.height() as f32 / content.height() as f32);
    let x = (canvas.width() as f32 - content.width() as f32 * scale) / 2.0;
    let y = (canvas.height() as f32 - content.height() as f32 * scale) / 2.0;
    canvas.draw_pixmap(
        0,
        0,
        content.as_ref(),
        &PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..PixmapPaint::default()
        },
        Transform::from_row(scale, 0.0, 0.0, scale, x, y),
        None,
    );
    if blur_radius > 0 {
        box_blur(canvas, usize::from(blur_radius))?;
    }
    if !tint.is_invisible() {
        let Some(rect) = Rect::from_xywh(0.0, 0.0, canvas.width() as f32, canvas.height() as f32)
        else {
            return Ok(());
        };
        let paint = shapes::paint(tint, 1.0, BlendMode::SourceOver, ColorTransform::identity());
        canvas.fill_rect(rect, &paint, Transform::identity(), None);
    }
    Ok(())
}

fn paint_image_background(
    canvas: &mut Pixmap,
    image: &BackgroundImage,
    target_color_space: ColorSpace,
) -> Result<()> {
    image.validate()?;
    let converted;
    let pixels = if image.color_space() != target_color_space
        && image.color_space() != ColorSpace::Unknown
        && target_color_space != ColorSpace::Unknown
    {
        let source = RgbaImage {
            width: image.width(),
            height: image.height(),
            data: image.pixels().to_vec(),
        };
        converted = convert_color_space(&source, image.color_space(), target_color_space)?;
        converted.data.as_slice()
    } else {
        image.pixels()
    };
    let mut source = Pixmap::new(image.width(), image.height()).ok_or_else(|| {
        Error::InvalidRequest(format!(
            "background image {}x{} is not allocatable",
            image.width(),
            image.height()
        ))
    })?;
    for (pixel, rgba) in source
        .pixels_mut()
        .iter_mut()
        .zip(pixels.as_chunks::<4>().0)
    {
        *pixel = premultiply(rgba[0], rgba[1], rgba[2], rgba[3]);
    }

    let scale = (canvas.width() as f32 / image.width() as f32)
        .max(canvas.height() as f32 / image.height() as f32);
    let draw_width = image.width() as f32 * scale;
    let draw_height = image.height() as f32 * scale;
    let x = ((canvas.width() as f32 - draw_width) / 2.0).round() as i32;
    let y = ((canvas.height() as f32 - draw_height) / 2.0).round() as i32;
    canvas.draw_pixmap(
        0,
        0,
        source.as_ref(),
        &PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..PixmapPaint::default()
        },
        Transform::from_row(scale, 0.0, 0.0, scale, x as f32, y as f32),
        None,
    );
    Ok(())
}

/// Paints a blurred silhouette of the rounded capture behind it.
fn draw_shadow(canvas: &mut Pixmap, image_rect: Rect, radius: f32, depth: f32) -> Result<()> {
    let sigma = depth / 2.0;
    let pass_radius = sigma
        .ceil()
        .clamp(1.0, canvas.width().max(canvas.height()) as f32) as usize;
    let support = pass_radius.saturating_mul(3).saturating_add(2) as f32;
    let offset = depth * SHADOW_DROP;
    let left = (image_rect.left() - support)
        .floor()
        .clamp(0.0, canvas.width() as f32) as u32;
    let top = (image_rect.top() + offset - support)
        .floor()
        .clamp(0.0, canvas.height() as f32) as u32;
    let right = (image_rect.right() + support)
        .ceil()
        .clamp(0.0, canvas.width() as f32) as u32;
    let bottom = (image_rect.bottom() + offset + support)
        .ceil()
        .clamp(0.0, canvas.height() as f32) as u32;
    let Some(width) = right.checked_sub(left).filter(|width| *width > 0) else {
        return Ok(());
    };
    let Some(height) = bottom.checked_sub(top).filter(|height| *height > 0) else {
        return Ok(());
    };
    let Some(mut layer) = Pixmap::new(width, height) else {
        return Err(Error::InvalidRequest(format!(
            "shadow layer {}x{} is not allocatable",
            width, height
        )));
    };
    let Some(shadow_rect) = Rect::from_xywh(
        image_rect.left() - left as f32,
        image_rect.top() + offset - top as f32,
        image_rect.width(),
        image_rect.height(),
    ) else {
        return Ok(());
    };
    let Some(path) = shapes::rounded_rect(shadow_rect, radius) else {
        return Ok(());
    };
    let paint = shapes::paint(
        Color::rgba(0, 0, 0, 125),
        1.0,
        BlendMode::SourceOver,
        ColorTransform::identity(),
    );
    layer.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    box_blur(&mut layer, pass_radius)?;
    canvas.draw_pixmap(
        left as i32,
        top as i32,
        layer.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    Ok(())
}

/// Three box passes approximate a Gaussian while remaining linear in pixel
/// count. Each pass has variance close to `sigma² / 3`; using `ceil(sigma)` as
/// its radius also gives the same three-sigma support as the old convolution.
fn box_blur(layer: &mut Pixmap, radius: usize) -> Result<()> {
    let len = layer.pixels().len();
    let mut scratch = Vec::new();
    scratch.try_reserve_exact(len).map_err(|_| {
        Error::InvalidRequest(format!(
            "blur scratch buffer for {}x{} is not allocatable",
            layer.width(),
            layer.height()
        ))
    })?;
    scratch.resize(len, transparent());

    let width = layer.width() as usize;
    let height = layer.height() as usize;
    for _ in 0..3 {
        box_blur_horizontal(layer.pixels(), &mut scratch, width, height, radius);
        box_blur_vertical(&scratch, layer.pixels_mut(), width, height, radius);
    }
    Ok(())
}

fn box_blur_horizontal(
    source: &[PremultipliedColorU8],
    target: &mut [PremultipliedColorU8],
    width: usize,
    height: usize,
    radius: usize,
) {
    let divisor = radius.saturating_mul(2).saturating_add(1) as u64;
    for y in 0..height {
        let row = y * width;
        let mut sum = [0_u64; 4];
        for x in 0..=radius.min(width.saturating_sub(1)) {
            add_pixel(&mut sum, source[row + x]);
        }
        for x in 0..width {
            target[row + x] = averaged_pixel(sum, divisor);
            if let Some(remove) = x.checked_sub(radius) {
                subtract_pixel(&mut sum, source[row + remove]);
            }
            if let Some(add) = x.checked_add(radius).and_then(|value| value.checked_add(1))
                && add < width
            {
                add_pixel(&mut sum, source[row + add]);
            }
        }
    }
}

fn box_blur_vertical(
    source: &[PremultipliedColorU8],
    target: &mut [PremultipliedColorU8],
    width: usize,
    height: usize,
    radius: usize,
) {
    let divisor = radius.saturating_mul(2).saturating_add(1) as u64;
    for x in 0..width {
        let mut sum = [0_u64; 4];
        for y in 0..=radius.min(height.saturating_sub(1)) {
            add_pixel(&mut sum, source[y * width + x]);
        }
        for y in 0..height {
            target[y * width + x] = averaged_pixel(sum, divisor);
            if let Some(remove) = y.checked_sub(radius) {
                subtract_pixel(&mut sum, source[remove * width + x]);
            }
            if let Some(add) = y.checked_add(radius).and_then(|value| value.checked_add(1))
                && add < height
            {
                add_pixel(&mut sum, source[add * width + x]);
            }
        }
    }
}

fn add_pixel(sum: &mut [u64; 4], pixel: PremultipliedColorU8) {
    sum[0] += u64::from(pixel.red());
    sum[1] += u64::from(pixel.green());
    sum[2] += u64::from(pixel.blue());
    sum[3] += u64::from(pixel.alpha());
}

fn subtract_pixel(sum: &mut [u64; 4], pixel: PremultipliedColorU8) {
    sum[0] -= u64::from(pixel.red());
    sum[1] -= u64::from(pixel.green());
    sum[2] -= u64::from(pixel.blue());
    sum[3] -= u64::from(pixel.alpha());
}

fn averaged_pixel(sum: [u64; 4], divisor: u64) -> PremultipliedColorU8 {
    let average = |channel: u64| ((channel + divisor / 2) / divisor).min(255) as u8;
    let alpha = average(sum[3]);
    PremultipliedColorU8::from_rgba(
        average(sum[0]).min(alpha),
        average(sum[1]).min(alpha),
        average(sum[2]).min(alpha),
        alpha,
    )
    .unwrap_or_else(transparent)
}

/// Draws the source clipped to an antialiased rounded rectangle.
fn draw_content(
    canvas: &mut Pixmap,
    content: &Pixmap,
    source_rect: IntRect,
    image_rect: Rect,
    radius: f32,
    preserve_source_pixels: bool,
) {
    let exact_copy = (image_rect.width() - source_rect.width() as f32).abs() < f32::EPSILON
        && (image_rect.height() - source_rect.height() as f32).abs() < f32::EPSILON
        && image_rect.left().fract().abs() < f32::EPSILON
        && image_rect.top().fract().abs() < f32::EPSILON
        && radius <= 0.0;
    if exact_copy {
        copy_source_rect(
            canvas,
            content,
            source_rect,
            image_rect.left() as u32,
            image_rect.top() as u32,
            preserve_source_pixels,
        );
        return;
    }

    let scale_x = image_rect.width() / source_rect.width() as f32;
    let scale_y = image_rect.height() / source_rect.height() as f32;
    let translate_x = image_rect.left() - source_rect.x() as f32 * scale_x;
    let translate_y = image_rect.top() - source_rect.y() as f32 * scale_y;
    let full_source = source_rect.x() == 0
        && source_rect.y() == 0
        && source_rect.width() == content.width()
        && source_rect.height() == content.height();
    let transform = if full_source
        && (scale_x - 1.0).abs() < f32::EPSILON
        && (scale_y - 1.0).abs() < f32::EPSILON
    {
        Transform::from_translate(image_rect.left(), image_rect.top())
    } else {
        Transform::from_row(scale_x, 0.0, 0.0, scale_y, translate_x, translate_y)
    };
    let shader = Pattern::new(
        content.as_ref(),
        SpreadMode::Pad,
        if (scale_x - 1.0).abs() < f32::EPSILON && (scale_y - 1.0).abs() < f32::EPSILON {
            tiny_skia::FilterQuality::Nearest
        } else {
            tiny_skia::FilterQuality::Bilinear
        },
        1.0,
        transform,
    );
    let paint = Paint {
        shader,
        anti_alias: radius > 0.0,
        blend_mode: if preserve_source_pixels {
            BlendMode::Source
        } else {
            BlendMode::SourceOver
        },
        ..Paint::default()
    };
    let Some(path) = shapes::rounded_rect(image_rect, radius) else {
        return;
    };
    canvas.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn copy_source_rect(
    canvas: &mut Pixmap,
    source: &Pixmap,
    source_rect: IntRect,
    destination_x: u32,
    destination_y: u32,
    replace: bool,
) {
    let canvas_width = canvas.width() as usize;
    let source_width = source.width() as usize;
    for row in 0..source_rect.height() as usize {
        for column in 0..source_rect.width() as usize {
            let source_index =
                (source_rect.y() as usize + row) * source_width + source_rect.x() as usize + column;
            let destination_index =
                (destination_y as usize + row) * canvas_width + destination_x as usize + column;
            let Some(source_pixel) = source.pixels().get(source_index).copied() else {
                continue;
            };
            let Some(destination) = canvas.pixels_mut().get_mut(destination_index) else {
                continue;
            };
            if replace || source_pixel.alpha() == 255 {
                *destination = source_pixel;
            } else if source_pixel.alpha() > 0 {
                *destination = source_over(source_pixel, *destination);
            }
        }
    }
}

fn source_over(
    source: PremultipliedColorU8,
    destination: PremultipliedColorU8,
) -> PremultipliedColorU8 {
    let inverse = 255_u32.saturating_sub(u32::from(source.alpha()));
    let blend = |source: u8, destination: u8| {
        (u32::from(source) + (u32::from(destination) * inverse + 127) / 255).min(255) as u8
    };
    let alpha = blend(source.alpha(), destination.alpha());
    PremultipliedColorU8::from_rgba(
        blend(source.red(), destination.red()).min(alpha),
        blend(source.green(), destination.green()).min(alpha),
        blend(source.blue(), destination.blue()).min(alpha),
        alpha,
    )
    .unwrap_or_else(transparent)
}

fn draw_watermark(
    canvas: &mut Pixmap,
    content: Rect,
    watermark: &crate::Watermark,
    scale: f64,
    target_color_space: ColorSpace,
) -> Result<()> {
    let size = crate::font::measure(&watermark.text, watermark.font_size);
    let width = (size.width * scale) as f32;
    let height = (size.height * scale) as f32;
    let margin = (watermark.margin * scale) as f32;
    let at_x = (canvas.width() as f32 - width - margin).max(margin);
    let below_y = content.bottom() + margin;
    let above_y = content.top() - margin - height;
    let at_y = if below_y + height <= canvas.height() as f32 {
        below_y
    } else if above_y >= 0.0 {
        above_y
    } else {
        return Ok(());
    };
    let style = Style::default()
        .with_stroke(converted_color(watermark.color, target_color_space)?)
        .with_font_size(watermark.font_size)
        .with_stroke_width((watermark.font_size * 0.12).max(1.0));
    let Some(path) = shapes::text(
        &watermark.text,
        LogicalPoint::new(at_x as f64 / scale, at_y as f64 / scale),
        &style,
        Scaled::new(scale),
    ) else {
        return Ok(());
    };
    let paint = shapes::paint(
        style.stroke,
        1.0,
        BlendMode::SourceOver,
        ColorTransform::identity(),
    );
    shapes::stroke_path(
        canvas,
        &path,
        &paint,
        (watermark.font_size * scale * 0.12).max(1.0) as f32,
    );
    Ok(())
}

fn draw_border(canvas: &mut Pixmap, image_rect: Rect, radius: f32, width: f32, color: Color) {
    let inset = width / 2.0;
    let Some(rect) = Rect::from_ltrb(
        image_rect.left() + inset,
        image_rect.top() + inset,
        image_rect.right() - inset,
        image_rect.bottom() - inset,
    ) else {
        return;
    };
    let inner_radius = Beautification::nested_radius(f64::from(radius), f64::from(inset)) as f32;
    let Some(path) = shapes::rounded_rect(rect, inner_radius) else {
        return;
    };
    let paint = shapes::paint(
        color,
        1.0,
        BlendMode::SourceOver,
        ColorTransform::identity(),
    );
    canvas.stroke_path(
        &path,
        &paint,
        &Stroke {
            width,
            ..Stroke::default()
        },
        Transform::identity(),
        None,
    );
}

fn sk_color(color: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(color.r, color.g, color.b, color.a)
}

fn converted_color(color: Color, target: ColorSpace) -> Result<Color> {
    let [r, g, b, a] = convert_srgb_color([color.r, color.g, color.b, color.a], target)?;
    Ok(Color::rgba(r, g, b, a))
}

fn premultiply(r: u8, g: u8, b: u8, a: u8) -> PremultipliedColorU8 {
    if a == 0 {
        return transparent();
    }
    let scale = |channel: u8| ((u16::from(channel) * u16::from(a) + 127) / 255) as u8;
    PremultipliedColorU8::from_rgba(scale(r), scale(g), scale(b), a).unwrap_or_else(transparent)
}

fn transparent() -> PremultipliedColorU8 {
    PremultipliedColorU8::from_rgba(0, 0, 0, 0).expect("transparent RGBA is valid")
}
