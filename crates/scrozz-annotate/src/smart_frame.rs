//! Deterministic Smart Frame analysis and portable preset data.
//!
//! Analysis is deliberately separate from rendering. It inspects one immutable
//! revision, resolves every automatic choice into persisted values, and hands
//! the renderer an ordinary [`Beautification`]. Reopening the document therefore
//! does not silently restyle it when the algorithm changes.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use scrozz_core::{ColorSpace, Error, Frame, LogicalRect, LogicalSize, Provenance, Result};
use scrozz_export::{RgbaImage, convert_to_srgb, to_straight_rgba8};
use serde::{Deserialize, Serialize};

use crate::{
    Alignment, AspectPreset, AutomaticBackground, Background, Beautification, Color,
    ExactOutputSize, SourceInsets, Watermark,
};

/// Version of the analysis whose resolved inputs are persisted in documents.
pub const SMART_FRAME_ALGORITHM_VERSION: u16 = 1;
/// Current schema for user-created presets.
pub const SMART_FRAME_PRESET_VERSION: u32 = 1;
/// Maximum number of sampled pixels used for salience and palette analysis.
pub const MAX_ANALYSIS_SAMPLES: u64 = 65_536;
/// Longest accepted preset name.
pub const MAX_PRESET_NAME_CHARS: usize = 64;

const MAX_ANALYSIS_PIXELS: u64 = 40_000_000;
const NORMALIZED_MAX: u16 = 10_000;
const FOREGROUND_DISTANCE: u32 = 24;
const UNIFORM_DISTANCE: u32 = 8;

/// Cooperative cancellation checked throughout image analysis.
#[derive(Debug, Clone, Default)]
pub struct AnalysisCancellation {
    cancelled: Arc<AtomicBool>,
}

impl AnalysisCancellation {
    /// Cancels this request. Repeated cancellation is harmless.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Coarse content classification used only to tune bounded framing tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentClass {
    /// Sparse text, a dialog, or a small isolated object.
    Sparse,
    /// A predominantly opaque interface capture.
    #[default]
    Interface,
    /// Continuous-tone or high-detail imagery.
    Photo,
    /// A capture with meaningful transparency.
    Transparent,
}

/// Why automatic inset did or did not trim the source-space edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsetDecision {
    /// Transparent outer pixels were removed.
    TransparentMargin,
    /// A highly uniform opaque outer colour was removed.
    UniformBackground,
    /// No excessive margin was present.
    #[default]
    NoExcessMargin,
    /// The edge was not uniform enough to crop safely.
    LowConfidence,
    /// Window pixels are immutable under decision D9.
    WindowPreserved,
}

impl InsetDecision {
    /// Short, user-facing explanation for the inspector.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::TransparentMargin => "Trimmed transparent outer margin",
            Self::UniformBackground => "Trimmed a high-confidence uniform margin",
            Self::NoExcessMargin => "No excessive outer margin detected",
            Self::LowConfidence => "Inset left at zero: the edge may contain meaningful pixels",
            Self::WindowPreserved => "Inset disabled: native window pixels are preserved",
        }
    }
}

/// Persisted visual focus in source-relative ten-thousandths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ResolvedFocus {
    /// Horizontal position, 0 at the left and 10,000 at the right.
    pub x: u16,
    /// Vertical position, 0 at the top and 10,000 at the bottom.
    pub y: u16,
    /// Confidence percentage. Low-confidence focus resolves to geometric centre.
    pub confidence: u8,
}

impl Default for ResolvedFocus {
    fn default() -> Self {
        Self {
            x: NORMALIZED_MAX / 2,
            y: NORMALIZED_MAX / 2,
            confidence: 0,
        }
    }
}

impl ResolvedFocus {
    /// Horizontal source coordinate for a raster of `width`.
    #[must_use]
    pub fn x_in(self, width: f64) -> f64 {
        width * f64::from(self.x) / f64::from(NORMALIZED_MAX)
    }

    /// Vertical source coordinate for a raster of `height`.
    #[must_use]
    pub fn y_in(self, height: f64) -> f64 {
        height * f64::from(self.y) / f64::from(NORMALIZED_MAX)
    }
}

