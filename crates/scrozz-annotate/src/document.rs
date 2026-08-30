//! The document: a capture plus every edit ever made to it.

use std::{collections::BTreeMap, sync::Arc};

use scrozz_core::{
    Capture, ColorSpace, ContentRevision, Error, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Result, ScaleFactor,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, decode, to_straight_rgba8};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::SerializeStruct,
};

use crate::{
    annotation::{Annotation, AnnotationId, AnnotationObject, RedactStyle},
    geom,
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

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let triple = u32::from(b0) << 16 | u32::from(b1) << 8 | u32::from(b2);
        out.push(BASE64_ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(BASE64_ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64_ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64_ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(text: &str) -> std::result::Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err("base64 text length must be a multiple of 4".to_owned());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let c0 = base64_value(chunk[0]).ok_or_else(|| {
            format!(
                "invalid base64 character {:?} at byte {}",
                chunk[0] as char,
                index * 4
            )
        })?;
        let c1 = base64_value(chunk[1]).ok_or_else(|| {
            format!(
                "invalid base64 character {:?} at byte {}",
                chunk[1] as char,
                index * 4 + 1
            )
        })?;
        let c2 = if chunk[2] == b'=' {
            64
        } else {
            base64_value(chunk[2]).ok_or_else(|| {
                format!(
                    "invalid base64 character {:?} at byte {}",
                    chunk[2] as char,
                    index * 4 + 2
                )
            })?
        };
        let c3 = if chunk[3] == b'=' {
            64
        } else {
            base64_value(chunk[3]).ok_or_else(|| {
                format!(
                    "invalid base64 character {:?} at byte {}",
                    chunk[3] as char,
                    index * 4 + 3
                )
            })?
        };
        if c2 == 64 && c3 != 64 {
            return Err("base64 padding is malformed".to_owned());
        }
        let triple = u32::from(c0) << 18
            | u32::from(c1) << 12
            | u32::from(c2.min(63)) << 6
            | u32::from(c3.min(63));
        out.push(((triple >> 16) & 0xFF) as u8);
        if c2 != 64 {
            out.push(((triple >> 8) & 0xFF) as u8);
        }
        if c3 != 64 {
            out.push((triple & 0xFF) as u8);
        }
        if (c2 == 64 || c3 == 64) && index + 1 != bytes.len() / 4 {
            return Err("base64 padding may appear only in the final quartet".to_owned());
        }
    }
    Ok(out)
}

const fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

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
    /// Largest accepted encoded source file for a Scene image.
    pub const MAX_INPUT_BYTES: u64 = MAX_ENCODED_BACKGROUND_BYTES;

    /// Validates image dimensions before a decoder allocates the raster.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when an edge or pixel count exceeds the
    /// Scene background limits.
    pub fn validate_dimensions(width: u32, height: u32) -> Result<()> {
        validate_background_area(width, height).map(|_| ())
    }

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
        state.serialize_field("pixels_png", &base64_encode(&self.encoded_png))?;
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
                let png = base64_decode(&encoded).map_err(D::Error::custom)?;
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
    /// A desktop image, retained separately from an ordinary imported image.
    Desktop(BackgroundImage),
    /// A locally generated, softened copy of the current capture.
    BlurredSource {
        /// Blur radius in output pixels before scale is applied.
        blur_radius: u16,
        /// Colour mixed over the blurred pixels for legibility.
        tint: Color,
    },
}

/// The four intentionally bounded generated-background directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeneratedStyle {
    /// Capture-aware colour with restrained contrast.
    #[default]
    Balanced,
    /// Low-saturation, low-contrast treatment.
    Soft,
    /// Stronger colour separation without random effects.
    Vibrant,
    /// Desaturated studio treatment.
    Neutral,
}

/// Art-directed renderer used for a capture-derived background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeneratedTemplate {
    /// One reliable, smooth directional field.
    SmoothGradient,
    /// Layered translucent fields that read as a soft mesh.
    #[default]
    SoftMesh,
    /// A restrained studio field with a central tonal lift.
    TonalStudio,
}

/// Resolved automatic background inputs and output colours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    /// User-selected generated direction.
    pub style: GeneratedStyle,
    /// Art-directed template selected for that direction.
    pub template: GeneratedTemplate,
    /// Stable seed controlling template placement, never unconstrained randomness.
    pub seed: u64,
    /// Resolved capture-derived palette used by the canonical renderer.
    pub palette: Vec<Color>,
    /// Unknown fields survive read-modify-write cycles.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(default)]
struct AutomaticBackgroundWire {
    algorithm_version: u16,
    start: Color,
    end: Color,
    edge_reference: Color,
    source_color_space: ColorSpace,
    minimum_contrast_x100: u16,
    style: GeneratedStyle,
    template: Option<GeneratedTemplate>,
    seed: Option<u64>,
    palette: Option<Vec<Color>>,
    #[serde(flatten)]
    extensions: BTreeMap<String, serde_json::Value>,
}

impl Default for AutomaticBackgroundWire {
    fn default() -> Self {
        let fallback = AutomaticBackground::default();
        Self {
            algorithm_version: fallback.algorithm_version,
            start: fallback.start,
            end: fallback.end,
            edge_reference: fallback.edge_reference,
            source_color_space: fallback.source_color_space,
            minimum_contrast_x100: fallback.minimum_contrast_x100,
            style: fallback.style,
            template: None,
            seed: None,
            palette: None,
            extensions: BTreeMap::new(),
        }
    }
}

