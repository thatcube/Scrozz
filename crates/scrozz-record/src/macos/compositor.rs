//! Multi-stream BGRA composition onto one global desktop canvas.

use std::ptr::{NonNull, null, null_mut};

use objc2_core_foundation::CFRetained;
use objc2_core_foundation::CFType;
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceSRGB};
use objc2_core_media::{
    CMSampleBuffer, CMSampleTimingInfo, CMVideoFormatDescription,
    CMVideoFormatDescriptionCreateForImageBuffer,
};
use objc2_core_video::{
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVImageBufferCGColorSpaceKey, kCVPixelFormatType_32BGRA,
};
use scrozz_core::{Error, Result};

use super::content::PixelRect;

pub(crate) struct Compositor {
    width: u32,
    height: u32,
    tiles: Vec<Tile>,
    cadence: FrameCadence,
    color_space: CFRetained<CFType>,
}

// SAFETY: the retained CGColorSpace is immutable, and a Compositor is only
// accessed while holding Shared's compositor mutex.
unsafe impl Send for Compositor {}

struct Tile {
    destination: PixelRect,
    pixels: Option<Vec<u8>>,
}

struct FrameCadence {
    interval_seconds: f64,
    next_output_seconds: Option<f64>,
}

impl Compositor {
    pub(crate) fn new(
        width: u32,
        height: u32,
        destinations: Vec<PixelRect>,
        fps: u32,
    ) -> Result<Self> {
        if width == 0 || height == 0 || destinations.is_empty() || fps == 0 {
            return Err(Error::InvalidRequest(
                "a composite recording needs a non-empty canvas, source list, and frame rate"
                    .to_owned(),
            ));
        }
        for destination in &destinations {
            if destination.width == 0
                || destination.height == 0
                || destination.x.saturating_add(destination.width) > width
                || destination.y.saturating_add(destination.height) > height
            {
                return Err(Error::InvalidRequest(
                    "a composite recording source lies outside its output canvas".to_owned(),
                ));
            }
        }
        // SAFETY: immutable weak-linked CoreGraphics color-space name.
        let color_space_name = unsafe { kCGColorSpaceSRGB };
        let color_space =
            CGColorSpace::with_name(Some(color_space_name)).ok_or_else(|| Error::Unsupported {
                what: "composited recording color conversion".to_owned(),
                why: "CoreGraphics did not expose the standard sRGB color space".to_owned(),
            })?;
        // SAFETY: every CGColorSpace is a CFType; type erasure preserves ownership.
        let color_space = unsafe { CFRetained::cast_unchecked::<CFType>(color_space) };
        Ok(Self {
            width,
            height,
            tiles: destinations
                .into_iter()
                .map(|destination| Tile {
                    destination,
                    pixels: None,
                })
                .collect(),
            cadence: FrameCadence {
                interval_seconds: 1.0 / f64::from(fps),
                next_output_seconds: None,
            },
            color_space,
        })
    }

    pub(crate) fn update(&mut self, source_index: usize, sample: &CMSampleBuffer) -> Result<bool> {
        let tile = self.tiles.get_mut(source_index).ok_or_else(|| {
            Error::Codec(format!(
                "ScreenCaptureKit returned an unknown composite source {source_index}"
            ))
        })?;
        copy_bgra(
            sample,
            tile.destination,
            tile.pixels.get_or_insert_with(Vec::new),
        )?;
        Ok(self.tiles.iter().all(|tile| tile.pixels.is_some()))
    }

    pub(crate) fn ready_to_emit(&mut self, timing_source: &CMSampleBuffer) -> Result<bool> {
        // SAFETY: immutable timestamp read from a live SCK sample.
        let seconds = unsafe { timing_source.presentation_time_stamp().seconds() };
        if !seconds.is_finite() {
            return Err(Error::Codec(
                "composite source had no numeric presentation timestamp".to_owned(),
            ));
        }
        Ok(self.cadence.accept(seconds))
    }

    pub(crate) fn sources_ready(&self) -> bool {
        self.tiles.iter().all(|tile| tile.pixels.is_some())
    }

