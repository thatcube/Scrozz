//! In-place BGRA overlay compositing for the opt-in overlay path.

use std::ffi::c_void;

use objc2_core_media::CMSampleBuffer;
use scrozz_core::{Error, PixelFormat, Result};

use crate::OverlayLayer;

const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

unsafe extern "C" {
    fn CVPixelBufferGetPixelFormatType(pixel_buffer: *const c_void) -> u32;
    fn CVPixelBufferGetWidth(pixel_buffer: *const c_void) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: *const c_void) -> usize;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: *const c_void) -> usize;
    fn CVPixelBufferGetBaseAddress(pixel_buffer: *const c_void) -> *mut c_void;
    fn CVPixelBufferLockBaseAddress(pixel_buffer: *const c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: *const c_void, flags: u64) -> i32;
}

pub(crate) fn composite(sample: &CMSampleBuffer, layers: &[OverlayLayer]) -> Result<()> {
    if layers.is_empty() {
        return Ok(());
    }
    // SAFETY: immutable sample-buffer property read.
    let image = unsafe { sample.image_buffer() }
        .ok_or_else(|| Error::Codec("overlay frame did not contain a pixel buffer".to_owned()))?;
    let pixel_buffer = std::ptr::from_ref(&*image).cast::<c_void>();
    // SAFETY: the pointer references the retained CVPixelBuffer for this scope.
    if unsafe { CVPixelBufferGetPixelFormatType(pixel_buffer) } != BGRA {
        return Err(Error::Codec(
            "overlays require ScreenCaptureKit BGRA frames".to_owned(),
        ));
    }
    // SAFETY: flags zero requests writable access; the retained pixel buffer
    // remains live until after the matching unlock.
    let lock_status = unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, 0) };
    if lock_status != 0 {
        return Err(Error::Codec(format!(
            "locking overlay pixel buffer failed with status {lock_status}"
        )));
    }

    // SAFETY: valid while the successful base-address lock is held.
    let (base, width, height, stride) = unsafe {
        (
            CVPixelBufferGetBaseAddress(pixel_buffer).cast::<u8>(),
            CVPixelBufferGetWidth(pixel_buffer),
            CVPixelBufferGetHeight(pixel_buffer),
            CVPixelBufferGetBytesPerRow(pixel_buffer),
        )
    };
    if base.is_null() || stride < width.saturating_mul(4) {
        // SAFETY: matches the successful lock above.
        unsafe {
            CVPixelBufferUnlockBaseAddress(pixel_buffer, 0);
        }
        return Err(Error::Codec(
            "overlay pixel buffer has no writable packed base address".to_owned(),
        ));
    }

    for layer in layers {
        if !layer.content.is_well_formed() {
            // SAFETY: matches the successful lock above.
            unsafe {
                CVPixelBufferUnlockBaseAddress(pixel_buffer, 0);
            }
            return Err(Error::Codec(
                "overlay source returned a malformed frame".to_owned(),
            ));
        }
        // SAFETY: destination dimensions and stride came from CoreVideo,
        // source bounds are checked by Frame::is_well_formed, and each pixel
        // is clipped to the destination before pointer arithmetic.
        unsafe { blend_layer(base, width, height, stride, layer) };
    }

    // SAFETY: matches the successful lock above.
    let unlock_status = unsafe { CVPixelBufferUnlockBaseAddress(pixel_buffer, 0) };
    if unlock_status == 0 {
        Ok(())
    } else {
        Err(Error::Codec(format!(
            "unlocking overlay pixel buffer failed with status {unlock_status}"
        )))
    }
}

