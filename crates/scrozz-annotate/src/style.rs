//! Per-object appearance: colour, stroke, fill, opacity, type size.
//!
//! Style is attached to each annotation rather than held globally, because the
//! user expects the arrow they drew in red to *stay* red when they next pick
//! yellow. Per decision D14 an annotation stays editable forever, so its
//! appearance has to be part of the persisted object, not a transient tool
//! setting.

use scrozz_core::LogicalPoint;
use serde::{Deserialize, Serialize};

/// An sRGB colour with straight — **not** premultiplied — alpha.
///
/// Straight alpha is the authoring representation: it is what a colour picker
/// produces and what a user reasons about. Premultiplication happens once, at
/// the rasteriser boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Color {
    /// Red, 0–255.
    pub r: u8,
    /// Green, 0–255.
    pub g: u8,
    /// Blue, 0–255.
    pub b: u8,
    /// Alpha, 0–255, where 255 is opaque.
    pub a: u8,
}

impl Color {
    /// Fully transparent.
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);
    /// Opaque black.
    pub const BLACK: Self = Self::rgb(0, 0, 0);
    /// Opaque white.
    pub const WHITE: Self = Self::rgb(255, 255, 255);
    /// The default annotation colour — a strong, legible red.
    pub const ACCENT: Self = Self::rgb(0xE5, 0x24, 0x2C);
    /// The default highlighter colour.
    pub const HIGHLIGHT: Self = Self::rgba(0xFF, 0xE0, 0x3A, 0xB3);

    /// An opaque colour.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// A colour with explicit alpha.
    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Whether this colour contributes nothing when composited.
    #[must_use]
    pub const fn is_invisible(self) -> bool {
        self.a == 0
    }

    /// The same colour with alpha scaled by `factor`, clamped to 0–1.
    #[must_use]
    pub fn scaled_alpha(self, factor: f32) -> Self {
        let a = f32::from(self.a) * factor.clamp(0.0, 1.0);
        Self {
            a: a.round().clamp(0.0, 255.0) as u8,
            ..self
        }
    }

    /// Relative luminance, 0–1, used to choose legible label text.
    #[must_use]
    pub fn luminance(self) -> f32 {
        let channel = |c: u8| {
            let c = f32::from(c) / 255.0;
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.072_2f32.mul_add(
            channel(self.b),
            0.212_6f32.mul_add(channel(self.r), 0.715_2 * channel(self.g)),
        )
    }

    /// Black or white, whichever stays legible on top of this colour.
    #[must_use]
    pub fn contrasting(self) -> Self {
        if self.luminance() > 0.45 {
            Self::BLACK
        } else {
            Self::WHITE
        }
    }

    /// The readable editor-palette name nearest this colour.
    ///
    /// Documents can contain arbitrary RGBA values, so the nearest named swatch
    /// is more useful to assistive technology than a generic "custom colour".
    #[must_use]
    pub fn name(self) -> &'static str {
        if self.a == 0 {
            return "Transparent";
        }

        const NAMED: &[(Color, &str)] = &[
            (Color::WHITE, "White"),
            (Color::rgb(200, 205, 214), "Silver"),
            (Color::rgb(116, 124, 138), "Slate"),
            (Color::rgb(25, 27, 33), "Black"),
            (Color::rgb(255, 74, 85), "Red"),
            (Color::rgb(255, 145, 52), "Orange"),
            (Color::rgb(255, 211, 69), "Yellow"),
            (Color::rgb(61, 207, 142), "Green"),
            (Color::rgb(61, 139, 255), "Blue"),
            (Color::rgb(121, 103, 255), "Indigo"),
            (Color::rgb(185, 91, 255), "Purple"),
            (Color::rgb(255, 88, 177), "Pink"),
        ];

        NAMED
            .iter()
            .min_by_key(|(candidate, _)| color_distance(self, *candidate))
            .map_or("Custom", |(_, name)| *name)
    }
}

fn color_distance(left: Color, right: Color) -> u32 {
    let dr = i32::from(left.r) - i32::from(right.r);
    let dg = i32::from(left.g) - i32::from(right.g);
    let db = i32::from(left.b) - i32::from(right.b);
    u32::try_from(dr * dr + dg * dg + db * db).unwrap_or(u32::MAX)
}