impl<'de> Deserialize<'de> for AutomaticBackground {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = AutomaticBackgroundWire::deserialize(deserializer)?;
        let palette = wire
            .palette
            .unwrap_or_else(|| vec![wire.start, wire.end, wire.start, wire.end]);
        Ok(Self {
            algorithm_version: wire.algorithm_version,
            start: wire.start,
            end: wire.end,
            edge_reference: wire.edge_reference,
            source_color_space: wire.source_color_space,
            minimum_contrast_x100: wire.minimum_contrast_x100,
            style: wire.style,
            template: wire.template.unwrap_or(GeneratedTemplate::SmoothGradient),
            seed: wire.seed.unwrap_or_default(),
            palette,
            extensions: wire.extensions,
        })
    }
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
            style: GeneratedStyle::Balanced,
            template: GeneratedTemplate::SoftMesh,
            seed: 0x5343_524f_5a5a,
            palette: vec![
                Color::rgb(34, 40, 55),
                Color::rgb(62, 74, 103),
                Color::rgb(102, 83, 146),
                Color::rgb(42, 88, 110),
            ],
            extensions: BTreeMap::new(),
        }
    }

    /// Mean luminance of the two resolved stops.
    #[must_use]
    pub fn average_luminance(&self) -> f32 {
        (self.start.luminance() + self.end.luminance()) / 2.0
    }

    /// Four resolved colours, with backward-compatible derivation for old data.
    #[must_use]
    pub fn resolved_palette(&self) -> [Color; 4] {
        [
            self.palette.first().copied().unwrap_or(self.start),
            self.palette.get(1).copied().unwrap_or(self.end),
            self.palette.get(2).copied().unwrap_or(self.start),
            self.palette.get(3).copied().unwrap_or(self.end),
        ]
    }

    /// Applies one of the four deterministic art directions to this resolved palette.
    #[must_use]
    pub fn restyled(mut self, style: GeneratedStyle) -> Self {
        self.style = style;
        self.template = match style {
            GeneratedStyle::Balanced | GeneratedStyle::Vibrant => GeneratedTemplate::SoftMesh,
            GeneratedStyle::Soft => GeneratedTemplate::SmoothGradient,
            GeneratedStyle::Neutral => GeneratedTemplate::TonalStudio,
        };
        let base = [
            self.start,
            self.end,
            mix_scene_color(self.start, self.edge_reference, 46),
            mix_scene_color(self.end, self.edge_reference, 34),
        ];
        let neutral = Color::rgb(112, 116, 124);
        self.palette = base
            .into_iter()
            .map(|color| match style {
                GeneratedStyle::Balanced => color,
                GeneratedStyle::Soft => mix_scene_color(color, Color::WHITE, 34),
                GeneratedStyle::Vibrant => scene_saturate(color),
                GeneratedStyle::Neutral => mix_scene_color(color, neutral, 72),
            })
            .collect();
        self
    }
}

fn mix_scene_color(left: Color, right: Color, right_weight: u16) -> Color {
    let right_weight = right_weight.min(255);
    let left_weight = 255 - right_weight;
    let channel = |left: u8, right: u8| {
        ((u16::from(left) * left_weight + u16::from(right) * right_weight + 127) / 255) as u8
    };
    Color::rgba(
        channel(left.r, right.r),
        channel(left.g, right.g),
        channel(left.b, right.b),
        channel(left.a, right.a),
    )
}

fn scene_saturate(color: Color) -> Color {
    let maximum = color.r.max(color.g).max(color.b);
    let minimum = color.r.min(color.g).min(color.b);
    let midpoint = ((u16::from(maximum) + u16::from(minimum)) / 2) as u8;
    let channel = |value: u8| {
        let delta = i16::from(value) - i16::from(midpoint);
        (i16::from(midpoint) + delta * 3 / 2).clamp(0, 255) as u8
    };
    Color::rgba(
        channel(color.r),
        channel(color.g),
        channel(color.b),
        color.a,
    )
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
    /// The largest share of one axis a single edge may ever remove.
    ///
    /// The inner inset exists to centre real content, not to become a second
    /// crop tool: a quarter of an axis per edge still leaves half the capture
    /// under the most aggressive setting the inspector can express, and the
    /// automatic detector is far more conservative than that again.
    pub const MAX_EDGE_FRACTION: f64 = 0.25;

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

    /// The largest inset one edge may take on an axis of `extent` points.
    #[must_use]
    pub fn limit_for(extent: f64) -> f64 {
        if extent.is_finite() && extent > 0.0 {
            (extent * Self::MAX_EDGE_FRACTION).max(0.0)
        } else {
            0.0
        }
    }

    /// This inset clamped into the bounds `size` allows, with NaN treated as
    /// zero.
    ///
    /// Every path that stores an inset goes through here, so a hand-edited
    /// sidecar, a preset authored against a larger capture, or a slider driven
    /// to its end all land on the same conservative envelope.
    #[must_use]
    pub fn clamped_within(self, size: LogicalSize) -> Self {
        let horizontal = Self::limit_for(size.width);
        let vertical = Self::limit_for(size.height);
        let clamp = |value: f64, limit: f64| {
            if value.is_finite() {
                value.clamp(0.0, limit)
            } else {
                0.0
            }
        };
        Self {
            left: clamp(self.left, horizontal),
            top: clamp(self.top, vertical),
            right: clamp(self.right, horizontal),
            bottom: clamp(self.bottom, vertical),
        }
    }

    /// The largest single-edge inset, in logical points.
    #[must_use]
    pub fn largest(self) -> f64 {
        [self.left, self.top, self.right, self.bottom]
            .into_iter()
            .filter(|value| value.is_finite())
            .fold(0.0_f64, f64::max)
    }
}

