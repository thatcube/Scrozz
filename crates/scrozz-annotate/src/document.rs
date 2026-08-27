//! The document: a capture plus every edit ever made to it.

use std::{
    collections::HashSet,
    hash::{BuildHasher, Hasher},
    sync::Arc,
};

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
    geom,
    style::{Color, Style},
};

/// Rendered raster limit shared with the compositor.
pub(crate) const MAX_RASTER_PIXELS: u64 = 40_000_000;
const MAX_BACKGROUND_PIXELS: u64 = 16_777_216;
const MAX_BACKGROUND_BYTES: u64 = MAX_BACKGROUND_PIXELS * 4;
const MAX_ENCODED_BACKGROUND_BYTES: u64 = MAX_BACKGROUND_BYTES + 1024 * 1024;

/// A procedural background shipped with Scrozz.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltInBackground {
    /// Quiet cool-grey paper.
    #[default]
    Mist,
    /// Periwinkle fading into violet.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundImage {
    width: u32,
    height: u32,
    pixels: Arc<[u8]>,
    color_space: ColorSpace,
    encoded_png: Arc<[u8]>,
}

impl BackgroundImage {
    /// Wraps tightly packed straight-alpha RGBA8 pixels.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for invalid or oversized geometry.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>, color_space: ColorSpace) -> Result<Self> {
        validate_background_pixels(width, height, &pixels)?;
        let stride = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(PixelFormat::Rgba8.bytes_per_pixel()))
            .ok_or_else(|| {
                Error::InvalidRequest(format!("background image {width}x{height} is too large"))
            })?;
        let frame = Frame {
            data: pixels,
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride,
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

    /// Tightly packed straight-alpha RGBA8 pixels.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Interpretation of the colour samples.
    #[must_use]
    pub const fn color_space(&self) -> ColorSpace {
        self.color_space
    }

    pub(crate) fn encoded_len(&self) -> usize {
        self.encoded_png.len()
    }

    /// Validates persisted geometry and encoded storage bounds.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for malformed or oversized data.
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
    if pixels.len() != expected {
        return Err(Error::InvalidRequest(format!(
            "background image {width}x{height} needs {expected} bytes, got {}",
            pixels.len()
        )));
    }
    Ok(())
}

