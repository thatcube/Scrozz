//! Shared helpers for the export integration tests.
//!
//! Frames are built byte-exactly on purpose. Every bug this crate exists to
//! prevent — skew, black fringing, swapped channels — is a bug about *which
//! bytes end up where*, so the tests construct the buffer explicitly rather
//! than through a convenience that might make the same mistake.

#![allow(dead_code)]

use scrozz_core::{ColorSpace, Frame, PhysicalSize, PixelFormat, ScaleFactor};

/// A byte written into stride padding.
///
/// Deliberately conspicuous: if any of it reaches an encoder it shows up as a
/// bright olive smear rather than as plausible-looking image data.
pub const PADDING_SENTINEL: u8 = 0xAB;

/// Builds a frame whose rows are padded by `pad` extra bytes.
///
/// `sample` returns the four bytes to store at `(x, y)` **in the frame's own
/// channel order**, so a test can assert on premultiplied or BGRA data without
/// the helper quietly converting it first.
pub fn frame(
    width: u32,
    height: u32,
    pad: usize,
    format: PixelFormat,
    color_space: ColorSpace,
    sample: impl Fn(u32, u32) -> [u8; 4],
) -> Frame {
    let stride = width as usize * 4 + pad;
    let mut data = vec![PADDING_SENTINEL; stride * height as usize];
    for y in 0..height {
        for x in 0..width {
            let i = y as usize * stride + x as usize * 4;
            data[i..i + 4].copy_from_slice(&sample(x, y));
        }
    }
    Frame {
        data,
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride,
        format,
        color_space,
        scale: ScaleFactor::IDENTITY,
    }
}

/// A tightly packed sRGB RGBA frame.
pub fn rgba(width: u32, height: u32, sample: impl Fn(u32, u32) -> [u8; 4]) -> Frame {
    frame(
        width,
        height,
        0,
        PixelFormat::Rgba8,
        ColorSpace::Srgb,
        sample,
    )
}

/// A solid opaque frame.
pub fn solid(width: u32, height: u32, colour: [u8; 3]) -> Frame {
    rgba(width, height, |_, _| [colour[0], colour[1], colour[2], 255])
}

/// A deterministic pattern with a different value in every channel.
///
/// Chosen so that a swapped channel, a shifted row and a shifted column are all
/// separately detectable: red varies along x, green along y, blue with both.
pub fn pattern(x: u32, y: u32) -> [u8; 4] {
    [
        (x * 37 % 256) as u8,
        (y * 53 % 256) as u8,
        ((x * 7 + y * 11) % 256) as u8,
        255,
    ]
}

/// Decodes to tightly packed straight-alpha RGBA.
pub fn decode(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let image = image::load_from_memory(bytes).expect("decodes").to_rgba8();
    let (w, h) = image.dimensions();
    (w, h, image.into_raw())
}

/// The pixel at `(x, y)` of a decoded buffer.
pub fn pixel_at(data: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = (y as usize * width as usize + x as usize) * 4;
    [data[i], data[i + 1], data[i + 2], data[i + 3]]
}

/// The ICC profile a PNG, JPEG or WebP carries, if any.
pub fn embedded_profile(bytes: &[u8]) -> Option<Vec<u8>> {
    use image::ImageDecoder;
    use std::io::Cursor;

    let cursor = Cursor::new(bytes.to_vec());
    match scrozz_export::ImageFormat::sniff(bytes)? {
        scrozz_export::ImageFormat::Png => image::codecs::png::PngDecoder::new(cursor)
            .ok()?
            .icc_profile()
            .ok()?,
        scrozz_export::ImageFormat::Jpeg => image::codecs::jpeg::JpegDecoder::new(cursor)
            .ok()?
            .icc_profile()
            .ok()?,
        scrozz_export::ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(cursor)
            .ok()?
            .icc_profile()
            .ok()?,
    }
}

/// A tiny linear congruential generator.
///
/// The property-style tests want many shapes without a `proptest` dependency,
/// and a fixed seed means a failure is reproducible from the test name alone.
pub struct Rng(u64);

impl Rng {
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    /// A value in `low..=high`.
    pub fn range(&mut self, low: u32, high: u32) -> u32 {
        low + self.next_u32() % (high - low + 1)
    }

    pub fn byte(&mut self) -> u8 {
        (self.next_u32() & 0xFF) as u8
    }
}
