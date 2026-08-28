//! The editor's toolbar: tool palette, colour, stroke width, and actions.
//!
//! Everything is drawn into an explicit rectangle rather than laid out by
//! egui's `Layout`, matching the rest of Scrozz's surfaces: the toolbar has to
//! stay put while the canvas underneath changes size, and a layout that reflows
//! would make every golden a hostage to text metrics.

use egui::{Rect, Ui, pos2, vec2};
use scrozz_annotate::{Color, Style};

use crate::icons::Icon;
use crate::paint::{
    ControlState, Reveal, Surface, color_swatch, divider_v, glass_panel, icon_button, stroke_width,
};
use crate::theme::{Radius, Space, corner};

use super::paint::CanvasView;
use super::state::{EditorState, Intent, Tool};

/// The toolbar's height, in points.
pub const HEIGHT: f32 = 52.0;

/// A tool or action button's side, in points.
const BUTTON: f32 = 30.0;

/// A colour swatch's side, in points.
const SWATCH: f32 = 18.0;

pub use super::state::{STROKE_MAX, STROKE_MIN};

/// The annotation palette.
///
/// Eight colours, not a picker. A picker is a second window and a colour space
/// argument; eight well-chosen colours cover every real annotation and can be
/// reached with one click. The first is Scrozz's accent, so the default needs
/// no choosing at all.
pub const PALETTE: [Color; 8] = [
    Color::ACCENT,
    Color::rgb(0xFF, 0x9F, 0x0A),
    Color::rgb(0xFF, 0xD6, 0x0A),
    Color::rgb(0x30, 0xD1, 0x58),
    Color::rgb(0x0A, 0x84, 0xFF),
    Color::rgb(0xBF, 0x5A, 0xF2),
    Color::WHITE,
    Color::BLACK,
];

/// The icon that stands for each tool.
#[must_use]
pub const fn tool_icon(tool: Tool) -> Icon {
    match tool {
        Tool::Select => Icon::Pointer,
        Tool::Arrow => Icon::ArrowUpRight,
        Tool::Line => Icon::Line,
        Tool::Rectangle => Icon::Square,
        Tool::Ellipse => Icon::Circle,
        Tool::Pen => Icon::Pencil,
        Tool::Text => Icon::LetterT,
        Tool::Highlight => Icon::Highlight,
        Tool::Blur => Icon::Droplet,
        Tool::Pixelate => Icon::GridDots,
        Tool::Counter => Icon::ListNumbers,
        Tool::Crop => Icon::Crop,
    }
}