/// Per-edge space between the subject and the Scene canvas.
///
/// `Beautification::padding` remains the portable uniform value. This optional
/// resolved form lets scrolling captures and future outward crop expansion share
/// the same canvas/background system without inventing a second fill model.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CanvasInsets {
    /// Left canvas inset.
    pub left: f64,
    /// Top canvas inset.
    pub top: f64,
    /// Right canvas inset.
    pub right: f64,
    /// Bottom canvas inset.
    pub bottom: f64,
}

impl CanvasInsets {
    /// Equal spacing on every edge.
    #[must_use]
    pub const fn uniform(value: f64) -> Self {
        Self {
            left: value,
            top: value,
            right: value,
            bottom: value,
        }
    }

    /// Different horizontal and vertical spacing.
    #[must_use]
    pub const fn symmetric(horizontal: f64, vertical: f64) -> Self {
        Self {
            left: horizontal,
            top: vertical,
            right: horizontal,
            bottom: vertical,
        }
    }

    /// Whether this adds no canvas space.
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

/// One Scene property whose resolved value may remain automatic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SceneProperty {
    /// Capture-derived or curated background.
    Background,
    /// Inner content inset around the meaningful part of the capture.
    Inset,
    /// Canvas padding.
    Padding,
    /// Optical placement.
    Placement,
    /// Subject corner treatment.
    Corners,
    /// Subject shadow treatment.
    Shadow,
    /// Output pixel size. Aspect ratio is intentionally never automatic.
    OutputSize,
}

/// Per-property automation retained in editable Scene state and presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SceneAutomatic {
    /// Resolve the background from capture colours.
    pub background: bool,
    /// Resolve the inner content inset from the capture's own margins.
    ///
    /// Absent from a sidecar or preset written before the inner inset existed,
    /// where `serde`'s `false` is exactly right: those Scenes were authored
    /// against a fixed, zero inset and must keep rendering that way.
    pub inset: bool,
    /// Resolve proportional canvas padding.
    pub padding: bool,
    /// Resolve subtle confidence-gated optical placement.
    pub placement: bool,
    /// Resolve subject corners when the capture is not a native window.
    pub corners: bool,
    /// Resolve subject shadow when the capture is not a native window.
    pub shadow: bool,
    /// Resolve output size from subject and padding.
    pub output_size: bool,
}

impl SceneAutomatic {
    /// All automatic properties that can apply to an ordinary capture.
    #[must_use]
    pub const fn ordinary() -> Self {
        Self {
            background: true,
            inset: true,
            padding: true,
            placement: true,
            corners: true,
            shadow: true,
            output_size: true,
        }
    }

    /// Automatic properties available while preserving native window appearance.
    #[must_use]
    pub const fn native_window() -> Self {
        Self {
            // D9: a window's own silhouette, transparent corners and shadow are
            // the OS's pixels. Nothing may trim them, so the inner inset is not
            // merely fixed at zero here — it is never resolved at all.
            inset: false,
            corners: false,
            shadow: false,
            ..Self::ordinary()
        }
    }

    /// Marks one resolved value as fixed after the user edits it.
    pub fn disable(&mut self, property: SceneProperty) {
        match property {
            SceneProperty::Background => self.background = false,
            SceneProperty::Inset => self.inset = false,
            SceneProperty::Padding => self.padding = false,
            SceneProperty::Placement => self.placement = false,
            SceneProperty::Corners => self.corners = false,
            SceneProperty::Shadow => self.shadow = false,
            SceneProperty::OutputSize => self.output_size = false,
        }
    }

    /// Whether any value will resolve again when used as a preset.
    #[must_use]
    pub const fn any(self) -> bool {
        self.background
            || self.inset
            || self.padding
            || self.placement
            || self.corners
            || self.shadow
            || self.output_size
    }
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
    /// Optional per-edge canvas padding, for asymmetric expansion.
    pub canvas_padding: Option<CanvasInsets>,
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
    /// Properties that should resolve from each capture when this is a preset.
    pub automatic: SceneAutomatic,
    /// Unknown fields survive read-modify-write cycles.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl Default for Beautification {
    fn default() -> Self {
        Self {
            padding: 0.0,
            canvas_padding: None,
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
            automatic: SceneAutomatic::ordinary(),
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
            canvas_padding: None,
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
            automatic: SceneAutomatic::ordinary(),
            extensions: BTreeMap::new(),
        }
    }