/// Inputs and decisions fixed when Smart Frame analysed a revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmartFrameMetadata {
    /// Algorithm that produced these values.
    pub algorithm_version: u16,
    /// Colour interpretation used while sampling.
    pub source_color_space: ColorSpace,
    /// Analysed raster width.
    pub source_width: u32,
    /// Analysed raster height.
    pub source_height: u32,
    /// Stored visual centre; rendering never needs to re-run analysis.
    pub focus: ResolvedFocus,
    /// Automatic inset decision.
    pub inset_decision: InsetDecision,
    /// Confidence percentage for the inset decision.
    pub inset_confidence: u8,
    /// Coarse content class used for adaptive values.
    pub content_class: ContentClass,
    /// Number of samples used for bounded palette/content analysis.
    pub analysis_samples: u32,
    /// Unknown fields survive read-modify-write cycles.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl Default for SmartFrameMetadata {
    fn default() -> Self {
        Self {
            algorithm_version: SMART_FRAME_ALGORITHM_VERSION,
            source_color_space: ColorSpace::Unknown,
            source_width: 0,
            source_height: 0,
            focus: ResolvedFocus::default(),
            inset_decision: InsetDecision::NoExcessMargin,
            inset_confidence: 0,
            content_class: ContentClass::Interface,
            analysis_samples: 0,
            extensions: BTreeMap::new(),
        }
    }
}

/// Result of analysing one immutable document revision.
#[derive(Debug, Clone, PartialEq)]
pub struct SmartFrameAnalysis {
    /// Fully resolved settings ready to preview or persist.
    pub beautification: Beautification,
    /// Plain-language explanation of the inset choice.
    pub inset_explanation: String,
}

/// Immediate, analysis-free Smart Frame draft used while background work runs.
#[must_use]
pub fn provisional(
    source: LogicalSize,
    source_scale: f64,
    provenance: Provenance,
    color_space: ColorSpace,
) -> Beautification {
    let logical_min = source.width.min(source.height).round().clamp(1.0, 16_384.0) as u32;
    let padding = quantize_even((logical_min * 8 / 100).clamp(24, 96));
    let radius = quantize_even((padding * 26 / 100).clamp(8, 24));
    let shadow = quantize_even((padding * 34 / 100).clamp(10, 28));
    let (corner_radius, shadow, border_width) = if provenance == Provenance::Window {
        (0.0, 0.0, 0.0)
    } else {
        (f64::from(radius), f64::from(shadow), 1.0)
    };
    Beautification {
        padding: f64::from(padding),
        inset: SourceInsets::default(),
        corner_radius,
        shadow,
        background: Background::Automatic(AutomaticBackground::fallback(color_space)),
        alignment: Alignment::Center,
        auto_balance: true,
        aspect: AspectPreset::Original,
        output_size: None,
        border_width,
        border_color: Color::rgba(255, 255, 255, 72),
        watermark: None,
        smart_frame: Some(SmartFrameMetadata {
            source_color_space: color_space,
            source_width: (source.width * source_scale).round().max(0.0) as u32,
            source_height: (source.height * source_scale).round().max(0.0) as u32,
            inset_decision: if provenance == Provenance::Window {
                InsetDecision::WindowPreserved
            } else {
                InsetDecision::NoExcessMargin
            },
            ..SmartFrameMetadata::default()
        }),
        extensions: BTreeMap::new(),
    }
}

/// A region suggested by a future OCR/privacy provider.
///
/// Smart Frame only transports these suggestions. It never converts one into a
/// redaction or modifies pixels without a separate reviewed user action.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SensitiveRegionSuggestion {
    /// Provider-owned stable identity.
    pub id: String,
    /// Source-space bounds.
    pub bounds: LogicalRect,
    /// Human-readable category such as "email address".
    pub category: String,
    /// Provider confidence percentage.
    pub confidence: u8,
    /// Unknown provider fields survive persistence and transport.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Reviewed suggestion payload associated with one immutable revision.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SensitiveRegionReview {
    /// Revision the provider inspected.
    pub revision: u64,
    /// Suggestions awaiting explicit review.
    pub suggestions: Vec<SensitiveRegionSuggestion>,
    /// Unknown provider fields survive transport.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Background choice saved in a reusable preset.
