//! The document: a capture plus every edit ever made to it.

use std::{collections::BTreeMap, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use scrozz_core::{
    Capture, ColorSpace, Error, Frame, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize,
    PixelFormat, Result, ScaleFactor,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, decode, to_straight_rgba8};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::SerializeStruct,
};

use crate::{
    annotation::{Annotation, AnnotationId, AnnotationObject},
    smart_frame::{SMART_FRAME_ALGORITHM_VERSION, SmartFrameMetadata},
    style::{Color, Style},
};

/// Rendered raster limit shared with the compositor.
///
/// Forty million pixels admits an 8K canvas while refusing dimensions
/// large enough to make Rust's infallible allocator abort the process.
pub(crate) const MAX_RASTER_PIXELS: u64 = 40_000_000;
pub(crate) const MAX_RASTER_EDGE: u32 = 16_384;
const MAX_BACKGROUND_PIXELS: u64 = 16_777_216;
const MAX_BACKGROUND_BYTES: u64 = MAX_BACKGROUND_PIXELS * 4;
const MAX_ENCODED_BACKGROUND_BYTES: u64 = MAX_BACKGROUND_BYTES + 1024 * 1024;

/// A procedural background shipped with Scrozz.
///
/// These are values rather than asset paths so a document never depends on the
/// current working directory, an install layout, or a file that can disappear.
/// The renderer owns their exact deterministic appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltInBackground {
    /// Quiet cool-grey paper.
    #[default]
    Mist,
    /// Periwinkle fading into violet, matching Scrozz's iris accent.
    Iris,
    /// Deep blue with a restrained cyan lift.
    Midnight,
    /// Soft peach and rose.
    Sunrise,
    /// Blue-green glass.
    Lagoon,
    /// Warm neutral studio paper.
    Sand,
}

/// A self-contained custom background image.
///
/// Pixels stay as shared straight-alpha RGBA8 in memory. Persistence carries a
/// bounded, base64-encoded PNG so a sidecar remains self-contained without
/// materialising one JSON value per channel byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundImage {
    /// Pixel width.
    width: u32,
    /// Pixel height.
    height: u32,
    /// Tightly packed straight-alpha RGBA8 samples.
    pixels: Arc<[u8]>,
    /// Interpretation of the colour samples.
    color_space: ColorSpace,
    encoded_png: Arc<[u8]>,
}

impl BackgroundImage {
    /// Wraps a tightly packed RGBA8 image.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for zero dimensions or a buffer whose
    /// length does not exactly match `width × height × 4`.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>, color_space: ColorSpace) -> Result<Self> {
        validate_background_pixels(width, height, &pixels)?;
        let frame = Frame {
            data: pixels,
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride: width as usize * PixelFormat::Rgba8.bytes_per_pixel(),
            format: PixelFormat::Rgba8,
            color_space,
            scale: ScaleFactor::IDENTITY,
        };
        let encoded_png = FrameEncoder::new().encode(&frame, ImageFormat::Png)?;
        Self::from_parts(width, height, frame.data, color_space, encoded_png)
    }

    fn from_parts(
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        color_space: ColorSpace,
        encoded_png: Vec<u8>,
    ) -> Result<Self> {
        validate_background_pixels(width, height, &pixels)?;
        if encoded_png.is_empty() || encoded_png.len() as u64 > MAX_ENCODED_BACKGROUND_BYTES {
            return Err(Error::InvalidRequest(format!(
                "encoded background is {} bytes; the limit is {MAX_ENCODED_BACKGROUND_BYTES}",
                encoded_png.len()
            )));
        }
        Ok(Self {
            width,
            height,
            pixels: pixels.into(),
            color_space,
            encoded_png: encoded_png.into(),
        })
    }

    /// Pixel width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Pixel height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Tightly packed straight-alpha RGBA8 samples.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Interpretation of the colour samples.
    #[must_use]
    pub const fn color_space(&self) -> ColorSpace {
        self.color_space
    }

    /// Bytes in the compact persisted representation.
    pub(crate) fn encoded_len(&self) -> usize {
        self.encoded_png.len()
    }

    /// Checks the pixel geometry without reading outside the buffer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the image is empty, too large to
    /// address, or not tightly packed RGBA8.
    pub fn validate(&self) -> Result<()> {
        validate_background_pixels(self.width, self.height, &self.pixels)?;
        if self.encoded_png.is_empty()
            || self.encoded_png.len() as u64 > MAX_ENCODED_BACKGROUND_BYTES
        {
            return Err(Error::InvalidRequest(format!(
                "encoded background is {} bytes; the limit is {MAX_ENCODED_BACKGROUND_BYTES}",
                self.encoded_png.len()
            )));
        }
        Ok(())
    }
}

