//! Turning a `CGImage` into a [`scrozz_core::Frame`].
//!
//! Four details here are the difference between a correct screenshot and a
//! subtly broken one, and each is called out where it is handled:
//!
//! 1. **Stride.** CoreGraphics pads rows to a convenient alignment, so a
//!    1512-pixel-wide image is very often not 6048 bytes per row. The padding
//!    is read from the image, never assumed away.
//! 2. **Byte order.** ScreenCaptureKit documents its SDR output as BGRA, which
//!    CoreGraphics expresses as "alpha first, 32-bit little-endian".
//! 3. **Premultiplication.** That output is premultiplied. Reporting it as
//!    straight alpha would make every semi-transparent edge composite wrong,
//!    so it is named [`PixelFormat::BgraPremultiplied8`] and left in place
//!    rather than converted at the capture boundary.
//! 4. **Colour space.** Modern Macs are Display P3. The frame reports whatever
//!    the image says it is, and `Unknown` when that is not a space the core
//!    model can name — never a hopeful sRGB.

use objc2_core_foundation::CFEqual;
use objc2_core_graphics::{
    CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
    kCGColorSpaceDisplayP3, kCGColorSpaceITUR_2020, kCGColorSpaceSRGB,
};
use scrozz_core::{ColorSpace, Error, Frame, PhysicalSize, PixelFormat, Result, ScaleFactor};

/// Converts a captured image into a frame, at its true pixel dimensions.
///
/// `scale` is the display's backing-scale factor, recorded so consumers can
/// recover the logical size. The pixel dimensions come from the image itself:
/// a window capture including its shadow is larger than the window's frame
/// implies, and the image is the authority on how much larger.
pub(crate) fn to_frame(image: &CGImage, scale: ScaleFactor) -> Result<Frame> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    if width == 0 || height == 0 {
        return Err(Error::Platform(
            "ScreenCaptureKit returned an image with no pixels".to_owned(),
        ));
    }

    let bits_per_pixel = CGImage::bits_per_pixel(Some(image));
    if bits_per_pixel != 32 {
        return Err(Error::Unsupported {
            what: "this capture's pixel layout".to_owned(),
            why: format!("expected 32 bits per pixel, the image has {bits_per_pixel}"),
        });
    }

    // Requirement: read the real row stride. Computing `width * 4` here is the
    // classic way to produce a picture that shears further with every row.
    let stride = CGImage::bytes_per_row(Some(image));
    if stride < width * 4 {
        return Err(Error::Platform(format!(
            "image claims {stride} bytes per row, too few for {width} pixels"
        )));
    }

    let mut data = pixel_bytes(image, stride, height)?;

    let alpha = CGImage::alpha_info(Some(image));
    let little_endian = matches!(
        CGImage::byte_order_info(Some(image)),
        CGImageByteOrderInfo::Order32Little
    );
    let format = normalise(&mut data, stride, width, height, alpha, little_endian)?;

    let frame = Frame {
        data,
        size: PhysicalSize::new(width as f64, height as f64),
        stride,
        format,
        color_space: colour_space(image),
        scale,
    };

    // Asserted rather than assumed: a short buffer is otherwise found much
    // later, as a panic inside an encoder with no useful context.
    debug_assert!(frame.is_well_formed(), "built a malformed frame");

    if frame.is_well_formed() {
        Ok(frame)
    } else {
        Err(Error::Platform(format!(
            "captured image is inconsistent: {width}x{height} at stride {stride} \
             needs {} bytes, got {}",
            stride * height,
            frame.data.len(),
        )))
    }
}

/// Copies the image's backing bytes out of CoreFoundation's ownership.
fn pixel_bytes(image: &CGImage, stride: usize, height: usize) -> Result<Vec<u8>> {
    let provider = CGImage::data_provider(Some(image))
        .ok_or_else(|| Error::Platform("captured image has no data provider".to_owned()))?;
    let data = CGDataProvider::data(Some(&provider))
        .ok_or_else(|| Error::Platform("captured image's pixel data is unreadable".to_owned()))?;

    let bytes = data.to_vec();
    let needed = stride
        .checked_mul(height)
        .ok_or_else(|| Error::Platform("captured image's size overflows".to_owned()))?;

    if bytes.len() < needed {
        return Err(Error::Platform(format!(
            "captured image is truncated: {} bytes for {height} rows of {stride}",
            bytes.len()
        )));
    }

    Ok(bytes)
}

