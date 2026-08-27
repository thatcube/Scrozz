//! Turning encoded image bytes back into a [`Frame`].
//!
//! The counterpart to [`crate::encode`], and deliberately in the same crate. A
//! decoder that lives anywhere else becomes a second, divergent path for the
//! same job — and the two then disagree about stride, channel order or
//! premultiplication in exactly the cases that are hardest to notice.
//!
//! Decoding matters because several Scrozz features operate on images the app
//! did not capture: OCR over a file on disk, re-opening a stored capture whose
//! annotation document outlived its pixels (D23), and importing an image to
//! annotate.

use image::{DynamicImage, ImageDecoder, ImageReader, Limits};
use scrozz_core::{ColorSpace, Error, Frame, PhysicalSize, PixelFormat, Result, ScaleFactor};

use crate::profile_for;

/// 8K-class inputs fit; dimensions large enough to threaten the process do not.
const MAX_DECODED_PIXELS: u64 = 40_000_000;
const MAX_DECODER_BYTES: u64 = 256 * 1024 * 1024;

/// Decodes encoded image bytes into a frame.
///
/// The result is always straight-alpha [`PixelFormat::Rgba8`] with a tightly
/// packed stride, because that is what the `image` crate produces and inventing
/// padding here would only create work for every consumer.
///
/// # Colour space
///
/// ICC profiles emitted by Scrozz are recognised exactly. Other embedded
/// profiles remain [`ColorSpace::Unknown`] rather than being guessed: claiming
/// sRGB for a Display P3 image is precisely the mistake that makes wide-gamut
/// screenshots look wrong.
///
/// # Scale
///
/// A file carries no notion of the display it came from, so the frame is
/// reported at [`ScaleFactor::IDENTITY`]. A caller that knows better — the store
/// reopening its own capture, say — should override it rather than let this
/// guess.
///
/// # Errors
///
/// Returns [`Error::Codec`] if the bytes are not a supported image, or if the
/// image is too large to address.
pub fn decode(bytes: &[u8]) -> Result<Frame> {
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::Codec(format!("could not read image header: {e}")))?;
    reader.limits(decoder_limits());

    let mut decoder = reader
        .into_decoder()
        .map_err(|e| Error::Codec(format!("could not create image decoder: {e}")))?;
    let (width, height) = decoder.dimensions();
    let decoded_bytes = decoder.total_bytes();
    validate_decoded_layout(width, height, decoded_bytes)?;
    let mut remaining = decoder_limits();
    remaining
        .reserve(decoded_bytes)
        .map_err(|e| Error::Codec(format!("decoded image exceeds memory limits: {e}")))?;
    decoder
        .set_limits(remaining)
        .map_err(|e| Error::Codec(format!("decoder cannot enforce memory limits: {e}")))?;
    let color_space = decoder
        .icc_profile()
        .map_err(|e| Error::Codec(format!("could not read image colour profile: {e}")))?
        .as_deref()
        .map_or(ColorSpace::Unknown, known_profile_space);
    let image = DynamicImage::from_decoder(decoder)
        .map_err(|e| Error::Codec(format!("could not decode image: {e}")))?;

    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();

    if width == 0 || height == 0 {
        return Err(Error::Codec(format!(
            "image has a zero dimension ({width}×{height})"
        )));
    }

    let stride = width as usize * PixelFormat::Rgba8.bytes_per_pixel();

    Ok(Frame {
        data: rgba.into_raw(),
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride,
        format: PixelFormat::Rgba8,
        color_space,
        scale: ScaleFactor::IDENTITY,
    })
}

fn decoder_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODED_PIXELS as u32);
    limits.max_image_height = Some(MAX_DECODED_PIXELS as u32);
    limits.max_alloc = Some(MAX_DECODER_BYTES);
    limits
}

fn validate_decoded_layout(width: u32, height: u32, decoded_bytes: u64) -> Result<()> {
    let pixel_count = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| Error::Codec(format!("image dimensions {width}x{height} overflow")))?;
    if pixel_count > MAX_DECODED_PIXELS {
        return Err(Error::Codec(format!(
            "image {width}x{height} has {pixel_count} pixels; the limit is {MAX_DECODED_PIXELS}"
        )));
    }
    if decoded_bytes > MAX_DECODER_BYTES {
        return Err(Error::Codec(format!(
            "decoded image needs {decoded_bytes} bytes; the limit is {MAX_DECODER_BYTES}"
        )));
    }
    Ok(())
}