///
/// Capture pixels never enter preset storage. `Automatic` asks each new capture
/// to resolve its own palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetBackground {
    /// Resolve a fresh palette from the capture.
    Automatic,
    /// No background.
    Transparent,
    /// A flat colour.
    Solid(Color),
    /// A fixed two-colour field.
    Gradient {
        /// First field colour.
        start: Color,
        /// Second field colour.
        end: Color,
    },
    /// A procedural Scrozz background.
    BuiltIn(crate::BuiltInBackground),
}

/// Pixel-free settings stored in a reusable preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmartFramePresetSettings {
    /// Padding in logical points.
    pub padding: f64,
    /// Source-space inset.
    pub inset: SourceInsets,
    /// Capture corner radius.
    pub corner_radius: f64,
    /// Shadow depth.
    pub shadow: f64,
    /// Background choice.
    pub background: PresetBackground,
    /// Manual placement.
    pub alignment: Alignment,
    /// Whether resolved visual focus is used.
    pub auto_balance: bool,
    /// Ratio-only canvas choice.
    pub aspect: AspectPreset,
    /// Optional exact output dimensions.
    pub output_size: Option<ExactOutputSize>,
    /// Border width.
    pub border_width: f64,
    /// Border colour.
    pub border_color: Color,
    /// Optional, off-by-default watermark.
    pub watermark: Option<Watermark>,
    /// Unknown settings survive migrations.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl Default for SmartFramePresetSettings {
    fn default() -> Self {
        Self::from_beautification(&Beautification::default())
            .expect("the default has no embedded pixels")
    }
}

impl SmartFramePresetSettings {
    /// Builds a reusable preset, refusing embedded custom-image pixels.
    pub fn from_beautification(value: &Beautification) -> Result<Self> {
        let background = match value.background {
            Background::Automatic(_) => PresetBackground::Automatic,
            Background::Transparent => PresetBackground::Transparent,
            Background::Solid(color) => PresetBackground::Solid(color),
            Background::Gradient { start, end } => PresetBackground::Gradient { start, end },
            Background::BuiltIn(background) => PresetBackground::BuiltIn(background),
            Background::Image(_) => {
                return Err(Error::InvalidRequest(
                    "custom background pixels cannot be stored in a Smart Frame preset".into(),
                ));
            }
        };
        Ok(Self {
            padding: value.padding,
            inset: value.inset,
            corner_radius: value.corner_radius,
            shadow: value.shadow,
            background,
            alignment: value.alignment,
            auto_balance: value.auto_balance,
            aspect: value.aspect,
            output_size: value.output_size,
            border_width: value.border_width,
            border_color: value.border_color,
            watermark: value.watermark.clone(),
            extensions: value.extensions.clone(),
        })
    }

    /// Applies this preset without carrying capture-specific analysis forward.
    #[must_use]
    pub fn to_beautification(&self) -> Beautification {
        let background = match self.background {
            PresetBackground::Automatic => {
                Background::Automatic(AutomaticBackground::fallback(ColorSpace::Unknown))
            }
            PresetBackground::Transparent => Background::Transparent,
            PresetBackground::Solid(color) => Background::Solid(color),
            PresetBackground::Gradient { start, end } => Background::Gradient { start, end },
            PresetBackground::BuiltIn(background) => Background::BuiltIn(background),
        };
        Beautification {
            padding: self.padding,
            inset: self.inset,
            corner_radius: self.corner_radius,
            shadow: self.shadow,
            background,
            alignment: self.alignment,
            auto_balance: self.auto_balance,
            aspect: self.aspect,
            output_size: self.output_size,
            border_width: self.border_width,
            border_color: self.border_color,
            watermark: self.watermark.clone(),
            smart_frame: None,
            extensions: self.extensions.clone(),
        }
    }
}

/// One user-created, cross-capture preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmartFramePreset {
    /// Schema version.
    pub version: u32,
    /// Stable, non-secret identifier.
    pub id: String,
    /// User-visible name.
    pub name: String,
    /// Pixel-free framing values.
    pub settings: SmartFramePresetSettings,
    /// Unknown fields survive updates.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl Default for SmartFramePreset {
    fn default() -> Self {
        Self {
            version: SMART_FRAME_PRESET_VERSION,
            id: String::new(),
            name: String::new(),
            settings: SmartFramePresetSettings::default(),
            extensions: BTreeMap::new(),
        }
    }
}

