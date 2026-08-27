//! Destructive redaction.
//!
//! # Why this is destructive, and must stay that way
//!
//! A redaction is the one annotation that is not allowed to be a *renderable
//! object over intact pixels*. Exporting a blur as a shape drawn on top of the
//! original ships the original underneath it: anyone with the file can lift the
//! overlay and read what was meant to be hidden. That is not a hypothetical —
//! it is a failure mode that has publicly burned other tools.
//!
//! So every function here rewrites the pixel buffer in place. The document's
//! source [`scrozz_core::Capture`] is never one of those buffers: the renderer
//! copies first, which is what lets decision D14's "annotations are never
//! permanent" and this module's "redactions are absolutely permanent" both be
//! true at once. The redaction object stays editable; the *exported pixels* are
//! gone.
//!
//! Secure blur and mosaic redactions use a persisted random seed and colours
//! that are independent of the covered pixels. A separate explicitly cosmetic
//! smooth-blur mode is source-dependent; secure blur remains the default.

use tiny_skia::{IntRect, Pixmap, PremultipliedColorU8};

use crate::style::Color;

/// Blur radius as a fraction of the region's shorter side.
const BLUR_FRACTION: f32 = 1.0 / 6.0;

/// Never blur less than this, or a small region stays legible.
const MIN_SIGMA: f32 = 3.0;

/// Cap on blur strength, so a full-screen redaction stays tractable.
///
/// Well past the point of unrecoverability; beyond it a larger kernel buys no
/// extra privacy, only time.
const MAX_SIGMA: f32 = 16.0;

/// Mosaic block size as a fraction of the region's shorter side.
const PIXELATE_FRACTION: f32 = 1.0 / 8.0;

/// Never mosaic in blocks smaller than this many pixels.
const MIN_BLOCK: u32 = 4;

/// Clips a region to the pixmap, returning `None` if nothing is left.
#[must_use]
pub fn clip(pixmap: &Pixmap, left: f32, top: f32, right: f32, bottom: f32) -> Option<IntRect> {
    let x0 = left.floor().max(0.0) as i32;
    let y0 = top.floor().max(0.0) as i32;
    let x1 = (right.ceil() as i32).min(pixmap.width() as i32);
    let y1 = (bottom.ceil() as i32).min(pixmap.height() as i32);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    IntRect::from_ltrb(x0, y0, x1, y1)
}

/// Fills a region with a flat colour, destroying what was there.
pub fn solid(pixmap: &mut Pixmap, region: IntRect, color: Color) {
    let premultiplied = premultiply(color);
    let width = pixmap.width() as usize;
    let pixels = pixmap.pixels_mut();
    for y in region.top()..region.bottom() {
        let row = y as usize * width;
        for x in region.left()..region.right() {
            pixels[row + x as usize] = premultiplied;
        }
    }
}

/// Replaces a region with a source-independent mosaic.
///
/// Blocks are aligned to the region, not to the image, so a redaction looks the
/// same wherever it is dragged.
pub fn pixelate(pixmap: &mut Pixmap, region: IntRect) {
    pixelate_with_strength_and_seed(pixmap, region, 0.65, 1);
}

/// Replaces a region with a secure mosaic adjusted by the editor strength.
pub fn pixelate_with_strength(pixmap: &mut Pixmap, region: IntRect, strength: f32) {
    pixelate_with_strength_and_seed(pixmap, region, strength, 1);
}

