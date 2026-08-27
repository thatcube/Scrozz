//! Deterministic OpenType text shaping over embedded font faces.
//!
//! Inter remains the ordinary annotation face. Noto Sans Mono is a real
//! monospaced face for code presets, while script-specific Noto fallbacks cover
//! Arabic and Hebrew bidirectional runs. Rustybuzz applies GSUB, GPOS, kerning,
//! ligatures and mark positioning; `unicode-bidi` supplies visual run order.
//! Rendering still ends as filled `tiny-skia` outlines, so no platform font
//! service can make exports differ between machines.

use rustybuzz::{
    Direction, Face, UnicodeBuffer, shape,
    ttf_parser::{GlyphId, OutlineBuilder},
};
use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};
use tiny_skia::{Path, PathBuilder};
use unicode_bidi::BidiInfo;
use unicode_segmentation::UnicodeSegmentation;

use crate::style::TextPreset;

const INTER: &[u8] = include_bytes!("../../scrozz-ui/assets/fonts/Inter-Regular.ttf");
const NOTO_MONO: &[u8] = include_bytes!("../../scrozz-ui/assets/fonts/NotoSansMono.ttf");
const NOTO_ARABIC: &[u8] = include_bytes!("../../scrozz-ui/assets/fonts/NotoSansArabic.ttf");
const NOTO_HEBREW: &[u8] = include_bytes!("../../scrozz-ui/assets/fonts/NotoSansHebrew.ttf");
const DEFAULT_LINE_HEIGHT: f64 = 1.22;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FontKind {
    Inter,
    Mono,
    Arabic,
    Hebrew,
}

impl FontKind {
    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Inter => INTER,
            Self::Mono => NOTO_MONO,
            Self::Arabic => NOTO_ARABIC,
            Self::Hebrew => NOTO_HEBREW,
        }
    }

    fn face(self) -> Face<'static> {
        Face::from_slice(self.bytes(), 0).expect("embedded annotation font must parse")
    }

    fn supports(self, ch: char) -> bool {
        ch.is_whitespace() || ch.is_control() || self.face().glyph_index(ch).is_some()
    }
}

#[derive(Debug, Clone, Copy)]
struct Metrics {
    scale: f64,
    ascender: f64,
    line_height: f64,
}