impl SmartFramePreset {
    /// Creates and validates a preset.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        settings: SmartFramePresetSettings,
    ) -> Result<Self> {
        let preset = Self {
            id: id.into(),
            name: name.into(),
            settings,
            ..Self::default()
        };
        preset.validate()?;
        Ok(preset)
    }

    /// Validates names and schema bounds.
    pub fn validate(&self) -> Result<()> {
        if self.version > SMART_FRAME_PRESET_VERSION {
            return Err(Error::InvalidRequest(format!(
                "Smart Frame preset version {} is newer than supported version {}",
                self.version, SMART_FRAME_PRESET_VERSION
            )));
        }
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > MAX_PRESET_NAME_CHARS {
            return Err(Error::InvalidRequest(format!(
                "preset name must contain 1 to {MAX_PRESET_NAME_CHARS} characters"
            )));
        }
        if self.id.is_empty()
            || self.id.len() > 96
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(Error::InvalidRequest(
                "preset id must contain only letters, digits, hyphens, or underscores".into(),
            ));
        }
        self.settings.to_beautification().validate()
    }
}

/// Analyses one rendered, unframed document revision.
///
/// Sampling and decisions are bounded and integer-based. The source is converted
/// to sRGB only for analysis; its stored pixels and output profile are untouched.
pub fn analyze(
    frame: &Frame,
    provenance: Provenance,
    cancellation: &AnalysisCancellation,
) -> Result<SmartFrameAnalysis> {
    cancellation.check()?;
    let pixel_count = u64::from(frame.width())
        .checked_mul(u64::from(frame.height()))
        .ok_or_else(|| Error::InvalidRequest("Smart Frame raster area overflowed".into()))?;
    if pixel_count == 0 || pixel_count > MAX_ANALYSIS_PIXELS {
        return Err(Error::InvalidRequest(format!(
            "Smart Frame input has {pixel_count} pixels; the limit is {MAX_ANALYSIS_PIXELS}"
        )));
    }
    if frame.width() > 16_384 || frame.height() > 16_384 {
        return Err(Error::InvalidRequest(format!(
            "Smart Frame input {}x{} exceeds the 16384-pixel edge limit",
            frame.width(),
            frame.height()
        )));
    }
    if !frame.is_well_formed() {
        return Err(Error::InvalidRequest(
            "Smart Frame cannot analyse a malformed frame".into(),
        ));
    }

    let original_space = frame.color_space;
    let straight = to_straight_rgba8(frame)?;
    let image = if matches!(original_space, ColorSpace::DisplayP3 | ColorSpace::Rec2020) {
        convert_to_srgb(&straight, original_space)?
    } else {
        straight
    };
    cancellation.check()?;

    let edge = edge_reference(&image, cancellation)?;
    let inset = if provenance == Provenance::Window {
        InsetAnalysis {
            inset: SourceInsets::default(),
            decision: InsetDecision::WindowPreserved,
            confidence: 100,
        }
    } else {
        detect_inset(&image, frame.scale.get(), edge, cancellation)?
    };
    let focus = visual_focus(&image, inset.inset, frame.scale.get(), edge, cancellation)?;
    let stats = sampled_stats(&image, edge, cancellation)?;
    let content_class = classify(&stats);
    let background = automatic_background(edge, original_space);
    let logical_min = (f64::from(frame.width().min(frame.height())) / frame.scale.get())
        .round()
        .clamp(1.0, 16_384.0) as u32;
    let sparse_lift = u32::from(100_u8.saturating_sub(stats.foreground_percent)) * 14 / 100;
    let padding = quantize_even(
        (logical_min * 8 / 100)
            .saturating_add(sparse_lift)
            .clamp(24, 96),
    );
    let radius = quantize_even((padding * 26 / 100).clamp(8, 24));
    let shadow = quantize_even((padding * 34 / 100).clamp(10, 28));
    let (corner_radius, shadow, border_width) = if provenance == Provenance::Window {
        (0.0, 0.0, 0.0)
    } else {
        (f64::from(radius), f64::from(shadow), 1.0)
    };
    let border_color = if background.average_luminance() > 0.45 {
        Color::rgba(20, 24, 34, 64)
    } else {
        Color::rgba(255, 255, 255, 72)
    };

    let metadata = SmartFrameMetadata {
        source_color_space: original_space,
        source_width: frame.width(),
        source_height: frame.height(),
        focus,
        inset_decision: inset.decision,
        inset_confidence: inset.confidence,
        content_class,
        analysis_samples: stats.samples,
        ..SmartFrameMetadata::default()
    };
    let beautification = Beautification {
        padding: f64::from(padding),
        inset: inset.inset,
        corner_radius,
        shadow,
        background: Background::Automatic(background),
        alignment: Alignment::Center,
        auto_balance: true,
        aspect: AspectPreset::Original,
        output_size: None,
        border_width,
        border_color,
        watermark: None,
        smart_frame: Some(metadata),
        extensions: BTreeMap::new(),
    };
    beautification.validate()?;
    Ok(SmartFrameAnalysis {
        beautification,
        inset_explanation: inset.decision.explanation().to_owned(),
    })
}

