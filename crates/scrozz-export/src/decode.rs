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

use image::ImageReader;
use scrozz_core::{
    ColorSpace, Error, Frame, PhysicalSize, PixelFormat, Result, ScaleFactor,
};

/// Decodes encoded image bytes into a frame.
///
/// The result is always straight-alpha [`PixelFormat::Rgba8`] with a tightly
/// packed stride, because that is what the `image` crate produces and inventing
/// padding here would only create work for every consumer.
///
/// # Colour space
///
/// Reported as [`ColorSpace::Unknown`] rather than assumed to be sRGB. The bytes
/// may carry an ICC profile this decoder does not interpret, and claiming sRGB
/// for a Display P3 image is precisely the mistake that makes wide-gamut
/// screenshots look wrong — washed out in some viewers, oversaturated in others.
/// `Unknown` lets an encoder decline to embed a profile instead of embedding a
/// false one.
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
    let reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::Codec(format!("could not read image header: {e}")))?;

    let image = reader
        .decode()
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
        color_space: ColorSpace::Unknown,
        scale: ScaleFactor::IDENTITY,
    })
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
    use crate::{encode::FrameEncoder, ImageFormat};
    use crate::Encoder as _;

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
    fn colour_space_is_unknown_rather_than_assumed_srgb() {
        // Claiming sRGB for what might be Display P3 is how wide-gamut captures
        // come out wrong. Not knowing is the honest answer.
        let bytes = FrameEncoder::new()
            .encode(&solid_frame(4, 4), ImageFormat::Png)
            .expect("encode");
        assert_eq!(decode(&bytes).unwrap().color_space, ColorSpace::Unknown);
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
}