    pub(crate) fn compose(
        &self,
        timing_source: &CMSampleBuffer,
    ) -> Result<CFRetained<CMSampleBuffer>> {
        let pixel_buffer = create_pixel_buffer(self.width, self.height)?;
        write_canvas(&pixel_buffer, self.width, self.height, &self.tiles)?;

        // SAFETY: immutable image-buffer read while the source sample is retained.
        if let Some(source_image) = unsafe { timing_source.image_buffer() } {
            source_image.propagate_attachments(&pixel_buffer);
        }
        // SAFETY: CoreVideo accepts a CGColorSpace CFType for this documented
        // attachment and retains it for the output buffer.
        unsafe {
            let key = kCVImageBufferCGColorSpaceKey;
            pixel_buffer.set_attachment(key, &self.color_space, CVAttachmentMode::ShouldPropagate);
        }

        let mut format: *const CMVideoFormatDescription = null();
        // SAFETY: output points to writable storage and the retained image buffer
        // stays alive through format construction.
        let status = unsafe {
            CMVideoFormatDescriptionCreateForImageBuffer(
                None,
                &pixel_buffer,
                NonNull::from(&mut format),
            )
        };
        if status != 0 {
            return Err(Error::Codec(format!(
                "creating a composite video format failed with status {status}"
            )));
        }
        let format = NonNull::new(format.cast_mut()).ok_or_else(|| {
            Error::Codec("CoreMedia returned no composite video format".to_owned())
        })?;
        // SAFETY: the Create rule transfers one owned retain.
        let format = unsafe { CFRetained::from_raw(format) };

        // SAFETY: immutable timing reads from a live SCK sample.
        let mut timing = unsafe {
            CMSampleTimingInfo {
                duration: timing_source.duration(),
                presentationTimeStamp: timing_source.presentation_time_stamp(),
                decodeTimeStamp: timing_source.decode_time_stamp(),
            }
        };
        let mut sample = null_mut();
        // SAFETY: all retained inputs and timing storage stay live for the call.
        let status = unsafe {
            CMSampleBuffer::create_ready_with_image_buffer(
                None,
                &pixel_buffer,
                &format,
                NonNull::from(&mut timing),
                NonNull::from(&mut sample),
            )
        };
        if status != 0 {
            return Err(Error::Codec(format!(
                "creating a composite video sample failed with status {status}"
            )));
        }
        let sample = NonNull::new(sample).ok_or_else(|| {
            Error::Codec("CoreMedia returned no composite video sample".to_owned())
        })?;
        // SAFETY: the Create rule transfers one owned retain.
        Ok(unsafe { CFRetained::from_raw(sample) })
    }
}

impl FrameCadence {
    fn accept(&mut self, seconds: f64) -> bool {
        let Some(next) = self.next_output_seconds else {
            self.next_output_seconds = Some(seconds + self.interval_seconds);
            return true;
        };
        let tolerance = self.interval_seconds * 0.1;
        if seconds + tolerance < next {
            return false;
        }
        let intervals = ((seconds + tolerance - next) / self.interval_seconds).floor() + 1.0;
        self.next_output_seconds = Some(next + intervals * self.interval_seconds);
        true
    }
}

fn copy_bgra(sample: &CMSampleBuffer, destination: PixelRect, pixels: &mut Vec<u8>) -> Result<()> {
    // SAFETY: immutable image-buffer read while the callback retains the sample.
    let image = unsafe { sample.image_buffer() }
        .ok_or_else(|| Error::Codec("composite source had no pixel buffer".to_owned()))?;
    if CVPixelBufferGetPixelFormatType(&image) != kCVPixelFormatType_32BGRA {
        return Err(Error::Codec(
            "multi-display composition requires BGRA ScreenCaptureKit frames".to_owned(),
        ));
    }
    let width = CVPixelBufferGetWidth(&image);
    let height = CVPixelBufferGetHeight(&image);
    if width != destination.width as usize || height != destination.height as usize {
        return Err(Error::Codec(format!(
            "composite source produced {width}x{height}, expected {}x{}",
            destination.width, destination.height
        )));
    }
    let flags = CVPixelBufferLockFlags::ReadOnly;
    // SAFETY: the retained image buffer remains live through the matching unlock.
    let status = unsafe { CVPixelBufferLockBaseAddress(&image, flags) };
    if status != 0 {
        return Err(Error::Codec(format!(
            "locking a composite source failed with status {status}"
        )));
    }

    let result = (|| {
        let stride = CVPixelBufferGetBytesPerRow(&image);
        let row_bytes = width
            .checked_mul(4)
            .ok_or_else(|| Error::Codec("composite source row size overflowed usize".to_owned()))?;
        let base = CVPixelBufferGetBaseAddress(&image).cast::<u8>();
        if base.is_null() || stride < row_bytes {
            return Err(Error::Codec(
                "composite source had no packed BGRA base address".to_owned(),
            ));
        }
        let length = row_bytes.checked_mul(height).ok_or_else(|| {
            Error::Codec("composite source buffer size overflowed usize".to_owned())
        })?;
        pixels.resize(length, 0);
        for row in 0..height {
            // SAFETY: the lock exposes height rows of at least stride bytes.
            let source = unsafe { std::slice::from_raw_parts(base.add(row * stride), row_bytes) };
            pixels[row * row_bytes..(row + 1) * row_bytes].copy_from_slice(source);
        }
        Ok(())
    })();

    // SAFETY: matches the successful read-only lock above.
    let unlock = unsafe { CVPixelBufferUnlockBaseAddress(&image, flags) };
    if unlock != 0 {
        return Err(Error::Codec(format!(
            "unlocking a composite source failed with status {unlock}"
        )));
    }
    result
}

