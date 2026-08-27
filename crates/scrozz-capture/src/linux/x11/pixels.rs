//! Turning an X11 `GetImage` reply into a [`scrozz_core::Frame`].
//!
//! X11 does not describe its images as "RGBA" or "BGRA". It describes them as a
//! depth, a bits-per-pixel, a scanline padding, a server byte order, and three
//! channel bitmasks on the visual — and expects the client to work the layout
//! out. Every one of those five inputs is a place to be wrong, and being wrong
//! produces the same family of famous screenshot bugs: colours swapped to look
//! like an old photograph, a fully transparent PNG, or an image sheared
//! diagonally because the row padding was ignored.
//!
//! All of it is arithmetic on numbers, so all of it is tested on any platform.

use scrozz_core::{PixelFormat, ShadowSupport, WindowPickingCapability};

/// Interactive window fidelity guaranteed by the X11 `GetImage` path.
///
/// Compositor shadows live in the root image, outside the window drawable, and
/// ordinary frame visuals are depth 24 with an undefined fourth byte that this
/// module correctly forces opaque. Some ARGB client visuals happen to preserve
/// alpha, but the backend cannot promise that for a decorated window, so the
/// backend-level capability is deliberately conservative.
#[must_use]
pub fn window_picking_capability() -> WindowPickingCapability {
    WindowPickingCapability::in_process(
        ShadowSupport::AlwaysExcluded {
            why: "X11 compositor shadows are separate root-window pixels; including one would \
                  require guessing its bounds and compositing it back onto the window"
                .to_owned(),
        },
        false,
    )
}

/// Bytes per row for a ZPixmap image.
///
/// X11 pads every scanline out to `scanline_pad` **bits** — usually 32, but the
/// server advertises it per depth in `Setup::pixmap_formats` and is entitled to
/// say 8 or 16. At 32 bits per pixel the padding is a no-op, which is exactly
/// why ignoring it survives casual testing and then shears the image on the one
/// machine whose server reports something else.
///
/// Returns `None` if the arithmetic would overflow, which for plausible screen
/// sizes it never does, but a `checked_` chain here is cheaper than an
/// allocation panic later.
#[must_use]
pub fn scanline_stride(width: u32, bits_per_pixel: u8, scanline_pad: u8) -> Option<usize> {
    let pad = usize::from(scanline_pad);
    if pad == 0 || pad % 8 != 0 {
        return None;
    }
    let bits = (width as usize).checked_mul(usize::from(bits_per_pixel))?;
    let units = bits.div_ceil(pad);
    units.checked_mul(pad / 8)
}

/// Where each colour channel sits inside one pixel, in memory order.
///
/// Derived rather than assumed. `Bgra8` is right on essentially every modern
/// little-endian X server, and hard-coding it is right up until it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteLayout {
    /// Byte offset of the red channel within a pixel.
    pub red: usize,
    /// Byte offset of the green channel.
    pub green: usize,
    /// Byte offset of the blue channel.
    pub blue: usize,
    /// Byte offset of a meaningful alpha channel, if the visual has one.
    ///
    /// `None` at depth 24, where the fourth byte exists but is undefined
    /// padding. Servers commonly leave it zero, so copying it through yields an
    /// image that is entirely, invisibly transparent.
    pub alpha: Option<usize>,
    /// Bytes occupied by one pixel.
    pub bytes_per_pixel: usize,
}