impl Serialize for BackgroundImage {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("BackgroundImage", 4)?;
        state.serialize_field("width", &self.width)?;
        state.serialize_field("height", &self.height)?;
        state.serialize_field("pixels_png", &STANDARD.encode(&self.encoded_png))?;
        state.serialize_field("color_space", &self.color_space)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for BackgroundImage {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StoredBackgroundImage {
            width: u32,
            height: u32,
            #[serde(default)]
            pixels: Option<Vec<u8>>,
            #[serde(default)]
            pixels_png: Option<String>,
            #[serde(default)]
            color_space: ColorSpace,
        }

        let stored = StoredBackgroundImage::deserialize(deserializer)?;
        validate_background_area(stored.width, stored.height).map_err(D::Error::custom)?;
        match (stored.pixels, stored.pixels_png) {
            (Some(pixels), None) => {
                Self::new(stored.width, stored.height, pixels, stored.color_space)
                    .map_err(D::Error::custom)
            }
            (None, Some(encoded)) => {
                let max_base64 = MAX_ENCODED_BACKGROUND_BYTES.div_ceil(3) * 4;
                if encoded.len() as u64 > max_base64 {
                    return Err(D::Error::custom(format!(
                        "encoded background text is {} bytes; the limit is {max_base64}",
                        encoded.len()
                    )));
                }
                let png = STANDARD.decode(encoded).map_err(D::Error::custom)?;
                if png.len() as u64 > MAX_ENCODED_BACKGROUND_BYTES {
                    return Err(D::Error::custom(format!(
                        "encoded background is {} bytes; the limit is \
                         {MAX_ENCODED_BACKGROUND_BYTES}",
                        png.len()
                    )));
                }
                let frame = decode(&png).map_err(D::Error::custom)?;
                if frame.width() != stored.width || frame.height() != stored.height {
                    return Err(D::Error::custom(format!(
                        "encoded background is {}x{}, expected {}x{}",
                        frame.width(),
                        frame.height(),
                        stored.width,
                        stored.height
                    )));
                }
                if frame.color_space != ColorSpace::Unknown
                    && frame.color_space != stored.color_space
                {
                    return Err(D::Error::custom(format!(
                        "encoded background profile is {:?}, metadata says {:?}",
                        frame.color_space, stored.color_space
                    )));
                }
                let pixels = to_straight_rgba8(&frame).map_err(D::Error::custom)?;
                Self::from_parts(
                    stored.width,
                    stored.height,
                    pixels.data,
                    stored.color_space,
                    png,
                )
                .map_err(D::Error::custom)
            }
            (Some(_), Some(_)) => Err(D::Error::custom(
                "background image carries both legacy and compressed pixels",
            )),
            (None, None) => Err(D::Error::custom("background image has no pixels")),
        }
    }
}

fn validate_background_pixels(width: u32, height: u32, pixels: &[u8]) -> Result<()> {
    let pixel_count = validate_background_area(width, height)?;
    let expected = usize::try_from(pixel_count)
        .ok()
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| {
            Error::InvalidRequest(format!("background image {width}x{height} is too large"))
        })?;
    if width == 0 || height == 0 || pixels.len() != expected {
        return Err(Error::InvalidRequest(format!(
            "background image must be non-empty RGBA8: {} bytes for {width}x{height}",
            pixels.len()
        )));
    }
    Ok(())
}

fn validate_background_area(width: u32, height: u32) -> Result<u64> {
    let pixel_count = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| {
            Error::InvalidRequest(format!("background image {width}x{height} is too large"))
        })?;
    if width == 0
        || height == 0
        || width > MAX_RASTER_EDGE
        || height > MAX_RASTER_EDGE
        || pixel_count > MAX_BACKGROUND_PIXELS
    {
        return Err(Error::InvalidRequest(format!(
            "background image {width}x{height} has {pixel_count} pixels; the limit is \
             {MAX_BACKGROUND_PIXELS}"
        )));
    }
    Ok(pixel_count)
}

/// The background painted behind a beautified capture.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    /// Nothing — the padding stays transparent.
    #[default]
    Transparent,
    /// A flat colour.
    Solid(Color),
    /// A vertical gradient from `start` at the top to `end` at the bottom.
    Gradient {
        /// Colour at the top edge.
        start: Color,
        /// Colour at the bottom edge.
        end: Color,
    },
    /// A procedural background bundled with Scrozz.
    BuiltIn(BuiltInBackground),
    /// A capture-derived, fully resolved two-stop field.
    Automatic(AutomaticBackground),
    /// A custom image, cropped to cover the canvas.
    Image(BackgroundImage),
}