fn validate_background_area(width: u32, height: u32) -> Result<u64> {
    if width == 0 || height == 0 {
        return Err(Error::InvalidRequest(
            "background image dimensions must be non-zero".to_owned(),
        ));
    }
    let pixel_count = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| Error::InvalidRequest("background image area overflowed".to_owned()))?;
    if pixel_count > MAX_BACKGROUND_PIXELS {
        return Err(Error::InvalidRequest(format!(
            "background image has {pixel_count} pixels; the limit is {MAX_BACKGROUND_PIXELS}"
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
    /// A deterministic procedural background bundled with Scrozz.
    BuiltIn(BuiltInBackground),
    /// A self-contained image cropped to cover the canvas.
    Image(BackgroundImage),
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

/// Output aspect ratios with destination-oriented names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AspectPreset {
    /// Keep the inner canvas's natural aspect ratio.
    #[default]
    Original,
    /// 1:1.
    Square,
    /// 4:5.
    Portrait,
    /// 9:16.
    Story,
    /// 16:9.
    Landscape,
    /// 3:1.
    Wide,
}

impl AspectPreset {
    /// Width divided by height, or `None` for the natural ratio.
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

/// Named combinations available in the editor and CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BeautificationPreset {
    /// Neutral framing.
    #[default]
    Clean,
    /// Square and colourful.
    Social,
    /// Tall story/reel canvas.
    Story,
    /// Restrained warm paper.
    Editorial,
}

/// Padding, background and framing applied around a capture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Beautification {
    /// Padding around the image, in logical points.
    pub padding: f64,
    /// Corner radius applied to the image.
    pub corner_radius: f64,
    /// Drop shadow depth.
    pub shadow: f64,
    /// What fills the padding.
    pub background: Background,
    /// Position within any extra canvas created by the aspect preset.
    pub alignment: Alignment,
    /// Shift the capture toward its deterministic visual centre.
    pub auto_balance: bool,
    /// Output aspect ratio.
    pub aspect: AspectPreset,
    /// Border width drawn inside the rounded content edge.
    pub border_width: f64,
    /// Border colour.
    pub border_color: Color,
}

impl Default for Beautification {
    fn default() -> Self {
        Self {
            padding: 0.0,
            corner_radius: 0.0,
            shadow: 0.0,
            background: Background::Transparent,
            alignment: Alignment::Center,
            auto_balance: false,
            aspect: AspectPreset::Original,
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        }
    }
}

impl Beautification {
    /// Largest supported logical measurement.
    pub const MAX_MEASUREMENT: f64 = 16_384.0;

    /// A preset: generous padding on a selected background.
    #[must_use]
    pub fn padded(padding: f64, background: Background) -> Self {
        Self {
            padding,
            background,
            ..Self::default()
        }
    }

    /// One of Scrozz's named starting points.
    #[must_use]
    pub const fn preset(preset: BeautificationPreset) -> Self {
        match preset {
            BeautificationPreset::Clean => Self {
                padding: 40.0,
                corner_radius: 16.0,
                shadow: 18.0,
                background: Background::BuiltIn(BuiltInBackground::Mist),
                alignment: Alignment::Center,
                auto_balance: false,
                aspect: AspectPreset::Original,
                border_width: 1.0,
                border_color: Color::rgba(255, 255, 255, 90),
            },
            BeautificationPreset::Social => Self {
                padding: 64.0,
                corner_radius: 20.0,
                shadow: 24.0,
                background: Background::BuiltIn(BuiltInBackground::Iris),
                alignment: Alignment::Center,
                auto_balance: true,
                aspect: AspectPreset::Square,
                border_width: 1.0,
                border_color: Color::rgba(255, 255, 255, 110),
            },
            BeautificationPreset::Story => Self {
                padding: 72.0,
                corner_radius: 24.0,
                shadow: 28.0,
                background: Background::BuiltIn(BuiltInBackground::Midnight),
                alignment: Alignment::Center,
                auto_balance: true,
                aspect: AspectPreset::Story,
                border_width: 1.0,
                border_color: Color::rgba(255, 255, 255, 90),
            },
            BeautificationPreset::Editorial => Self {
                padding: 56.0,
                corner_radius: 10.0,
                shadow: 14.0,
                background: Background::BuiltIn(BuiltInBackground::Sand),
                alignment: Alignment::Center,
                auto_balance: false,
                aspect: AspectPreset::Portrait,
                border_width: 1.0,
                border_color: Color::rgba(65, 53, 43, 65),
            },
        }
    }

    /// The radius a nested rounded shape needs to remain concentric.
    #[must_use]
    pub fn nested_radius(outer_radius: f64, padding: f64) -> f64 {
        (outer_radius - padding).max(0.0)
    }

    /// Whether every framing field is at its neutral value.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.padding <= 0.0
            && self.corner_radius <= 0.0
            && self.shadow <= 0.0
            && self.background == Background::Transparent
            && self.alignment == Alignment::Center
            && !self.auto_balance
            && self.aspect == AspectPreset::Original
            && self.border_width <= 0.0
            && self.border_color == Color::TRANSPARENT
    }

    /// Validates values before allocation or rasterisation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for invalid measurements or a malformed
    /// custom background.
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("padding", self.padding),
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
        Ok(())
    }

    /// Logical output size. Aspect presets add canvas; they never crop content.
    #[must_use]
    pub fn output_size(&self, content: LogicalSize) -> LogicalSize {
        let base_width = content.width + self.padding * 2.0;
        let base_height = content.height + self.padding * 2.0;
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

/// A non-destructive quarter-turn applied to the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanvasRotation {
    /// Source orientation.
    #[default]
    None,
    /// Ninety degrees clockwise.
    Clockwise90,
    /// One hundred and eighty degrees.
    HalfTurn,
    /// Ninety degrees counter-clockwise.
    CounterClockwise90,
}

impl CanvasRotation {
    /// The next clockwise quarter-turn.
    #[must_use]
    pub const fn clockwise(self) -> Self {
        match self {
            Self::None => Self::Clockwise90,
            Self::Clockwise90 => Self::HalfTurn,
            Self::HalfTurn => Self::CounterClockwise90,
            Self::CounterClockwise90 => Self::None,
        }
    }

    /// The next counter-clockwise quarter-turn.
    #[must_use]
    pub const fn counter_clockwise(self) -> Self {
        match self {
            Self::None => Self::CounterClockwise90,
            Self::CounterClockwise90 => Self::HalfTurn,
            Self::HalfTurn => Self::Clockwise90,
            Self::Clockwise90 => Self::None,
        }
    }

    const fn swaps_axes(self) -> bool {
        matches!(self, Self::Clockwise90 | Self::CounterClockwise90)
    }
}

/// Non-destructive image framing owned by document format v2.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Canvas {
    /// The visible source region. `None` means the untouched source bounds.
    #[serde(default)]
    pub crop: Option<LogicalRect>,
    /// A quarter-turn applied after flips.
    #[serde(default)]
    pub rotation: CanvasRotation,
    /// Mirror around the vertical axis.
    #[serde(default)]
    pub flip_horizontal: bool,
    /// Mirror around the horizontal axis.
    #[serde(default)]
    pub flip_vertical: bool,
    /// Grow the output to retain annotations drawn beyond the crop.
    #[serde(default)]
    pub auto_expand: bool,
}