/// Where the four channels actually sit in memory.
///
/// CoreGraphics splits this across two enums, and the split is a trap.
/// `alphaInfo` says where alpha sits *in the 32-bit word*; `byteOrder` says
/// how that word is laid out *in memory*. Combining them gives four distinct
/// byte orders, and reading either one alone gives the wrong answer half the
/// time — `PremultipliedLast` is RGBA big-endian but ABGR little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryOrder {
    /// B, G, R, A — ScreenCaptureKit's usual output.
    Bgra,
    /// R, G, B, A.
    Rgba,
    /// A, R, G, B.
    Argb,
    /// A, B, G, R.
    Abgr,
}

impl MemoryOrder {
    fn of(alpha_first: bool, little_endian: bool) -> Self {
        match (alpha_first, little_endian) {
            (true, true) => Self::Bgra,
            (true, false) => Self::Argb,
            (false, true) => Self::Abgr,
            (false, false) => Self::Rgba,
        }
    }

    /// Whether the alpha byte is the first of each pixel rather than the last.
    const fn alpha_leads(self) -> bool {
        matches!(self, Self::Argb | Self::Abgr)
    }
}

/// Reports the pixel layout, rewriting the buffer in place when it must.
///
/// BGRA — ScreenCaptureKit's usual output — is nameable in both alpha
/// flavours, so the common case touches no colour bytes at all. That is worth
/// protecting: a pass over a 6K display's pixels on every frame is exactly the
/// cost [`PixelFormat::BgraPremultiplied8`] exists to avoid, and it would be
/// paid during recording as well as for stills.
///
/// ARGB and ABGR have no representation and are permuted into RGBA. That is a
/// genuine conversion rather than a relabelling, so it is done honestly and
/// only for the layouts that need it.
fn normalise(
    data: &mut [u8],
    stride: usize,
    width: usize,
    height: usize,
    alpha: CGImageAlphaInfo,
    little_endian: bool,
) -> Result<PixelFormat> {
    if matches!(alpha, CGImageAlphaInfo::Only) {
        return Err(Error::Unsupported {
            what: "this capture's pixel layout".to_owned(),
            why: "the image is an alpha mask with no colour channels".to_owned(),
        });
    }

    let alpha_first = matches!(
        alpha,
        CGImageAlphaInfo::PremultipliedFirst
            | CGImageAlphaInfo::First
            | CGImageAlphaInfo::NoneSkipFirst
    );
    let order = MemoryOrder::of(alpha_first, little_endian);

    let premultiplied = matches!(
        alpha,
        CGImageAlphaInfo::PremultipliedFirst | CGImageAlphaInfo::PremultipliedLast
    );
    let opaque = matches!(
        alpha,
        CGImageAlphaInfo::None | CGImageAlphaInfo::NoneSkipFirst | CGImageAlphaInfo::NoneSkipLast
    );

    // A skipped alpha channel holds undefined bytes. Anything downstream that
    // reads it — a PNG encoder, a texture upload — would show that garbage, so
    // make the pixels honestly opaque.
    if opaque {
        let offset = usize::from(!order.alpha_leads()) * 3;
        for_each_pixel(data, stride, width, height, |pixel| pixel[offset] = 0xFF);
    }

    // BGRA is describable exactly, either way round, without moving a byte.
    if order == MemoryOrder::Bgra {
        return Ok(if premultiplied {
            PixelFormat::BgraPremultiplied8
        } else {
            PixelFormat::Bgra8
        });
    }

    match order {
        MemoryOrder::Rgba | MemoryOrder::Bgra => {}
        MemoryOrder::Argb => {
            // A R G B -> R G B A
            for_each_pixel(data, stride, width, height, |pixel| pixel.rotate_left(1));
        }
        MemoryOrder::Abgr => {
            // A B G R -> R G B A
            for_each_pixel(data, stride, width, height, |pixel| pixel.reverse());
        }
    }

    Ok(if premultiplied {
        PixelFormat::RgbaPremultiplied8
    } else {
        PixelFormat::Rgba8
    })
}

/// Applies `f` to every pixel, row by row.
///
/// Iterating per row rather than over the whole buffer is what keeps the
/// padding bytes at the end of each row untouched — treating the buffer as one
/// contiguous run of pixels would walk into the padding and corrupt the image.
fn for_each_pixel(
    data: &mut [u8],
    stride: usize,
    width: usize,
    height: usize,
    mut f: impl FnMut(&mut [u8; 4]),
) {
    for row in 0..height {
        let start = row * stride;
        for pixel in data[start..start + width * 4].as_chunks_mut::<4>().0 {
            f(pixel);
        }
    }
}

