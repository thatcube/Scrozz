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
//! The blur approximates a Gaussian with three bounded separable box passes.
//! Each uses clamp-to-edge sampling and a sliding window, so work is linear in
//! the region rather than in the blur radius. Zero-padding at the image border
//! would darken the edge of the redaction — a visible artifact that also
//! advertises where the content was.

use scrozz_core::{Error, Result};
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
const MAX_BLUR_SCRATCH_BYTES: usize = 256 * 1024 * 1024;

/// Mosaic block size as a fraction of the region's shorter side.
const PIXELATE_FRACTION: f32 = 1.0 / 8.0;

/// Never mosaic in blocks smaller than this many pixels.
const MIN_BLOCK: u32 = 4;
const SECURE_MIN_BLOCK: u32 = 6;
const SECURE_MAX_BLOCK: u32 = 64;

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

/// Irreversibly replaces a region with a deterministic randomized mosaic.
///
/// Unlike the legacy visual Pixelate variant, this combines every block with
/// the region mean, quantizes channels, and adds seed-derived noise before
/// writing opaque replacement pixels. Even the lowest valid intensity therefore
/// destroys local detail rather than storing a reversible overlay or a faithful
/// downsample. Higher intensity monotonically grows blocks and increases the
/// global mixing/quantization.
pub fn secure_mosaic(pixmap: &mut Pixmap, region: IntRect, intensity: f32, seed: u64) {
    let intensity = if intensity.is_finite() {
        intensity.clamp(0.0, 1.0)
    } else {
        crate::REDACT_INTENSITY_DEFAULT
    };
    let block = secure_block(region, intensity);
    let width = pixmap.width() as usize;
    let (left, top) = (region.left(), region.top());
    let (right, bottom) = (region.right(), region.bottom());
    let global = average_region(pixmap, region);
    let global_mix = 0.55 + intensity * 0.35;
    let quantization = 32 + (intensity * 48.0).round() as i32;
    let noise = 5 + (intensity * 13.0).round() as i32;

    let mut block_y = 0u64;
    let mut by = top;
    while by < bottom {
        let block_bottom = (by + block as i32).min(bottom);
        let mut block_x = 0u64;
        let mut bx = left;
        while bx < right {
            let block_right = (bx + block as i32).min(right);
            let local = average_rect(pixmap, bx, by, block_right, block_bottom);
            let channel = |index: usize| {
                let mixed = f32::from(local[index])
                    .mul_add(1.0 - global_mix, f32::from(global[index]) * global_mix)
                    .round() as i32;
                let quantized = ((mixed + quantization / 2) / quantization) * quantization;
                let hash = mix_seed(
                    seed ^ block_x.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                        ^ block_y.wrapping_mul(0xD1B5_4A32_D192_ED03)
                        ^ (index as u64).wrapping_mul(0x94D0_49BB_1331_11EB),
                );
                let span = noise * 2 + 1;
                let mut offset = (hash % span as u64) as i32 - noise;
                if offset == 0 {
                    offset = if hash & 1 == 0 { 1 } else { -1 };
                }
                (quantized + offset).clamp(8, 247) as u8
            };
            let flat = PremultipliedColorU8::from_rgba(channel(0), channel(1), channel(2), 255)
                .expect("opaque channels are valid premultiplied colours");
            let pixels = pixmap.pixels_mut();
            for y in by..block_bottom {
                let row = y as usize * width;
                for x in bx..block_right {
                    pixels[row + x as usize] = flat;
                }
            }
            block_x += 1;
            bx = block_right;
        }
        block_y += 1;
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
pub fn blur(pixmap: &mut Pixmap, region: IntRect) -> Result<()> {
    let sigma = blur_sigma(region);
    blur_with_sigma(pixmap, region, sigma)
}

/// Blurs a region at an explicit sigma, in pixels.
pub fn blur_with_sigma(pixmap: &mut Pixmap, region: IntRect, sigma: f32) -> Result<()> {
    if sigma <= 0.0 {
        return Ok(());
    }
    for radius in box_radii(sigma.min(MAX_SIGMA)) {
        box_blur_once(pixmap, region, radius)?;
    }
    Ok(())
}

fn box_blur_once(pixmap: &mut Pixmap, region: IntRect, radius: i32) -> Result<()> {
    if radius < 1 {
        return Ok(());
    }

    let img_w = pixmap.width() as i32;
    let img_h = pixmap.height() as i32;
    let rw = region.width() as usize;
    let rh = region.height() as usize;
    let tmp_h = rh + 2 * radius as usize;
    let stride = rw.checked_mul(4).ok_or_else(|| {
        Error::InvalidRequest("blur scratch row is too large to address".to_owned())
    })?;
    let scratch_len = stride.checked_mul(tmp_h).ok_or_else(|| {
        Error::InvalidRequest("blur scratch buffer is too large to address".to_owned())
    })?;
    if scratch_len > MAX_BLUR_SCRATCH_BYTES {
        return Err(Error::InvalidRequest(format!(
            "blur scratch buffer of {scratch_len} bytes exceeds the 256 MiB render limit"
        )));
    }
    let mut tmp = Vec::new();
    tmp.try_reserve_exact(scratch_len).map_err(|error| {
        Error::InvalidRequest(format!(
            "blur scratch buffer of {scratch_len} bytes is not allocatable: {error}"
        ))
    })?;
    tmp.resize(scratch_len, 0u8);
    let diameter = u64::try_from(2 * radius + 1).unwrap_or(1);

    // Horizontal pass into a strip tall enough for the vertical pass to read its
    // halo without returning to the source.
    {
        let pixels = pixmap.pixels();
        for ty in 0..tmp_h {
            let sy = (region.top() + ty as i32 - radius).clamp(0, img_h - 1);
            let src_row = sy as usize * img_w as usize;
            let mut sum = [0u64; 4];
            for dx in -radius..=radius {
                let sx = (region.left() + dx).clamp(0, img_w - 1);
                add_pixel(&mut sum, pixels[src_row + sx as usize]);
            }
            for tx in 0..rw {
                write_average(&mut tmp[ty * stride + tx * 4..][..4], sum, diameter);
                if tx + 1 < rw {
                    let cx = region.left() + tx as i32;
                    let remove = (cx - radius).clamp(0, img_w - 1);
                    let add = (cx + radius + 1).clamp(0, img_w - 1);
                    sub_pixel(&mut sum, pixels[src_row + remove as usize]);
                    add_pixel(&mut sum, pixels[src_row + add as usize]);
                }
            }
        }
    }

    // Vertical pass, writing only inside the redaction.
    let width = pixmap.width() as usize;
    let pixels = pixmap.pixels_mut();
    for x in 0..rw {
        let mut sum = [0u64; 4];
        for y in 0..=2 * radius as usize {
            add_channels(&mut sum, &tmp[y * stride + x * 4..][..4]);
        }
        for y in 0..rh {
            let channels = averaged(sum, diameter);
            let a = channels[3];
            let dst_row = (region.top() as usize + y) * width;
            pixels[dst_row + region.left() as usize + x] = PremultipliedColorU8::from_rgba(
                channels[0].min(a),
                channels[1].min(a),
                channels[2].min(a),
                a,
            )
            .unwrap_or_else(transparent);
            if y + 1 < rh {
                sub_channels(&mut sum, &tmp[y * stride + x * 4..][..4]);
                add_channels(
                    &mut sum,
                    &tmp[(y + 2 * radius as usize + 1) * stride + x * 4..][..4],
                );
            }
        }
    }
    Ok(())
}

fn box_radii(sigma: f32) -> [i32; 3] {
    const PASSES: f32 = 3.0;
    let ideal = (12.0f32.mul_add(sigma * sigma / PASSES, 1.0)).sqrt();
    let mut lower = ideal.floor() as i32;
    if lower % 2 == 0 {
        lower -= 1;
    }
    lower = lower.max(1);
    let upper = lower + 2;
    let lower_f = lower as f32;
    let choose_lower = ((12.0 * sigma * sigma
        - PASSES * lower_f * lower_f
        - 4.0 * PASSES * lower_f
        - 3.0 * PASSES)
        / (-4.0 * lower_f - 4.0))
        .round()
        .clamp(0.0, PASSES) as usize;
    std::array::from_fn(|index| {
        let width = if index < choose_lower { lower } else { upper };
        (width - 1) / 2
    })
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

/// Secure-mosaic block size for `intensity`.
#[must_use]
pub fn secure_block(region: IntRect, intensity: f32) -> u32 {
    let intensity = if intensity.is_finite() {
        intensity.clamp(0.0, 1.0)
    } else {
        crate::REDACT_INTENSITY_DEFAULT
    };
    let shorter = region.width().min(region.height()).max(1) as f32;
    (shorter * (0.10 + intensity * 0.20))
        .round()
        .clamp(SECURE_MIN_BLOCK as f32, SECURE_MAX_BLOCK as f32) as u32
}

fn average_region(pixmap: &Pixmap, region: IntRect) -> [u8; 4] {
    average_rect(
        pixmap,
        region.left(),
        region.top(),
        region.right(),
        region.bottom(),
    )
}

fn average_rect(pixmap: &Pixmap, left: i32, top: i32, right: i32, bottom: i32) -> [u8; 4] {
    let width = pixmap.width() as usize;
    let mut sum = [0u64; 4];
    let mut count = 0u64;
    for y in top..bottom {
        let row = y as usize * width;
        for x in left..right {
            add_straight_pixel(&mut sum, pixmap.pixels()[row + x as usize]);
            count += 1;
        }
    }
    if count == 0 {
        [0, 0, 0, 255]
    } else {
        let mut average = averaged(sum, count);
        average[3] = 255;
        average
    }
}

fn add_straight_pixel(sum: &mut [u64; 4], pixel: PremultipliedColorU8) {
    let alpha = pixel.alpha();
    let straight = |channel: u8| {
        if alpha == 0 {
            128
        } else {
            ((u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255) as u8
        }
    };
    sum[0] += u64::from(straight(pixel.red()));
    sum[1] += u64::from(straight(pixel.green()));
    sum[2] += u64::from(straight(pixel.blue()));
    sum[3] += 255;
}

fn mix_seed(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn add_pixel(sum: &mut [u64; 4], pixel: PremultipliedColorU8) {
    for (total, channel) in
        sum.iter_mut()
            .zip([pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()])
    {
        *total += u64::from(channel);
    }
}

fn sub_pixel(sum: &mut [u64; 4], pixel: PremultipliedColorU8) {
    for (total, channel) in
        sum.iter_mut()
            .zip([pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()])
    {
        *total -= u64::from(channel);
    }
}

fn add_channels(sum: &mut [u64; 4], channels: &[u8]) {
    for (total, channel) in sum.iter_mut().zip(channels) {
        *total += u64::from(*channel);
    }
}

fn sub_channels(sum: &mut [u64; 4], channels: &[u8]) {
    for (total, channel) in sum.iter_mut().zip(channels) {
        *total -= u64::from(*channel);
    }
}

fn averaged(sum: [u64; 4], count: u64) -> [u8; 4] {
    sum.map(|channel| ((channel + count / 2) / count) as u8)
}

fn write_average(output: &mut [u8], sum: [u64; 4], count: u64) {
    output.copy_from_slice(&averaged(sum, count));
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