/// Replaces a region with a deterministic, input-independent secure mosaic.
///
/// The seed is persisted by the document and mixed with the annotation id
/// before it reaches this function. Each block is opaque and generated without
/// reading the source pixels, so the exported mosaic contains no block averages
/// from which a coarse copy of the hidden content could be reconstructed.
pub fn pixelate_with_strength_and_seed(
    pixmap: &mut Pixmap,
    region: IntRect,
    strength: f32,
    seed: u64,
) {
    let block =
        ((pixelate_block(region) as f32 * strength_factor(strength)).round() as u32).max(MIN_BLOCK);
    let width = pixmap.width() as usize;
    let (left, top) = (region.left(), region.top());
    let (right, bottom) = (region.right(), region.bottom());

    let mut block_y = 0_u64;
    let mut by = top;
    while by < bottom {
        let block_bottom = (by + block as i32).min(bottom);
        let mut block_x = 0_u64;
        let mut bx = left;
        while bx < right {
            let block_right = (bx + block as i32).min(right);
            let random = splitmix64(
                seed ^ block_x.wrapping_mul(0xD6E8_FEB8_6659_FD93)
                    ^ block_y.wrapping_mul(0xA5A3_564E_27F8_865D),
            );
            let level = 28 + (random & 0x5f) as u8;
            let blue = level.saturating_add(((random >> 8) & 0x0f) as u8);
            let flat = PremultipliedColorU8::from_rgba(level, level, blue, 255)
                .expect("opaque channels are valid premultiplied colours");
            let pixels = pixmap.pixels_mut();
            for y in by..block_bottom {
                let row = y as usize * width;
                for x in bx..block_right {
                    pixels[row + x as usize] = flat;
                }
            }
            bx = block_right;
            block_x += 1;
        }
        by = block_bottom;
        block_y += 1;
    }
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

/// Replaces a region with a smooth source-independent field.
///
/// This preserves the visual language of blur without deriving any output
/// value from the secret pixels it covers.
pub fn blur(pixmap: &mut Pixmap, region: IntRect) {
    blur_with_strength_and_seed(pixmap, region, 0.65, 1);
}

/// Secure-blurs a region adjusted by the persisted editor strength.
pub fn blur_with_strength(pixmap: &mut Pixmap, region: IntRect, strength: f32) {
    blur_with_strength_and_seed(pixmap, region, strength, 1);
}

/// Secure-blurs a region using a deterministic per-object seed.
pub fn blur_with_strength_and_seed(pixmap: &mut Pixmap, region: IntRect, strength: f32, seed: u64) {
    let sigma = (blur_sigma(region) * strength_factor(strength)).clamp(MIN_SIGMA, 24.0);
    blur_with_sigma_and_seed(pixmap, region, sigma, seed);
}

/// Applies a conventional source-dependent smooth blur.
///
/// This mode is intentionally cosmetic rather than reconstruction-resistant.
/// The editor labels it separately from the source-independent secure blur.
pub fn smooth_blur_with_strength(pixmap: &mut Pixmap, region: IntRect, strength: f32) {
    let strength = if strength.is_finite() {
        strength.clamp(0.0, 1.0)
    } else {
        0.65
    };
    let weight = (18.0_f32.mul_add(strength, 4.0)).round() as u16;
    for _ in 0..2 {
        smooth_horizontal(pixmap, region, weight);
        smooth_vertical(pixmap, region, weight);
    }
}

fn smooth_horizontal(pixmap: &mut Pixmap, region: IntRect, weight: u16) {
    let width = pixmap.width() as usize;
    let left = region.left() as usize;
    let right = region.right() as usize;
    let pixels = pixmap.pixels_mut();
    for y in region.top() as usize..region.bottom() as usize {
        let row = y * width;
        let mut previous = pixels[row + left];
        for x in left + 1..right {
            previous = blend(previous, pixels[row + x], weight);
            pixels[row + x] = previous;
        }
        previous = pixels[row + right - 1];
        for x in (left..right - 1).rev() {
            previous = blend(previous, pixels[row + x], weight);
            pixels[row + x] = previous;
        }
    }
}

fn smooth_vertical(pixmap: &mut Pixmap, region: IntRect, weight: u16) {
    let width = pixmap.width() as usize;
    let top = region.top() as usize;
    let bottom = region.bottom() as usize;
    let pixels = pixmap.pixels_mut();
    for x in region.left() as usize..region.right() as usize {
        let mut previous = pixels[top * width + x];
        for y in top + 1..bottom {
            previous = blend(previous, pixels[y * width + x], weight);
            pixels[y * width + x] = previous;
        }
        previous = pixels[(bottom - 1) * width + x];
        for y in (top..bottom - 1).rev() {
            previous = blend(previous, pixels[y * width + x], weight);
            pixels[y * width + x] = previous;
        }
    }
}

fn blend(
    previous: PremultipliedColorU8,
    current: PremultipliedColorU8,
    weight: u16,
) -> PremultipliedColorU8 {
    let divisor = weight + 1;
    let channel =
        |left: u8, right: u8| ((u16::from(left) * weight + u16::from(right)) / divisor) as u8;
    PremultipliedColorU8::from_rgba(
        channel(previous.red(), current.red()),
        channel(previous.green(), current.green()),
        channel(previous.blue(), current.blue()),
        channel(previous.alpha(), current.alpha()),
    )
    .expect("weighted premultiplied channels remain premultiplied")
}

/// Secure-blurs a region at an explicit visual scale, in pixels.
pub fn blur_with_sigma(pixmap: &mut Pixmap, region: IntRect, sigma: f32) {
    blur_with_sigma_and_seed(pixmap, region, sigma, 1);
}

fn blur_with_sigma_and_seed(pixmap: &mut Pixmap, region: IntRect, sigma: f32, seed: u64) {
    if sigma <= 0.0 {
        return;
    }
    let cell = (sigma * 2.0).round().clamp(4.0, 64.0) as u32;
    let width = pixmap.width() as usize;
    let pixels = pixmap.pixels_mut();
    for y in 0..region.height() {
        let dst_row = (region.top() as usize + y as usize) * width;
        for x in 0..region.width() {
            pixels[dst_row + region.left() as usize + x as usize] =
                secure_blur_pixel(seed, x, y, cell);
        }
    }
}

fn secure_blur_pixel(seed: u64, x: u32, y: u32, cell: u32) -> PremultipliedColorU8 {
    let grid_x = x / cell;
    let grid_y = y / cell;
    let x_mix = (x % cell) as f32 / cell as f32;
    let y_mix = (y % cell) as f32 / cell as f32;
    let top = lerp(
        blur_level(seed, grid_x, grid_y),
        blur_level(seed, grid_x + 1, grid_y),
        x_mix,
    );
    let bottom = lerp(
        blur_level(seed, grid_x, grid_y + 1),
        blur_level(seed, grid_x + 1, grid_y + 1),
        x_mix,
    );
    let level = lerp(top, bottom, y_mix).round().clamp(0.0, 255.0) as u8;
    PremultipliedColorU8::from_rgba(level, level, level, 255)
        .expect("opaque channels are valid premultiplied colours")
}

fn blur_level(seed: u64, x: u32, y: u32) -> f32 {
    let random = splitmix64(
        seed ^ u64::from(x).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ u64::from(y).wrapping_mul(0xBF58_476D_1CE4_E5B9),
    );
    (56 + (random % 104)) as f32
}

fn lerp(from: f32, to: f32, amount: f32) -> f32 {
    from + (to - from) * amount
}

/// The blur strength used for a region of this size.
#[must_use]
pub fn blur_sigma(region: IntRect) -> f32 {
    let shorter = region.width().min(region.height()) as f32;
    (shorter * BLUR_FRACTION).clamp(MIN_SIGMA, MAX_SIGMA)
}

/// The mosaic block size used for a region of this size.
#[must_use]
pub fn pixelate_block(region: IntRect) -> u32 {
    let shorter = region.width().min(region.height()) as f32;
    ((shorter * PIXELATE_FRACTION).round() as u32).max(MIN_BLOCK)
}

fn strength_factor(strength: f32) -> f32 {
    let strength = if strength.is_finite() {
        strength.clamp(0.0, 1.0)
    } else {
        0.65
    };
    // Preserve the v1 renderer's output at the v2 default (0.65), while still
    // giving the editor meaningful room in both directions.
    0.5 + strength * (10.0 / 13.0)
}

fn premultiply(color: Color) -> PremultipliedColorU8 {
    let a = color.a;
    let scale = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
    PremultipliedColorU8::from_rgba(scale(color.r), scale(color.g), scale(color.b), a)
        .unwrap_or_else(transparent)
}

fn transparent() -> PremultipliedColorU8 {
    PremultipliedColorU8::from_rgba(0, 0, 0, 0).expect("transparent is valid premultiplied")
}