/// Resolved canvas bounds and reversible coordinate mapping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasGeometry {
    source_bounds: LogicalRect,
    source_crop: LogicalRect,
    output_size: LogicalSize,
    rotation: CanvasRotation,
    flip_horizontal: bool,
    flip_vertical: bool,
}

impl CanvasGeometry {
    /// Source-space bounds represented by the output, including auto-expansion.
    #[must_use]
    pub const fn source_bounds(self) -> LogicalRect {
        self.source_bounds
    }

    /// Source pixels that remain visible after cropping.
    #[must_use]
    pub const fn source_crop(self) -> LogicalRect {
        self.source_crop
    }

    /// Output size after flips and rotation.
    #[must_use]
    pub const fn output_size(self) -> LogicalSize {
        self.output_size
    }

    /// Maps an editable source-space point into the transformed canvas.
    #[must_use]
    pub fn source_to_canvas(self, source: LogicalPoint) -> LogicalPoint {
        let width = self.source_bounds.size.width;
        let height = self.source_bounds.size.height;
        let mut x = source.x - self.source_bounds.origin.x;
        let mut y = source.y - self.source_bounds.origin.y;
        if self.flip_horizontal {
            x = width - x;
        }
        if self.flip_vertical {
            y = height - y;
        }
        match self.rotation {
            CanvasRotation::None => LogicalPoint::new(x, y),
            CanvasRotation::Clockwise90 => LogicalPoint::new(height - y, x),
            CanvasRotation::HalfTurn => LogicalPoint::new(width - x, height - y),
            CanvasRotation::CounterClockwise90 => LogicalPoint::new(y, width - x),
        }
    }

    /// Maps a transformed canvas point back into editable source space.
    #[must_use]
    pub fn canvas_to_source(self, canvas: LogicalPoint) -> LogicalPoint {
        let width = self.source_bounds.size.width;
        let height = self.source_bounds.size.height;
        let (mut x, mut y) = match self.rotation {
            CanvasRotation::None => (canvas.x, canvas.y),
            CanvasRotation::Clockwise90 => (canvas.y, height - canvas.x),
            CanvasRotation::HalfTurn => (width - canvas.x, height - canvas.y),
            CanvasRotation::CounterClockwise90 => (width - canvas.y, canvas.x),
        };
        if self.flip_horizontal {
            x = width - x;
        }
        if self.flip_vertical {
            y = height - y;
        }
        LogicalPoint::new(
            x + self.source_bounds.origin.x,
            y + self.source_bounds.origin.y,
        )
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
    /// Non-destructive crop, rotation, flips, and auto-expansion.
    #[serde(default)]
    pub canvas: Canvas,
    /// Random seed for irreversible, reproducible pixelation.
    ///
    /// Pixelated redactions deliberately do not derive their mosaic colours from
    /// the covered pixels. Persisting the seed keeps the destructive pattern
    /// stable across reopen and re-render without retaining any recoverable
    /// summary of the source region.
    #[serde(default)]
    pub redaction_seed: u64,
    /// The next identifier to hand out.
    ///
    /// Persisted so a reopened document cannot reissue an id that an undo stack
    /// or a selection still refers to.
    pub next_id: u64,
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
            canvas: Canvas::default(),
            redaction_seed: 0,
            next_id: 1,
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
    canvas: Canvas,
    redaction_seed: u64,
    next_id: u64,
}

impl Document {
    /// Wraps a fresh capture in an empty document.
    #[must_use]
    pub fn new(source: Capture) -> Self {
        let redaction_seed = fresh_redaction_seed(&source);
        Self {
            source,
            objects: Vec::new(),
            beautification: None,
            canvas: Canvas::default(),
            redaction_seed,
            next_id: 1,
        }
    }

