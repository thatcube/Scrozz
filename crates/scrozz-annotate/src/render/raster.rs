//! Conversion between a [`Frame`] and a `tiny-skia` pixmap.
//!
//! Two details here are load-bearing, and both are classic sources of silently
//! wrong images:
//!
//! - **Stride.** Capture APIs pad rows to an alignment boundary, so a frame's
//!   rows are frequently wider than `width × 4`. Ignoring that yields the
//!   familiar diagonally-skewed screenshot.
//! - **Premultiplication.** `tiny-skia` composites premultiplied RGBA. Feeding
//!   it straight-alpha data silhouettes every semi-transparent edge, which is
//!   exactly the halo seen around rounded window corners.

use scrozz_core::{ColorSpace, Error, Frame, PhysicalSize, PixelFormat, Result, ScaleFactor};
use tiny_skia::{Pixmap, PremultipliedColorU8};

/// Copies a frame into a premultiplied RGBA pixmap.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] if the frame is empty or its buffer is too
/// short for the geometry it declares.
pub fn to_pixmap(frame: &Frame) -> Result<Pixmap> {
    let (width, height) = (frame.width(), frame.height());
    if width == 0 || height == 0 {
        return Err(Error::InvalidRequest(
            "cannot render a zero-sized frame".to_owned(),
        ));
    }
    if !frame.is_well_formed() {
        return Err(Error::InvalidRequest(format!(
            "frame buffer is too short: {} bytes for {width}x{height} at stride {}",
            frame.data.len(),
            frame.stride
        )));
    }
    let mut pixmap = Pixmap::new(width, height).ok_or_else(|| {
        Error::InvalidRequest(format!("frame size {width}x{height} is not renderable"))
    })?;

    let bpp = frame.format.bytes_per_pixel();
    let out = pixmap.pixels_mut();
    for y in 0..height as usize {
        let row = &frame.data[y * frame.stride..];
        for x in 0..width as usize {
            let px = &row[x * bpp..x * bpp + 4];
            let (r, g, b, a) = match frame.format {
                PixelFormat::Rgba8 | PixelFormat::RgbaPremultiplied8 => {
                    (px[0], px[1], px[2], px[3])
                }
                PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8 => {
                    (px[2], px[1], px[0], px[3])
                }
            };
            out[y * width as usize + x] = if frame.format.is_premultiplied() {
                // Clamp rather than trust: a channel above alpha is invalid
                // premultiplied data, and tiny-skia rejects it outright.
                PremultipliedColorU8::from_rgba(r.min(a), g.min(a), b.min(a), a)
                    .unwrap_or_else(transparent)
            } else {
                premultiply(r, g, b, a)
            };
        }
    }
    Ok(pixmap)
}

/// Wraps a rendered pixmap back up as a frame.
///
/// The result is [`PixelFormat::RgbaPremultiplied8`]: that is `tiny-skia`'s
/// native layout, and un-premultiplying would throw away precision in exactly
/// the low-alpha pixels along a window's rounded edge that decision D9 cares
/// most about.
#[must_use]
pub fn from_pixmap(pixmap: Pixmap, color_space: ColorSpace, scale: ScaleFactor) -> Frame {
    let width = pixmap.width();
    let height = pixmap.height();
    Frame {
        data: pixmap.take(),
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride: width as usize * 4,
        format: PixelFormat::RgbaPremultiplied8,
        color_space,
        scale,
    }
}

fn premultiply(r: u8, g: u8, b: u8, a: u8) -> PremultipliedColorU8 {
    if a == 255 {
        return PremultipliedColorU8::from_rgba(r, g, b, a).unwrap_or_else(transparent);
    }
    if a == 0 {
        return transparent();
    }
    let scale = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
    PremultipliedColorU8::from_rgba(scale(r), scale(g), scale(b), a).unwrap_or_else(transparent)
}

fn transparent() -> PremultipliedColorU8 {
    // Unreachable in practice: every construction above is valid premultiplied
    // data. Kept as a total fallback so no rendering path can panic on pixels.
    PremultipliedColorU8::from_rgba(0, 0, 0, 0).expect("transparent is valid premultiplied")
}