#[derive(Debug, Clone, Copy)]
struct EdgeReference {
    color: Color,
    transparent: bool,
    spread: u8,
}

#[derive(Debug, Clone, Copy)]
struct InsetAnalysis {
    inset: SourceInsets,
    decision: InsetDecision,
    confidence: u8,
}

#[derive(Debug, Clone, Copy, Default)]
struct SampledStats {
    visible_percent: u8,
    foreground_percent: u8,
    detail_percent: u8,
    samples: u32,
}

fn edge_reference(image: &RgbaImage, cancellation: &AnalysisCancellation) -> Result<EdgeReference> {
    let patch_x = (image.width / 64).clamp(1, 12);
    let patch_y = (image.height / 64).clamp(1, 12);
    let mut samples = Vec::with_capacity((patch_x * patch_y * 4) as usize);
    for y in 0..patch_y {
        cancellation.check()?;
        for x in 0..patch_x {
            for (sx, sy) in [
                (x, y),
                (image.width - 1 - x, y),
                (x, image.height - 1 - y),
                (image.width - 1 - x, image.height - 1 - y),
            ] {
                if let Some(pixel) = image.pixel(sx, sy) {
                    samples.push(pixel);
                }
            }
        }
    }
    let transparent =
        samples.iter().filter(|pixel| pixel[3] <= 8).count() * 4 >= samples.len().saturating_mul(3);
    let visible: Vec<[u8; 4]> = samples
        .iter()
        .copied()
        .filter(|pixel| pixel[3] > 8)
        .collect();
    if visible.is_empty() {
        return Ok(EdgeReference {
            color: Color::TRANSPARENT,
            transparent: true,
            spread: 0,
        });
    }
    let mean = mean_color(&visible);
    let spread = visible
        .iter()
        .map(|pixel| channel_distance(*pixel, [mean.r, mean.g, mean.b, mean.a]))
        .max()
        .unwrap_or(0)
        .min(u32::from(u8::MAX)) as u8;
    Ok(EdgeReference {
        color: mean,
        transparent,
        spread,
    })
}

