//! Encoding frames to PNG, JPEG and WebP.

use std::borrow::Cow;

use image::{
    ExtendedColorType, ImageEncoder,
    codecs::{
        jpeg::JpegEncoder,
        png::{CompressionType, FilterType, PngEncoder},
        webp::WebPEncoder,
    },
};
use scrozz_core::{ColorSpace, Error, Frame, Result};

use crate::{
    Encoder, ImageFormat,
    icc::profile_for,
    pixels::{RgbaImage, to_straight_rgba8},
};

/// How hard PNG works to make the file small.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PngEffort {
    /// Fastest, largest. For the clipboard, where the file is never stored.
    Fast,
    /// The default balance.
    #[default]
    Balanced,
    /// Slowest, smallest. For files that are kept or uploaded.
    Maximum,
}

impl From<PngEffort> for CompressionType {
    fn from(effort: PngEffort) -> Self {
        match effort {
            PngEffort::Fast => Self::Fast,
            PngEffort::Balanced => Self::Default,
            PngEffort::Maximum => Self::Best,
        }
    }
}

/// Knobs on the encoder, all with defaults tuned so nobody needs to touch them.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodeOptions {
    /// JPEG quality, 1–100.
    ///
    /// 90 is the point above which JPEG spends bytes on detail nobody can see,
    /// and below which text — which every screenshot has — starts to ring.
    pub jpeg_quality: u8,
    /// What transparency becomes when encoding to JPEG, which has no alpha.
    pub jpeg_background: [u8; 3],
    /// PNG compression effort.
    pub png_effort: PngEffort,
    /// Whether to embed a profile for [`ColorSpace::Srgb`].
    ///
    /// On by default, and the extra few hundred bytes are worth it: several
    /// viewers treat an untagged image as *display native* rather than sRGB, so
    /// on the wide-gamut displays this app is mostly used on, "untagged" and
    /// "sRGB" are not the same picture. [`ColorSpace::Unknown`] embeds nothing
    /// regardless of this setting.
    pub embed_srgb_profile: bool,
    /// Whether to drop a fully opaque alpha channel before encoding.
    ///
    /// Screenshots are nearly always opaque, and three channels are about a
    /// quarter smaller than four for no visible difference.
    pub drop_opaque_alpha: bool,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            jpeg_quality: 90,
            jpeg_background: [255, 255, 255],
            png_effort: PngEffort::default(),
            embed_srgb_profile: true,
            drop_opaque_alpha: true,
        }
    }
}

/// The [`Encoder`] built on the [`image`] crate.
#[derive(Debug, Clone, Default)]
pub struct FrameEncoder {
    options: EncodeOptions,
}

impl FrameEncoder {
    /// An encoder with default options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An encoder with the given options.
    #[must_use]
    pub const fn with_options(options: EncodeOptions) -> Self {
        Self { options }
    }

    /// The options in force.
    #[must_use]
    pub const fn options(&self) -> &EncodeOptions {
        &self.options
    }

    /// The profile to embed for a frame, honouring [`EncodeOptions`].
    fn profile(&self, space: ColorSpace) -> Option<Vec<u8>> {
        if space == ColorSpace::Srgb && !self.options.embed_srgb_profile {
            return None;
        }
        profile_for(space)
    }

    /// Encodes an already-normalised image, for callers that have one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Codec`] if the underlying encoder failed.
    pub fn encode_rgba(
        &self,
        image: &RgbaImage,
        space: ColorSpace,
        format: ImageFormat,
    ) -> Result<Vec<u8>> {
        let profile = self.profile(space);
        match format {
            ImageFormat::Png => self.encode_png(image, profile),
            ImageFormat::Jpeg => self.encode_jpeg(image, profile),
            ImageFormat::WebP => self.encode_webp(image, profile),
        }
    }

    fn encode_png(&self, image: &RgbaImage, profile: Option<Vec<u8>>) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut encoder = PngEncoder::new_with_quality(
            &mut out,
            self.options.png_effort.into(),
            FilterType::Adaptive,
        );
        set_profile(&mut encoder, profile, "PNG");
        let (buffer, colour) = self.body(image);
        encoder
            .write_image(buffer.as_ref(), image.width, image.height, colour)
            .map_err(|e| Error::Codec(format!("PNG encoding failed: {e}")))?;
        Ok(out)
    }

    fn encode_jpeg(&self, image: &RgbaImage, profile: Option<Vec<u8>>) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut out, self.options.jpeg_quality);
        set_profile(&mut encoder, profile, "JPEG");
        // Unconditional, unlike PNG and WebP: JPEG has no alpha channel at all,
        // so transparency must be resolved rather than merely dropped.
        let rgb = image.to_rgb8(self.options.jpeg_background);
        encoder
            .write_image(&rgb, image.width, image.height, ExtendedColorType::Rgb8)
            .map_err(|e| Error::Codec(format!("JPEG encoding failed: {e}")))?;
        Ok(out)
    }

    fn encode_webp(&self, image: &RgbaImage, profile: Option<Vec<u8>>) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        // Lossless is not a choice: `image` 0.25 ships no lossy WebP encoder,
        // and adding one means linking libwebp. Lossless WebP still beats PNG
        // on size, so the gap costs less than it sounds.
        let mut encoder = WebPEncoder::new_lossless(&mut out);
        set_profile(&mut encoder, profile, "WebP");
        let (buffer, colour) = self.body(image);
        encoder
            .write_image(buffer.as_ref(), image.width, image.height, colour)
            .map_err(|e| Error::Codec(format!("WebP encoding failed: {e}")))?;
        Ok(out)
    }

    /// The pixel buffer and colour type to hand an alpha-capable encoder.
    fn body<'a>(&self, image: &'a RgbaImage) -> (Cow<'a, [u8]>, ExtendedColorType) {
        if self.options.drop_opaque_alpha && image.is_opaque() {
            (
                Cow::Owned(image.to_rgb8([0, 0, 0])),
                ExtendedColorType::Rgb8,
            )
        } else {
            (Cow::Borrowed(&image.data), ExtendedColorType::Rgba8)
        }
    }
}

/// Attaches a profile, tolerating an encoder that cannot carry one.
///
/// A format that refuses the profile is a worse picture, not a failed export —
/// so this warns rather than propagating. All three formats here do accept one;
/// the branch exists so that adding a fourth cannot turn into a hard failure at
/// the worst moment.
fn set_profile<E: ImageEncoder>(encoder: &mut E, profile: Option<Vec<u8>>, format: &str) {
    let Some(profile) = profile else { return };
    if let Err(e) = encoder.set_icc_profile(profile) {
        tracing::warn!(
            format,
            error = %e,
            "encoder rejected the colour profile; the image will be untagged"
        );
    }
}

impl Encoder for FrameEncoder {
    fn encode(&self, frame: &Frame, format: ImageFormat) -> Result<Vec<u8>> {
        let image = to_straight_rgba8(frame)?;
        self.encode_rgba(&image, frame.color_space, format)
    }
}
