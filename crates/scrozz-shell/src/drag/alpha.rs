//! Straight-alpha BGRA, for the Windows drag image.
//!
//! # Why this is its own module
//!
//! `IDragSourceHelper::InitializeFromBitmap` is documented to multiply the
//! colour channels by alpha *itself*:
//!
//! > Because `InitializeFromBitmap` always performs the RGB multiplication step
//! > in calculating the alpha value, you should always pass a bitmap without
//! > premultiplied alpha blending. Note that no error will result from passing
//! > the method a bitmap with premultiplied alpha blending, but this method will
//! > multiply it again, doubling the resulting alpha value.
//!
//! So the helper wants *straight* alpha. Handing it premultiplied pixels is the
//! one mistake it explicitly promises not to report: no error code, just a drag
//! thumbnail that is too dark and too transparent wherever it is translucent —
//! which, for a rounded screenshot card with a soft shadow, is the entire
//! border.
//!
//! That is a silent, unreportable, visually-wrong result produced by code that
//! cannot be executed on the machine it is written on. It is exactly the shape
//! of bug [`super::hdrop`] exists for, so it gets the same treatment: the
//! arithmetic lives here, free of `cfg`, and is tested on every platform.
//!
//! # Layout
//!
//! Four bytes per pixel, `[b, g, r, a]`, which is what a 32-bit `BI_RGB` DIB
//! holds on a little-endian machine and what WIC's `32bppBGRA` and `32bppPBGRA`
//! both hand over.

/// Bytes per pixel in every buffer this module touches.
const STRIDE: usize = 4;

/// Index of the alpha byte within a pixel.
const ALPHA: usize = 3;

/// Converts premultiplied BGRA to straight BGRA, in place.
///
/// Trailing bytes that do not make a whole pixel are left alone rather than
/// treated as a partial pixel, because a buffer of the wrong length is a caller
/// bug and quietly reinterpreting three bytes as a pixel would hide it.
///
/// # Precision
///
/// This is lossy, and unavoidably so: premultiplication quantised the colour
/// channels against alpha, and dividing back out cannot recover what that
/// discarded. At `a = 1` a single colour step spans the whole output range, so
/// the result is coarse. Prefer asking the decoder for straight alpha in the
/// first place; this exists for the path where it cannot.
pub fn unpremultiply_bgra(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<STRIDE>().0 {
        let alpha = pixel[ALPHA];

        // Opaque needs nothing, and it is the overwhelmingly common case in a
        // screenshot — worth the branch to skip three divisions.
        if alpha == u8::MAX {
            continue;
        }

        // Fully transparent carries no colour information at all: whatever the
        // channels hold is arithmetically meaningless, since every one of them
        // was multiplied by zero. Zero is the conventional canonical value and
        // avoids dividing by it.
        if alpha == 0 {
            pixel[..ALPHA].fill(0);
            continue;
        }

        for channel in &mut pixel[..ALPHA] {
            *channel = unpremultiply_channel(*channel, alpha);
        }
    }
}

/// Converts straight BGRA to premultiplied BGRA, in place.
///
/// The inverse of [`unpremultiply_bgra`], and the operation
/// `InitializeFromBitmap` performs internally. Kept public so a caller can
/// reason about — and a test can demonstrate — what passing the wrong flavour
/// actually does to a pixel.
pub fn premultiply_bgra(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<STRIDE>().0 {
        let alpha = pixel[ALPHA];

        if alpha == u8::MAX {
            continue;
        }

        for channel in &mut pixel[..ALPHA] {
            *channel = premultiply_channel(*channel, alpha);
        }
    }
}

/// Divides one channel back out of its alpha, rounding to nearest.
///
/// Saturates rather than wrapping: a premultiplied buffer should never hold a
/// channel above its alpha, but a malformed one might, and a wrapped byte turns
/// a slightly-too-bright pixel into a black one.
fn unpremultiply_channel(channel: u8, alpha: u8) -> u8 {
    let numerator = u32::from(channel) * 255 + u32::from(alpha) / 2;
    let value = numerator / u32::from(alpha);
    u8::try_from(value).unwrap_or(u8::MAX)
}

