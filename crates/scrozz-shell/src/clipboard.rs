//! Native clipboard delivery where the portable backend cannot preserve the
//! complete image offer.

use scrozz_core::{Frame, Result};
use scrozz_export::{ClipboardReport, ImageFormat};

/// Writes one finalized screenshot to the system clipboard.
///
/// `png` is the exact immutable artifact retained by the capture pipeline.
/// macOS declares it directly before an ICC-bearing TIFF fallback; other
/// platforms use the existing native image backend over the same frame.
///
/// # Errors
///
/// Returns a codec error when `png` is not a PNG, or a platform error when the
/// clipboard rejects any required representation.
pub fn write_capture(frame: &Frame, png: &[u8]) -> Result<ClipboardReport> {
    if ImageFormat::sniff(png) != Some(ImageFormat::Png) {
        return Err(scrozz_core::Error::Codec(
            "clipboard delivery requires the finalized PNG artifact".to_owned(),
        ));
    }
    write_platform(frame, png)
}

#[cfg(target_os = "macos")]
fn write_platform(frame: &Frame, png: &[u8]) -> Result<ClipboardReport> {
    use std::sync::{Mutex, OnceLock};

    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{
        NSPasteboard, NSPasteboardItem, NSPasteboardTypePNG, NSPasteboardTypeTIFF,
        NSPasteboardWriting,
    };
    use objc2_foundation::{NSArray, NSData};
    use scrozz_export::{ClipboardPlatform, FlavourKind, clipboard::encode_flavour};

    let tiff = encode_flavour(frame, ClipboardPlatform::MacOs, FlavourKind::Tiff)?;

    // SAFETY: AppKit's pasteboard-type globals are immortal NSString constants.
    let (png_type, tiff_type) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
    let item = NSPasteboardItem::new();
    if !item.setData_forType(&NSData::with_bytes(png), png_type) {
        return Err(scrozz_core::Error::Platform(
            "the macOS pasteboard item refused the finalized PNG".to_owned(),
        ));
    }
    if !item.setData_forType(&NSData::with_bytes(&tiff.bytes), tiff_type) {
        return Err(scrozz_core::Error::Platform(
            "the macOS pasteboard item accepted PNG but refused the TIFF fallback".to_owned(),
        ));
    }
    let writer = ProtocolObject::<dyn NSPasteboardWriting>::from_retained(item);
    let objects = NSArray::from_retained_slice(&[writer]);

    static PASTEBOARD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = PASTEBOARD_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    if !pasteboard.writeObjects(&objects) {
        return Err(scrozz_core::Error::Platform(
            "the macOS pasteboard refused the complete image item".to_owned(),
        ));
    }

    Ok(ClipboardReport {
        platform: ClipboardPlatform::MacOs,
        delivered: &["public.png", "NSPasteboardTypeTIFF"],
        missing: &[],
    })
}

#[cfg(not(target_os = "macos"))]
fn write_platform(frame: &Frame, _png: &[u8]) -> Result<ClipboardReport> {
    scrozz_export::SystemClipboard::new().write_image_reporting(frame)
}

#[cfg(test)]
mod tests {
    use scrozz_core::{ColorSpace, PhysicalSize, PixelFormat, ScaleFactor};
    use scrozz_export::{Encoder, FrameEncoder};

    use super::*;

    fn frame() -> Frame {
        Frame {
            data: vec![10, 20, 30, 255],
            size: PhysicalSize::new(1.0, 1.0),
            stride: 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::DisplayP3,
            scale: ScaleFactor::IDENTITY,
        }
    }

    #[test]
    fn non_png_artifacts_are_refused_before_the_clipboard_is_touched() {
        let error = write_capture(&frame(), b"not a png").unwrap_err();
        assert!(error.to_string().contains("finalized PNG"), "{error}");
    }

    #[test]
    fn the_finalized_png_is_profiled_before_native_delivery() {
        use image::ImageDecoder as _;
        use std::io::Cursor;

        let png = FrameEncoder::new()
            .encode(&frame(), ImageFormat::Png)
            .expect("encode");
        assert_eq!(ImageFormat::sniff(&png), Some(ImageFormat::Png));
        let embedded = image::codecs::png::PngDecoder::new(Cursor::new(png.clone()))
            .expect("decode exact clipboard PNG")
            .icc_profile()
            .expect("read profile")
            .expect("profile present");
        assert_eq!(
            embedded,
            scrozz_export::profile_for(ColorSpace::DisplayP3).expect("Display P3 profile"),
            "the exact finalized clipboard bytes must carry the current color profile"
        );
        assert!(
            png.windows(4).any(|window| window == b"iCCP"),
            "the exact artifact offered to the pasteboard must carry an ICC chunk"
        );
    }
}