impl Default for Color {
    fn default() -> Self {
        Self::ACCENT
    }
}

/// The geometry and endings used by an arrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArrowStyle {
    /// A straight shaft with one filled arrowhead.
    #[default]
    Straight,
    /// A gently bowed shaft with one filled arrowhead.
    Curved,
    /// A straight shaft with filled arrowheads at both ends.
    DoubleEnded,
    /// A dashed shaft with one filled arrowhead.
    Dashed,
}

impl ArrowStyle {
    /// Every style, in the editor's stable menu order.
    pub const ALL: [Self; 4] = [
        Self::Straight,
        Self::Curved,
        Self::DoubleEnded,
        Self::Dashed,
    ];

    /// The label shown in the editor.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Straight => "Straight",
            Self::Curved => "Curved",
            Self::DoubleEnded => "Double ended",
            Self::Dashed => "Dashed",
        }
    }
}

/// One of the seven text treatments exposed by the annotation editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextPreset {
    /// Inter with no surrounding treatment.
    #[default]
    Standard,
    /// A softer, heavier Inter treatment.
    Rounded,
    /// Fixed advances for code, dimensions, and data.
    Monospaced,
    /// Filled glyphs with a contrasting outline.
    Outlined,
    /// A compact square-cornered label.
    Boxed,
    /// A compact rounded label.
    RoundedBoxed,
    /// A boxed label using fixed advances.
    MonospacedBoxed,
}

impl TextPreset {
    /// Every preset, in the editor's stable menu order.
    pub const ALL: [Self; 7] = [
        Self::Standard,
        Self::Rounded,
        Self::Monospaced,
        Self::Outlined,
        Self::Boxed,
        Self::RoundedBoxed,
        Self::MonospacedBoxed,
    ];

    /// The label shown in the editor.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Standard => "Standard",
            Self::Rounded => "Rounded",
            Self::Monospaced => "Monospaced",
            Self::Outlined => "Outlined",
            Self::Boxed => "Boxed",
            Self::RoundedBoxed => "Rounded Boxed",
            Self::MonospacedBoxed => "Monospaced Boxed",
        }
    }

    /// Whether this preset uses one fixed horizontal advance per character.
    #[must_use]
    pub const fn is_monospaced(self) -> bool {
        matches!(self, Self::Monospaced | Self::MonospacedBoxed)
    }

    /// Whether this preset paints text over a label background.
    #[must_use]
    pub const fn is_boxed(self) -> bool {
        matches!(
            self,
            Self::Boxed | Self::RoundedBoxed | Self::MonospacedBoxed
        )
    }
}

/// How one annotation is drawn.
///
/// All measurements are logical points, matching the coordinates annotations
/// are authored in, so a style is as resolution-independent as the geometry it
/// decorates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Style {
    /// Outline colour, and the fill colour for shapes that are inherently
    /// solid — counters, solid redactions, text.
    pub stroke: Color,
    /// Outline width in logical points. Clamped to a sane minimum on use.
    pub stroke_width: f64,
    /// Interior fill, if any. `None` leaves the shape hollow.
    pub fill: Option<Color>,
    /// Whole-object opacity, 0–1, multiplied into every colour.
    pub opacity: f32,
    /// Cap height for text and counter labels, in logical points.
    pub font_size: f64,
    /// Geometry and endings for arrow annotations.
    #[serde(default)]
    pub arrow_style: ArrowStyle,
    /// Explicit quadratic control point for a curved arrow.
    ///
    /// `None` preserves the legacy automatically bowed curve.
    #[serde(default)]
    pub curve_control: Option<LogicalPoint>,
    /// Strength of destructive blur or pixelation in `0.0..=1.0`.
    #[serde(default = "default_redact_strength")]
    pub redact_strength: f32,
    /// Whether ordinary annotation objects cast a small drop shadow.
    #[serde(default)]
    pub shadow: bool,
    /// The text treatment used by text annotations.
    #[serde(default)]
    pub text_preset: TextPreset,
}

impl Style {
    /// The smallest stroke that still renders as a visible line.
    pub const MIN_STROKE_WIDTH: f64 = 0.5;

    /// The default look for outlined shapes and arrows.
    #[must_use]
    pub fn stroked() -> Self {
        Self::default()
    }