/// Reads the image's colour space, refusing to guess.
///
/// Two probes, in order of confidence.
///
/// First identity: a space created from one of CoreGraphics' own constants is
/// `CFEqual` to that constant, whether or not anyone asked for its name. This
/// catches spaces that were built without a name and would otherwise fall
/// through.
///
/// Then the name, for spaces that carry one but are not the same object.
///
/// Everything else is [`ColorSpace::Unknown`], and that is a real answer rather
/// than a shrug. A Mac laptop panel usually reports a *per-unit calibrated ICC
/// profile* — measured at the factory for that individual display — which is
/// close to Display P3 but is not Display P3, has no name, and is `CFEqual` to
/// nothing. Tagging those pixels `DisplayP3` would embed a profile that does
/// not describe them; tagging them `Srgb` would desaturate them. `Unknown` lets
/// the encoder carry the original profile through instead of substituting one.
fn colour_space(image: &CGImage) -> ColorSpace {
    let Some(space) = CGImage::color_space(Some(image)) else {
        return ColorSpace::Unknown;
    };

    // SAFETY: reading immutable global colour-space name constants.
    let named = unsafe {
        [
            (kCGColorSpaceDisplayP3, ColorSpace::DisplayP3),
            (kCGColorSpaceITUR_2020, ColorSpace::Rec2020),
            (kCGColorSpaceSRGB, ColorSpace::Srgb),
        ]
    };

    for (constant, answer) in named {
        if let Some(reference) = CGColorSpace::with_name(Some(constant))
            && is_same_space(&space, &reference)
        {
            return answer;
        }
    }

    let Some(name) = CGColorSpace::name(Some(&space)) else {
        return ColorSpace::Unknown;
    };
    let name = name.to_string();

    if name.contains("Display P3") || name.contains("DisplayP3") {
        ColorSpace::DisplayP3
    } else if name.contains("2020") {
        ColorSpace::Rec2020
    } else if name.contains("sRGB") && !name.contains("Extended") {
        // Extended sRGB carries out-of-range values that plain sRGB cannot
        // represent, so it is not the same space and must not claim to be.
        ColorSpace::Srgb
    } else {
        ColorSpace::Unknown
    }
}

