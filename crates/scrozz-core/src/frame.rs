//! Pixel buffers and their colour interpretation.

use serde::{Deserialize, Serialize};

use crate::geometry::{PhysicalSize, ScaleFactor};

/// Byte layout of a [`Frame`]'s samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PixelFormat {
    /// 8 bits per channel, red-green-blue-alpha byte order, **not** premultiplied.
    Rgba8,
    /// 8 bits per channel, blue-green-red-alpha byte order, **not** premultiplied.
    ///
    /// Windows DXGI and several X11 paths hand back BGRA. Converting eagerly at
    /// the capture boundary costs a full-image pass on every frame, which is
    /// wasteful during recording, so the format travels with the buffer instead.
    Bgra8,
    /// 8 bits per channel, red-green-blue-alpha, **premultiplied** by alpha.
    ///
    /// macOS `CGImage` and Core Animation commonly produce this. Compositing
    /// premultiplied data as if it were straight silhouettes every semi-transparent
    /// edge with black, which is exactly the halo seen around rounded window
    /// corners when this distinction is dropped.
    RgbaPremultiplied8,
    /// 8 bits per channel, blue-green-red-alpha byte order, **premultiplied**.
    ///
    /// What `Windows.Graphics.Capture` actually produces. It exists because the
    /// alternative was worse in both directions: reporting it as [`Self::Bgra8`]
    /// lies about premultiplication and black-fringes every antialiased window
    /// corner, while swizzling to [`Self::RgbaPremultiplied8`] at the capture
    /// boundary costs a full-image pass on every frame — unacceptable while
    /// recording, which is the case the no-eager-conversion rule exists for.
    BgraPremultiplied8,
}

impl PixelFormat {
    /// Bytes occupied by a single pixel.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgba8 | Self::Bgra8 | Self::RgbaPremultiplied8 | Self::BgraPremultiplied8 => 4,
        }
    }

    /// Whether colour channels are scaled by alpha.
    #[must_use]
    pub const fn is_premultiplied(self) -> bool {
        matches!(self, Self::RgbaPremultiplied8 | Self::BgraPremultiplied8)
    }
}

/// The colour space a frame's samples are encoded in.
///
/// Screenshots are one of the few places where colour management is immediately
/// and obviously visible: capture a wide-gamut display, tag the result sRGB, and
/// every saturated colour shifts. Modern Macs are Display P3 by default, so
/// assuming sRGB is wrong on the platform we ship first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ColorSpace {
    /// Standard sRGB.
    #[default]
    Srgb,
    /// Display P3 — the default on current Apple displays.
    DisplayP3,
    /// Rec. 2020, for HDR-capable displays.
    Rec2020,
    /// The backend could not determine the space.
    ///
    /// Distinct from assuming sRGB: it lets a downstream encoder decline to
    /// embed a profile rather than embed a wrong one.
    Unknown,
}

/// A captured image: a pixel buffer plus everything needed to interpret it.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Raw samples, `size.height` rows of `stride` bytes.
    pub data: Vec<u8>,
    /// Dimensions in real pixels.
    pub size: PhysicalSize,
    /// Bytes per row.
    ///
    /// Frequently exceeds `width * bytes_per_pixel`, because GPU and OS capture
    /// APIs pad rows to an alignment boundary. Ignoring stride yields the classic
    /// diagonally-skewed image.
    pub stride: usize,
    /// Sample layout.
    pub format: PixelFormat,
    /// Colour interpretation.
    pub color_space: ColorSpace,
    /// Scale of the display this came from, so logical geometry can be recovered.
    pub scale: ScaleFactor,
}

impl Frame {
    /// Width in whole pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.size.width.round() as u32
    }

    /// Height in whole pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.size.height.round() as u32
    }

    /// Whether `data` is large enough for the declared geometry.
    ///
    /// Cheap, and worth asserting at every backend boundary: a short buffer is
    /// otherwise discovered as a panic deep inside an encoder.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        let min_stride = self.width() as usize * self.format.bytes_per_pixel();
        self.stride >= min_stride
            && self
                .stride
                .checked_mul(self.height() as usize)
                .is_some_and(|needed| self.data.len() >= needed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_stride_overflow_is_malformed_not_a_panic() {
        let frame = Frame {
            data: Vec::new(),
            size: PhysicalSize::new(1.0, 2.0),
            stride: usize::MAX,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::IDENTITY,
        };
        assert!(!frame.is_well_formed());
    }
}