    /// Rebuilds a document from a capture and its persisted edits.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the data is from a newer format
    /// version, or if it carries beautification for a capture that forbids it —
    /// a document that was hand-edited or that changed provenance must not be
    /// silently accepted and then quietly rendered wrong.
    pub fn from_data(source: Capture, data: DocumentData) -> Result<Self> {
        let data = Self::prepare_data(&source, data)?;
        let mut document = Self {
            source,
            objects: data.annotations,
            beautification: data.beautification,
            canvas: data.canvas,
            redaction_seed: data.redaction_seed,
            next_id: data.next_id,
        };
        document.renumber_counters();
        Ok(document)
    }

    fn prepare_data(source: &Capture, mut data: DocumentData) -> Result<DocumentData> {
        if data.version > DocumentData::VERSION {
            return Err(Error::InvalidRequest(format!(
                "document format version {} is newer than supported version {}",
                data.version,
                DocumentData::VERSION
            )));
        }
        if data.beautification.is_some() && source.provenance.forbids_compositing() {
            return Err(Error::InvalidRequest(
                "beautification is not permitted for window captures (decision D9)".to_owned(),
            ));
        }
        if let Some(beautification) = &data.beautification {
            beautification.validate()?;
        }
        normalize_annotation_ids(&mut data)?;
        data.canvas = normalize_canvas(capture_logical_bounds(source), data.canvas)?;
        if data.redaction_seed == 0 {
            data.redaction_seed = fresh_redaction_seed(source);
        }
        data.version = DocumentData::VERSION;
        Ok(data)
    }

    /// The editable part of this document, ready to persist.
    #[must_use]
    pub fn data(&self) -> DocumentData {
        DocumentData {
            version: DocumentData::VERSION,
            annotations: self.objects.clone(),
            beautification: self.beautification.clone(),
            canvas: self.canvas,
            redaction_seed: self.redaction_seed,
            next_id: self.next_id,
        }
    }

    /// Restores an editable snapshot while retaining the immutable source pixels.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::from_data`].
    pub fn restore(&mut self, data: DocumentData) -> Result<()> {
        let high_water = self.next_id;
        let data = Self::prepare_data(&self.source, data)?;
        self.objects = data.annotations;
        self.beautification = data.beautification;
        self.canvas = data.canvas;
        // The seed identifies this document's destructive transform. Undoing an
        // edit must never replace it with an old-format snapshot's fresh seed.
        self.next_id = high_water.max(data.next_id);
        self.renumber_counters();
        Ok(())
    }

    /// Every annotation, bottom-most first.
    #[must_use]
    pub fn annotations(&self) -> &[AnnotationObject] {
        &self.objects
    }