fn create_pixel_buffer(width: u32, height: u32) -> Result<CFRetained<CVPixelBuffer>> {
    let mut pixel_buffer = null_mut();
    // SAFETY: CoreVideo allocates storage and writes one Create-rule object to
    // the non-null output pointer.
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width as usize,
            height as usize,
            kCVPixelFormatType_32BGRA,
            None,
            NonNull::from(&mut pixel_buffer),
        )
    };
    if status != 0 {
        return Err(Error::Codec(format!(
            "allocating a composite pixel buffer failed with status {status}"
        )));
    }
    let pixel_buffer = NonNull::new(pixel_buffer)
        .ok_or_else(|| Error::Codec("CoreVideo returned no composite pixel buffer".to_owned()))?;
    // SAFETY: the Create rule transfers one owned retain.
    Ok(unsafe { CFRetained::from_raw(pixel_buffer) })
}

fn write_canvas(
    pixel_buffer: &CVPixelBuffer,
    width: u32,
    height: u32,
    tiles: &[Tile],
) -> Result<()> {
    let flags = CVPixelBufferLockFlags::empty();
    // SAFETY: the retained pixel buffer remains live through the matching unlock.
    let status = unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, flags) };
    if status != 0 {
        return Err(Error::Codec(format!(
            "locking the composite canvas failed with status {status}"
        )));
    }

    let result = (|| {
        let stride = CVPixelBufferGetBytesPerRow(pixel_buffer);
        let row_bytes = width as usize * 4;
        let base = CVPixelBufferGetBaseAddress(pixel_buffer).cast::<u8>();
        if base.is_null() || stride < row_bytes {
            return Err(Error::Codec(
                "composite canvas had no writable packed BGRA base address".to_owned(),
            ));
        }
        // SAFETY: the successful lock exposes `height` rows of `stride` bytes.
        let canvas =
            unsafe { std::slice::from_raw_parts_mut(base, stride.saturating_mul(height as usize)) };
        canvas.fill(0);
        for row in canvas.chunks_exact_mut(stride).take(height as usize) {
            for alpha in row[3..row_bytes].iter_mut().step_by(4) {
                *alpha = 255;
            }
        }
        for tile in tiles {
            let pixels = tile.pixels.as_ref().ok_or_else(|| {
                Error::Codec("composite frame was emitted before every source arrived".to_owned())
            })?;
            blit(pixels, tile.destination, canvas, stride);
        }
        Ok(())
    })();

    // SAFETY: matches the successful writable lock above.
    let unlock = unsafe { CVPixelBufferUnlockBaseAddress(pixel_buffer, flags) };
    if unlock != 0 {
        return Err(Error::Codec(format!(
            "unlocking the composite canvas failed with status {unlock}"
        )));
    }
    result
}

fn blit(source: &[u8], destination: PixelRect, canvas: &mut [u8], canvas_stride: usize) {
    let source_stride = destination.width as usize * 4;
    for row in 0..destination.height as usize {
        let source_start = row * source_stride;
        let destination_start =
            (destination.y as usize + row) * canvas_stride + destination.x as usize * 4;
        canvas[destination_start..destination_start + source_stride]
            .copy_from_slice(&source[source_start..source_start + source_stride]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_preserves_tile_positions_and_black_gaps() {
        let mut canvas = vec![0_u8; 4 * 3 * 4];
        let left = vec![1_u8; 2 * 2 * 4];
        let right = vec![2_u8; 3 * 4];
        blit(
            &left,
            PixelRect {
                x: 0,
                y: 1,
                width: 2,
                height: 2,
            },
            &mut canvas,
            4 * 4,
        );
        blit(
            &right,
            PixelRect {
                x: 3,
                y: 0,
                width: 1,
                height: 3,
            },
            &mut canvas,
            4 * 4,
        );

        assert_eq!(&canvas[0..12], &[0; 12]);
        assert_eq!(&canvas[12..16], &[2; 4]);
        assert_eq!(&canvas[16..24], &[1; 8]);
        assert_eq!(&canvas[28..32], &[2; 4]);
        assert_eq!(&canvas[32..40], &[1; 8]);
        assert_eq!(&canvas[44..48], &[2; 4]);
    }

    #[test]
    fn cadence_accepts_changes_from_any_source_without_exceeding_requested_fps() {
        let mut cadence = FrameCadence {
            interval_seconds: 1.0 / 30.0,
            next_output_seconds: None,
        };

        assert!(cadence.accept(10.0));
        assert!(!cadence.accept(10.001));
        assert!(cadence.accept(10.033));
        assert!(!cadence.accept(10.034));
        assert!(cadence.accept(10.067));
    }
}