fn detect_inset(
    image: &RgbaImage,
    source_scale: f64,
    edge: EdgeReference,
    cancellation: &AnalysisCancellation,
) -> Result<InsetAnalysis> {
    if !edge.transparent && u32::from(edge.spread) > UNIFORM_DISTANCE {
        return Ok(InsetAnalysis {
            inset: SourceInsets::default(),
            decision: InsetDecision::LowConfidence,
            confidence: 0,
        });
    }
    let max_x = (image.width / 5).min(512);
    let max_y = (image.height / 5).min(512);
    let left = scan_vertical(image, 0..max_x, edge, cancellation)?;
    let right = scan_vertical(
        image,
        image.width.saturating_sub(max_x)..image.width,
        edge,
        cancellation,
    )?;
    let top = scan_horizontal(image, 0..max_y, edge, cancellation)?;
    let bottom = scan_horizontal(
        image,
        image.height.saturating_sub(max_y)..image.height,
        edge,
        cancellation,
    )?;

    let right = image.width.saturating_sub(right);
    let bottom = image.height.saturating_sub(bottom);
    let minimum = (image.width.min(image.height) / 100).clamp(3, 16);
    let mut px = [left, top, right, bottom];
    for value in &mut px {
        if *value < minimum {
            *value = 0;
        }
    }
    if px[0].saturating_add(px[2]) >= image.width.saturating_sub(16)
        || px[1].saturating_add(px[3]) >= image.height.saturating_sub(16)
    {
        px = [0; 4];
    }
    let has_inset = px.iter().any(|value| *value > 0);
    let decision = if has_inset {
        if edge.transparent {
            InsetDecision::TransparentMargin
        } else {
            InsetDecision::UniformBackground
        }
    } else {
        InsetDecision::NoExcessMargin
    };
    let confidence = if !has_inset {
        0
    } else if edge.transparent {
        100
    } else {
        96_u8.saturating_sub(edge.spread.saturating_mul(4))
    };
    let to_logical = |value: u32| f64::from(value) / source_scale;
    Ok(InsetAnalysis {
        inset: SourceInsets {
            left: to_logical(px[0]),
            top: to_logical(px[1]),
            right: to_logical(px[2]),
            bottom: to_logical(px[3]),
        },
        decision,
        confidence,
    })
}

fn scan_vertical(
    image: &RgbaImage,
    columns: std::ops::Range<u32>,
    edge: EdgeReference,
    cancellation: &AnalysisCancellation,
) -> Result<u32> {
    let reverse = columns.start > 0;
    let mut consumed = 0;
    let iterator: Box<dyn Iterator<Item = u32>> = if reverse {
        Box::new(columns.rev())
    } else {
        Box::new(columns)
    };
    for x in iterator {
        cancellation.check()?;
        if !(0..image.height).all(|y| is_margin_pixel(image.pixel(x, y), edge)) {
            break;
        }
        consumed += 1;
    }
    Ok(if reverse {
        image.width.saturating_sub(consumed)
    } else {
        consumed
    })
}

fn scan_horizontal(
    image: &RgbaImage,
    rows: std::ops::Range<u32>,
    edge: EdgeReference,
    cancellation: &AnalysisCancellation,
) -> Result<u32> {
    let reverse = rows.start > 0;
    let mut consumed = 0;
    let iterator: Box<dyn Iterator<Item = u32>> = if reverse {
        Box::new(rows.rev())
    } else {
        Box::new(rows)
    };
    for y in iterator {
        cancellation.check()?;
        if !(0..image.width).all(|x| is_margin_pixel(image.pixel(x, y), edge)) {
            break;
        }
        consumed += 1;
    }
    Ok(if reverse {
        image.height.saturating_sub(consumed)
    } else {
        consumed
    })
}

fn is_margin_pixel(pixel: Option<[u8; 4]>, edge: EdgeReference) -> bool {
    let Some(pixel) = pixel else {
        return false;
    };
    if edge.transparent {
        pixel[3] <= 8
    } else {
        pixel[3] >= 247
            && channel_distance(
                pixel,
                [edge.color.r, edge.color.g, edge.color.b, edge.color.a],
            ) <= UNIFORM_DISTANCE
    }
}