    /// Seed used to make secure pixelation deterministic for this document.
    #[must_use]
    pub const fn redaction_seed(&self) -> u64 {
        self.redaction_seed
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

    /// The whole source image as a logical rectangle.
    #[must_use]
    pub fn logical_bounds(&self) -> LogicalRect {
        capture_logical_bounds(&self.source)
    }

    /// The current non-destructive canvas transform.
    #[must_use]
    pub const fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    /// Replaces crop, rotation, flip, and auto-expand settings.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an empty, non-finite, or wholly
    /// out-of-source crop. A crop that partly overlaps the source is clamped to
    /// the source bounds.
    pub fn set_canvas(&mut self, canvas: Canvas) -> Result<()> {
        self.canvas = normalize_canvas(self.logical_bounds(), canvas)?;
        Ok(())
    }

    /// Resolves crop and auto-expansion into a reversible coordinate mapping.
    #[must_use]
    pub fn canvas_geometry(&self) -> CanvasGeometry {
        self.resolve_canvas_geometry(self.canvas)
    }

    /// Resolves a temporary canvas without changing the persisted document.
    ///
    /// This is useful for editing surfaces that need to show more of the source
    /// than the final cropped export while preserving the same rotation, flips,
    /// and annotation expansion.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an empty, non-finite, or wholly
    /// out-of-source crop. A crop that partly overlaps the source is clamped to
    /// the source bounds.
    pub fn canvas_geometry_for(&self, canvas: Canvas) -> Result<CanvasGeometry> {
        let canvas = normalize_canvas(self.logical_bounds(), canvas)?;
        Ok(self.resolve_canvas_geometry(canvas))
    }

    fn resolve_canvas_geometry(&self, canvas: Canvas) -> CanvasGeometry {
        let source = self.logical_bounds();
        let source_crop = canvas.crop.unwrap_or(source);
        let mut source_bounds = source_crop;
        if canvas.auto_expand {
            for object in &self.objects {
                let visual = object.visual_bounds();
                if rect_is_finite(&visual) && !visual.is_empty() {
                    source_bounds = geom::union(&source_bounds, &visual);
                }
            }
        }

        let output_size = if canvas.rotation.swaps_axes() {
            LogicalSize::new(source_bounds.size.height, source_bounds.size.width)
        } else {
            source_bounds.size
        };
        CanvasGeometry {
            source_bounds,
            source_crop,
            output_size,
            rotation: canvas.rotation,
            flip_horizontal: canvas.flip_horizontal,
            flip_vertical: canvas.flip_vertical,
        }
    }

    /// Output size after crop, expansion, and quarter-turn rotation.
    #[must_use]
    pub fn canvas_size(&self) -> LogicalSize {
        self.canvas_geometry().output_size()
    }

    /// Final logical output size after inner canvas edits and outer framing.
    #[must_use]
    pub fn output_logical_size(&self) -> LogicalSize {
        let content = self.canvas_size();
        self.beautification
            .as_ref()
            .map_or(content, |beautification| {
                beautification.output_size(content)
            })
    }

    /// Adds an annotation on top of everything else.
    ///
    /// Counter markers are numbered by the document, so the `index` on a
    /// [`Annotation::Counter`] passed in here is ignored and replaced.
    /// Returns [`Error::InvalidRequest`] if the document has exhausted its
    /// identifier space.
    pub fn add(&mut self, annotation: Annotation, style: Style) -> Result<AnnotationId> {
        if self.next_id == u64::MAX {
            return Err(Error::InvalidRequest(
                "annotation identifier space is exhausted".to_owned(),
            ));
        }
        let id = AnnotationId(self.next_id);
        self.next_id += 1;
        self.objects
            .push(AnnotationObject::new(id, annotation, style));
        self.renumber_counters();
        Ok(id)
    }

    /// Adds an annotation with the default style for its kind.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the document has exhausted its
    /// identifier space.
    pub fn add_default(&mut self, annotation: Annotation) -> Result<AnnotationId> {
        let style = match &annotation {
            Annotation::Highlight(_) => Style::highlighter(),
            Annotation::Spotlight(_) => Style::spotlight(),
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
                if matches!(self.objects[index].annotation, Annotation::Arrow { .. })
                    && let Some(control) = &mut self.objects[index].style.curve_control
                {
                    control.x += dx;
                    control.y += dy;
                }
                true
            }
            None => false,
        }
    }

