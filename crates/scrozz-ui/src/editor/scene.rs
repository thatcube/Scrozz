//! The harness [`Scene`] that draws the editor for a golden.
//!
//! Everything it needs is synthesised from the fixture: a capture, some
//! annotations, and a fixed instant. No filesystem, no clock, no RNG beyond the
//! fixture's seed — the harness contract, and the reason the golden means
//! anything.

#![allow(missing_docs)]

use scrozz_annotate::{Annotation, Color, Document, RedactStyle, Style};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Provenance, ScaleFactor,
};
use std::sync::{Arc, Mutex, PoisonError};

use crate::harness::{Fixture, Scene, SceneCtx};
use crate::theme::{self, Appearance, Theme};

use super::EditorUi;

/// Draws the annotation editor.
#[derive(Debug, Default)]
pub struct EditorScene;

impl Scene for EditorScene {
    fn name(&self) -> &str {
        "editor"
    }

    fn setup(&self, ctx: &egui::Context) {
        theme::install_fonts(ctx);
    }

    fn ui(&self, ui: &mut egui::Ui, ctx: &SceneCtx<'_>) {
        let theme = if ctx.theme == egui::Theme::Dark {
            Theme::for_appearance(Appearance::Dark)
        } else {
            Theme::for_appearance(Appearance::Light)
        };
        theme::install_style(ui.ctx(), &theme);

        // Built once and kept in egui's memory for the same reason the icon
        // store is: the editor owns texture handles, and a handle dropped in
        // the pass that created it uploads and frees within one texture delta,
        // so the preview would paint as nothing at all. Keeping it also means
        // the warm-up passes settle a real editor rather than four fresh ones.
        // Scenario-local: the golden renderer reuses one egui context across the
        // corpus, and an annotating scene must not donate its closed popover to
        // the dedicated open-popover scenario that follows it.
        let id = egui::Id::new(("scrozz-editor-scene", ctx.fixture.scenario.slug()));
        let editor = match ui.ctx().data(|data| data.get_temp::<Shared>(id)) {
            Some(existing) => existing,
            None => {
                let mut editor = EditorUi::new(sample_document(ctx.fixture, ctx.seed));
                prime(ctx.fixture, &mut editor);
                let shared: Shared = Arc::new(Mutex::new(editor));
                ui.ctx()
                    .data_mut(|data| data.insert_temp(id, shared.clone()));
                shared
            }
        };
        let mut editor = editor.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = editor.update(ui);
    }
}

/// Shorthand for the editor the scene keeps between passes.
type Shared = Arc<Mutex<EditorUi>>;

/// Puts the editor into the state the scenario is meant to show.
fn prime(fixture: &Fixture, editor: &mut EditorUi) {
    if !fixture.annotating {
        return;
    }
    let state = editor.state_mut();
    if fixture.scenario == crate::harness::Scenario::EditorCrop {
        state.set_tool(super::Tool::Crop);
        state.set_crop_aspect(super::CropAspect::Landscape16x9);
        state.set_crop_width(state.document().logical_size().width * 0.68);
        return;
    }
    state.set_tool(super::Tool::Arrow);
    // Keep Arrow in hand while selecting the sample arrow, so the golden proves
    // the tool can edit an existing arrow and paints endpoint chrome only.
    let arrow = state
        .document()
        .annotations()
        .iter()
        .find(|object| matches!(object.annotation, Annotation::Arrow { .. }))
        .map(|object| object.id);
    state.select(arrow);
    if fixture.scenario == crate::harness::Scenario::EditorColorPopover {
        editor.open_color_popover();
    } else if fixture.scenario == crate::harness::Scenario::EditorArrowStyles {
        editor
            .state_mut()
            .set_arrow_style(scrozz_annotate::ArrowStyle::Curved);
        editor.state_mut().set_stroke_width(8.0);
        editor.open_arrow_popover();
    }
}