fn visual_focus(
    image: &RgbaImage,
    inset: SourceInsets,
    source_scale: f64,
    edge: EdgeReference,
    cancellation: &AnalysisCancellation,
) -> Result<ResolvedFocus> {
    let left = (inset.left * source_scale)
        .round()
        .clamp(0.0, f64::from(image.width)) as u32;
    let top = (inset.top * source_scale)
        .round()
        .clamp(0.0, f64::from(image.height)) as u32;
    let right = image
        .width
        .saturating_sub((inset.right * source_scale).round().max(0.0) as u32)
        .max(left + 1);
    let bottom = image
        .height
        .saturating_sub((inset.bottom * source_scale).round().max(0.0) as u32)
        .max(top + 1);
    let width = right.saturating_sub(left);
    let height = bottom.saturating_sub(top);
    let step_x = sample_step(width);
    let step_y = sample_step(height);
    let mut total = 0u128;
    let mut sum_x = 0u128;
    let mut sum_y = 0u128;
    let mut active = 0u64;
    let mut sampled = 0u64;
    let reference = [edge.color.r, edge.color.g, edge.color.b, edge.color.a];

    for y in (top..bottom).step_by(step_y as usize) {
        cancellation.check()?;
        for x in (left..right).step_by(step_x as usize) {
            let pixel = image.pixel(x, y).unwrap_or([0, 0, 0, 0]);
            sampled += 1;
            let alpha = u64::from(pixel[3]);
            let distance = u64::from(channel_distance(pixel, reference));
            let saturation = u64::from(
                pixel[..3].iter().max().copied().unwrap_or(0)
                    - pixel[..3].iter().min().copied().unwrap_or(0),
            );
            let weight = if edge.transparent {
                alpha.saturating_mul(8 + saturation)
            } else {
                alpha.saturating_mul(distance.saturating_mul(3) + saturation)
            };
            if weight > 255 * 24 {
                active += 1;
            }
            total = total.saturating_add(u128::from(weight));
            sum_x = sum_x.saturating_add(u128::from(x - left) * u128::from(weight));
            sum_y = sum_y.saturating_add(u128::from(y - top) * u128::from(weight));
        }
    }
    if total == 0 || sampled == 0 {
        return Ok(ResolvedFocus::default());
    }
    let mut x = (sum_x * u128::from(NORMALIZED_MAX) / total / u128::from(width.max(1))) as u16;
    let mut y = (sum_y * u128::from(NORMALIZED_MAX) / total / u128::from(height.max(1))) as u16;
    let displacement = x.abs_diff(NORMALIZED_MAX / 2) + y.abs_diff(NORMALIZED_MAX / 2);
    if displacement < 300 {
        x = NORMALIZED_MAX / 2;
        y = NORMALIZED_MAX / 2;
    }
    let signal = ((active.saturating_mul(100) / sampled).min(100)) as u8;
    let confidence = if displacement < 300 {
        signal.min(20)
    } else {
        signal.clamp(20, 100)
    };
    Ok(ResolvedFocus {
        x: x.min(NORMALIZED_MAX),
        y: y.min(NORMALIZED_MAX),
        confidence,
    })
}

fn sampled_stats(
    image: &RgbaImage,
    edge: EdgeReference,
    cancellation: &AnalysisCancellation,
) -> Result<SampledStats> {
    let step_x = sample_step(image.width);
    let step_y = sample_step(image.height);
    let reference = [edge.color.r, edge.color.g, edge.color.b, edge.color.a];
    let mut samples = 0u64;
    let mut visible = 0u64;
    let mut foreground = 0u64;
    let mut detail = 0u64;
    let mut previous = None;
    for y in (0..image.height).step_by(step_y as usize) {
        cancellation.check()?;
        for x in (0..image.width).step_by(step_x as usize) {
            let pixel = image.pixel(x, y).unwrap_or([0; 4]);
            samples += 1;
            if pixel[3] > 8 {
                visible += 1;
            }
            if channel_distance(pixel, reference) > FOREGROUND_DISTANCE {
                foreground += 1;
            }
            if previous.is_some_and(|before| channel_distance(pixel, before) > 18) {
                detail += 1;
            }
            previous = Some(pixel);
        }
    }
    let percent = |count: u64| {
        count
            .saturating_mul(100)
            .checked_div(samples)
            .unwrap_or(0)
            .min(100) as u8
    };
    Ok(SampledStats {
        visible_percent: percent(visible),
        foreground_percent: percent(foreground),
        detail_percent: percent(detail),
        samples: samples.min(u64::from(u32::MAX)) as u32,
    })
}

fn classify(stats: &SampledStats) -> ContentClass {
    if stats.visible_percent < 92 {
        ContentClass::Transparent
    } else if stats.detail_percent > 55 && stats.foreground_percent > 55 {
        ContentClass::Photo
    } else if stats.foreground_percent < 28 {
        ContentClass::Sparse
    } else {
        ContentClass::Interface
    }
}