/// Draws the toolbar, returning an intent if an action button was pressed.
pub fn draw(
    ui: &mut Ui,
    surface: &Surface<'_>,
    state: &mut EditorState,
    bar: Rect,
    _view: &CanvasView,
) -> Option<Intent> {
    let palette = surface.palette();
    glass_panel(ui.painter(), bar, 0.0, palette, true);
    divider_v(ui.painter(), bar.right(), bar.top(), bar.bottom(), palette);
    ui.painter().line_segment(
        [
            pos2(bar.left(), bar.bottom() - 0.5),
            pos2(bar.right(), bar.bottom() - 0.5),
        ],
        egui::Stroke::new(1.0, palette.hairline),
    );

    let mut x = bar.left() + Space::MD;
    let cy = bar.center().y;

    // Tools.
    let mut picked = None;
    for tool in Tool::ALL {
        let rect = Rect::from_center_size(pos2(x + BUTTON / 2.0, cy), vec2(BUTTON, BUTTON));
        let state_flags = ControlState::new().selected(state.tool() == tool);
        let response = icon_button(
            ui,
            surface,
            rect,
            ui.id().with(("editor-tool", tool)),
            tool_icon(tool),
            tool.label(),
            state_flags,
            Reveal::SHOWN,
        );
        if response.clicked() {
            picked = Some(tool);
        }
        let label = tool.label();
        let key = tool.accelerator().to_ascii_uppercase();
        response.on_hover_text(format!("{label}  ({key})"));
        x += BUTTON + Space::HAIR;
    }
    if let Some(tool) = picked {
        state.set_tool(tool);
    }

    x += Space::SM;
    divider_v(
        ui.painter(),
        x,
        bar.top() + Space::SM,
        bar.bottom() - Space::SM,
        palette,
    );
    x += Space::SM + Space::XS;

    // Colour.
    let mut chosen = None;
    for (index, color) in PALETTE.into_iter().enumerate() {
        let rect = Rect::from_center_size(pos2(x + SWATCH / 2.0, cy), vec2(SWATCH, SWATCH));
        let selected = state.stroke_color() == color;
        let response = color_swatch(
            ui,
            surface,
            rect,
            ui.id().with(("editor-color", index)),
            to_egui(color),
            "Colour",
            selected,
        );
        if response.clicked() {
            chosen = Some(color);
        }
        x += SWATCH + Space::SM;
    }
    if let Some(color) = chosen {
        state.set_stroke_color(color);
    }

    x += Space::XS;
    divider_v(
        ui.painter(),
        x,
        bar.top() + Space::SM,
        bar.bottom() - Space::SM,
        palette,
    );
    x += Space::SM + Space::XS;

    // Stroke width.
    let width_rect = Rect::from_center_size(pos2(x + 52.0, cy), vec2(104.0, BUTTON - 4.0));
    let fraction = width_fraction(state.stroke_width());
    let response = stroke_width(
        ui,
        surface,
        width_rect,
        ui.id().with("editor-stroke"),
        fraction,
    );
    if (response.dragged() || response.clicked())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let inner_l = width_rect.left() + Space::MD;
        let inner_r = width_rect.right() - Space::MD;
        let f = ((pos.x - inner_l) / (inner_r - inner_l)).clamp(0.0, 1.0);
        state.set_stroke_width(width_for_fraction(f));
    }
    response.on_hover_text(format!("Stroke {:.0} pt", state.stroke_width()));

    // Actions, right-aligned. Laid out from the right so the group stays
    // anchored to the window edge as the toolbar grows and shrinks.
    let mut rx = bar.right() - Space::MD - BUTTON;
    let mut intent = None;
    for (icon, label, action) in [
        (Icon::DeviceFloppy, "Save", Action::Intent(Intent::Save)),
        (Icon::Copy, "Copy", Action::Intent(Intent::Copy)),
        (Icon::ArrowForwardUp, "Redo", Action::Redo),
        (Icon::ArrowBackUp, "Undo", Action::Undo),
    ] {
        let rect = Rect::from_center_size(pos2(rx + BUTTON / 2.0, cy), vec2(BUTTON, BUTTON));
        let enabled = match action {
            Action::Undo => state.can_undo(),
            Action::Redo => state.can_redo(),
            Action::Intent(_) => true,
        };
        let flags = if enabled {
            ControlState::new()
        } else {
            ControlState::disabled()
        };
        let response = icon_button(
            ui,
            surface,
            rect,
            ui.id().with(("editor-action", label)),
            icon,
            label,
            flags,
            Reveal::SHOWN,
        );
        if response.clicked() && enabled {
            match action {
                Action::Undo => {
                    let _ = state.command(super::state::Command::Undo);
                }
                Action::Redo => {
                    let _ = state.command(super::state::Command::Redo);
                }
                Action::Intent(next) => intent = Some(next),
            }
        }
        response.on_hover_text(shortcut_hint(label));
        rx -= BUTTON + Space::HAIR;
    }

    // Crop confirmation sits beside the actions only while a crop is pending,
    // because a permanent "apply" button that is disabled 99% of the time is
    // just noise.
    if state.pending_crop().is_some() {
        rx -= Space::SM;
        let rect = Rect::from_center_size(pos2(rx + BUTTON / 2.0, cy), vec2(BUTTON, BUTTON));
        let response = icon_button(
            ui,
            surface,
            rect,
            ui.id().with("editor-apply-crop"),
            Icon::Check,
            "Apply crop",
            ControlState::on(),
            Reveal::SHOWN,
        );
        if response.clicked() {
            let _ = state.command(super::state::Command::ApplyCrop);
        }
        response.on_hover_text("Apply crop  (Return)");
    }

    intent
}

enum Action {
    Undo,
    Redo,
    Intent(Intent),
}

fn shortcut_hint(label: &str) -> String {
    let key = match label {
        "Save" => "⌘S",
        "Copy" => "⌘C",
        "Undo" => "⌘Z",
        "Redo" => "⇧⌘Z",
        _ => return label.to_owned(),
    };
    format!("{label}  ({key})")
}

/// Where a stroke width sits on the control, 0–1.
///
/// Perceptually spaced: the difference between 1 pt and 3 pt matters far more
/// than between 20 pt and 22 pt, so a linear track would waste most of its
/// travel on widths nobody picks.
#[must_use]
pub fn width_fraction(width: f64) -> f32 {
    let clamped = width.clamp(STROKE_MIN, STROKE_MAX);
    let t = (clamped / STROKE_MIN).ln() / (STROKE_MAX / STROKE_MIN).ln();
    t as f32
}

/// The stroke width at a position on the control.
#[must_use]
pub fn width_for_fraction(fraction: f32) -> f64 {
    let t = f64::from(fraction.clamp(0.0, 1.0));
    STROKE_MIN * (STROKE_MAX / STROKE_MIN).powf(t)
}

/// Converts an annotation colour into an egui one.
#[must_use]
pub fn to_egui(color: Color) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a)
}

/// Draws the style the next annotation will have, as a stroke sample.
///
/// Small, but it answers "what will this look like" without drawing one and
/// undoing it, which is exactly the question the two controls beside it raise.
pub fn draw_style_preview(ui: &Ui, surface: &Surface<'_>, rect: Rect, style: &Style) {
    let painter = ui.painter();
    painter.rect_filled(rect, corner(Radius::CHIP), surface.palette().chip_fill);
    let w = style.effective_stroke_width() as f32;
    painter.line_segment(
        [
            pos2(rect.left() + Space::SM, rect.center().y),
            pos2(rect.right() - Space::SM, rect.center().y),
        ],
        egui::Stroke::new(w.clamp(1.0, rect.height() - 4.0), to_egui(style.stroke)),
    );
}