/// Resolved automatic background inputs and output colours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutomaticBackground {
    /// Algorithm that chose these colours.
    pub algorithm_version: u16,
    /// First field colour, stored in sRGB authoring space.
    pub start: Color,
    /// Second field colour, stored in sRGB authoring space.
    pub end: Color,
    /// Average outer-edge colour used for separation checks.
    pub edge_reference: Color,
    /// Colour space of the source samples before analysis.
    pub source_color_space: ColorSpace,
    /// Lowest contrast ratio between either stop and `edge_reference`, ×100.
    pub minimum_contrast_x100: u16,
    /// Unknown fields survive read-modify-write cycles.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl Default for AutomaticBackground {
    fn default() -> Self {
        Self::fallback(ColorSpace::Unknown)
    }
}

impl AutomaticBackground {
    /// Stable neutral used before analysis completes or when colour is unknown.
    #[must_use]
    pub fn fallback(source_color_space: ColorSpace) -> Self {
        Self {
            algorithm_version: SMART_FRAME_ALGORITHM_VERSION,
            start: Color::rgb(34, 40, 55),
            end: Color::rgb(62, 74, 103),
            edge_reference: Color::rgb(128, 128, 128),
            source_color_space,
            minimum_contrast_x100: 300,
            extensions: BTreeMap::new(),
        }
    }

    /// Mean luminance of the two resolved stops.
    #[must_use]
    pub fn average_luminance(&self) -> f32 {
        (self.start.luminance() + self.end.luminance()) / 2.0
    }
}

/// Non-destructive inset in source logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceInsets {
    /// Left edge.
    pub left: f64,
    /// Top edge.
    pub top: f64,
    /// Right edge.
    pub right: f64,
    /// Bottom edge.
    pub bottom: f64,
}

impl SourceInsets {
    /// Equal inset on every edge.
    #[must_use]
    pub const fn uniform(value: f64) -> Self {
        Self {
            left: value,
            top: value,
            right: value,
            bottom: value,
        }
    }

    /// Whether no source pixels are excluded.
    #[must_use]
    pub fn is_zero(self) -> bool {
        [self.left, self.top, self.right, self.bottom]
            .into_iter()
            .all(|value| value <= 0.0)
    }
}

/// Exact presentation-canvas dimensions in output pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExactOutputSize {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl ExactOutputSize {
    /// Creates exact output dimensions.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Width divided by height.
    #[must_use]
    pub fn ratio(self) -> f64 {
        f64::from(self.width) / f64::from(self.height.max(1))
    }
}

/// Optional text placed on the presentation canvas, never over source pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Watermark {
    /// Text to draw.
    pub text: String,
    /// sRGB text colour.
    pub color: Color,
    /// Logical cap height.
    pub font_size: f64,
    /// Logical distance from the canvas edge.
    pub margin: f64,
}

impl Default for Watermark {
    fn default() -> Self {
        Self {
            text: String::new(),
            color: Color::rgba(255, 255, 255, 180),
            font_size: 12.0,
            margin: 12.0,
        }
    }
}

/// Where the capture sits when an aspect preset creates extra canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Alignment {
    /// Top-left.
    TopLeft,
    /// Top edge, centred horizontally.
    Top,
    /// Top-right.
    TopRight,
    /// Left edge, centred vertically.
    Left,
    /// Centred on both axes.
    #[default]
    Center,
    /// Right edge, centred vertically.
    Right,
    /// Bottom-left.
    BottomLeft,
    /// Bottom edge, centred horizontally.
    Bottom,
    /// Bottom-right.
    BottomRight,
}

impl Alignment {
    /// Horizontal alignment as a normalised 0–1 factor.
    #[must_use]
    pub const fn horizontal(self) -> f64 {
        match self {
            Self::TopLeft | Self::Left | Self::BottomLeft => 0.0,
            Self::Top | Self::Center | Self::Bottom => 0.5,
            Self::TopRight | Self::Right | Self::BottomRight => 1.0,
        }
    }

    /// Vertical alignment as a normalised 0–1 factor.
    #[must_use]
    pub const fn vertical(self) -> f64 {
        match self {
            Self::TopLeft | Self::Top | Self::TopRight => 0.0,
            Self::Left | Self::Center | Self::Right => 0.5,
            Self::BottomLeft | Self::Bottom | Self::BottomRight => 1.0,
        }
    }
}

/// Output aspect ratios with names people recognise from their destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AspectPreset {
    /// Keep the capture's natural aspect ratio.
    #[default]
    Original,
    /// 1:1 social post.
    Square,
    /// 4:5 portrait post.
    Portrait,
    /// 9:16 story/reel.
    Story,
    /// 16:9 landscape post or thumbnail.
    Landscape,
    /// 3:1 social header.
    Wide,
}