unsafe fn blend_layer(
    destination: *mut u8,
    destination_width: usize,
    destination_height: usize,
    destination_stride: usize,
    layer: &OverlayLayer,
) {
    let origin_x = layer.origin.x.round() as i64;
    let origin_y = layer.origin.y.round() as i64;
    let opacity = layer.opacity.clamp(0.0, 1.0);
    let source_width = layer.content.width() as usize;
    let source_height = layer.content.height() as usize;
    let invert = layer.adaptive_contrast
        // SAFETY: `blend_layer`'s caller established this locked allocation and
        // the helper only samples coordinates clipped to its dimensions.
        && unsafe {
            background_is_dark(
                destination,
                destination_width,
                destination_height,
                destination_stride,
                origin_x,
                origin_y,
                source_width,
                source_height,
            )
        };

    for source_y in 0..source_height {
        let destination_y = origin_y + source_y as i64;
        if !(0..destination_height as i64).contains(&destination_y) {
            continue;
        }
        for source_x in 0..source_width {
            let destination_x = origin_x + source_x as i64;
            if !(0..destination_width as i64).contains(&destination_x) {
                continue;
            }

            let source_offset = source_y * layer.content.stride + source_x * 4;
            let source = &layer.content.data[source_offset..source_offset + 4];
            let (mut red, mut green, mut blue, alpha) = channels(source, layer.content.format);
            if invert {
                let ceiling = if layer.content.format.is_premultiplied() {
                    alpha
                } else {
                    1.0
                };
                red = ceiling - red;
                green = ceiling - green;
                blue = ceiling - blue;
            }
            let effective_alpha = alpha * opacity;
            let premultiplied = if layer.content.format.is_premultiplied() {
                [blue * opacity, green * opacity, red * opacity]
            } else {
                [
                    blue * effective_alpha,
                    green * effective_alpha,
                    red * effective_alpha,
                ]
            };

            let destination_offset =
                destination_y as usize * destination_stride + destination_x as usize * 4;
            // SAFETY: caller established the destination allocation and all
            // offsets are clipped to its width and height.
            let destination =
                unsafe { std::slice::from_raw_parts_mut(destination.add(destination_offset), 4) };
            let inverse = 1.0 - effective_alpha;
            for channel in 0..3 {
                destination[channel] = (premultiplied[channel]
                    + f32::from(destination[channel]) / 255.0 * inverse)
                    .clamp(0.0, 1.0)
                    .mul_add(255.0, 0.0)
                    .round() as u8;
            }

            destination[3] = (effective_alpha + f32::from(destination[3]) / 255.0 * inverse)
                .clamp(0.0, 1.0)
                .mul_add(255.0, 0.0)
                .round() as u8;
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn background_is_dark(
        destination: *mut u8,
        destination_width: usize,
        destination_height: usize,
        destination_stride: usize,
        origin_x: i64,
        origin_y: i64,
        source_width: usize,
        source_height: usize,
    ) -> bool {
        let x0 = origin_x.max(0) as usize;
        let y0 = origin_y.max(0) as usize;
        let x1 = (origin_x + source_width as i64).clamp(0, destination_width as i64) as usize;
        let y1 = (origin_y + source_height as i64).clamp(0, destination_height as i64) as usize;
        if x0 >= x1 || y0 >= y1 {
            return false;
        }
        let step_x = ((x1 - x0) / 16).max(1);
        let step_y = ((y1 - y0) / 8).max(1);
        let mut total = 0_u64;
        let mut count = 0_u64;
        for y in (y0..y1).step_by(step_y) {
            for x in (x0..x1).step_by(step_x) {
                // SAFETY: caller's locked BGRA allocation contains every sampled
                // coordinate and row stride.
                let pixel = unsafe {
                    std::slice::from_raw_parts(destination.add(y * destination_stride + x * 4), 3)
                };
                total +=
                    u64::from(pixel[2]) * 54 + u64::from(pixel[1]) * 183 + u64::from(pixel[0]) * 19;
                count += 256;
            }
        }
        count > 0 && total / count < 116
    }
}

fn channels(pixel: &[u8], format: PixelFormat) -> (f32, f32, f32, f32) {
    let (red, green, blue, alpha) = match format {
        PixelFormat::Rgba8 | PixelFormat::RgbaPremultiplied8 => {
            (pixel[0], pixel[1], pixel[2], pixel[3])
        }
        PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8 => {
            (pixel[2], pixel[1], pixel[0], pixel[3])
        }
    };
    (
        f32::from(red) / 255.0,
        f32::from(green) / 255.0,
        f32::from(blue) / 255.0,
        f32::from(alpha) / 255.0,
    )
}

#[cfg(test)]
mod tests {
    use scrozz_core::{ColorSpace, PhysicalPoint, PhysicalSize, ScaleFactor};

    use super::*;

    #[test]
    fn channel_conversion_respects_rgba_and_bgra_layouts() {
        assert_eq!(
            channels(&[255, 128, 0, 255], PixelFormat::Rgba8),
            (1.0, 128.0 / 255.0, 0.0, 1.0)
        );
        assert_eq!(
            channels(&[0, 128, 255, 255], PixelFormat::Bgra8),
            (1.0, 128.0 / 255.0, 0.0, 1.0)
        );
    }

    #[test]
    fn a_translucent_rgba_layer_blends_into_bgra_video() {
        let layer = OverlayLayer {
            content: scrozz_core::Frame {
                data: vec![255, 0, 0, 128],
                size: PhysicalSize::new(1.0, 1.0),
                stride: 4,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            origin: PhysicalPoint::new(0.0, 0.0),
            opacity: 1.0,
            adaptive_contrast: false,
        };
        let mut destination = [0, 0, 0, 255];

        // SAFETY: destination is one tightly packed BGRA pixel matching the
        // dimensions and stride supplied to the helper.
        unsafe { blend_layer(destination.as_mut_ptr(), 1, 1, 4, &layer) };

        assert_eq!(destination, [0, 0, 128, 255]);
    }
}
