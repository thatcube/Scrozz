//! Per-object appearance: colour, stroke, fill, opacity, type size.
//!
//! Style is attached to each annotation rather than held globally, because the
//! user expects the arrow they drew in red to *stay* red when they next pick
//! yellow. Per decision D14 an annotation stays editable forever, so its
//! appearance has to be part of the persisted object, not a transient tool
//! setting.

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
}

impl Default for Color {
    fn default() -> Self {
        Self::ACCENT
    }
}

/// Shape language used by an arrow annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArrowStyle {
    /// Broad head and tapered shaft, the default.
    #[default]
    Bold,
    /// Single head on an editable quadratic curve.
    Curved,
    /// Deterministically varied hand-drawn silhouette.
    Sketch,
    /// Matching heads at both endpoints.
    Double,
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
    /// Arrow shape language. Ignored by non-arrow annotations.
    #[serde(default)]
    pub arrow_style: ArrowStyle,
    /// Signed curve amount relative to arrow length, clamped on use.
    #[serde(default)]
    pub arrow_bend: f64,
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

    /// This style with a different arrow shape language.
    #[must_use]
    pub fn with_arrow_style(mut self, style: ArrowStyle) -> Self {
        self.arrow_style = style;
        self
    }

    /// This style with a different signed arrow bend.
    #[must_use]
    pub fn with_arrow_bend(mut self, bend: f64) -> Self {
        self.arrow_bend = bend;
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

    /// Arrow bend clamped to a useful, non-self-intersecting range.
    #[must_use]
    pub fn effective_arrow_bend(&self) -> f64 {
        if self.arrow_bend.is_finite() {
            self.arrow_bend.clamp(-0.75, 0.75)
        } else {
            0.0
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
            arrow_style: ArrowStyle::Bold,
            arrow_bend: 0.0,
        }
    }
}