/// Works out the channel layout from a visual and the server's byte order.
///
/// Only 32-bits-per-pixel TrueColor/DirectColor visuals are handled, because
/// that is what every display an X server drives in 2020s hardware actually
/// uses. Anything else returns `None`, which the backend surfaces as a truthful
/// [`scrozz_core::Error::Unsupported`] rather than a wrongly-coloured capture.
///
/// `lsb_first` is `Setup::image_byte_order`, not the host's endianness: the two
/// normally agree but the protocol permits them to differ.
#[must_use]
pub fn byte_layout(
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    depth: u8,
    bits_per_pixel: u8,
    lsb_first: bool,
) -> Option<ByteLayout> {
    if bits_per_pixel != 32 {
        return None;
    }
    let bytes_per_pixel = 4usize;

    let offset = |mask: u32| -> Option<usize> {
        // A channel must be exactly one byte wide and byte-aligned for a plain
        // byte copy to be correct.
        if mask == 0 {
            return None;
        }
        let shift = mask.trailing_zeros();
        if !shift.is_multiple_of(8) || mask >> shift != 0xff {
            return None;
        }
        let index = (shift / 8) as usize;
        Some(if lsb_first {
            index
        } else {
            bytes_per_pixel - 1 - index
        })
    };

    let red = offset(red_mask)?;
    let green = offset(green_mask)?;
    let blue = offset(blue_mask)?;
    if red == green || green == blue || red == blue {
        return None;
    }

    // The one remaining byte is alpha — but only at depth 32, where the visual
    // actually defines it.
    let alpha = (depth == 32)
        .then(|| (0..bytes_per_pixel).find(|i| *i != red && *i != green && *i != blue))
        .flatten();

    Some(ByteLayout {
        red,
        green,
        blue,
        alpha,
        bytes_per_pixel,
    })
}

/// The [`PixelFormat`] a layout already is, if it is one of ours exactly.
///
/// When this returns `Some`, the channel bytes need no rearranging and the only
/// work is the alpha fill, which matters because a 4K capture is 33 MB and a
/// gratuitous swizzle is a visible pause.
#[must_use]
pub fn direct_format(layout: &ByteLayout) -> Option<PixelFormat> {
    match (layout.red, layout.green, layout.blue) {
        (0, 1, 2) => Some(PixelFormat::Rgba8),
        (2, 1, 0) => Some(PixelFormat::Bgra8),
        _ => None,
    }
}

/// Repacks a `GetImage` body into a tightly-packed frame buffer.
///
/// Does three things in one pass, because each of them alone would otherwise
/// cost a full traversal of a multi-megabyte buffer:
///
/// 1. drops the scanline padding, so the result's stride is exactly
///    `width * 4` and downstream code cannot get it wrong;
/// 2. reorders channels when the visual is not already one of ours;
/// 3. forces alpha to opaque at depth 24, where X supplies undefined padding.
///
/// Returns the buffer and the format it is in. Short input is truncated to the
/// rows that are actually present rather than panicking: a `GetImage` racing a
/// window resize genuinely can come back short, and losing the bottom rows of a
/// screenshot beats losing the process.
#[must_use]
pub fn repack(
    src: &[u8],
    src_stride: usize,
    width: u32,
    height: u32,
    layout: &ByteLayout,
) -> (Vec<u8>, PixelFormat) {
    let width = width as usize;
    let height = height as usize;
    let dst_stride = width * 4;
    let mut out = vec![0u8; dst_stride * height];

    let format = direct_format(layout).unwrap_or(PixelFormat::Rgba8);
    // When the layout is already ours, "red" means byte 0 of our own format.
    let (r, g, b) = match format {
        PixelFormat::Bgra8 => (2usize, 1usize, 0usize),
        _ => (0usize, 1usize, 2usize),
    };

    for row in 0..height {
        let src_row = row * src_stride;
        let dst_row = row * dst_stride;
        let Some(src_line) = src.get(src_row..src_row + width * layout.bytes_per_pixel) else {
            break;
        };
        for column in 0..width {
            let s = column * layout.bytes_per_pixel;
            let d = dst_row + column * 4;
            out[d + r] = src_line[s + layout.red];
            out[d + g] = src_line[s + layout.green];
            out[d + b] = src_line[s + layout.blue];
            out[d + 3] = layout.alpha.map_or(0xff, |a| src_line[s + a]);
        }
    }

    (out, format)
}

/// The plane mask for `GetImage`: every plane the visual defines.
///
/// `!0` is the conventional value and is what every X client passes.
#[must_use]
pub const fn all_planes() -> u32 {
    u32::MAX
}

#[cfg(test)]
mod tests {
    use super::window_picking_capability;
    use scrozz_core::{ShadowSupport, WindowSelection};

    #[test]
    fn x11_picker_reports_only_output_the_get_image_path_guarantees() {
        let capability = window_picking_capability();
        assert_eq!(capability.selection, WindowSelection::InProcess);
        assert!(matches!(
            capability.shadow,
            ShadowSupport::AlwaysExcluded { .. }
        ));
        assert!(!capability.native_alpha);
    }
}