impl AspectPreset {
    /// Width divided by height, or `None` for the source's natural ratio.
    #[must_use]
    pub const fn ratio(self) -> Option<f64> {
        match self {
            Self::Original => None,
            Self::Square => Some(1.0),
            Self::Portrait => Some(4.0 / 5.0),
            Self::Story => Some(9.0 / 16.0),
            Self::Landscape => Some(16.0 / 9.0),
            Self::Wide => Some(3.0),
        }
    }
}

/// Named combinations available in the CLI and editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BeautificationPreset {
    /// Neutral framing that works around any screenshot.
    #[default]
    Clean,
    /// Square, colourful, and visually balanced for a social post.
    Social,
    /// Tall story/reel canvas.
    Story,
    /// Restrained warm paper with a fine border.
    Editorial,
}

/// Padding, background and framing applied around a capture.
///
/// Per decision D9 a window capture may use only the outer-canvas fields. The OS
/// supplied the subject's true shape and shadow, so inset, synthetic corners,
/// border, and shadow are rejected by [`Document::set_beautification`] and by
/// the renderer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Beautification {
    /// Padding around the image, in logical points.
    pub padding: f64,
    /// Non-destructive source-space trim.
    pub inset: SourceInsets,
    /// Corner radius applied to the image.
    pub corner_radius: f64,
    /// Drop shadow depth.
    pub shadow: f64,
    /// What fills the padding.
    pub background: Background,
    /// Position within any extra canvas created by the aspect preset.
    pub alignment: Alignment,
    /// Shift the capture so its visual weight, not merely its bounds, is centred.
    pub auto_balance: bool,
    /// Output aspect ratio.
    pub aspect: AspectPreset,
    /// Exact output dimensions, which take precedence over `aspect`.
    pub output_size: Option<ExactOutputSize>,
    /// Border width drawn inside the rounded capture edge, in logical points.
    pub border_width: f64,
    /// Border colour.
    pub border_color: Color,
    /// Optional user-authored watermark. Off by default.
    pub watermark: Option<Watermark>,
    /// Resolved Smart Frame analysis for stable rendering across upgrades.
    pub smart_frame: Option<SmartFrameMetadata>,
    /// Unknown fields survive read-modify-write cycles.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl Default for Beautification {
    fn default() -> Self {
        Self {
            padding: 0.0,
            inset: SourceInsets::uniform(0.0),
            corner_radius: 0.0,
            shadow: 0.0,
            background: Background::Transparent,
            alignment: Alignment::Center,
            auto_balance: false,
            aspect: AspectPreset::Original,
            output_size: None,
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
            watermark: None,
            smart_frame: None,
            extensions: BTreeMap::new(),
        }
    }
}

impl Beautification {
    /// Largest supported logical measurement.
    ///
    /// This is intentionally generous. The limit exists to turn corrupt
    /// sidecars into a useful error before they request a multi-terabyte pixmap.
    pub const MAX_MEASUREMENT: f64 = 16_384.0;

    /// A preset: generous padding on a flat neutral background, no shadow.
    #[must_use]
    pub fn padded(padding: f64, background: Background) -> Self {
        Self {
            padding,
            inset: SourceInsets::uniform(0.0),
            corner_radius: 0.0,
            shadow: 0.0,
            background,
            alignment: Alignment::Center,
            auto_balance: false,
            aspect: AspectPreset::Original,
            output_size: None,
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
            watermark: None,
            smart_frame: None,
            extensions: BTreeMap::new(),
        }
    }

    /// One of Scrozz's named starting points.
    #[must_use]
    pub const fn preset(preset: BeautificationPreset) -> Self {
        match preset {
            BeautificationPreset::Clean => Self {
                padding: 40.0,
                inset: SourceInsets::uniform(0.0),
                corner_radius: 16.0,
                shadow: 18.0,
                background: Background::BuiltIn(BuiltInBackground::Mist),
                alignment: Alignment::Center,
                auto_balance: true,
                aspect: AspectPreset::Original,
                output_size: None,
                border_width: 1.0,
                border_color: Color::rgba(255, 255, 255, 90),
                watermark: None,
                smart_frame: None,
                extensions: BTreeMap::new(),
            },
            BeautificationPreset::Social => Self {
                padding: 64.0,
                inset: SourceInsets::uniform(0.0),
                corner_radius: 20.0,
                shadow: 24.0,
                background: Background::BuiltIn(BuiltInBackground::Iris),
                alignment: Alignment::Center,
                auto_balance: true,
                aspect: AspectPreset::Square,
                output_size: None,
                border_width: 1.0,
                border_color: Color::rgba(255, 255, 255, 110),
                watermark: None,
                smart_frame: None,
                extensions: BTreeMap::new(),
            },
            BeautificationPreset::Story => Self {
                padding: 72.0,
                inset: SourceInsets::uniform(0.0),
                corner_radius: 24.0,
                shadow: 28.0,
                background: Background::BuiltIn(BuiltInBackground::Midnight),
                alignment: Alignment::Center,
                auto_balance: true,
                aspect: AspectPreset::Story,
                output_size: None,
                border_width: 1.0,
                border_color: Color::rgba(255, 255, 255, 90),
                watermark: None,
                smart_frame: None,
                extensions: BTreeMap::new(),
            },
            BeautificationPreset::Editorial => Self {
                padding: 56.0,
                inset: SourceInsets::uniform(0.0),
                corner_radius: 10.0,
                shadow: 14.0,
                background: Background::BuiltIn(BuiltInBackground::Sand),
                alignment: Alignment::Center,
                auto_balance: true,
                aspect: AspectPreset::Portrait,
                output_size: None,
                border_width: 1.0,
                border_color: Color::rgba(65, 53, 43, 65),
                watermark: None,
                smart_frame: None,
                extensions: BTreeMap::new(),
            },
        }
    }

