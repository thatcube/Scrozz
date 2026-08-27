//! Padding, background, corner rounding and drop shadow.
//!
//! # Decision D9: this is refused for window captures
//!
//! For a window, the OS output *is* the truth. `ScreenCaptureKit` already
//! returns correct corners, shadow and alpha, and re-rounding or re-shadowing
//! them produces the subtly wrong image that separates a flawless tool from an
//! almost-right one. So beautification is not merely disabled in the UI: it is
//! refused here, at the renderer, so a document assembled by any other route
//! still cannot slip through.
//!
//! Where the same rounded shape nests inside another, D9's corollary applies:
//! `inner_radius = outer_radius − padding`. See
//! [`Beautification::nested_radius`](crate::Beautification::nested_radius).

use scrozz_core::{
    ColorSpace, Error, LogicalSize, PhysicalPoint, PhysicalRect, PhysicalSize, Result,
};
use scrozz_export::{RgbaImage, convert_to_srgb};
use tiny_skia::{
    BlendMode, FillRule, GradientStop, LinearGradient, Paint, Pattern, Pixmap, PixmapPaint, Point,
    PremultipliedColorU8, Rect, SpreadMode, Stroke, Transform,
};

use crate::{
    document::{Background, BackgroundImage, Beautification, BuiltInBackground, MAX_RASTER_PIXELS},
    render::shapes,
    style::Color,
};

const SHADOW_DROP: f32 = 0.32;
const BALANCE_INSET: f64 = 0.35;
const MAX_WORKING_BYTES: u64 = 768 * 1024 * 1024;
const BYTES_PER_PIXEL: u64 = 4;

/// Actual outer canvas placement for one render.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedLayout {
    /// Final raster width.
    pub width: u32,
    /// Final raster height.
    pub height: u32,
    /// Entire transformed inner canvas inside the final raster.
    pub content: PhysicalRect,
}

/// Frames `content` according to `beautification`.
///
/// `scale` is the canvas's physical pixels per logical point, so padding, radius
/// and shadow all scale with the export size exactly as the annotations do.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] if the framed canvas would be too large to
/// allocate.
pub fn apply(content: &Pixmap, beautification: &Beautification, scale: f64) -> Result<Pixmap> {
    let logical = LogicalSize::new(
        f64::from(content.width()) / scale,
        f64::from(content.height()) / scale,
    );
    Ok(apply_with_layout(content, logical, beautification, scale, 0, None)?.0)
}

/// Applies framing and returns its exact final raster placement.
pub(crate) fn apply_with_layout(
    content: &Pixmap,
    logical_content: LogicalSize,
    beautification: &Beautification,
    scale: f64,
    retained_source_bytes: u64,
    target_width: Option<u32>,
) -> Result<(Pixmap, ResolvedLayout)> {
    let layout = resolve_layout_with_width(
        content,
        logical_content,
        beautification,
        scale,
        target_width,
    )?;
    preflight_working_set(content, beautification, layout, retained_source_bytes)?;
    if beautification.is_noop() {
        return Ok((content.clone(), layout));
    }

    let mut canvas = Pixmap::new(layout.width, layout.height).ok_or_else(|| {
        Error::InvalidRequest(format!(
            "beautified canvas {}x{} is not allocatable",
            layout.width, layout.height
        ))
    })?;
    paint_background(&mut canvas, &beautification.background)?;

    let image_rect = physical_to_skia(layout.content)?;
    let radius = (beautification.corner_radius * scale) as f32;
    let radius = radius.min(image_rect.width().min(image_rect.height()) / 2.0);
    let shadow = (beautification.shadow * scale) as f32;
    if shadow > 0.0 {
        draw_shadow(&mut canvas, image_rect, radius, shadow)?;
    }
    draw_content(&mut canvas, content, image_rect, radius);

    let border = (beautification.border_width * scale) as f32;
    if border > 0.0 && !beautification.border_color.is_invisible() {
        draw_border(
            &mut canvas,
            image_rect,
            radius,
            border,
            beautification.border_color,
        );
    }
    Ok((canvas, layout))
}

