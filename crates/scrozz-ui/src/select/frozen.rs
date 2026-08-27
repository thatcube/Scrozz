use std::collections::BTreeMap;

use egui::{ColorImage, TextureHandle, TextureOptions};
use scrozz_core::{ColorSpace, Display, DisplayId, Error, Frame, PixelFormat, Result};

use super::geom;

/// One straight-alpha pixel from a frozen desktop image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrozenPixel {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha.
    pub a: u8,
}

impl FrozenPixel {
    /// This pixel as egui colour data.
    #[must_use]
    pub fn to_color32(self) -> egui::Color32 {
        egui::Color32::from_rgba_unmultiplied(self.r, self.g, self.b, self.a)
    }
}

/// A captured display frame converted into an owned, test-friendly form.
#[derive(Debug, Clone, PartialEq)]
pub struct FrozenDisplayFrame {
    /// The measured display this frame belongs to.
    pub display: Display,
    /// The captured colour space.
    pub color_space: ColorSpace,
    pixels: Vec<FrozenPixel>,
    width: usize,
    height: usize,
}

impl FrozenDisplayFrame {
    /// Converts a captured frame into a frozen owned frame.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Codec`] when the pixel buffer is malformed.
    pub fn from_frame(display: Display, frame: Frame) -> Result<Self> {
        if !frame.is_well_formed() {
            return Err(Error::Codec(format!(
                "display `{}`: malformed frame {}x{} stride {}",
                display.id.0,
                frame.width(),
                frame.height(),
                frame.stride
            )));
        }
        let width = frame.width() as usize;
        let height = frame.height() as usize;
        let swap = matches!(
            frame.format,
            PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8
        );
        let premultiplied = frame.format.is_premultiplied();
        let mut pixels = Vec::with_capacity(width * height);
        for y in 0..height {
            let row = &frame.data[y * frame.stride..y * frame.stride + width * 4];
            for pixel in row.as_chunks::<4>().0 {
                let (r, g, b, a) = if swap {
                    (pixel[2], pixel[1], pixel[0], pixel[3])
                } else {
                    (pixel[0], pixel[1], pixel[2], pixel[3])
                };
                let (r, g, b) = if premultiplied {
                    unpremultiply(r, g, b, a)
                } else {
                    (r, g, b)
                };
                pixels.push(FrozenPixel { r, g, b, a });
            }
        }
        Ok(Self {
            display,
            color_space: frame.color_space,
            pixels,
            width,
            height,
        })
    }

    /// Builds a deterministic synthetic frame for tests and harness scenes.
    #[must_use]
    pub fn synthetic(display: Display, seed: u64) -> Self {
        let width = ((display.bounds.size.width * display.scale.get()).round() as usize).max(1);
        let height = ((display.bounds.size.height * display.scale.get()).round() as usize).max(1);
        let mut pixels = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                let xf = x as f32 / width.max(1) as f32;
                let yf = y as f32 / height.max(1) as f32;
                let salt = seed.rotate_left(((x + y) % 63) as u32);
                let wave = (((x * 13 + y * 7) as u64) ^ salt) as u8;
                let r = ((f32::from(wave) * 0.18) + 32.0 + xf * 170.0).round() as u8;
                let g = ((f32::from(wave) * 0.10) + 28.0 + yf * 150.0).round() as u8;
                let b = ((f32::from(wave) * 0.08) + 54.0 + (1.0 - xf) * 120.0).round() as u8;
                pixels.push(FrozenPixel { r, g, b, a: 255 });
            }
        }
        Self {
            display,
            color_space: ColorSpace::Srgb,
            pixels,
            width,
            height,
        }
    }

    /// The frame width in physical pixels.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// The frame height in physical pixels.
    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// Converts the frozen pixels into an egui image.
    #[must_use]
    pub fn color_image(&self) -> ColorImage {
        let mut rgba = Vec::with_capacity(self.pixels.len() * 4);
        for pixel in &self.pixels {
            rgba.extend_from_slice(&[pixel.r, pixel.g, pixel.b, pixel.a]);
        }
        ColorImage::from_rgba_unmultiplied([self.width, self.height], &rgba)
    }

    /// Uploads the frame as an egui texture.
    pub fn upload(&self, ctx: &egui::Context) -> TextureHandle {
        ctx.load_texture(
            format!("selection-frozen:{}", self.display.id.0),
            self.color_image(),
            TextureOptions::LINEAR,
        )
    }

    /// Samples a clamped local physical pixel.
    #[must_use]
    pub fn sample_local(&self, x: u32, y: u32) -> FrozenPixel {
        let x = (x as usize).min(self.width.saturating_sub(1));
        let y = (y as usize).min(self.height.saturating_sub(1));
        self.pixels[y * self.width + x]
    }

    /// Samples the global logical desktop point that lies on this display.
    #[must_use]
    pub fn sample_global_logical(&self, point: scrozz_core::LogicalPoint) -> FrozenPixel {
        let (x, y) = geom::logical_to_local_physical(&self.display, point);
        self.sample_local(x, y)
    }
}

/// A frozen multi-display desktop plus optional uploaded textures.
#[derive(Debug, Clone, Default)]
pub struct FrozenDesktop {
    frames: BTreeMap<String, FrozenDisplayFrame>,
}

impl FrozenDesktop {
    /// Creates a frozen desktop from the measured display frames.
    #[must_use]
    pub fn new(frames: Vec<FrozenDisplayFrame>) -> Self {
        let mut map = BTreeMap::new();
        for frame in frames {
            map.insert(frame.display.id.0.clone(), frame);
        }
        Self { frames: map }
    }

    /// Every frozen display frame, in stable id order.
    pub fn frames(&self) -> impl Iterator<Item = &FrozenDisplayFrame> {
        self.frames.values()
    }

    /// Whether the selector should leave the live desktop visible underneath.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The frozen frame for `display`, if present.
    #[must_use]
    pub fn frame(&self, display: &DisplayId) -> Option<&FrozenDisplayFrame> {
        self.frames.get(&display.0)
    }

    /// Uploads every frozen frame once and returns the handles by display id.
    #[must_use]
    pub fn upload_all(&self, ctx: &egui::Context) -> BTreeMap<String, TextureHandle> {
        self.frames
            .values()
            .map(|frame| (frame.display.id.0.clone(), frame.upload(ctx)))
            .collect()
    }
}

fn unpremultiply(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
    if a == 0 {
        return (0, 0, 0);
    }
    let a = u32::from(a);
    let expand = |channel: u8| ((u32::from(channel) * 255 + a / 2) / a).min(255) as u8;
    (expand(r), expand(g), expand(b))
}