    /// The default look for a highlighter.
    #[must_use]
    pub fn highlighter() -> Self {
        Self {
            stroke: Color::HIGHLIGHT,
            fill: Some(Color::HIGHLIGHT),
            ..Self::default()
        }
    }

    /// The default dimming layer used by a spotlight.
    #[must_use]
    pub fn spotlight() -> Self {
        Self {
            stroke: Color::BLACK,
            fill: Some(Color::BLACK),
            stroke_width: 0.0,
            opacity: 0.62,
            ..Self::default()
        }
    }

    /// The default look for a redaction: opaque black, no outline.
    #[must_use]
    pub fn redaction() -> Self {
        Self {
            stroke: Color::BLACK,
            fill: Some(Color::BLACK),
            stroke_width: 0.0,
            ..Self::default()
        }
    }

    /// This style with a different stroke colour.
    #[must_use]
    pub fn with_stroke(mut self, color: Color) -> Self {
        self.stroke = color;
        self
    }

    /// This style with a different stroke width.
    #[must_use]
    pub fn with_stroke_width(mut self, width: f64) -> Self {
        self.stroke_width = width;
        self
    }

    /// This style with a fill.
    #[must_use]
    pub fn with_fill(mut self, color: Option<Color>) -> Self {
        self.fill = color;
        self
    }

    /// This style with a different whole-object opacity.
    #[must_use]
    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }

    /// This style with a different type size.
    #[must_use]
    pub fn with_font_size(mut self, size: f64) -> Self {
        self.font_size = size;
        self
    }

    /// This style with a different arrow treatment.
    #[must_use]
    pub fn with_arrow_style(mut self, arrow_style: ArrowStyle) -> Self {
        self.arrow_style = arrow_style;
        self
    }

    /// This style with an explicit curved-arrow control point.
    #[must_use]
    pub fn with_curve_control(mut self, control: LogicalPoint) -> Self {
        self.curve_control = Some(control);
        self
    }

    /// Stroke width clamped away from zero and non-finite values.
    #[must_use]
    pub fn effective_stroke_width(&self) -> f64 {
        if self.stroke_width.is_finite() {
            self.stroke_width.max(Self::MIN_STROKE_WIDTH)
        } else {
            Self::MIN_STROKE_WIDTH
        }
    }

    /// Opacity clamped to 0–1, with non-finite values treated as opaque.
    #[must_use]
    pub fn effective_opacity(&self) -> f32 {
        if self.opacity.is_finite() {
            self.opacity.clamp(0.0, 1.0)
        } else {
            1.0
        }
    }

    /// Type size clamped away from zero and non-finite values.
    #[must_use]
    pub fn effective_font_size(&self) -> f64 {
        if self.font_size.is_finite() {
            self.font_size.max(1.0)
        } else {
            16.0
        }
    }

    /// Redaction strength clamped to `0.0..=1.0`.
    #[must_use]
    pub fn effective_redact_strength(&self) -> f32 {
        if self.redact_strength.is_finite() {
            self.redact_strength.clamp(0.0, 1.0)
        } else {
            default_redact_strength()
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Self {
            stroke: Color::ACCENT,
            stroke_width: 4.0,
            fill: None,
            opacity: 1.0,
            font_size: 18.0,
            arrow_style: ArrowStyle::default(),
            curve_control: None,
            redact_strength: default_redact_strength(),
            shadow: false,
            text_preset: TextPreset::default(),
        }
    }
}

const fn default_redact_strength() -> f32 {
    0.65
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_style_json_receives_new_defaults() {
        let style: Style = serde_json::from_str(
            r#"{
                "stroke":{"r":229,"g":36,"b":44,"a":255},
                "stroke_width":4.0,
                "fill":null,
                "opacity":1.0,
                "font_size":18.0
            }"#,
        )
        .expect("legacy styles load");

        assert_eq!(style.arrow_style, ArrowStyle::Straight);
        assert_eq!(style.redact_strength, 0.65);
        assert!(!style.shadow);
        assert_eq!(style.text_preset, TextPreset::Standard);
    }

    #[test]
    fn arbitrary_colors_get_a_stable_readable_name() {
        assert_eq!(Color::rgba(248, 80, 90, 255).name(), "Red");
        assert_eq!(Color::rgba(200, 100, 10, 0).name(), "Transparent");
    }
}