/// A document with one of each interesting annotation on a synthetic capture.
fn sample_document(fixture: &Fixture, seed: u64) -> Document {
    let (w, h) = (
        f64::from(fixture.size_pt.0).max(320.0),
        f64::from(fixture.size_pt.1).max(240.0)
            - f64::from(super::toolbar::height_for(fixture.size_pt.0)),
    );
    let size = LogicalSize::new(w, h);
    let mut document = Document::new(synthetic_capture(size, seed));

    let red = Style::stroked()
        .with_stroke(Color::ACCENT)
        .with_stroke_width(4.0);
    let blue = Style::stroked()
        .with_stroke(Color::rgb(0x0A, 0x84, 0xFF))
        .with_stroke_width(4.0);

    document.add(
        Annotation::Rectangle(rect(w * 0.07, h * 0.18, w * 0.22, h * 0.26)),
        red,
    );
    document.add(
        Annotation::Arrow {
            from: LogicalPoint::new(w * 0.46, h * 0.62),
            to: LogicalPoint::new(w * 0.66, h * 0.30),
        },
        red,
    );
    document.add(
        Annotation::Ellipse(rect(w * 0.60, h * 0.52, w * 0.22, h * 0.24)),
        blue,
    );
    document.add(
        Annotation::Highlight(rect(w * 0.10, h * 0.60, w * 0.26, h * 0.08)),
        Style::highlighter(),
    );
    document.add(
        Annotation::Redact {
            area: rect(w * 0.34, h * 0.08, w * 0.17, h * 0.13),
            style: RedactStyle::Pixelate,
        },
        Style::redaction(),
    );
    document.add(
        Annotation::Redact {
            area: rect(w * 0.53, h * 0.08, w * 0.17, h * 0.13),
            style: RedactStyle::Blur,
        },
        Style::redaction(),
    );
    document.add(
        Annotation::Counter {
            at: LogicalPoint::new(w * 0.86, h * 0.20),
            index: 1,
        },
        red.with_fill(Some(Color::ACCENT)),
    );
    document.add(
        Annotation::Text {
            at: LogicalPoint::new(w * 0.10, h * 0.82),
            content: "Ship it".to_owned(),
        },
        red.with_font_size(22.0),
    );
    document
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> LogicalRect {
    LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(w, h))
}

/// A deterministic stand-in for a real screenshot.
///
/// A soft gradient with a faint grid and a band of fake text rules. The rules
/// matter: pixelating a smooth gradient looks identical to not pixelating it,
/// so a golden over flat content would pass whether or not redaction ran.
fn synthetic_capture(size: LogicalSize, seed: u64) -> Capture {
    let scale = ScaleFactor::new(2.0);
    let w = (size.width * 2.0).round().max(1.0) as u32;
    let h = (size.height * 2.0).round().max(1.0) as u32;
    let stride = w as usize * 4;
    let mut data = vec![0u8; stride * h as usize];

    let tint = (seed % 3) as u8;
    let band = (h as f32 * 0.06)..(h as f32 * 0.42);
    for y in 0..h as usize {
        let fy = y as f32 / h as f32;
        for x in 0..w as usize {
            let fx = x as f32 / w as f32;
            let base = 0.86 - fy * 0.22 - fx * 0.06;
            let grid = f32::from(u8::from(x % 96 < 2 || y % 96 < 2)) * 0.05;
            // Fake text: 6 px rules on a 22 px rhythm, broken into words.
            let rule = band.contains(&(y as f32))
                && (y % 22) < 6
                && (x % 190) < 150
                && (x as f32) > w as f32 * 0.30
                && (x as f32) < w as f32 * 0.72;
            let v = if rule {
                0.30
            } else {
                (base - grid).clamp(0.0, 1.0)
            };
            let v = (v * 255.0) as u8;
            let p = y * stride + x * 4;
            data[p] = v.saturating_sub(tint * 6);
            data[p + 1] = v;
            data[p + 2] = v.saturating_add(10);
            data[p + 3] = 255;
        }
    }

    Capture {
        frame: Frame {
            data,
            size: PhysicalSize::new(f64::from(w), f64::from(h)),
            stride,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale,
        },
        provenance: Provenance::Region,
        target: CaptureTarget::Region(LogicalRect::new(LogicalPoint::new(0.0, 0.0), size)),
    }
}