fn is_same_space(a: &CGColorSpace, b: &CGColorSpace) -> bool {
    CFEqual(Some(a.as_ref()), Some(b.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four pixels wide with eight bytes of row padding, so any code that
    /// assumes `width * 4` walks straight into the padding.
    fn padded_bgra(premultiplied: bool) -> (Vec<u8>, usize, usize, usize) {
        let (width, height, stride) = (4usize, 2usize, 4 * 4 + 8);
        let mut data = vec![0u8; stride * height];
        for row in 0..height {
            for pixel in 0..width {
                let at = row * stride + pixel * 4;
                data[at] = 0x10 + pixel as u8; // B
                data[at + 1] = 0x20; // G
                data[at + 2] = 0x30 + pixel as u8; // R
                data[at + 3] = if premultiplied { 0x80 } else { 0xFF }; // A
            }
            // Padding, deliberately not zero: it must survive untouched.
            for byte in 0..8 {
                data[row * stride + width * 4 + byte] = 0xEE;
            }
        }
        (data, stride, width, height)
    }

    /// The common ScreenCaptureKit case, and the one that must cost nothing:
    /// premultiplied BGRA is nameable outright, so no colour byte moves.
    #[test]
    fn premultiplied_bgra_is_named_rather_than_converted() {
        let (mut data, stride, width, height) = padded_bgra(true);
        let before = data.clone();
        let format = normalise(
            &mut data,
            stride,
            width,
            height,
            CGImageAlphaInfo::PremultipliedFirst,
            true,
        )
        .expect("supported layout");

        assert_eq!(format, PixelFormat::BgraPremultiplied8);
        assert_eq!(data, before, "BGRA is describable, so no bytes should move");
    }

    /// ARGB genuinely has to be permuted, so it is the layout that proves the
    /// permutation walks rows rather than the whole buffer.
    #[test]
    fn row_padding_survives_a_permutation() {
        let (mut data, stride, width, height) = padded_bgra(true);
        normalise(
            &mut data,
            stride,
            width,
            height,
            CGImageAlphaInfo::PremultipliedFirst,
            false,
        )
        .expect("supported layout");

        for row in 0..height {
            let padding = &data[row * stride + width * 4..(row + 1) * stride];
            assert_eq!(padding, &[0xEE; 8], "padding in row {row} was rewritten");
        }
    }

    #[test]
    fn straight_alpha_bgra_is_left_alone() {
        let (mut data, stride, width, height) = padded_bgra(false);
        let before = data.clone();

        let format = normalise(
            &mut data,
            stride,
            width,
            height,
            CGImageAlphaInfo::First,
            true,
        )
        .expect("supported layout");

        assert_eq!(format, PixelFormat::Bgra8);
        assert_eq!(data, before, "no swizzle needed, so no bytes should move");
    }

    /// The bug this replaced: `alpha_first == little_endian` is true for both
    /// (first, little) and (last, big), so BGRA and RGBA were treated alike and
    /// every big-endian RGBA capture came back with red and blue swapped.
    #[test]
    fn big_endian_alpha_last_is_already_rgba_and_is_left_alone() {
        let (mut data, stride, width, height) = padded_bgra(true);
        let before = data.clone();
        let format = normalise(
            &mut data,
            stride,
            width,
            height,
            CGImageAlphaInfo::PremultipliedLast,
            false,
        )
        .expect("supported layout");

        assert_eq!(format, PixelFormat::RgbaPremultiplied8);
        assert_eq!(data, before, "RGBA needs no permutation");
    }

    #[test]
    fn big_endian_alpha_first_is_argb_and_rotates_into_rgba() {
        let (mut data, stride, width, height) = padded_bgra(true);
        let format = normalise(
            &mut data,
            stride,
            width,
            height,
            CGImageAlphaInfo::PremultipliedFirst,
            false,
        )
        .expect("supported layout");

        // Bytes were A=0x10 R=0x20 G=0x30 B=0x80 read as ARGB.
        assert_eq!(format, PixelFormat::RgbaPremultiplied8);
        assert_eq!(&data[0..4], &[0x20, 0x30, 0x80, 0x10]);
    }

    #[test]
    fn little_endian_alpha_last_is_abgr_and_reverses_into_rgba() {
        let (mut data, stride, width, height) = padded_bgra(true);
        let format = normalise(
            &mut data,
            stride,
            width,
            height,
            CGImageAlphaInfo::PremultipliedLast,
            true,
        )
        .expect("supported layout");

        // Bytes were A=0x10 B=0x20 G=0x30 R=0x80 read as ABGR.
        assert_eq!(format, PixelFormat::RgbaPremultiplied8);
        assert_eq!(&data[0..4], &[0x80, 0x30, 0x20, 0x10]);
    }

    #[test]
    fn a_skipped_alpha_channel_is_forced_opaque() {
        let (mut data, stride, width, height) = padded_bgra(true);
        normalise(
            &mut data,
            stride,
            width,
            height,
            CGImageAlphaInfo::NoneSkipFirst,
            true,
        )
        .expect("supported layout");

        // NoneSkipFirst plus little-endian is BGRA, so the skipped byte sits
        // last in memory, and BGRA is reported as-is.
        for row in 0..height {
            for pixel in 0..width {
                assert_eq!(data[row * stride + pixel * 4 + 3], 0xFF);
            }
        }
    }

    #[test]
    fn an_alpha_only_mask_is_refused_rather_than_misread() {
        let (mut data, stride, width, height) = padded_bgra(true);
        let error = normalise(
            &mut data,
            stride,
            width,
            height,
            CGImageAlphaInfo::Only,
            true,
        )
        .expect_err("alpha masks are not captures");
        assert!(matches!(error, Error::Unsupported { .. }));
    }

    /// The exact bug the stride rule exists to prevent: a frame whose buffer is
    /// sized as if rows were unpadded is short, and the core model says so.
    #[test]
    fn a_buffer_sized_without_the_padding_is_not_well_formed() {
        let (width, height, stride) = (4usize, 2usize, 24usize);
        let honest = Frame {
            data: vec![0; stride * height],
            size: PhysicalSize::new(width as f64, height as f64),
            stride,
            format: PixelFormat::RgbaPremultiplied8,
            color_space: ColorSpace::DisplayP3,
            scale: ScaleFactor::new(2.0),
        };
        assert!(honest.is_well_formed());

        let short = Frame {
            data: vec![0; width * 4 * height],
            ..honest
        };
        assert!(
            !short.is_well_formed(),
            "a width*4-sized buffer under a padded stride must be caught"
        );
    }
}