    /// One of Scrozz's named starting points.
    #[must_use]
    pub const fn preset(preset: BeautificationPreset) -> Self {
        match preset {
            BeautificationPreset::Clean => Self {
                padding: 40.0,
                canvas_padding: None,
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
                automatic: SceneAutomatic::ordinary(),
                extensions: BTreeMap::new(),
            },
            BeautificationPreset::Social => Self {
                padding: 64.0,
                canvas_padding: None,
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
                automatic: SceneAutomatic::ordinary(),
                extensions: BTreeMap::new(),
            },
            BeautificationPreset::Story => Self {
                padding: 72.0,
                canvas_padding: None,
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
                automatic: SceneAutomatic::ordinary(),
                extensions: BTreeMap::new(),
            },
            BeautificationPreset::Editorial => Self {
                padding: 56.0,
                canvas_padding: None,
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
                automatic: SceneAutomatic::ordinary(),
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
        self.resolved_padding().is_zero()
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
            (
                "left canvas padding",
                self.canvas_padding.unwrap_or_default().left,
            ),
            (
                "top canvas padding",
                self.canvas_padding.unwrap_or_default().top,
            ),
            (
                "right canvas padding",
                self.canvas_padding.unwrap_or_default().right,
            ),
            (
                "bottom canvas padding",
                self.canvas_padding.unwrap_or_default().bottom,
            ),
        ] {
            if !value.is_finite() || !(0.0..=Self::MAX_MEASUREMENT).contains(&value) {
                return Err(Error::InvalidRequest(format!(
                    "beautification {name} must be between 0 and {}, got {value}",
                    Self::MAX_MEASUREMENT
                )));
            }
        }
        if let Background::Image(image) | Background::Desktop(image) = &self.background {
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

    /// Stops resolved visual focus from being reused after source geometry changes.
    pub fn invalidate_resolved_focus(&mut self) {
        if let Some(metadata) = &mut self.smart_frame {
            metadata.focus.confidence = 0;
        }
    }

    /// Logical output size before rasterisation.
    ///
    /// Aspect presets only add canvas; they never crop or scale the capture.
    #[must_use]
    pub fn output_size(&self, content: LogicalSize) -> LogicalSize {
        let content_width = (content.width - self.inset.left - self.inset.right).max(1.0);
        let content_height = (content.height - self.inset.top - self.inset.bottom).max(1.0);
        let padding = self.resolved_padding();
        let base_width = content_width + padding.left + padding.right;
        let base_height = content_height + padding.top + padding.bottom;
        let Some(ratio) = self.aspect.ratio() else {
            return LogicalSize::new(base_width, base_height);
        };
        if base_width / base_height < ratio {
            LogicalSize::new(base_height * ratio, base_height)
        } else {
            LogicalSize::new(base_width, base_width / ratio)
        }
    }

    /// Logical Scene size with an exact-output request resolved at `source_scale`.
    ///
    /// Exact dimensions are minimums. When they cannot contain the untouched
    /// source plus canvas spacing, the canvas grows at the requested ratio.
    #[must_use]
    pub fn output_size_at_scale(&self, content: LogicalSize, source_scale: f64) -> LogicalSize {
        let Some(exact) = self.output_size else {
            return self.output_size(content);
        };
        let content_width = (content.width - self.inset.left - self.inset.right).max(1.0);
        let content_height = (content.height - self.inset.top - self.inset.bottom).max(1.0);
        let padding = self.resolved_padding();
        let minimum = LogicalSize::new(
            content_width + padding.left + padding.right,
            content_height + padding.top + padding.bottom,
        );
        let ratio = exact.ratio();
        let requested_width = f64::from(exact.width) / source_scale;
        let requested_height = f64::from(exact.height) / source_scale;
        let width = requested_width
            .max(requested_height * ratio)
            .max(minimum.width)
            .max(minimum.height * ratio);
        LogicalSize::new(width, width / ratio)
    }

    /// Resolved per-edge canvas spacing.
    #[must_use]
    pub fn resolved_padding(&self) -> CanvasInsets {
        self.canvas_padding
            .unwrap_or_else(|| CanvasInsets::uniform(self.padding))
    }

    /// Changes uniform padding and clears an old asymmetric override.
    pub fn set_uniform_padding(&mut self, padding: f64) {
        self.padding = padding;
        self.canvas_padding = None;
    }

    /// Converts one automatic resolved value into a fixed value.
    pub fn fix(&mut self, property: SceneProperty) {
        self.automatic.disable(property);
    }
}

/// Canonical product name for nondestructive presentation state.
pub type Scene = Beautification;

/// Canonical product name for the built-in Scene starting points.
pub type ScenePreset = BeautificationPreset;

/// Whether Scene may style the subject or must preserve native appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubjectAppearance {
    /// Corners and shadow are editable because the capture is an ordinary raster.
    Editable,
    /// The capture already contains the platform's true silhouette and shadow.
    Native,
}

/// Non-destructive orientation of source pixels after crop and before framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageOrientation {
    /// Source pixels appear unchanged.
    #[default]
    Identity,
    /// A clockwise quarter turn.
    RotateRight,
    /// A half turn.
    Rotate180,
    /// A counter-clockwise quarter turn.
    RotateLeft,
    /// Reflection across the vertical axis.
    FlipHorizontal,
    /// Reflection across the horizontal axis.
    FlipVertical,
    /// Reflection across the top-left to bottom-right diagonal.
    Transpose,
    /// Reflection across the top-right to bottom-left diagonal.
    Transverse,
}

impl ImageOrientation {
    /// Applies `operation` in the currently displayed coordinate system.
    #[must_use]
    pub fn then(self, operation: Self) -> Self {
        let left = operation.matrix();
        let right = self.matrix();
        Self::from_matrix([
            left[0] * right[0] + left[1] * right[2],
            left[0] * right[1] + left[1] * right[3],
            left[2] * right[0] + left[3] * right[2],
            left[2] * right[1] + left[3] * right[3],
        ])
    }

