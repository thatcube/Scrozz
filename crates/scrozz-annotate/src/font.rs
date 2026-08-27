//! Deterministic text shaping over an embedded Inter face.
//!
//! Text never consults the operating system. The same Inter bytes are parsed by
//! `ttf-parser`, shaped from cmap/advance data, and converted to filled
//! `tiny-skia` outlines on every platform. That keeps annotation exports and
//! golden images byte-stable while rendering real lowercase and Unicode glyphs.

use scrozz_core::{LogicalPoint, LogicalSize};
use tiny_skia::{Path, PathBuilder, Rect};
use ttf_parser::{Face, GlyphId, OutlineBuilder};

use crate::style::TextPreset;

const INTER: &[u8] = include_bytes!("../../scrozz-ui/assets/fonts/Inter-Regular.ttf");
const DEFAULT_LINE_HEIGHT: f64 = 1.22;
const MONO_ADVANCE: f64 = 0.64;

#[derive(Debug, Clone, Copy)]
struct Metrics {
    scale: f64,
    ascender: f64,
    line_height: f64,
    mono_advance: f64,
}

impl Metrics {
    fn new(face: &Face<'_>, font_size: f64) -> Self {
        let units = f64::from(face.units_per_em()).max(1.0);
        let scale = font_size.max(1.0) / units;
        let natural_height = f64::from(face.height() + face.line_gap()) * scale;
        Self {
            scale,
            ascender: f64::from(face.ascender()) * scale,
            line_height: natural_height.max(font_size * DEFAULT_LINE_HEIGHT),
            mono_advance: font_size * MONO_ADVANCE,
        }
    }
}

fn face() -> Option<Face<'static>> {
    Face::parse(INTER, 0).ok()
}

fn glyph(face: &Face<'_>, ch: char) -> GlyphId {
    face.glyph_index(ch)
        .or_else(|| face.glyph_index('\u{fffd}'))
        .unwrap_or(GlyphId(0))
}

fn advance(face: &Face<'_>, glyph: GlyphId, metrics: Metrics, preset: TextPreset) -> f64 {
    if preset.is_monospaced() {
        metrics.mono_advance
    } else {
        f64::from(face.glyph_hor_advance(glyph).unwrap_or(face.units_per_em())) * metrics.scale
    }
}

fn line_width(face: &Face<'_>, line: &str, metrics: Metrics, preset: TextPreset) -> f64 {
    line.chars()
        .map(|ch| {
            if ch == '\t' {
                metrics.mono_advance * 4.0
            } else {
                advance(face, glyph(face, ch), metrics, preset)
            }
        })
        .sum()
}

/// The measured text box using the standard Inter preset.
#[must_use]
pub fn measure(text: &str, font_size: f64) -> LogicalSize {
    measure_with_preset(text, font_size, TextPreset::Standard)
}

/// The measured text box for a specific text preset.
#[must_use]
pub fn measure_with_preset(text: &str, font_size: f64, preset: TextPreset) -> LogicalSize {
    let Some(face) = face() else {
        return fallback_measure(text, font_size, preset);
    };
    let metrics = Metrics::new(&face, font_size);
    let lines = text.split('\n').collect::<Vec<_>>();
    let widest = lines
        .iter()
        .map(|line| line_width(&face, line, metrics, preset))
        .fold(0.0_f64, f64::max);
    let height = if lines.is_empty() {
        font_size.max(1.0)
    } else {
        metrics.line_height * lines.len() as f64
    };
    LogicalSize::new(widest, height)
}

fn fallback_measure(text: &str, font_size: f64, preset: TextPreset) -> LogicalSize {
    let advance = if preset.is_monospaced() {
        MONO_ADVANCE
    } else {
        0.62
    };
    let mut widest = 0_usize;
    let mut lines = 0_usize;
    for line in text.split('\n') {
        widest = widest.max(line.chars().count());
        lines += 1;
    }
    LogicalSize::new(
        widest as f64 * advance * font_size,
        lines.max(1) as f64 * DEFAULT_LINE_HEIGHT * font_size,
    )
}

/// Filled Inter glyph outlines in logical document coordinates.
#[must_use]
pub fn outline(text: &str, at: LogicalPoint, font_size: f64, preset: TextPreset) -> Option<Path> {
    let face = face()?;
    let metrics = Metrics::new(&face, font_size);
    let mut path = PathBuilder::new();
    let mut any = false;

    for (row, line) in text.split('\n').enumerate() {
        let baseline = at.y + metrics.ascender + row as f64 * metrics.line_height;
        let mut cursor = at.x;
        for ch in line.chars() {
            if ch == '\t' {
                cursor += metrics.mono_advance * 4.0;
                continue;
            }
            let glyph = glyph(&face, ch);
            let mut sink = GlyphPath {
                path: &mut path,
                origin_x: cursor as f32,
                baseline: baseline as f32,
                scale: metrics.scale as f32,
            };
            if face.outline_glyph(glyph, &mut sink).is_some() {
                any = true;
            } else if !ch.is_whitespace() {
                let width = advance(&face, glyph, metrics, preset) as f32;
                let top = (baseline - metrics.ascender) as f32;
                if let Some(rect) = Rect::from_xywh(
                    cursor as f32,
                    top,
                    width.max(1.0),
                    font_size.max(1.0) as f32,
                ) {
                    path.push_rect(rect);
                    any = true;
                }
            }
            cursor += advance(&face, glyph, metrics, preset);
        }
    }

    any.then(|| path.finish()).flatten()
}

struct GlyphPath<'a> {
    path: &'a mut PathBuilder,
    origin_x: f32,
    baseline: f32,
    scale: f32,
}

impl GlyphPath<'_> {
    fn point(&self, x: f32, y: f32) -> (f32, f32) {
        (
            x.mul_add(self.scale, self.origin_x),
            (-y).mul_add(self.scale, self.baseline),
        )
    }
}

impl OutlineBuilder for GlyphPath<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.point(x, y);
        self.path.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.point(x, y);
        self.path.line_to(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (x1, y1) = self.point(x1, y1);
        let (x, y) = self.point(x, y);
        self.path.quad_to(x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (x1, y1) = self.point(x1, y1);
        let (x2, y2) = self.point(x2, y2);
        let (x, y) = self.point(x, y);
        self.path.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.path.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_inter_has_real_lowercase_and_unicode_glyphs() {
        let face = face().expect("embedded Inter parses");
        assert!(face.glyph_index('a').is_some());
        assert!(face.glyph_index('é').is_some());
        assert!(
            outline(
                "Scrozz café",
                LogicalPoint::new(0.0, 0.0),
                24.0,
                TextPreset::Standard,
            )
            .is_some()
        );
    }

    #[test]
    fn monospaced_preset_uses_fixed_advances() {
        let narrow = measure_with_preset("iiii", 20.0, TextPreset::Monospaced);
        let wide = measure_with_preset("WWWW", 20.0, TextPreset::Monospaced);
        assert_eq!(narrow.width, wide.width);
    }
}