/// Resolves final geometry without allocating the outer canvas.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] for invalid settings or dimensions.
pub fn resolve_layout(
    content: &Pixmap,
    beautification: &Beautification,
    scale: f64,
) -> Result<ResolvedLayout> {
    let logical = LogicalSize::new(
        f64::from(content.width()) / scale,
        f64::from(content.height()) / scale,
    );
    resolve_layout_with_width(content, logical, beautification, scale, None)
}

fn resolve_layout_with_width(
    content: &Pixmap,
    logical_content: LogicalSize,
    beautification: &Beautification,
    scale: f64,
    target_width: Option<u32>,
) -> Result<ResolvedLayout> {
    beautification.validate()?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(Error::InvalidRequest(format!(
            "beautification scale must be finite and positive, got {scale}"
        )));
    }
    if beautification.is_noop() {
        let width = target_width.unwrap_or(content.width());
        if width != content.width() {
            return Err(Error::InvalidRequest(format!(
                "no-op framing cannot change inner width {} to {width}",
                content.width()
            )));
        }
        return Ok(ResolvedLayout {
            width,
            height: content.height(),
            content: PhysicalRect::new(
                PhysicalPoint::new(0.0, 0.0),
                PhysicalSize::new(f64::from(content.width()), f64::from(content.height())),
            ),
        });
    }

    let output = beautification.output_size(logical_content);
    let width = target_width.unwrap_or(checked_dimension(output.width * scale, "width")?);
    let height = checked_dimension(output.height * scale, "height")?;
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| Error::InvalidRequest("beautified canvas area overflowed".to_owned()))?;
    if pixels > MAX_RASTER_PIXELS {
        return Err(Error::InvalidRequest(format!(
            "beautified canvas {width}x{height} has {pixels} pixels; the limit is \
             {MAX_RASTER_PIXELS}"
        )));
    }
    if content.width() > width || content.height() > height {
        return Err(Error::InvalidRequest(format!(
            "inner canvas {}x{} does not fit beautified output {width}x{height}",
            content.width(),
            content.height()
        )));
    }

    let available_x = f64::from(width - content.width());
    let available_y = f64::from(height - content.height());
    let padding = beautification.padding * scale;
    let (x, y) = if beautification.auto_balance {
        let focus = visual_centroid(content);
        (
            balanced_position(
                f64::from(width),
                f64::from(content.width()),
                available_x,
                padding,
                focus.0,
            ),
            balanced_position(
                f64::from(height),
                f64::from(content.height()),
                available_y,
                padding,
                focus.1,
            ),
        )
    } else {
        (
            aligned_position(available_x, padding, beautification.alignment.horizontal()),
            aligned_position(available_y, padding, beautification.alignment.vertical()),
        )
    };
    Ok(ResolvedLayout {
        width,
        height,
        content: PhysicalRect::new(
            PhysicalPoint::new(x, y),
            PhysicalSize::new(f64::from(content.width()), f64::from(content.height())),
        ),
    })
}

fn checked_dimension(value: f64, name: &str) -> Result<u32> {
    if !value.is_finite() || value <= 0.0 || value > f64::from(u32::MAX) {
        return Err(Error::InvalidRequest(format!(
            "beautified canvas {name} {value} is not renderable"
        )));
    }
    Ok(value.round().max(1.0) as u32)
}

fn physical_to_skia(rect: PhysicalRect) -> Result<Rect> {
    Rect::from_xywh(
        rect.origin.x as f32,
        rect.origin.y as f32,
        rect.size.width as f32,
        rect.size.height as f32,
    )
    .ok_or_else(|| Error::InvalidRequest("resolved content rectangle is empty".to_owned()))
}

