//! Renders known text into an RGBA8 buffer, offscreen.
//!
//! A `CGBitmapContext` draws into memory we own — no window, no display, no
//! screen recording permission. That matters: the test has to run on a
//! developer's machine and in CI without either opening a window or asking for
//! anything.
//!
//! Core Graphics' `SelectFont`/`ShowTextAtPoint` pair is deprecated, but it is
//! the only text API reachable from this crate's declared dependencies (Core
//! Text is not one of them), and a test is exactly the place where a deprecated
//! API is acceptable. If it ever stops rendering, [`Canvas::draw_text`] falls
//! back to a blocky built-in font, so the recognition tests keep meaning
//! something rather than silently passing over a blank image.

#![allow(deprecated)]

use std::ffi::c_char;

use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGTextEncoding};

/// `kCGImageAlphaPremultipliedLast` — the only 8-bit RGBA layout Core Graphics
/// will actually allocate a context for. Everything here is drawn opaque, so
/// premultiplied and straight alpha agree byte for byte.
const ALPHA_PREMULTIPLIED_LAST: u32 = 1;

/// How wide a hand-drawn glyph cell is, as a fraction of its height.
const FALLBACK_ASPECT: f64 = 0.62;

/// Which of the two paths actually put ink on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    /// Real font rendering through Core Graphics.
    CoreGraphics,
    /// The built-in 5x7 font, used only if the deprecated API stops drawing.
    FallbackFont,
}

pub struct Canvas {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0; (width as usize) * (height as usize) * 4],
        }
    }

    pub fn fill_white(&mut self) {
        self.data.fill(0xFF);
    }

    /// Draws `text` with its baseline at `(x, y)` in Core Graphics coordinates
    /// (origin bottom-left), in black.
    ///
    /// `y` is deliberately bottom-left: it keeps the test's mental model the
    /// same as Vision's, so a test that says "drawn at y = 300 of 400, i.e. the
    /// upper half" is checkable by hand.
    pub fn draw_text(&mut self, text: &str, x: f64, y: f64, size: f64) -> Renderer {
        if self.draw_text_with_core_graphics(text, x, y, size) {
            Renderer::CoreGraphics
        } else {
            self.draw_text_with_fallback_font(text, x, y, size);
            Renderer::FallbackFont
        }
    }

    /// Non-white pixel count, exposed so a test can prove something was drawn.
    pub fn ink(&self) -> usize {
        self.ink_count()
    }

    /// Returns the buffer as tightly packed, top-row-first RGBA8.
    pub fn into_rgba8(self) -> Vec<u8> {
        self.data
    }

    /// Returns `false` if the context could not be made or nothing was drawn.
    fn draw_text_with_core_graphics(&mut self, text: &str, x: f64, y: f64, size: f64) -> bool {
        let Some(color_space) = CGColorSpace::new_device_rgb() else {
            return false;
        };
        let before = self.ink_count();

        // SAFETY: the pointer is a live buffer of exactly `height * width * 4`
        // bytes and outlives the context, which is dropped at the end of this
        // function.
        let context = unsafe {
            CGBitmapContextCreate(
                self.data.as_mut_ptr().cast(),
                self.width as usize,
                self.height as usize,
                8,
                self.width as usize * 4,
                Some(&color_space),
                ALPHA_PREMULTIPLIED_LAST,
            )
        };
        let Some(context) = context else {
            return false;
        };

        CGContext::set_should_antialias(Some(&context), true);
        CGContext::set_rgb_fill_color(Some(&context), 0.0, 0.0, 0.0, 1.0);

        // MacRoman is fine: the tests draw ASCII only.
        let font = c"Helvetica-Bold";
        // SAFETY: `font` is a live, NUL-terminated C string.
        unsafe {
            CGContext::select_font(
                Some(&context),
                font.as_ptr().cast::<c_char>(),
                size,
                CGTextEncoding::EncodingMacRoman,
            );
        }

        let bytes = text.as_bytes();
        // SAFETY: `bytes` is a live buffer and `len` is its exact length.
        unsafe {
            CGContext::show_text_at_point(
                Some(&context),
                x,
                y,
                bytes.as_ptr().cast::<c_char>(),
                bytes.len(),
            );
        }

        CGContext::flush(Some(&context));
        drop(context);

        self.ink_count() > before
    }

    /// Number of non-white pixels — enough to tell "something was drawn" from
    /// "the deprecated API quietly did nothing".
    fn ink_count(&self) -> usize {
        self.data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] < 200 || p[1] < 200 || p[2] < 200)
            .count()
    }

    /// Draws with a 5×7 built-in font, scaled up.
    ///
    /// Crude, but high-contrast and large, which is what a recogniser wants.
    fn draw_text_with_fallback_font(&mut self, text: &str, x: f64, y: f64, size: f64) {
        let cell_h = size / 7.0;
        let cell_w = (size * FALLBACK_ASPECT) / 5.0;
        let advance = size * FALLBACK_ASPECT * 1.3;

        for (index, ch) in text.chars().enumerate() {
            let Some(glyph) = glyph(ch) else { continue };
            let origin_x = x + index as f64 * advance;
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..5u32 {
                    if bits & (1 << (4 - col)) == 0 {
                        continue;
                    }
                    // Row 0 is the top of the glyph; `y` is its baseline in
                    // bottom-left space, so the glyph's top is `y + size`.
                    let top = y + size - row as f64 * cell_h;
                    self.fill_cg_rect(
                        origin_x + f64::from(col) * cell_w,
                        top - cell_h,
                        cell_w.max(1.0),
                        cell_h.max(1.0),
                    );
                }
            }
        }
    }

    /// Fills a rectangle given in bottom-left coordinates with black.
    fn fill_cg_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let top = f64::from(self.height) - (y + h);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (x0, y0) = (x.max(0.0).round() as u32, top.max(0.0).round() as u32);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (x1, y1) = (
            (x + w).max(0.0).round() as u32,
            (top + h).max(0.0).round() as u32,
        );

        for py in y0..y1.min(self.height) {
            for px in x0..x1.min(self.width) {
                let at = ((py * self.width + px) * 4) as usize;
                self.data[at] = 0;
                self.data[at + 1] = 0;
                self.data[at + 2] = 0;
                self.data[at + 3] = 255;
            }
        }
    }
}

/// A 5×7 bitmap for the handful of characters the tests draw.
fn glyph(ch: char) -> Option<[u8; 7]> {
    let rows = match ch.to_ascii_uppercase() {
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        _ => return None,
    };
    Some(rows)
}