/// Multiplies one channel by its alpha, rounding to nearest.
fn premultiply_channel(channel: u8, alpha: u8) -> u8 {
    let numerator = u32::from(channel) * u32::from(alpha) + 127;
    // The classic "divide by 255 without dividing": exact for every input here.
    let value = (numerator + numerator / 255) / 256;
    u8::try_from(value).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A translucent pixel is the whole point: this is the case that looks
    /// wrong when the flavours are swapped, and the case an opaque-only test
    /// would pass while shipping the bug.
    #[test]
    fn a_translucent_pixel_is_divided_back_out() {
        // Straight white at half alpha premultiplies to roughly half grey.
        let mut pixels = vec![128, 128, 128, 128];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, vec![255, 255, 255, 128]);
    }

    #[test]
    fn alpha_itself_is_never_touched() {
        let mut pixels = vec![10, 20, 30, 77];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels[ALPHA], 77);

        let mut pixels = vec![10, 20, 30, 77];
        premultiply_bgra(&mut pixels);
        assert_eq!(pixels[ALPHA], 77);
    }

    #[test]
    fn an_opaque_pixel_is_left_exactly_alone() {
        let mut pixels = vec![1, 2, 3, 255];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, vec![1, 2, 3, 255]);
    }

    /// Colour under zero alpha is arithmetically meaningless, and dividing by
    /// it would be a panic rather than a wrong pixel.
    #[test]
    fn a_transparent_pixel_is_canonicalised_to_zero() {
        let mut pixels = vec![9, 9, 9, 0];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, vec![0, 0, 0, 0]);
    }

    /// A channel brighter than its alpha cannot occur in well-formed
    /// premultiplied data, so the only question is what a malformed buffer
    /// does. Saturating keeps it bright; wrapping would make it black.
    #[test]
    fn a_channel_above_its_alpha_saturates_rather_than_wrapping() {
        let mut pixels = vec![200, 200, 200, 10];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, vec![255, 255, 255, 10]);
    }

    #[test]
    fn every_whole_pixel_in_a_buffer_is_converted() {
        let mut pixels = vec![128, 128, 128, 128, 0, 0, 0, 0, 7, 8, 9, 255];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(
            pixels,
            vec![255, 255, 255, 128, 0, 0, 0, 0, 7, 8, 9, 255],
            "each pixel is independent"
        );
    }

    /// A short tail is a caller bug. Leaving it untouched keeps the bug
    /// visible; silently converting three bytes as if they were a pixel would
    /// not.
    #[test]
    fn a_partial_trailing_pixel_is_left_alone() {
        let mut pixels = vec![128, 128, 128, 128, 1, 2, 3];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels[4..], [1, 2, 3]);
    }

    #[test]
    fn an_empty_buffer_is_accepted() {
        let mut pixels: Vec<u8> = Vec::new();
        unpremultiply_bgra(&mut pixels);
        assert!(pixels.is_empty());
    }

    /// Round-tripping a straight pixel through premultiplication and back is
    /// what the drag helper effectively does to correctly-supplied input, so it
    /// has to land where it started for every alpha that carries real signal.
    #[test]
    fn a_round_trip_returns_translucent_pixels_to_themselves() {
        for alpha in [255_u8, 200, 128, 64] {
            for channel in [0_u8, 1, 64, 127, 200, 255] {
                let mut pixels = vec![channel, channel, channel, alpha];
                premultiply_bgra(&mut pixels);
                unpremultiply_bgra(&mut pixels);

                let drift = i32::from(pixels[0]) - i32::from(channel);
                assert!(
                    drift.abs() <= 2,
                    "channel {channel} at alpha {alpha} drifted to {} ({drift})",
                    pixels[0]
                );
            }
        }
    }

    /// The failure this module exists to prevent, written down as an
    /// executable fact: premultiplied input passed straight through gets
    /// multiplied a second time, and a half-alpha pixel comes out at a quarter
    /// of its brightness.
    #[test]
    fn passing_premultiplied_input_would_darken_a_translucent_pixel() {
        let straight = 255_u8;
        let alpha = 128_u8;

        let mut correct = vec![straight, straight, straight, alpha];
        premultiply_bgra(&mut correct);

        // What the helper would do to already-premultiplied input.
        let mut doubled = correct.clone();
        premultiply_bgra(&mut doubled);

        assert!(
            doubled[0] < correct[0] / 2 + 2,
            "double premultiplication should roughly halve {} again, got {}",
            correct[0],
            doubled[0]
        );
    }
}