fn paint_background(canvas: &mut Pixmap, background: &Background) -> Result<()> {
    match background {
        Background::Transparent => {}
        Background::Solid(color) => {
            canvas.fill(sk_color(*color));
        }
        Background::Gradient { start, end } => {
            paint_gradient(canvas, *start, *end, false);
        }
        Background::BuiltIn(background) => {
            let (start, end, diagonal) = built_in_colors(*background);
            paint_gradient(canvas, start, end, diagonal);
        }
        Background::Image(image) => paint_image_background(canvas, image)?,
    }
    Ok(())
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
        Background::Image(image) => {
            let bytes = raster_bytes(image.width(), image.height())?;
            let copies = if matches!(
                image.color_space(),
                ColorSpace::DisplayP3 | ColorSpace::Rec2020
            ) {
                2
            } else {
                1
            };
            (
                bytes.checked_add(image.encoded_len() as u64),
                bytes.checked_mul(copies),
            )
        }
        _ => (Some(0), Some(0)),
    };
    let retained = background_retained
        .and_then(|background| base.checked_add(background))
        .ok_or_else(|| Error::InvalidRequest("background working set overflowed".to_owned()))?;
    let background_peak = background_scratch
        .and_then(|scratch| retained.checked_add(scratch))
        .ok_or_else(|| Error::InvalidRequest("background working set overflowed".to_owned()))?;
    let shadow_peak =
        if beautification.shadow > 0.0 {
            retained
                .checked_add(canvas_bytes.checked_mul(2).ok_or_else(|| {
                    Error::InvalidRequest("shadow working set overflowed".to_owned())
                })?)
                .ok_or_else(|| Error::InvalidRequest("shadow working set overflowed".to_owned()))?
        } else {
            retained
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

fn aligned_position(available: f64, padding: f64, alignment: f64) -> f64 {
    let safe_padding = padding.min(available / 2.0).max(0.0);
    safe_padding + (available - safe_padding * 2.0).max(0.0) * alignment
}

fn balanced_position(canvas: f64, content: f64, available: f64, padding: f64, focus: f64) -> f64 {
    let inset = (padding * BALANCE_INSET).min(available / 2.0).max(0.0);
    (canvas / 2.0 - focus).clamp(inset, (canvas - content - inset).max(inset))
}

fn visual_centroid(content: &Pixmap) -> (f64, f64) {
    let step_x = (content.width() / 256).max(1);
    let step_y = (content.height() / 256).max(1);
    let edge = edge_luma(content);
    let mut total = 0_u64;
    let mut sum_x = 0_u128;
    let mut sum_y = 0_u128;
    for y in (0..content.height()).step_by(step_y as usize) {
        for x in (0..content.width()).step_by(step_x as usize) {
            let pixel = content.pixel(x, y).unwrap_or_else(transparent);
            let alpha = u64::from(pixel.alpha());
            if alpha == 0 {
                continue;
            }
            let r = pixel.red();
            let g = pixel.green();
            let b = pixel.blue();
            let luma = luma(r, g, b);
            let saturation = u64::from(r.max(g).max(b) - r.min(g).min(b));
            let contrast = u64::from(luma.abs_diff(edge));
            let weight = alpha * (8 + saturation + contrast);
            total = total.saturating_add(weight);
            sum_x = sum_x.saturating_add(u128::from(x) * u128::from(weight));
            sum_y = sum_y.saturating_add(u128::from(y) * u128::from(weight));
        }
    }
    if total == 0 {
        return (
            f64::from(content.width()) / 2.0,
            f64::from(content.height()) / 2.0,
        );
    }
    (
        sum_x as f64 / total as f64 + 0.5,
        sum_y as f64 / total as f64 + 0.5,
    )
}

fn edge_luma(content: &Pixmap) -> u8 {
    let mut total = 0_u64;
    let mut count = 0_u64;
    let step_x = (content.width() / 128).max(1);
    let step_y = (content.height() / 128).max(1);
    for x in (0..content.width()).step_by(step_x as usize) {
        for y in [0, content.height() - 1] {
            if let Some(pixel) = content.pixel(x, y) {
                total += u64::from(luma(pixel.red(), pixel.green(), pixel.blue()));
                count += 1;
            }
        }
    }
    for y in (0..content.height()).step_by(step_y as usize) {
        for x in [0, content.width() - 1] {
            if let Some(pixel) = content.pixel(x, y) {
                total += u64::from(luma(pixel.red(), pixel.green(), pixel.blue()));
                count += 1;
            }
        }
    }
    total.checked_div(count).map_or(0, |value| value as u8)
}

fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((u32::from(r) * 54 + u32::from(g) * 183 + u32::from(b) * 19) / 256) as u8
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
    canvas.fill_rect(
        rect,
        &Paint {
            shader,
            anti_alias: false,
            ..Paint::default()
        },
        Transform::identity(),
        None,
    );
}

