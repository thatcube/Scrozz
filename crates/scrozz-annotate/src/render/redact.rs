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
//! The blur is a real separable Gaussian with clamp-to-edge sampling. A box blur
//! leaves recoverable structure, and zero-padding at the image border darkens
//! the edge of the redaction — a visible artifact that also advertises exactly
//! where the sensitive content was.

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

/// Replaces a region with a mosaic of block averages.
///
/// Blocks are aligned to the region, not to the image, so a redaction looks the
/// same wherever it is dragged.
pub fn pixelate(pixmap: &mut Pixmap, region: IntRect) {
    let block = pixelate_block(region);
    let width = pixmap.width() as usize;
    let (left, top) = (region.left(), region.top());
    let (right, bottom) = (region.right(), region.bottom());

    let mut by = top;
    while by < bottom {
        let block_bottom = (by + block as i32).min(bottom);
        let mut bx = left;
        while bx < right {
            let block_right = (bx + block as i32).min(right);
            let mut sum = [0u64; 4];
            let mut count = 0u64;
            {
                let pixels = pixmap.pixels();
                for y in by..block_bottom {
                    let row = y as usize * width;
                    for x in bx..block_right {
                        let p = pixels[row + x as usize];
                        sum[0] += u64::from(p.red());
                        sum[1] += u64::from(p.green());
                        sum[2] += u64::from(p.blue());
                        sum[3] += u64::from(p.alpha());
                        count += 1;
                    }
                }
            }
            if count > 0 {
                let avg = |c: u64| ((c + count / 2) / count) as u8;
                let a = avg(sum[3]);
                let flat = PremultipliedColorU8::from_rgba(
                    avg(sum[0]).min(a),
                    avg(sum[1]).min(a),
                    avg(sum[2]).min(a),
                    a,
                )
                .unwrap_or_else(transparent);
                let pixels = pixmap.pixels_mut();
                for y in by..block_bottom {
                    let row = y as usize * width;
                    for x in bx..block_right {
                        pixels[row + x as usize] = flat;
                    }
                }
            }
            bx = block_right;
        }
        by = block_bottom;
    }
}

/// Blurs a region with a separable Gaussian, sampling with clamp-to-edge.
///
/// Only pixels inside `region` are written; samples are taken from the whole
/// image, so the blur blends with its surroundings instead of fading into a
/// dark or transparent border. Weights are normalised once and every sample is
/// a real pixel, so a uniform image blurs to exactly itself — the canonical
/// check that the edges are handled correctly.
pub fn blur(pixmap: &mut Pixmap, region: IntRect) {
    let sigma = blur_sigma(region);
    blur_with_sigma(pixmap, region, sigma);
}

/// Blurs a region at an explicit sigma, in pixels.
pub fn blur_with_sigma(pixmap: &mut Pixmap, region: IntRect, sigma: f32) {
    if sigma <= 0.0 {
        return;
    }
    let radius = (sigma * 3.0).ceil() as i32;
    if radius < 1 {
        return;
    }
    let weights = gaussian_kernel(sigma, radius);

    let img_w = pixmap.width() as i32;
    let img_h = pixmap.height() as i32;
    let rw = region.width() as usize;
    let rh = region.height() as usize;
    let tmp_h = rh + 2 * radius as usize;

    // Horizontal pass into a strip tall enough for the vertical pass to read
    // `radius` rows above and below the region without going back to source.
    let mut tmp = vec![[0f32; 4]; rw * tmp_h];
    {
        let pixels = pixmap.pixels();
        for ty in 0..tmp_h {
            let sy = (region.top() + ty as i32 - radius).clamp(0, img_h - 1);
            let src_row = sy as usize * img_w as usize;
            for tx in 0..rw {
                let cx = region.left() + tx as i32;
                let mut acc = [0f32; 4];
                for (k, w) in weights.iter().enumerate() {
                    let sx = (cx + k as i32 - radius).clamp(0, img_w - 1);
                    let p = pixels[src_row + sx as usize];
                    acc[0] = f32::from(p.red()).mul_add(*w, acc[0]);
                    acc[1] = f32::from(p.green()).mul_add(*w, acc[1]);
                    acc[2] = f32::from(p.blue()).mul_add(*w, acc[2]);
                    acc[3] = f32::from(p.alpha()).mul_add(*w, acc[3]);
                }
                tmp[ty * rw + tx] = acc;
            }
        }
    }

    // Vertical pass, writing only inside the region.
    let width = pixmap.width() as usize;
    let pixels = pixmap.pixels_mut();
    for y in 0..rh {
        let dst_row = (region.top() as usize + y) * width;
        for x in 0..rw {
            let mut acc = [0f32; 4];
            for (k, w) in weights.iter().enumerate() {
                let s = tmp[(y + k) * rw + x];
                acc[0] = s[0].mul_add(*w, acc[0]);
                acc[1] = s[1].mul_add(*w, acc[1]);
                acc[2] = s[2].mul_add(*w, acc[2]);
                acc[3] = s[3].mul_add(*w, acc[3]);
            }
            let to_u8 = |v: f32| v.round().clamp(0.0, 255.0) as u8;
            let a = to_u8(acc[3]);
            pixels[dst_row + region.left() as usize + x] = PremultipliedColorU8::from_rgba(
                to_u8(acc[0]).min(a),
                to_u8(acc[1]).min(a),
                to_u8(acc[2]).min(a),
                a,
            )
            .unwrap_or_else(transparent);
        }
    }
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

fn gaussian_kernel(sigma: f32, radius: i32) -> Vec<f32> {
    let denom = 2.0 * sigma * sigma;
    let mut weights: Vec<f32> = (-radius..=radius)
        .map(|i| {
            let x = i as f32;
            (-(x * x) / denom).exp()
        })
        .collect();
    let total: f32 = weights.iter().sum();
    if total > 0.0 {
        for w in &mut weights {
            *w /= total;
        }
    }
    weights
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