    /// The radius a shape nested inside another must use to look concentric.
    ///
    /// D9's corollary: `inner_radius = outer_radius − padding`. Nesting two
    /// rounded shapes at the *same* radius is the specific mistake that makes
    /// corners look subtly wrong even though both shapes are "rounded".
    #[must_use]
    pub fn nested_radius(outer_radius: f64, padding: f64) -> f64 {
        (outer_radius - padding).max(0.0)
    }

    /// Whether this would visibly change the image at all.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.padding <= 0.0
            && self.corner_radius <= 0.0
            && self.shadow <= 0.0
            && self.background == Background::Transparent
            && self.aspect == AspectPreset::Original
            && self.output_size.is_none()
            && self.border_width <= 0.0
            && self.inset.is_zero()
            && self
                .watermark
                .as_ref()
                .is_none_or(|watermark| watermark.text.is_empty())
    }

    /// Validates values before they can reach allocation or rasterisation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a non-finite, negative, or
    /// implausibly large measurement, or malformed custom background pixels.
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("padding", self.padding),
            ("left inset", self.inset.left),
            ("top inset", self.inset.top),
            ("right inset", self.inset.right),
            ("bottom inset", self.inset.bottom),
            ("corner radius", self.corner_radius),
            ("shadow", self.shadow),
            ("border width", self.border_width),
        ] {
            if !value.is_finite() || !(0.0..=Self::MAX_MEASUREMENT).contains(&value) {
                return Err(Error::InvalidRequest(format!(
                    "beautification {name} must be between 0 and {}, got {value}",
                    Self::MAX_MEASUREMENT
                )));
            }
        }
        if let Background::Image(image) = &self.background {
            image.validate()?;
        }
        if let Some(size) = self.output_size {
            let pixels = u64::from(size.width)
                .checked_mul(u64::from(size.height))
                .ok_or_else(|| Error::InvalidRequest("exact output area overflowed".to_owned()))?;
            if size.width == 0
                || size.height == 0
                || size.width > MAX_RASTER_EDGE
                || size.height > MAX_RASTER_EDGE
                || pixels > MAX_RASTER_PIXELS
            {
                return Err(Error::InvalidRequest(format!(
                    "exact output {}x{} has {pixels} pixels; the limit is {MAX_RASTER_PIXELS}",
                    size.width, size.height
                )));
            }
        }
        if let Some(watermark) = &self.watermark {
            if watermark.text.chars().count() > 160 {
                return Err(Error::InvalidRequest(
                    "watermark text cannot exceed 160 characters".to_owned(),
                ));
            }
            for (name, value) in [
                ("watermark size", watermark.font_size),
                ("watermark margin", watermark.margin),
            ] {
                if !value.is_finite() || !(0.0..=Self::MAX_MEASUREMENT).contains(&value) {
                    return Err(Error::InvalidRequest(format!(
                        "{name} must be between 0 and {}, got {value}",
                        Self::MAX_MEASUREMENT
                    )));
                }
            }
        }
        Ok(())
    }

    /// Whether this framing leaves every source pixel untouched.
    #[must_use]
    pub fn preserves_subject_pixels(&self) -> bool {
        self.inset.is_zero()
            && self.corner_radius <= 0.0
            && self.shadow <= 0.0
            && self.border_width <= 0.0
    }

    /// Logical output size before rasterisation.
    ///
    /// Aspect presets only add canvas; they never crop or scale the capture.
    #[must_use]
    pub fn output_size(&self, content: LogicalSize) -> LogicalSize {
        let content_width = (content.width - self.inset.left - self.inset.right).max(1.0);
        let content_height = (content.height - self.inset.top - self.inset.bottom).max(1.0);
        let base_width = content_width + self.padding * 2.0;
        let base_height = content_height + self.padding * 2.0;
        let Some(ratio) = self.aspect.ratio() else {
            return LogicalSize::new(base_width, base_height);
        };
        if base_width / base_height < ratio {
            LogicalSize::new(base_height * ratio, base_height)
        } else {
            LogicalSize::new(base_width, base_width / ratio)
        }
    }
}