fn paint_image_background(canvas: &mut Pixmap, image: &BackgroundImage) -> Result<()> {
    image.validate()?;
    let converted;
    let pixels = if matches!(
        image.color_space(),
        ColorSpace::DisplayP3 | ColorSpace::Rec2020
    ) {
        converted = convert_to_srgb(
            &RgbaImage {
                width: image.width(),
                height: image.height(),
                data: image.pixels().to_vec(),
            },
            image.color_space(),
        )?;
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
    for (pixel, rgba) in source.pixels_mut().iter_mut().zip(pixels.chunks_exact(4)) {
        *pixel = premultiply(rgba[0], rgba[1], rgba[2], rgba[3]);
    }

    let scale = (canvas.width() as f32 / image.width() as f32)
        .max(canvas.height() as f32 / image.height() as f32);
    let x = ((canvas.width() as f32 - image.width() as f32 * scale) / 2.0).round();
    let y = ((canvas.height() as f32 - image.height() as f32 * scale) / 2.0).round();
    canvas.draw_pixmap(
        0,
        0,
        source.as_ref(),
        &PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..PixmapPaint::default()
        },
        Transform::from_row(scale, 0.0, 0.0, scale, x, y),
        None,
    );
    Ok(())
}

/// Paints a clipped, bounded blurred silhouette behind the content.
fn draw_shadow(canvas: &mut Pixmap, image_rect: Rect, radius: f32, depth: f32) -> Result<()> {
    let pass_radius = (depth / 2.0)
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
    let mut layer = Pixmap::new(width, height).ok_or_else(|| {
        Error::InvalidRequest(format!("shadow layer {width}x{height} is not allocatable"))
    })?;
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
    let paint = shapes::paint(Color::rgba(0, 0, 0, 125), 1.0, BlendMode::SourceOver);
    layer.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    box_blur_shadow(&mut layer, pass_radius)?;
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

fn box_blur_shadow(layer: &mut Pixmap, radius: usize) -> Result<()> {
    let len = layer.pixels().len();
    let mut scratch = Vec::new();
    scratch.try_reserve_exact(len).map_err(|_| {
        Error::InvalidRequest(format!(
            "shadow scratch buffer for {}x{} is not allocatable",
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

/// Draws the content clipped to a rounded rectangle.
///
/// Filling the rounded path with the image as a pattern, rather than drawing the
/// image and then masking it, keeps the corner antialiased against whatever is
/// behind it instead of against an opaque matte.
fn draw_content(canvas: &mut Pixmap, content: &Pixmap, image_rect: Rect, radius: f32) {
    let shader = Pattern::new(
        content.as_ref(),
        SpreadMode::Pad,
        tiny_skia::FilterQuality::Nearest,
        1.0,
        Transform::from_translate(image_rect.left(), image_rect.top()),
    );
    let paint = Paint {
        shader,
        anti_alias: true,
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
    canvas.stroke_path(
        &path,
        &shapes::paint(color, 1.0, BlendMode::SourceOver),
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
