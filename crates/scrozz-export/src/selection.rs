//! Destination-aware format and compression selection.

use scrozz_core::{Error, Frame, Result};

use crate::{
    ImageFormat,
    encode::{ColorConversion, EncodeOptions, PngEffort},
    pixels::to_straight_rgba8,
};

/// The kind of destination an export profile describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationKind {
    /// Ephemeral system clipboard data.
    Clipboard,
    /// A durable file in a user-selected folder.
    Folder,
    /// A durable object intended for a browser or remote consumer.
    Upload,
}

/// Formats a destination says it can consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationCapabilities {
    accepted_formats: Vec<ImageFormat>,
}

impl DestinationCapabilities {
    /// Creates capabilities from accepted formats, preserving preference order.
    #[must_use]
    pub fn new(formats: impl IntoIterator<Item = ImageFormat>) -> Self {
        let mut accepted_formats = Vec::new();
        for format in formats {
            if !accepted_formats.contains(&format) {
                accepted_formats.push(format);
            }
        }
        Self { accepted_formats }
    }

    /// PNG, JPEG, and WebP, as supported by a normal filesystem folder.
    #[must_use]
    pub fn folder() -> Self {
        Self::new([ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP])
    }

    /// Whether the destination accepts `format`.
    #[must_use]
    pub fn accepts(&self, format: ImageFormat) -> bool {
        self.accepted_formats.contains(&format)
    }

    /// Accepted formats in the order supplied by the destination.
    #[must_use]
    pub fn accepted_formats(&self) -> &[ImageFormat] {
        &self.accepted_formats
    }
}

/// Colour compatibility requested by a destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationColorSpace {
    /// Preserve source samples and profile.
    Preserve,
    /// Convert known source samples to sRGB.
    Srgb,
}

/// Capabilities and policy for a concrete export destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationProfile {
    /// Destination lifetime and purpose.
    pub kind: DestinationKind,
    /// Formats the consumer accepts.
    pub capabilities: DestinationCapabilities,
    /// Colour space the consumer requires.
    pub color_space: DestinationColorSpace,
}

impl DestinationProfile {
    /// A clipboard profile. PNG is mandatory per D10.
    #[must_use]
    pub fn clipboard() -> Self {
        Self {
            kind: DestinationKind::Clipboard,
            capabilities: DestinationCapabilities::new([ImageFormat::Png]),
            color_space: DestinationColorSpace::Preserve,
        }
    }

    /// A normal folder supporting all built-in formats.
    #[must_use]
    pub fn folder() -> Self {
        Self {
            kind: DestinationKind::Folder,
            capabilities: DestinationCapabilities::folder(),
            color_space: DestinationColorSpace::Preserve,
        }
    }

    /// A web or S3 destination with a declared accepted-format list.
    ///
    /// Browser-facing output defaults to sRGB conversion rather than relying on
    /// every consumer to honour wide-gamut profiles.
    #[must_use]
    pub fn upload(capabilities: DestinationCapabilities) -> Self {
        Self {
            kind: DestinationKind::Upload,
            capabilities,
            color_space: DestinationColorSpace::Srgb,
        }
    }
}

/// What kind of pixels the caller says the frame contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentKind {
    /// Screenshot, UI, text, or other hard-edged content. Prefer lossless.
    #[default]
    Screenshot,
    /// Photographic content where high-quality JPEG is appropriate when opaque.
    Photographic,
}

/// Deterministic format and encoder settings selected for one export.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportSelection {
    /// Chosen output format.
    pub format: ImageFormat,
    /// Encoder settings tuned for the destination.
    pub options: EncodeOptions,
}

/// Selects format and compression from content and destination capabilities.
///
/// Clipboard always selects PNG regardless of a malformed capability list.
/// JPEG is never selected for a frame containing any transparency.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] when no accepted format can preserve the
/// frame (for example, a transparent frame sent to a JPEG-only endpoint).
pub fn select_export(
    frame: &Frame,
    profile: &DestinationProfile,
    content: ContentKind,
) -> Result<ExportSelection> {
    let image = to_straight_rgba8(frame)?;
    let transparent = !image.is_opaque();

    let format = if profile.kind == DestinationKind::Clipboard {
        ImageFormat::Png
    } else {
        let preferences: &[ImageFormat] = match (profile.kind, content, transparent) {
            (_, ContentKind::Photographic, false) => {
                &[ImageFormat::Jpeg, ImageFormat::WebP, ImageFormat::Png]
            }
            (DestinationKind::Upload, _, _) => {
                &[ImageFormat::WebP, ImageFormat::Png, ImageFormat::Jpeg]
            }
            _ => &[ImageFormat::Png, ImageFormat::WebP, ImageFormat::Jpeg],
        };
        preferences
            .iter()
            .copied()
            .find(|format| {
                profile.capabilities.accepts(*format) && (!transparent || format.supports_alpha())
            })
            .ok_or_else(|| Error::Unsupported {
                what: "automatic image-format selection".into(),
                why: if transparent {
                    "the destination accepts no alpha-capable format; transparency will not be \
                     silently flattened to JPEG"
                        .into()
                } else {
                    "the destination accepts none of PNG, JPEG, or WebP".into()
                },
            })?
    };

    let png_effort = match profile.kind {
        DestinationKind::Clipboard => PngEffort::Fast,
        DestinationKind::Folder | DestinationKind::Upload => PngEffort::Maximum,
    };
    let jpeg_quality = match profile.kind {
        DestinationKind::Clipboard => 90,
        DestinationKind::Folder => 92,
        DestinationKind::Upload => 88,
    };
    let color_conversion = match profile.color_space {
        DestinationColorSpace::Preserve => ColorConversion::Preserve,
        DestinationColorSpace::Srgb => ColorConversion::ToSrgb,
    };

    Ok(ExportSelection {
        format,
        options: EncodeOptions {
            png_effort,
            jpeg_quality,
            color_conversion,
            ..EncodeOptions::default()
        },
    })
}