/// The editable part of a document: everything except the pixels.
///
/// Per decision D14 this is persisted invisibly alongside the capture rather
/// than exposed as a `.scrozz` project file, so reopening a capture months later
/// restores every arrow with nothing for the user to have managed or lost. It is
/// deliberately an internal, unadvertised format: keeping it unpublished is what
/// lets it change freely while the tool set is still being designed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentData {
    /// Format version, so an old document can be migrated rather than rejected.
    pub version: u32,
    /// Edits, in z-order: last is on top.
    pub annotations: Vec<AnnotationObject>,
    /// Framing, if permitted.
    pub beautification: Option<Beautification>,
    /// The next identifier to hand out.
    ///
    /// Persisted so a reopened document cannot reissue an id that an undo stack
    /// or a selection still refers to.
    pub next_id: u64,
    /// Unknown fields survive read-modify-write cycles.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl DocumentData {
    /// The current format version.
    pub const VERSION: u32 = 3;
}

impl Default for DocumentData {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            annotations: Vec::new(),
            beautification: None,
            next_id: 1,
            extensions: BTreeMap::new(),
        }
    }
}

/// A capture plus every edit ever made to it.
///
/// The annotation list is private on purpose. Two invariants have to hold for
/// the document to behave the way decision D14 promises — identifiers are unique
/// and never reused, and counter markers stay numbered 1..n with no gaps — and
/// neither survives a `pub Vec` that any caller can splice.
#[derive(Debug, Clone)]
pub struct Document {
    /// The untouched source. Never mutated.
    ///
    /// Rendering copies before it composites, and redaction destroys pixels only
    /// in that copy. A redacted export is unrecoverable; the document it came
    /// from is still fully editable.
    pub source: Capture,
    objects: Vec<AnnotationObject>,
    beautification: Option<Beautification>,
    next_id: u64,
    extensions: BTreeMap<String, serde_json::Value>,
}

impl Document {
    /// Wraps a fresh capture in an empty document.
    #[must_use]
    pub fn new(source: Capture) -> Self {
        Self {
            source,
            objects: Vec::new(),
            beautification: None,
            next_id: 1,
            extensions: BTreeMap::new(),
        }
    }

    /// Rebuilds a document from a capture and its persisted edits.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the data is from a newer format
    /// version, malformed framing, or window framing that violates D9.
    pub fn from_data(source: Capture, data: DocumentData) -> Result<Self> {
        if data.version > DocumentData::VERSION {
            return Err(Error::InvalidRequest(format!(
                "document format version {} is newer than supported version {}",
                data.version,
                DocumentData::VERSION
            )));
        }
        if let Some(beautification) = &data.beautification {
            beautification.validate()?;
            Self::validate_provenance(beautification, &source)?;
        }
        let highest = data
            .annotations
            .iter()
            .map(|o| o.id.0)
            .max()
            .map_or(0, |id| id + 1);
        let mut document = Self {
            source,
            objects: data.annotations,
            beautification: data.beautification,
            next_id: data.next_id.max(highest).max(1),
            extensions: data.extensions,
        };
        document.renumber_counters();
        Ok(document)
    }

    /// The editable part of this document, ready to persist.
    #[must_use]
    pub fn data(&self) -> DocumentData {
        DocumentData {
            version: DocumentData::VERSION,
            annotations: self.objects.clone(),
            beautification: self.beautification.clone(),
            next_id: self.next_id,
            extensions: self.extensions.clone(),
        }
    }

    /// Every annotation, bottom-most first.
    #[must_use]
    pub fn annotations(&self) -> &[AnnotationObject] {
        &self.objects
    }

    /// How many annotations the document holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Whether the document has no annotations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// The source image's size in logical points.
    ///
    /// This, not the pixel size, is the space annotations are authored in.
    #[must_use]
    pub fn logical_size(&self) -> LogicalSize {
        let scale = self.source.frame.scale.get();
        LogicalSize::new(
            self.source.frame.size.width / scale,
            self.source.frame.size.height / scale,
        )
    }

    /// Logical size after optional framing and aspect expansion.
    #[must_use]
    pub fn output_logical_size(&self) -> LogicalSize {
        self.beautification.as_ref().map_or_else(
            || self.logical_size(),
            |beautification| {
                beautification.output_size.map_or_else(
                    || beautification.output_size(self.logical_size()),
                    |size| {
                        LogicalSize::new(
                            f64::from(size.width) / self.source.frame.scale.get(),
                            f64::from(size.height) / self.source.frame.scale.get(),
                        )
                    },
                )
            },
        )
    }