    /// Whether width and height exchange places.
    #[must_use]
    pub const fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::RotateRight | Self::RotateLeft | Self::Transpose | Self::Transverse
        )
    }

    /// Size after applying this orientation.
    #[must_use]
    pub fn apply_size(self, size: LogicalSize) -> LogicalSize {
        if self.swaps_axes() {
            LogicalSize::new(size.height, size.width)
        } else {
            size
        }
    }

    /// Maps a source-space point into displayed full-image coordinates.
    #[must_use]
    pub fn apply_point(self, point: LogicalPoint, source: LogicalSize) -> LogicalPoint {
        match self {
            Self::Identity => point,
            Self::RotateRight => LogicalPoint::new(source.height - point.y, point.x),
            Self::Rotate180 => LogicalPoint::new(source.width - point.x, source.height - point.y),
            Self::RotateLeft => LogicalPoint::new(point.y, source.width - point.x),
            Self::FlipHorizontal => LogicalPoint::new(source.width - point.x, point.y),
            Self::FlipVertical => LogicalPoint::new(point.x, source.height - point.y),
            Self::Transpose => LogicalPoint::new(point.y, point.x),
            Self::Transverse => LogicalPoint::new(source.height - point.y, source.width - point.x),
        }
    }

    /// Maps a displayed full-image point back to source space.
    #[must_use]
    pub fn invert_point(self, point: LogicalPoint, source: LogicalSize) -> LogicalPoint {
        self.inverse().apply_point(point, self.apply_size(source))
    }

    /// Maps a source-space axis-aligned rectangle into displayed coordinates.
    #[must_use]
    pub fn apply_rect(self, rect: LogicalRect, source: LogicalSize) -> LogicalRect {
        let right = geom::max_x(&rect);
        let bottom = geom::max_y(&rect);
        let points = [
            rect.origin,
            LogicalPoint::new(right, rect.origin.y),
            LogicalPoint::new(rect.origin.x, bottom),
            LogicalPoint::new(right, bottom),
        ]
        .map(|point| self.apply_point(point, source));
        let left = points
            .iter()
            .map(|point| point.x)
            .fold(f64::INFINITY, f64::min);
        let top = points
            .iter()
            .map(|point| point.y)
            .fold(f64::INFINITY, f64::min);
        let right = points
            .iter()
            .map(|point| point.x)
            .fold(f64::NEG_INFINITY, f64::max);
        let bottom = points
            .iter()
            .map(|point| point.y)
            .fold(f64::NEG_INFINITY, f64::max);
        geom::from_edges(left, top, right, bottom)
    }

    /// Maps a displayed axis-aligned rectangle back to source space.
    #[must_use]
    pub fn invert_rect(self, rect: LogicalRect, source: LogicalSize) -> LogicalRect {
        self.inverse().apply_rect(rect, self.apply_size(source))
    }

    /// Maps an axis-aligned direction through this orientation.
    #[must_use]
    pub const fn apply_vector(self, vector: (i8, i8)) -> (i8, i8) {
        let matrix = self.matrix();
        (
            matrix[0] * vector.0 + matrix[1] * vector.1,
            matrix[2] * vector.0 + matrix[3] * vector.1,
        )
    }

    const fn inverse(self) -> Self {
        match self {
            Self::RotateRight => Self::RotateLeft,
            Self::RotateLeft => Self::RotateRight,
            other => other,
        }
    }

    const fn matrix(self) -> [i8; 4] {
        match self {
            Self::Identity => [1, 0, 0, 1],
            Self::RotateRight => [0, -1, 1, 0],
            Self::Rotate180 => [-1, 0, 0, -1],
            Self::RotateLeft => [0, 1, -1, 0],
            Self::FlipHorizontal => [-1, 0, 0, 1],
            Self::FlipVertical => [1, 0, 0, -1],
            Self::Transpose => [0, 1, 1, 0],
            Self::Transverse => [0, -1, -1, 0],
        }
    }

    const fn from_matrix(matrix: [i8; 4]) -> Self {
        match matrix {
            [1, 0, 0, 1] => Self::Identity,
            [0, -1, 1, 0] => Self::RotateRight,
            [-1, 0, 0, -1] => Self::Rotate180,
            [0, 1, -1, 0] => Self::RotateLeft,
            [-1, 0, 0, 1] => Self::FlipHorizontal,
            [1, 0, 0, -1] => Self::FlipVertical,
            [0, 1, 1, 0] => Self::Transpose,
            [0, -1, -1, 0] => Self::Transverse,
            _ => unreachable!(),
        }
    }
}

/// Margin requested outside the captured source.
///
/// Crop itself deliberately owns no fill. A non-zero value is a hand-off to
/// Scene, which can expand its existing canvas/background model and then apply
/// `source_crop`; introducing a second crop-only background would make export
/// semantics depend on which surface created the margin.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CropExpansion {
    /// Requested margin left of the source.
    pub left: f64,
    /// Requested margin above the source.
    pub top: f64,
    /// Requested margin right of the source.
    pub right: f64,
    /// Requested margin below the source.
    pub bottom: f64,
}

impl CropExpansion {
    /// Whether Scene must provide any canvas.
    #[must_use]
    pub fn is_zero(self) -> bool {
        [self.left, self.top, self.right, self.bottom]
            .into_iter()
            .all(|value| value <= 0.0)
    }

    /// Maps source-side margins into their displayed sides.
    #[must_use]
    pub const fn apply_orientation(self, orientation: ImageOrientation) -> Self {
        let [left, top, right, bottom] = match orientation {
            ImageOrientation::Identity => [self.left, self.top, self.right, self.bottom],
            ImageOrientation::RotateRight => [self.bottom, self.left, self.top, self.right],
            ImageOrientation::Rotate180 => [self.right, self.bottom, self.left, self.top],
            ImageOrientation::RotateLeft => [self.top, self.right, self.bottom, self.left],
            ImageOrientation::FlipHorizontal => [self.right, self.top, self.left, self.bottom],
            ImageOrientation::FlipVertical => [self.left, self.bottom, self.right, self.top],
            ImageOrientation::Transpose => [self.top, self.left, self.bottom, self.right],
            ImageOrientation::Transverse => [self.bottom, self.right, self.top, self.left],
        };
        Self {
            left,
            top,
            right,
            bottom,
        }
    }
}

/// Validated split between source trimming and Scene-owned outward canvas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropResolution {
    /// Source-space crop after intersecting the request with the capture.
    pub source_crop: Option<LogicalRect>,
    /// Asymmetric canvas Scene must add around that source result.
    pub expansion: CropExpansion,
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
    /// The visible region of the source, if it has been cropped.
    ///
    /// In source-logical coordinates, and never applied to the pixels: the
    /// source keeps every pixel it was captured with, so a crop can be widened
    /// again — or cleared entirely — months later. Annotations outside it are
    /// kept too, and simply fall outside the rendered area.
    #[serde(default)]
    pub crop: Option<LogicalRect>,
    /// Non-destructive orientation applied after source crop and before framing.
    #[serde(default)]
    pub orientation: ImageOrientation,
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
    pub const VERSION: u32 = 6;
}

