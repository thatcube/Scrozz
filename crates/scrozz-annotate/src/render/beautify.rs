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

use scrozz_core::{Result, Transform as ColorTransform};
use tiny_skia::{
    BlendMode, FillRule, GradientStop, IntRect, LinearGradient, Paint, Pattern, Pixmap, Point,
    Rect, SpreadMode, Transform,
};

use crate::{
    document::{Background, Beautification},
    render::{redact, shapes},
    style::Color,
};

/// How far the shadow is offset downwards, as a fraction of its depth.
const SHADOW_DROP: f32 = 0.35;

/// Frames `content` according to `beautification`.
///
/// `scale` is the canvas's physical pixels per logical point, so padding, radius
/// and shadow all scale with the export size exactly as the annotations do.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] if the framed canvas would be too large to
/// allocate.
pub fn apply(
    content: &Pixmap,
    beautification: &Beautification,
    scale: f64,
    into: ColorTransform,
) -> Result<Pixmap> {
    if beautification.is_noop() {
        return Ok(content.clone());
    }

    let pad = (beautification.padding.max(0.0) * scale).round() as u32;
    let radius = (beautification.corner_radius.max(0.0) * scale) as f32;
    let shadow = (beautification.shadow.max(0.0) * scale) as f32;

    let width = content.width() + pad * 2;
    let height = content.height() + pad * 2;
    let mut canvas = super::new_pixmap(width, height, "beautified canvas")?;

    paint_background(&mut canvas, beautification.background, into);

    let Some(image_rect) = Rect::from_xywh(
        pad as f32,
        pad as f32,
        content.width() as f32,
        content.height() as f32,
    ) else {
        return Ok(canvas);
    };

    if shadow > 0.0 {
        draw_shadow(&mut canvas, image_rect, radius, shadow)?;
    }
    draw_content(&mut canvas, content, image_rect, radius);
    Ok(canvas)
}

/// One backdrop colour, converted into the working space.
fn converted(color: Color, into: ColorTransform) -> tiny_skia::Color {
    let [r, g, b] = into.convert_u8([color.r, color.g, color.b]);
    tiny_skia::Color::from_rgba8(r, g, b, color.a)
}

fn paint_background(canvas: &mut Pixmap, background: Background, into: ColorTransform) {
    match background {
        Background::Transparent => {}
        Background::Solid(color) => {
            let [r, g, b] = into.convert_u8([color.r, color.g, color.b]);
            canvas.fill(tiny_skia::Color::from_rgba8(r, g, b, color.a));
        }
        Background::Gradient { start, end } => {
            let Some(rect) =
                Rect::from_xywh(0.0, 0.0, canvas.width() as f32, canvas.height() as f32)
            else {
                return;
            };
            let shader = LinearGradient::new(
                Point::from_xy(0.0, 0.0),
                Point::from_xy(0.0, canvas.height() as f32),
                vec![
                    GradientStop::new(0.0, converted(start, into)),
                    GradientStop::new(1.0, converted(end, into)),
                ],
                SpreadMode::Pad,
                Transform::identity(),
            );
            let Some(shader) = shader else { return };
            let paint = Paint {
                shader,
                anti_alias: false,
                ..Paint::default()
            };
            canvas.fill_rect(rect, &paint, Transform::identity(), None);
        }
    }
}

/// Paints a blurred silhouette of the image behind it.
///
/// The silhouette is the *same rounded shape* as the image, so the shadow hugs
/// the corners instead of squaring them off — the exact mistake D9's field note
/// records.
fn draw_shadow(canvas: &mut Pixmap, image_rect: Rect, radius: f32, depth: f32) -> Result<()> {
    let mut layer = super::new_pixmap(canvas.width(), canvas.height(), "shadow layer")?;
    let offset = depth * SHADOW_DROP;
    let Some(shadow_rect) = Rect::from_xywh(
        image_rect.left(),
        image_rect.top() + offset,
        image_rect.width(),
        image_rect.height(),
    ) else {
        return Ok(());
    };
    let Some(path) = shapes::rounded_rect(shadow_rect, radius) else {
        return Ok(());
    };
    // Neutral black is the same triple in every space sharing a white point, so
    // the shadow needs no conversion and this function needs no working space.
    let paint = shapes::paint(
        Color::rgba(0, 0, 0, 130),
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

    if let Some(region) = IntRect::from_ltrb(0, 0, layer.width() as i32, layer.height() as i32) {
        redact::blur_with_sigma(&mut layer, region, depth / 2.0)?;
    }

    canvas.draw_pixmap(
        0,
        0,
        layer.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    Ok(())
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