    /// The whole source image as a logical rectangle.
    #[must_use]
    pub fn logical_bounds(&self) -> LogicalRect {
        LogicalRect::new(LogicalPoint::new(0.0, 0.0), self.logical_size())
    }

    /// Adds an annotation on top of everything else.
    ///
    /// Counter markers are numbered by the document, so the `index` on a
    /// [`Annotation::Counter`] passed in here is ignored and replaced.
    pub fn add(&mut self, annotation: Annotation, style: Style) -> AnnotationId {
        let id = AnnotationId(self.next_id);
        self.next_id += 1;
        self.objects
            .push(AnnotationObject::new(id, annotation, style));
        self.renumber_counters();
        id
    }

    /// Adds an annotation with the default style for its kind.
    pub fn add_default(&mut self, annotation: Annotation) -> AnnotationId {
        let style = match &annotation {
            Annotation::Highlight(_) => Style::highlighter(),
            Annotation::Redact { .. } => Style::redaction(),
            _ => Style::stroked(),
        };
        self.add(annotation, style)
    }

    /// Removes an annotation, renumbering counters to close the gap.
    pub fn remove(&mut self, id: AnnotationId) -> Option<AnnotationObject> {
        let index = self.index_of(id)?;
        let removed = self.objects.remove(index);
        self.renumber_counters();
        Some(removed)
    }

    /// Removes every annotation, leaving the source untouched.
    pub fn clear(&mut self) {
        self.objects.clear();
    }

    /// Looks up an annotation.
    #[must_use]
    pub fn get(&self, id: AnnotationId) -> Option<&AnnotationObject> {
        self.objects.iter().find(|o| o.id == id)
    }

