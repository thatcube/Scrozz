//! Native macOS image clipboard delivery.
//!
//! `arboard` advertises TIFF on macOS. Scrozz also advertises the original PNG
//! bytes because browsers, design tools, and chat composers frequently prefer
//! `public.png`. Both representations are committed in one pasteboard update.

use std::io::Cursor;

use image::ImageDecoder;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeTIFF};
use objc2_foundation::NSData;
use scrozz_core::{Error, Result};

/// Writes one PNG as both `public.png` and `public.tiff`.
///
/// # Errors
///
/// Returns an error when the PNG is empty, cannot be transcoded, or AppKit
/// refuses either representation.
pub fn write_png(png: &[u8]) -> Result<()> {
    let (png_data, tiff_data) = representations(png)?;
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();

    // SAFETY: AppKit's pasteboard type globals are immortal NSString constants.
    let (png_type, tiff_type) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
    if !pasteboard.setData_forType(Some(&png_data), png_type) {
        return Err(Error::Platform(
            "macOS pasteboard refused the public.png representation".to_owned(),
        ));
    }
    if !pasteboard.setData_forType(Some(&tiff_data), tiff_type) {
        return Err(Error::Platform(
            "macOS pasteboard refused the public.tiff representation".to_owned(),
        ));
    }
    Ok(())
}

fn representations(
    png: &[u8],
) -> Result<(objc2::rc::Retained<NSData>, objc2::rc::Retained<NSData>)> {
    if png.is_empty() {
        return Err(Error::InvalidRequest(
            "cannot copy an empty PNG to the clipboard".to_owned(),
        ));
    }
    let png_data = NSData::with_bytes(png);
    let tiff_data = tiff_representation(png)?;
    Ok((png_data, tiff_data))
}

pub(super) fn tiff_representation(png: &[u8]) -> Result<objc2::rc::Retained<NSData>> {
    let mut decoder = image::codecs::png::PngDecoder::new(Cursor::new(png))
        .map_err(|error| Error::Codec(format!("could not inspect clipboard PNG bytes: {error}")))?;
    let profile = decoder
        .icc_profile()
        .map_err(|error| Error::Codec(format!("could not read clipboard PNG profile: {error}")))?;
    let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|error| Error::Codec(format!("could not decode clipboard PNG bytes: {error}")))?;
    let rgba = decoded.to_rgba8();
    let image = scrozz_export::RgbaImage {
        width: rgba.width(),
        height: rgba.height(),
        data: rgba.into_raw(),
    };
    let encoded = scrozz_export::clipboard::tiff(&image, profile.as_deref());
    Ok(NSData::with_bytes(&encoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    // One opaque 1x1 PNG. The test does not touch the system pasteboard.
    const PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    #[test]
    fn one_png_produces_both_clipboard_representations() {
        let (png, tiff) = representations(PNG).expect("the PNG should produce both flavours");
        assert_eq!(png.to_vec(), PNG);
        assert!(matches!(
            tiff.to_vec().as_slice(),
            [b'I', b'I', 42, 0, ..] | [b'M', b'M', 0, 42, ..]
        ));
    }

    #[test]
    fn empty_clipboard_images_are_refused() {
        assert!(representations(&[]).is_err());
    }

    #[test]
    fn tiff_keeps_the_png_colour_profile() {
        use scrozz_core::{ColorSpace, Frame, PhysicalSize, PixelFormat, ScaleFactor};
        use scrozz_export::{Encoder, FrameEncoder, ImageFormat, profile_for};

        let frame = Frame {
            data: vec![255, 0, 0, 255],
            size: PhysicalSize::new(1.0, 1.0),
            stride: 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::DisplayP3,
            scale: ScaleFactor::IDENTITY,
        };
        let png = FrameEncoder::new()
            .encode(&frame, ImageFormat::Png)
            .expect("profiled PNG");
        let profile = profile_for(ColorSpace::DisplayP3).expect("Display P3 profile");
        let tiff = tiff_representation(&png).expect("profiled TIFF").to_vec();

        assert!(
            tiff.windows(profile.len()).any(|bytes| bytes == profile),
            "the TIFF dropped its embedded Display P3 profile"
        );
    }
}