impl Metrics {
    fn new(face: &Face<'_>, font_size: f64) -> Self {
        let units = f64::from(face.units_per_em()).max(1.0);
        let scale = font_size.max(1.0) / units;
        let natural_height = f64::from(face.height() + face.line_gap()) * scale;
        Self {
            scale,
            ascender: f64::from(face.ascender()) * scale,
            line_height: natural_height.max(font_size.max(1.0) * DEFAULT_LINE_HEIGHT),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PositionedGlyph {
    font: FontKind,
    id: GlyphId,
    x: f64,
    y_offset: f64,
}

#[derive(Debug)]
struct ShapedLine {
    glyphs: Vec<PositionedGlyph>,
    width: f64,
}

fn primary_font(preset: TextPreset) -> FontKind {
    if preset.is_monospaced() {
        FontKind::Mono
    } else {
        FontKind::Inter
    }
}

fn font_for_cluster(preset: TextPreset, cluster: &str) -> FontKind {
    let primary = primary_font(preset);
    for font in [
        primary,
        FontKind::Arabic,
        FontKind::Hebrew,
        FontKind::Mono,
        FontKind::Inter,
    ] {
        if cluster.chars().all(|ch| font.supports(ch)) {
            return font;
        }
    }
    primary
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FontSpan {
    range: std::ops::Range<usize>,
    font: FontKind,
}

fn font_spans(text: &str, preset: TextPreset) -> Vec<FontSpan> {
    let mut spans: Vec<FontSpan> = Vec::new();
    for (start, cluster) in text.grapheme_indices(true) {
        let end = start + cluster.len();
        let is_spacing = cluster
            .chars()
            .all(|ch| ch.is_whitespace() || ch.is_control());
        let font = if is_spacing {
            spans.last().map_or_else(
                || {
                    text[end..]
                        .graphemes(true)
                        .find(|next| !next.chars().all(|ch| ch.is_whitespace() || ch.is_control()))
                        .map_or_else(
                            || primary_font(preset),
                            |next| font_for_cluster(preset, next),
                        )
                },
                |span| span.font,
            )
        } else {
            font_for_cluster(preset, cluster)
        };
        if let Some(span) = spans.last_mut()
            && span.font == font
        {
            span.range.end = end;
        } else {
            spans.push(FontSpan {
                range: start..end,
                font,
            });
        }
    }
    spans
}

fn shape_line(line: &str, font_size: f64, preset: TextPreset) -> ShapedLine {
    let expanded = line.replace('\t', "    ");
    if expanded.is_empty() {
        return ShapedLine {
            glyphs: Vec::new(),
            width: 0.0,
        };
    }

    let bidi = BidiInfo::new(&expanded, None);
    let Some(paragraph) = bidi.paragraphs.first() else {
        return ShapedLine {
            glyphs: Vec::new(),
            width: 0.0,
        };
    };
    let (levels, runs) = bidi.visual_runs(paragraph, paragraph.range.clone());
    let mut glyphs = Vec::new();
    let mut cursor = 0.0;

    for run in runs {
        let text = &expanded[run.clone()];
        let rtl = levels[run.start].is_rtl();
        let mut spans = font_spans(text, preset);
        if rtl {
            spans.reverse();
        }
        for span in spans {
            let face = span.font.face();
            let metrics = Metrics::new(&face, font_size);
            let mut buffer = UnicodeBuffer::new();
            buffer.push_str(&text[span.range]);
            buffer.set_direction(if rtl {
                Direction::RightToLeft
            } else {
                Direction::LeftToRight
            });
            buffer.guess_segment_properties();
            let shaped = shape(&face, &[], buffer);

            for (info, position) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
                glyphs.push(PositionedGlyph {
                    font: span.font,
                    id: GlyphId(info.glyph_id as u16),
                    x: cursor + f64::from(position.x_offset) * metrics.scale,
                    y_offset: f64::from(position.y_offset) * metrics.scale,
                });
                cursor += f64::from(position.x_advance) * metrics.scale;
            }
        }
    }

    ShapedLine {
        glyphs,
        width: cursor.max(0.0),
    }
}

/// The measured text box using the standard Inter preset.
#[must_use]
pub fn measure(text: &str, font_size: f64) -> LogicalSize {
    measure_with_preset(text, font_size, TextPreset::Standard)
}

/// The measured text box for a specific text preset.
#[must_use]
pub fn measure_with_preset(text: &str, font_size: f64, preset: TextPreset) -> LogicalSize {
    let face = primary_font(preset).face();
    let metrics = Metrics::new(&face, font_size);
    let mut widest = 0.0_f64;
    let mut line_count = 0_usize;
    for line in text.split('\n') {
        widest = widest.max(shape_line(line, font_size, preset).width);
        line_count += 1;
    }
    LogicalSize::new(widest, metrics.line_height * line_count.max(1) as f64)
}

/// Filled, shaped glyph outlines in logical document coordinates.
#[must_use]
pub fn outline(text: &str, at: LogicalPoint, font_size: f64, preset: TextPreset) -> Option<Path> {
    let primary = primary_font(preset).face();
    let metrics = Metrics::new(&primary, font_size);
    let mut path = PathBuilder::new();
    let mut any = false;

    for (row, line) in text.split('\n').enumerate() {
        let baseline = at.y + metrics.ascender + row as f64 * metrics.line_height;
        for glyph in shape_line(line, font_size, preset).glyphs {
            let face = glyph.font.face();
            let glyph_metrics = Metrics::new(&face, font_size);
            let mut sink = GlyphPath {
                path: &mut path,
                origin_x: (at.x + glyph.x) as f32,
                baseline: (baseline - glyph.y_offset) as f32,
                scale: glyph_metrics.scale as f32,
            };
            if face.outline_glyph(glyph.id, &mut sink).is_some() {
                any = true;
            }
        }
    }

    any.then(|| path.finish()).flatten()
}

/// The exact filled-outline bounds in logical document coordinates.
#[must_use]
pub fn ink_bounds(
    text: &str,
    at: LogicalPoint,
    font_size: f64,
    preset: TextPreset,
) -> Option<LogicalRect> {
    let bounds = outline(text, at, font_size, preset)?.bounds();
    Some(LogicalRect::new(
        LogicalPoint::new(f64::from(bounds.left()), f64::from(bounds.top())),
        LogicalSize::new(f64::from(bounds.width()), f64::from(bounds.height())),
    ))
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
    fn embedded_faces_have_expected_real_glyphs() {
        assert!(FontKind::Inter.face().glyph_index('é').is_some());
        assert!(FontKind::Mono.face().glyph_index('W').is_some());
        assert!(FontKind::Arabic.face().glyph_index('س').is_some());
        assert!(FontKind::Hebrew.face().glyph_index('א').is_some());
    }

    #[test]
    fn gpos_kerning_affects_layout() {
        let kerned = shape_line("AV", 24.0, TextPreset::Standard).width;
        let separate = shape_line("A", 24.0, TextPreset::Standard).width
            + shape_line("V", 24.0, TextPreset::Standard).width;
        assert!(kerned < separate, "Inter's GPOS kerning was not applied");
    }

    #[test]
    fn combining_marks_and_bidi_runs_are_shaped() {
        let decomposed = shape_line("e\u{301}", 24.0, TextPreset::Standard);
        let composed = shape_line("é", 24.0, TextPreset::Standard);
        assert_eq!(decomposed.glyphs.len(), composed.glyphs.len());
        assert_eq!(
            decomposed.glyphs.len(),
            1,
            "GSUB should compose the decomposed Latin cluster"
        );
        assert!((decomposed.width - composed.width).abs() < 0.01);

        let bidi = shape_line("abc אבג", 24.0, TextPreset::Standard);
        assert!(
            bidi.glyphs
                .iter()
                .any(|glyph| glyph.font == FontKind::Hebrew)
        );
        assert!(bidi.glyphs.iter().all(|glyph| glyph.id.0 != 0));
    }

    #[test]
    fn combining_marks_remain_with_their_script_font() {
        let arabic = font_spans("س\u{64e}لام", TextPreset::Standard);
        assert_eq!(arabic.len(), 1);
        assert_eq!(arabic[0].font, FontKind::Arabic);
        let shaped = shape_line("س\u{64e}لام", 24.0, TextPreset::Standard);
        assert!(
            shaped
                .glyphs
                .iter()
                .all(|glyph| glyph.font == FontKind::Arabic && glyph.id.0 != 0)
        );

        let hebrew = font_spans("ש\u{5b8}לום", TextPreset::Standard);
        assert_eq!(hebrew.len(), 1);
        assert_eq!(hebrew[0].font, FontKind::Hebrew);
    }

    #[test]
    fn mixed_script_runs_split_by_embedded_font_coverage() {
        let spans = font_spans("Latin سلام אבג", TextPreset::Standard);
        assert!(spans.iter().any(|span| span.font == FontKind::Inter));
        assert!(spans.iter().any(|span| span.font == FontKind::Arabic));
        assert!(spans.iter().any(|span| span.font == FontKind::Hebrew));

        let shaped = shape_line("Latin سلام אבג", 24.0, TextPreset::Standard);
        assert!(shaped.glyphs.iter().all(|glyph| glyph.id.0 != 0));
    }

    #[test]
    fn arabic_joining_uses_open_type_substitution() {
        let shaped = shape_line("سلام", 24.0, TextPreset::Standard);
        assert!(shaped.glyphs.iter().all(|glyph| glyph.id.0 != 0));
        let face = FontKind::Arabic.face();
        let nominal = "سلام"
            .chars()
            .map(|ch| face.glyph_index(ch).expect("Arabic glyph"))
            .collect::<Vec<_>>();
        let positioned = shaped
            .glyphs
            .iter()
            .map(|glyph| glyph.id)
            .collect::<Vec<_>>();
        assert_ne!(
            positioned, nominal,
            "Arabic contextual GSUB substitutions were not applied"
        );
    }

    #[test]
    fn monospaced_presets_use_the_embedded_mono_face() {
        let narrow = measure_with_preset("iiii", 20.0, TextPreset::Monospaced);
        let wide = measure_with_preset("WWWW", 20.0, TextPreset::Monospaced);
        assert!((narrow.width - wide.width).abs() < 0.001);

        let shaped = shape_line("code", 20.0, TextPreset::Monospaced);
        assert!(
            shaped
                .glyphs
                .iter()
                .all(|glyph| glyph.font == FontKind::Mono)
        );
    }

    #[test]
    fn shaping_produces_filled_outlines() {
        assert!(
            outline(
                "Scrozz café — אבג",
                LogicalPoint::new(0.0, 0.0),
                24.0,
                TextPreset::Standard,
            )
            .is_some()
        );
    }

    #[test]
    fn ink_bounds_include_real_glyph_overhangs() {
        let at = LogicalPoint::new(20.0, 30.0);
        let bounds = ink_bounds("jÁ", at, 40.0, TextPreset::Standard).expect("text has outlines");
        let layout = measure_with_preset("jÁ", 40.0, TextPreset::Standard);
        assert!(bounds.size.width > 0.0);
        assert!(bounds.size.height > 0.0);
        assert!(bounds.origin.x <= at.x + layout.width);
        assert!(bounds.origin.y < at.y + layout.height);
    }
}