    /// Looks up an annotation for editing.
    ///
    /// Counter numbering is re-derived after any edit made through this handle,
    /// so a caller cannot leave the sequence inconsistent.
    pub fn get_mut(&mut self, id: AnnotationId) -> Option<AnnotationMut<'_>> {
        let index = self.index_of(id)?;
        Some(AnnotationMut {
            document: self,
            index,
        })
    }

    /// Replaces one annotation's style.
    pub fn set_style(&mut self, id: AnnotationId, style: Style) -> bool {
        match self.index_of(id) {
            Some(index) => {
                self.objects[index].style = style;
                true
            }
            None => false,
        }
    }

    /// Moves an annotation by `dx`, `dy` logical points.
    pub fn translate(&mut self, id: AnnotationId, dx: f64, dy: f64) -> bool {
        match self.index_of(id) {
            Some(index) => {
                self.objects[index].annotation.translate(dx, dy);
                true
            }
            None => false,
        }
    }

    /// Reshapes an annotation to fill `bounds`.
    pub fn set_bounds(&mut self, id: AnnotationId, bounds: LogicalRect) -> bool {
        match self.index_of(id) {
            Some(index) => {
                self.objects[index].annotation.set_bounds(bounds);
                true
            }
            None => false,
        }
    }

    /// The top-most annotation under `point`, if any.
    ///
    /// Top-most is what a click means: the object the user can see at that
    /// position is the one they are pointing at.
    #[must_use]
    pub fn hit_test(&self, point: LogicalPoint) -> Option<AnnotationId> {
        self.objects
            .iter()
            .rev()
            .find(|o| o.hit(point))
            .map(|o| o.id)
    }

    /// Every annotation under `point`, top-most first.
    #[must_use]
    pub fn hit_test_all(&self, point: LogicalPoint) -> Vec<AnnotationId> {
        self.objects
            .iter()
            .rev()
            .filter(|o| o.hit(point))
            .map(|o| o.id)
            .collect()
    }

    /// Moves an annotation above every other.
    pub fn bring_to_front(&mut self, id: AnnotationId) -> bool {
        match self.index_of(id) {
            Some(index) => {
                let object = self.objects.remove(index);
                self.objects.push(object);
                true
            }
            None => false,
        }
    }

    /// Moves an annotation below every other.
    pub fn send_to_back(&mut self, id: AnnotationId) -> bool {
        match self.index_of(id) {
            Some(index) => {
                let object = self.objects.remove(index);
                self.objects.insert(0, object);
                true
            }
            None => false,
        }
    }

    /// Moves an annotation one step up in z-order.
    pub fn raise(&mut self, id: AnnotationId) -> bool {
        match self.index_of(id) {
            Some(index) if index + 1 < self.objects.len() => {
                self.objects.swap(index, index + 1);
                true
            }
            _ => false,
        }
    }

    /// Moves an annotation one step down in z-order.
    pub fn lower(&mut self, id: AnnotationId) -> bool {
        match self.index_of(id) {
            Some(index) if index > 0 => {
                self.objects.swap(index, index - 1);
                true
            }
            _ => false,
        }
    }

    /// The annotation's position in z-order, if it exists.
    #[must_use]
    pub fn z_index(&self, id: AnnotationId) -> Option<usize> {
        self.index_of(id)
    }

    /// Whether framing may be applied at all.
    ///
    /// Window captures may receive an outer canvas, but their source pixels are
    /// immutable under D9.
    #[must_use]
    pub fn may_beautify(&self) -> bool {
        true
    }

    /// Whether inset, synthetic corners, shadow, or border may touch the subject.
    #[must_use]
    pub fn may_style_subject(&self) -> bool {
        !self.source.provenance.forbids_compositing()
    }

    /// The framing currently applied, if any.
    #[must_use]
    pub fn beautification(&self) -> Option<&Beautification> {
        self.beautification.as_ref()
    }

    /// Applies or clears framing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when framing would crop, round, border,
    /// or re-shadow a window capture. An outer canvas remains permitted.
    pub fn set_beautification(&mut self, beautification: Option<Beautification>) -> Result<()> {
        if let Some(beautification) = &beautification {
            beautification.validate()?;
            Self::validate_provenance(beautification, &self.source)?;
        }
        self.beautification = beautification;
        Ok(())
    }

    fn validate_provenance(beautification: &Beautification, source: &Capture) -> Result<()> {
        if source.provenance.forbids_compositing() && !beautification.preserves_subject_pixels() {
            return Err(Error::InvalidRequest(
                "window Smart Frame may add only an outer canvas; inset, corners, shadow, and border \
                 are disabled to preserve native pixels (decision D9)"
                    .to_owned(),
            ));
        }
        if source.provenance.forbids_compositing()
            && let Some(size) = beautification.output_size
        {
            let padding = (beautification.padding * source.frame.scale.get()).ceil() as u32;
            let minimum_width = source
                .frame
                .width()
                .saturating_add(padding.saturating_mul(2));
            let minimum_height = source
                .frame
                .height()
                .saturating_add(padding.saturating_mul(2));
            if size.width < minimum_width || size.height < minimum_height {
                return Err(Error::InvalidRequest(format!(
                    "exact output {}x{} is too small for the native {}x{} window plus padding",
                    size.width,
                    size.height,
                    source.frame.width(),
                    source.frame.height()
                )));
            }
        }
        Ok(())
    }

    /// The highest number currently assigned to a counter marker.
    #[must_use]
    pub fn counter_count(&self) -> u32 {
        self.objects
            .iter()
            .filter(|o| matches!(o.annotation, Annotation::Counter { .. }))
            .count() as u32
    }

    fn index_of(&self, id: AnnotationId) -> Option<usize> {
        self.objects.iter().position(|o| o.id == id)
    }

    /// Renumbers counter markers 1..n in creation order.
    ///
    /// Creation order, not z-order: raising a marker to the front must not
    /// silently renumber the whole sequence, and identifiers are handed out
    /// monotonically, so sorting by id recovers the order they were drawn in.
    fn renumber_counters(&mut self) {
        let mut counters: Vec<(AnnotationId, usize)> = self
            .objects
            .iter()
            .enumerate()
            .filter(|(_, o)| matches!(o.annotation, Annotation::Counter { .. }))
            .map(|(i, o)| (o.id, i))
            .collect();
        counters.sort_unstable_by_key(|(id, _)| *id);
        for (number, (_, index)) in counters.into_iter().enumerate() {
            if let Annotation::Counter { index: n, .. } = &mut self.objects[index].annotation {
                *n = number as u32 + 1;
            }
        }
    }
}

/// A borrowed, invariant-preserving handle to one annotation.
///
/// Dropping it re-derives counter numbering, so an edit that turns something
/// into (or away from) a counter cannot leave a gap in the sequence.
#[derive(Debug)]
pub struct AnnotationMut<'a> {
    document: &'a mut Document,
    index: usize,
}

impl AnnotationMut<'_> {
    /// The annotation being edited.
    #[must_use]
    pub fn object(&mut self) -> &mut AnnotationObject {
        &mut self.document.objects[self.index]
    }

    /// The geometry being edited.
    #[must_use]
    pub fn annotation(&mut self) -> &mut Annotation {
        &mut self.document.objects[self.index].annotation
    }

    /// The style being edited.
    #[must_use]
    pub fn style(&mut self) -> &mut Style {
        &mut self.document.objects[self.index].style
    }
}

impl Drop for AnnotationMut<'_> {
    fn drop(&mut self) {
        self.document.renumber_counters();
    }
}