impl Default for DocumentData {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            annotations: Vec::new(),
            beautification: None,
            crop: None,
            orientation: ImageOrientation::Identity,
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
    source: Capture,
    objects: Vec<AnnotationObject>,
    beautification: Option<Beautification>,
    crop: Option<LogicalRect>,
    orientation: ImageOrientation,
    next_id: u64,
    extensions: BTreeMap<String, serde_json::Value>,
    revision: ContentRevision,
}

impl Document {
    /// Wraps a fresh capture in an empty document.
    #[must_use]
    pub fn new(source: Capture) -> Self {
        Self {
            source,
            objects: Vec::new(),
            beautification: None,
            crop: None,
            orientation: ImageOrientation::Identity,
            next_id: 1,
            extensions: BTreeMap::new(),
            revision: ContentRevision::fresh(),
        }
    }

    /// Rebuilds a document from a capture and its persisted edits.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the data is from a newer format
    /// version, malformed framing, or window framing that violates D9.
    pub fn from_data(source: Capture, mut data: DocumentData) -> Result<Self> {
        Self::validate_data(&source, &data)?;
        normalize_redaction_styles(&mut data.annotations);
        let crop = normalize_crop(capture_bounds(&source), data.crop)?;
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
            crop,
            orientation: data.orientation,
            next_id: data.next_id.max(highest).max(1),
            extensions: data.extensions,
            revision: ContentRevision::fresh(),
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
            crop: self.crop,
            orientation: self.orientation,
            next_id: self.next_id,
            extensions: self.extensions.clone(),
        }
    }

    /// Replaces every editable part of this document at once.
    ///
    /// The source is untouched — a snapshot only ever travels between states of
    /// the same document, so restoring one must not be able to swap the image
    /// out from under it.
    ///
    /// # Errors
    ///
    /// The same conditions as [`Self::from_data`].
    pub fn restore(&mut self, mut data: DocumentData) -> Result<()> {
        Self::validate_data(&self.source, &data)?;
        normalize_redaction_styles(&mut data.annotations);
        let crop = normalize_crop(self.logical_bounds(), data.crop)?;
        let highest = data
            .annotations
            .iter()
            .map(|object| object.id.0)
            .max()
            .map_or(0, |id| id + 1);
        self.objects = data.annotations;
        self.beautification = data.beautification;
        self.crop = crop;
        self.orientation = data.orientation;
        self.next_id = data.next_id.max(highest).max(1);
        self.extensions = data.extensions;
        self.renumber_counters();
        self.touch();
        Ok(())
    }

    fn validate_data(source: &Capture, data: &DocumentData) -> Result<()> {
        if data.version > DocumentData::VERSION {
            return Err(Error::InvalidRequest(format!(
                "document format version {} is newer than supported version {}",
                data.version,
                DocumentData::VERSION
            )));
        }
        if let Some(beautification) = &data.beautification {
            beautification.validate()?;
            Self::validate_provenance(beautification, source)?;
        }
        Ok(())
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
            || self.content_size(),
            |beautification| {
                beautification
                    .output_size_at_scale(self.content_size(), self.source.frame.scale.get())
            },
        )
    }

    /// The whole source image as a logical rectangle.
    #[must_use]
    pub fn logical_bounds(&self) -> LogicalRect {
        LogicalRect::new(LogicalPoint::new(0.0, 0.0), self.logical_size())
    }

    /// The full source size after the non-destructive orientation.
    #[must_use]
    pub fn display_size(&self) -> LogicalSize {
        self.orientation.apply_size(self.logical_size())
    }

    /// The full oriented image as a displayed logical rectangle.
    #[must_use]
    pub fn display_bounds(&self) -> LogicalRect {
        LogicalRect::new(LogicalPoint::new(0.0, 0.0), self.display_size())
    }

    /// The immutable identity of the document's current editable content.
    #[must_use]
    pub const fn revision(&self) -> ContentRevision {
        self.revision
    }

    /// Untouched source capture.
    #[must_use]
    pub const fn source(&self) -> &Capture {
        &self.source
    }

    /// Replaces the source capture and invalidates every prior analysis result.
    ///
    /// # Errors
    ///
    /// Refuses a source whose provenance conflicts with current beautification.
    pub fn replace_source(&mut self, source: Capture) -> Result<()> {
        if let Some(beautification) = self.beautification.clone() {
            Self::validate_provenance(&beautification, &source)?;
        }
        let crop = normalize_crop(capture_bounds(&source), self.crop)?;
        self.source = source;
        self.crop = crop;
        if let Some(beautification) = &mut self.beautification {
            beautification.invalidate_resolved_focus();
        }
        self.touch();
        Ok(())
    }

    /// The crop, if the document has been cropped.
    #[must_use]
    pub fn crop(&self) -> Option<LogicalRect> {
        self.crop
    }

    /// Current non-destructive image orientation.
    #[must_use]
    pub const fn orientation(&self) -> ImageOrientation {
        self.orientation
    }

    /// Changes the image orientation without touching source pixels.
    pub fn set_orientation(&mut self, orientation: ImageOrientation) {
        if self.orientation != orientation {
            self.orientation = orientation;
            if let Some(beautification) = &mut self.beautification {
                beautification.invalidate_resolved_focus();
            }
            self.touch();
        }
    }

    /// Maps source coordinates to the currently displayed full image.
    #[must_use]
    pub fn source_to_display(&self, point: LogicalPoint) -> LogicalPoint {
        self.orientation.apply_point(point, self.logical_size())
    }

    /// Maps displayed full-image coordinates back to source coordinates.
    #[must_use]
    pub fn display_to_source(&self, point: LogicalPoint) -> LogicalPoint {
        self.orientation.invert_point(point, self.logical_size())
    }

    /// Maps a source rectangle into displayed full-image coordinates.
    #[must_use]
    pub fn source_rect_to_display(&self, rect: LogicalRect) -> LogicalRect {
        self.orientation.apply_rect(rect, self.logical_size())
    }

    /// The region that renders: the crop if there is one, else the whole image.
    #[must_use]
    pub fn content_bounds(&self) -> LogicalRect {
        self.crop.unwrap_or_else(|| self.logical_bounds())
    }

    /// Oriented location of the visible source region in full-image coordinates.
    #[must_use]
    pub fn display_content_bounds(&self) -> LogicalRect {
        self.source_rect_to_display(self.content_bounds())
    }

    /// The rendered size in logical points.
    #[must_use]
    pub fn content_size(&self) -> LogicalSize {
        self.orientation.apply_size(self.content_bounds().size)
    }

    /// Crops the document to `area`, or clears the crop with `None`.
    ///
    /// The rectangle is clamped to the source: a crop dragged past the edge
    /// trims to the edge rather than inventing transparent margin, which is
    /// what the drag gesture visibly promises.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the rectangle is not finite, or if
    /// clamping leaves it with no area — an empty crop would render a
    /// zero-pixel image, and silently ignoring the request would leave the
    /// editor showing a selection the document does not have.
    pub fn set_crop(&mut self, area: Option<LogicalRect>) -> Result<()> {
        let crop = normalize_crop(self.logical_bounds(), area)?;
        if self.crop != crop {
            self.crop = crop;
            if let Some(beautification) = &mut self.beautification {
                beautification.invalidate_resolved_focus();
            }
            self.touch();
        }
        Ok(())
    }

    /// Validates a crop request and separates trimming from outward expansion.
    ///
    /// `source_crop` remains in original source coordinates. Scene must rotate
    /// or flip the asymmetric `expansion` with [`Self::orientation`] when it
    /// materialises canvas, then apply the returned source crop. Crop does not
    /// choose a fill and does not mutate the document through this method.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a non-finite request or one that
    /// does not overlap the source.
    pub fn resolve_crop(&self, area: Option<LogicalRect>) -> Result<CropResolution> {
        resolve_crop(self.logical_bounds(), area)
    }

    /// Physical size of the source-trimming portion of `area`.
    ///
    /// Quantizes absolute edges exactly as the renderer does, rather than
    /// rounding width and height independently. Outward Scene expansion is not
    /// included.
    pub fn source_crop_pixel_size(&self, area: Option<LogicalRect>) -> Result<(u32, u32)> {
        let bounds = self.logical_bounds();
        let content = self.resolve_crop(area)?.source_crop.unwrap_or(bounds);
        let frame = &self.source.frame;
        let (left, top, right, bottom) = quantized_crop_edges(
            bounds,
            content,
            frame.scale,
            (frame.width(), frame.height()),
        );
        Ok((right - left, bottom - top))
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
        self.touch();
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
        self.touch();
        Some(removed)
    }

    /// Removes every annotation, leaving the source untouched.
    pub fn clear(&mut self) {
        if !self.objects.is_empty() {
            self.objects.clear();
            self.touch();
        }
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
                if self.objects[index].style != style {
                    self.objects[index].style = style;
                    self.touch();
                }
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
                self.touch();
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
                self.touch();
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
                self.touch();
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
                self.touch();
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
                self.touch();
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
                self.touch();
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

    /// Subject appearance contract exposed to the Scene inspector.
    #[must_use]
    pub fn subject_appearance(&self) -> SubjectAppearance {
        if self.may_style_subject() {
            SubjectAppearance::Editable
        } else {
            SubjectAppearance::Native
        }
    }

    /// Editable Scene state currently wrapped around the untouched source.
    #[must_use]
    pub fn scene(&self) -> Option<&Scene> {
        self.beautification()
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
        if self.beautification != beautification {
            self.beautification = beautification;
            self.touch();
        }
        Ok(())
    }

    /// Applies or removes the nondestructive Scene around the source pixels.
    ///
    /// A Scene has two independent spacings, and this is where the inner one is
    /// made safe. [`Beautification::inset`] is the *content* inset: the margin
    /// the capture already carries around its own subject, held back so the
    /// subject sits centred inside the Scene rather than adrift in whatever
    /// dead space the screenshot happened to include.
    /// [`Beautification::padding`] is the outer one, between that subject and
    /// the Scene background. Neither destroys anything: the complete source
    /// stays in the document, and clearing the inset restores it exactly.
    ///
    /// The inset is clamped to [`SourceInsets::MAX_EDGE_FRACTION`] of the
    /// content on each axis rather than rejected, so a preset authored against
    /// a larger capture, or a sidecar edited by hand, degrades to the nearest
    /// safe framing instead of refusing to open.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when a Scene would alter native window
    /// pixels or exceed render bounds.
    pub fn set_scene(&mut self, scene: Option<Scene>) -> Result<()> {
        let scene = scene.map(|mut scene| {
            scene.inset = scene.inset.clamped_within(self.content_size());
            scene
        });
        self.set_beautification(scene)
    }

    fn validate_provenance(beautification: &Beautification, source: &Capture) -> Result<()> {
        if source.provenance.forbids_compositing() && !beautification.preserves_subject_pixels() {
            return Err(Error::InvalidRequest(
                "window Smart Frame may add only an outer canvas; inset, corners, shadow, and border \
                 are disabled to preserve native pixels (decision D9)"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// Adds secure redaction annotations only when an analysis result still
    /// matches this exact document revision.
    ///
    /// The operation is all-or-nothing: every area is validated before the first
    /// annotation is created, so malformed or stale analysis cannot partially
    /// edit the document.
    pub fn add_redactions_at_revision<I>(
        &mut self,
        expected: ContentRevision,
        areas: I,
    ) -> Result<Vec<AnnotationId>>
    where
        I: IntoIterator<Item = LogicalRect>,
    {
        if self.revision != expected {
            return Err(Error::InvalidRequest(
                "the sensitive-information scan is stale; scan the current revision again"
                    .to_owned(),
            ));
        }
        let mut unique_areas = Vec::new();
        for area in areas {
            if !unique_areas.contains(&area) {
                unique_areas.push(area);
            }
        }
        let bounds = self.logical_bounds();
        if unique_areas
            .iter()
            .any(|area| !valid_redaction_area(*area, bounds))
        {
            return Err(Error::InvalidRequest(
                "a proposed sensitive-information redaction lies outside the source image"
                    .to_owned(),
            ));
        }
        Ok(unique_areas
            .into_iter()
            .map(|area| {
                self.add_default(Annotation::Redact {
                    area,
                    // Sensitive-information suggestions must destroy content
                    // independently of the original pixels. Blur and mosaic
                    // remain available as manual effects, not privacy claims.
                    style: RedactStyle::Solid,
                })
            })
            .collect())
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

    fn touch(&mut self) {
        self.revision = self.revision.next();
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

fn capture_bounds(source: &Capture) -> LogicalRect {
    let scale = source.frame.scale.get();
    LogicalRect::new(
        LogicalPoint::new(0.0, 0.0),
        LogicalSize::new(
            source.frame.size.width / scale,
            source.frame.size.height / scale,
        ),
    )
}

fn normalize_crop(bounds: LogicalRect, area: Option<LogicalRect>) -> Result<Option<LogicalRect>> {
    Ok(resolve_crop(bounds, area)?.source_crop)
}

fn resolve_crop(bounds: LogicalRect, area: Option<LogicalRect>) -> Result<CropResolution> {
    let Some(area) = area else {
        return Ok(CropResolution {
            source_crop: None,
            expansion: CropExpansion::default(),
        });
    };
    if ![
        area.origin.x,
        area.origin.y,
        area.size.width,
        area.size.height,
    ]
    .iter()
    .all(|value| value.is_finite())
    {
        return Err(Error::InvalidRequest(
            "crop rectangle must be finite".to_owned(),
        ));
    }

    let left = area.origin.x.max(bounds.origin.x);
    let top = area.origin.y.max(bounds.origin.y);
    let right = geom::max_x(&area).min(geom::max_x(&bounds));
    let bottom = geom::max_y(&area).min(geom::max_y(&bounds));
    if right - left <= 0.0 || bottom - top <= 0.0 {
        return Err(Error::InvalidRequest(
            "crop rectangle does not overlap the capture".to_owned(),
        ));
    }
    let clamped = geom::from_edges(left, top, right, bottom);
    Ok(CropResolution {
        source_crop: (clamped != bounds).then_some(clamped),
        expansion: CropExpansion {
            left: (bounds.origin.x - area.origin.x).max(0.0),
            top: (bounds.origin.y - area.origin.y).max(0.0),
            right: (geom::max_x(&area) - geom::max_x(&bounds)).max(0.0),
            bottom: (geom::max_y(&area) - geom::max_y(&bounds)).max(0.0),
        },
    })
}

/// Renderer-compatible absolute-edge quantization.
///
/// Returns `(left, top, right, bottom)` in physical pixels, with at least one
/// pixel on each axis.
pub(crate) fn quantized_crop_edges(
    bounds: LogicalRect,
    content: LogicalRect,
    scale: ScaleFactor,
    canvas: (u32, u32),
) -> (u32, u32, u32, u32) {
    let quantize = |start: f64, end: f64, origin: f64, limit: u32| {
        let mut start = ((start - origin) * scale.get())
            .round()
            .clamp(0.0, f64::from(limit)) as u32;
        let mut end = ((end - origin) * scale.get())
            .round()
            .clamp(0.0, f64::from(limit)) as u32;
        if end <= start {
            start = start.min(limit.saturating_sub(1));
            end = (start + 1).min(limit);
        }
        (start, end)
    };
    let (left, right) = quantize(
        content.origin.x,
        geom::max_x(&content),
        bounds.origin.x,
        canvas.0,
    );
    let (top, bottom) = quantize(
        content.origin.y,
        geom::max_y(&content),
        bounds.origin.y,
        canvas.1,
    );
    (left, top, right, bottom)
}

fn normalize_redaction_styles(objects: &mut [AnnotationObject]) {
    for object in objects {
        if matches!(object.annotation, Annotation::Redact { .. }) {
            object.style.redact_intensity = object.style.effective_redact_intensity();
        } else {
            object.style.redact_intensity = None;
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
        self.document.touch();
    }
}

fn valid_redaction_area(area: LogicalRect, bounds: LogicalRect) -> bool {
    let values = [
        area.origin.x,
        area.origin.y,
        area.size.width,
        area.size.height,
    ];
    values.into_iter().all(f64::is_finite)
        && !area.is_empty()
        && area.origin.x >= bounds.origin.x
        && area.origin.y >= bounds.origin.y
        && area.origin.x + area.size.width <= bounds.origin.x + bounds.size.width
        && area.origin.y + area.size.height <= bounds.origin.y + bounds.size.height
}