    /// Reshapes an annotation to fill `bounds`.
    pub fn set_bounds(&mut self, id: AnnotationId, bounds: LogicalRect) -> bool {
        match self.index_of(id) {
            Some(index) => {
                let old_bounds = self.objects[index].annotation.bounds();
                let curve_control = self.objects[index].style.curve_control;
                self.objects[index].annotation.set_bounds(bounds);
                if matches!(self.objects[index].annotation, Annotation::Arrow { .. })
                    && let Some(control) = curve_control
                {
                    self.objects[index].style.curve_control =
                        Some(geom::remap(control, &old_bounds, &bounds));
                }
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
    /// False for window captures. The UI disables the controls entirely rather
    /// than letting them be set and quietly ignored.
    #[must_use]
    pub fn may_beautify(&self) -> bool {
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
    /// Returns [`Error::InvalidRequest`] when framing is requested for a window
    /// capture. Per decision D9 the OS output *is* the truth for a window, and
    /// compositing padding, corners or a shadow onto it produces a subtly wrong
    /// image. Refusing here, rather than accepting and ignoring, is what stops a
    /// caller believing the setting took effect.
    pub fn set_beautification(&mut self, beautification: Option<Beautification>) -> Result<()> {
        if beautification.is_some() && !self.may_beautify() {
            return Err(Error::InvalidRequest(
                "beautification is not permitted for window captures (decision D9)".to_owned(),
            ));
        }
        if let Some(beautification) = &beautification {
            beautification.validate()?;
        }
        self.beautification = beautification;
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

fn normalize_annotation_ids(data: &mut DocumentData) -> Result<()> {
    let mut seen = HashSet::with_capacity(data.annotations.len());
    let mut next_id = 1_u64;
    for object in &data.annotations {
        let id = object.id.0;
        if id == 0 || id == u64::MAX {
            return Err(Error::InvalidRequest(format!(
                "annotation identifier {id} is outside the supported range"
            )));
        }
        if !seen.insert(id) {
            return Err(Error::InvalidRequest(format!(
                "annotation identifier {id} appears more than once"
            )));
        }
        next_id = next_id.max(id + 1);
    }
    data.next_id = data.next_id.max(next_id).max(1);
    Ok(())
}

fn rect_is_finite(rect: &LogicalRect) -> bool {
    rect.origin.x.is_finite()
        && rect.origin.y.is_finite()
        && rect.size.width.is_finite()
        && rect.size.height.is_finite()
}

fn capture_logical_bounds(source: &Capture) -> LogicalRect {
    let scale = source.frame.scale.get();
    LogicalRect::new(
        LogicalPoint::new(0.0, 0.0),
        LogicalSize::new(
            source.frame.size.width / scale,
            source.frame.size.height / scale,
        ),
    )
}

fn fresh_redaction_seed(source: &Capture) -> u64 {
    // RandomState is independently keyed on construction. Hashing stable capture
    // properties turns those process-provided random keys into a compact seed
    // without making infallible document construction depend on an I/O API.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(source.frame.size.width.to_bits());
    hasher.write_u64(source.frame.size.height.to_bits());
    hasher.write_u64(source.frame.scale.get().to_bits());
    hasher.finish().max(1)
}

fn normalize_canvas(source: LogicalRect, mut canvas: Canvas) -> Result<Canvas> {
    if let Some(crop) = canvas.crop {
        if !rect_is_finite(&crop) || crop.is_empty() {
            return Err(Error::InvalidRequest(
                "canvas crop must be finite and have positive width and height".to_owned(),
            ));
        }
        canvas.crop = Some(geom::intersection(&crop, &source).ok_or_else(|| {
            Error::InvalidRequest(
                "canvas crop must intersect the immutable source image".to_owned(),
            )
        })?);
    }
    Ok(canvas)
}

/// Bounded undo/redo history made only of editable document snapshots.
///
/// Source pixels never enter this stack: D14 keeps the capture immutable and
/// snapshots only the compact [`DocumentData`] that can actually change.
#[derive(Debug, Clone)]
pub struct UndoHistory {
    undo: Vec<DocumentData>,
    redo: Vec<DocumentData>,
    limit: usize,
}

impl UndoHistory {
    /// A history seeded with the document's current state.
    #[must_use]
    pub fn new(document: &Document) -> Self {
        Self {
            undo: vec![document.data()],
            redo: Vec::new(),
            limit: 128,
        }
    }

    /// Records the current state after an edit.
    ///
    /// Equal adjacent states are deduplicated, and any edit after undoing starts
    /// a new branch by clearing redo.
    pub fn checkpoint(&mut self, document: &Document) {
        let snapshot = document.data();
        if self.undo.last() == Some(&snapshot) {
            return;
        }
        self.undo.push(snapshot);
        self.redo.clear();
        if self.undo.len() > self.limit {
            self.undo.remove(0);
        }
    }

    /// Whether an older snapshot exists.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.undo.len() > 1
    }

    /// Whether an undone snapshot can be restored.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Restores the previous editable state.
    ///
    /// # Errors
    ///
    /// Propagates snapshot validation errors from [`Document::restore`].
    pub fn undo(&mut self, document: &mut Document) -> Result<bool> {
        if !self.can_undo() {
            return Ok(false);
        }
        if let Some(current) = self.undo.pop() {
            self.redo.push(current);
        }
        let Some(previous) = self.undo.last().cloned() else {
            return Ok(false);
        };
        document.restore(previous)?;
        Ok(true)
    }

    /// Restores the next editable state.
    ///
    /// # Errors
    ///
    /// Propagates snapshot validation errors from [`Document::restore`].
    pub fn redo(&mut self, document: &mut Document) -> Result<bool> {
        let Some(next) = self.redo.pop() else {
            return Ok(false);
        };
        document.restore(next.clone())?;
        self.undo.push(next);
        Ok(true)
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