fn known_profile_space(profile: &[u8]) -> ColorSpace {
    [ColorSpace::Srgb, ColorSpace::DisplayP3, ColorSpace::Rec2020]
        .into_iter()
        .find(|space| {
            profile_for(*space)
                .as_deref()
                .is_some_and(|known| known == profile)
        })
        .unwrap_or(ColorSpace::Unknown)
}

/// Reads and decodes an image file.
///
/// # Errors
///
/// Returns [`Error::Io`] if the file cannot be read, or [`Error::Codec`] if its
/// contents are not a supported image.
pub fn decode_file(path: &std::path::Path) -> Result<Frame> {
    let bytes = std::fs::read(path)?;
    decode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Encoder as _;
    use crate::{EncodeOptions, ImageFormat, encode::FrameEncoder};

    fn solid_frame(w: u32, h: u32) -> Frame {
        let stride = w as usize * 4;
        Frame {
            data: (0..w * h)
                .flat_map(|i| {
                    let v = (i % 251) as u8;
                    [v, v.wrapping_add(40), v.wrapping_add(80), 255]
                })
                .collect(),
            size: PhysicalSize::new(f64::from(w), f64::from(h)),
            stride,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::new(2.0),
        }
    }

    #[test]
    fn png_round_trips_pixel_for_pixel() {
        let original = solid_frame(37, 19);
        let bytes = FrameEncoder::new()
            .encode(&original, ImageFormat::Png)
            .expect("encode");
        let decoded = decode(&bytes).expect("decode");

        assert_eq!(decoded.width(), 37);
        assert_eq!(decoded.height(), 19);
        assert_eq!(decoded.data, original.data, "PNG must be lossless");
        assert!(decoded.is_well_formed());
    }

    #[test]
    fn decoded_frames_are_tightly_packed() {
        let bytes = FrameEncoder::new()
            .encode(&solid_frame(13, 7), ImageFormat::Png)
            .expect("encode");
        let decoded = decode(&bytes).expect("decode");
        assert_eq!(decoded.stride, 13 * 4);
    }

    #[test]
    fn an_embedded_srgb_profile_is_restored() {
        let bytes = FrameEncoder::new()
            .encode(&solid_frame(4, 4), ImageFormat::Png)
            .expect("encode");
        assert_eq!(decode(&bytes).unwrap().color_space, ColorSpace::Srgb);
    }

    #[test]
    fn an_unprofiled_image_remains_unknown() {
        let encoder = FrameEncoder::with_options(EncodeOptions {
            embed_srgb_profile: false,
            ..EncodeOptions::default()
        });
        let bytes = encoder
            .encode(&solid_frame(4, 4), ImageFormat::Png)
            .expect("encode");
        assert_eq!(decode(&bytes).unwrap().color_space, ColorSpace::Unknown);
    }

    #[test]
    fn a_known_embedded_profile_is_restored() {
        let mut frame = solid_frame(4, 4);
        frame.color_space = ColorSpace::DisplayP3;
        let bytes = FrameEncoder::new()
            .encode(&frame, ImageFormat::Png)
            .expect("encode");
        assert_eq!(decode(&bytes).unwrap().color_space, ColorSpace::DisplayP3);
    }

    #[test]
    fn scale_is_identity_because_a_file_does_not_know_its_display() {
        let bytes = FrameEncoder::new()
            .encode(&solid_frame(4, 4), ImageFormat::Png)
            .expect("encode");
        // The source frame was 2×; the file cannot carry that.
        assert_eq!(decode(&bytes).unwrap().scale, ScaleFactor::IDENTITY);
    }

    #[test]
    fn garbage_is_a_codec_error_not_a_panic() {
        let err = decode(b"this is definitely not a PNG").unwrap_err();
        assert!(matches!(err, Error::Codec(_)), "got {err:?}");
    }

    #[test]
    fn empty_input_is_a_codec_error() {
        assert!(matches!(decode(&[]).unwrap_err(), Error::Codec(_)));
    }

    #[test]
    fn declared_dimensions_are_limited_before_pixel_allocation() {
        let error = validate_decoded_layout(100_000, 100_000, 1)
            .expect_err("ten billion pixels must be refused");
        assert!(error.to_string().contains("pixels"));
    }
}