fn automatic_background(edge: EdgeReference, source_space: ColorSpace) -> AutomaticBackground {
    if edge.transparent || source_space == ColorSpace::Unknown {
        return AutomaticBackground::fallback(source_space);
    }
    let edge_color = Color::rgb(edge.color.r, edge.color.g, edge.color.b);
    let light_field = edge_color.luminance() < 0.34;
    let dominant = if edge_color.r >= edge_color.g && edge_color.r >= edge_color.b {
        0
    } else if edge_color.g >= edge_color.b {
        1
    } else {
        2
    };
    let (anchor_start, anchor_end) = match (light_field, dominant) {
        (true, 0) => (Color::rgb(205, 222, 242), Color::rgb(226, 202, 233)),
        (true, 1) => (Color::rgb(217, 211, 244), Color::rgb(188, 226, 216)),
        (true, _) => (Color::rgb(230, 210, 238), Color::rgb(185, 220, 239)),
        (false, 0) => (Color::rgb(42, 35, 62), Color::rgb(28, 53, 69)),
        (false, 1) => (Color::rgb(42, 34, 68), Color::rgb(27, 58, 55)),
        (false, _) => (Color::rgb(48, 33, 64), Color::rgb(27, 49, 72)),
    };
    let complement = Color::rgb(
        255_u8.saturating_sub(edge_color.r),
        255_u8.saturating_sub(edge_color.g),
        255_u8.saturating_sub(edge_color.b),
    );
    let mut start = mix_color(anchor_start, complement, 32);
    let mut end = mix_color(anchor_end, complement, 24);
    start = enforce_contrast(start, edge_color);
    end = enforce_contrast(end, edge_color);
    let minimum = contrast_ratio(start, edge_color).min(contrast_ratio(end, edge_color));
    AutomaticBackground {
        algorithm_version: SMART_FRAME_ALGORITHM_VERSION,
        start,
        end,
        edge_reference: edge_color,
        source_color_space: source_space,
        minimum_contrast_x100: (minimum * 100.0).round().clamp(0.0, 2_100.0) as u16,
        extensions: BTreeMap::new(),
    }
}

fn enforce_contrast(mut candidate: Color, edge: Color) -> Color {
    let toward = if edge.luminance() < 0.34 {
        Color::WHITE
    } else {
        Color::BLACK
    };
    for _ in 0..16 {
        if contrast_ratio(candidate, edge) >= 3.0 {
            return candidate;
        }
        candidate = mix_color(candidate, toward, 28);
    }
    toward
}

fn mix_color(a: Color, b: Color, amount: u8) -> Color {
    let channel = |left: u8, right: u8| {
        ((u16::from(left) * u16::from(255 - amount) + u16::from(right) * u16::from(amount) + 127)
            / 255) as u8
    };
    Color::rgb(channel(a.r, b.r), channel(a.g, b.g), channel(a.b, b.b))
}

/// WCAG contrast ratio between two opaque sRGB colours.
#[must_use]
pub fn contrast_ratio(a: Color, b: Color) -> f32 {
    let (lighter, darker) = if a.luminance() >= b.luminance() {
        (a.luminance(), b.luminance())
    } else {
        (b.luminance(), a.luminance())
    };
    (lighter + 0.05) / (darker + 0.05)
}

fn mean_color(samples: &[[u8; 4]]) -> Color {
    let mut sum = [0u64; 4];
    for pixel in samples {
        for (total, channel) in sum.iter_mut().zip(pixel) {
            *total += u64::from(*channel);
        }
    }
    let divisor = samples.len().max(1) as u64;
    Color::rgba(
        (sum[0] / divisor) as u8,
        (sum[1] / divisor) as u8,
        (sum[2] / divisor) as u8,
        (sum[3] / divisor) as u8,
    )
}

fn channel_distance(a: [u8; 4], b: [u8; 4]) -> u32 {
    a.into_iter()
        .zip(b)
        .map(|(left, right)| u32::from(left.abs_diff(right)))
        .max()
        .unwrap_or(0)
}

fn sample_step(length: u32) -> u32 {
    let side = (MAX_ANALYSIS_SAMPLES as f64).sqrt() as u32;
    length.div_ceil(side.max(1)).max(1)
}

fn quantize_even(value: u32) -> u32 {
    value.div_ceil(2) * 2
}
