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

/// The height of one row of controls, in points.
const ROW: f32 = 40.0;

/// The toolbar's height when everything fits on one row, in points.
pub const HEIGHT: f32 = ROW + Space::MD;

/// The toolbar's height when its controls wrap onto a second row.
pub const HEIGHT_WRAPPED: f32 = ROW * 2.0 + Space::MD;

/// A tool or action button's side, in points.
const BUTTON: f32 = 30.0;

/// A colour swatch's side, in points.
const SWATCH: f32 = 18.0;

/// The stroke-width slider's length, in points.
const STROKE: f32 = 104.0;

/// The width the tool palette occupies, including its trailing gap.
const TOOLS_W: f32 = Tool::ALL.len() as f32 * (BUTTON + Space::HAIR);

/// The width the colour swatches occupy, including their trailing gaps.
const SWATCHES_W: f32 = PALETTE.len() as f32 * (SWATCH + Space::SM);

/// The space a divider needs: the gap before it, and the gap after.
const DIVIDER_W: f32 = Space::SM + Space::SM + Space::XS;

/// The width of the right-hand action group, at its widest.
///
/// Four buttons, plus the crop confirmation that appears while a crop is
/// pending. Sized for the wide case so the toolbar does not start overlapping
/// itself the moment the user drags a crop out.
const ACTIONS_W: f32 = 4.0f32.mul_add(BUTTON, 3.0 * Space::HAIR) + Space::SM + BUTTON;

/// The width the whole toolbar needs to sit on a single row.
///
/// Below this the controls wrap, because the alternative — forcing the window
/// to be this wide — would stop the editor fitting beside anything else on a
/// laptop display.
pub const SINGLE_ROW_W: f32 = Space::MD
    + TOOLS_W
    + DIVIDER_W
    + SWATCHES_W
    + Space::XS
    + DIVIDER_W
    + STROKE
    + Space::MD
    + ACTIONS_W
    + Space::MD;

/// The width the controls need once they have wrapped onto two rows.
///
/// The wider of the two rows wins: tools on the first, everything else on the
/// second. This is the real floor under the editor window's minimum width.
pub const WRAPPED_W: f32 = {
    let tools = Space::MD + TOOLS_W + Space::MD;
    let rest =
        Space::MD + SWATCHES_W + Space::XS + DIVIDER_W + STROKE + Space::MD + ACTIONS_W + Space::MD;
    if tools > rest { tools } else { rest }
};

/// How tall the toolbar must be to hold its controls at `width`.
#[must_use]
pub fn height_for(width: f32) -> f32 {
    if width >= SINGLE_ROW_W {
        HEIGHT
    } else {
        HEIGHT_WRAPPED
    }
}

/// The vertical centre of each of the toolbar's rows.
///
/// One row means both groups share it; two rows put the tools above the
/// controls. Returned as a pair so the drawing code never has to ask "did we
/// wrap?" more than once.
#[must_use]
pub fn rows(bar: Rect) -> (f32, f32) {
    if height_for(bar.width()) == HEIGHT {
        let cy = bar.center().y;
        (cy, cy)
    } else {
        let top = bar.top() + Space::SM + ROW / 2.0;
        (top, top + ROW)
    }
}

/// Where the left-hand controls end, for a bar of this width.
///
/// Exposed so a test can prove the two groups never overlap, at any size the
/// editor window allows.
#[must_use]
pub fn controls_right(bar: Rect) -> f32 {
    let wrapped = height_for(bar.width()) > HEIGHT;
    let mut x = bar.left() + Space::MD;
    if !wrapped {
        x += TOOLS_W + DIVIDER_W;
    }
    x + SWATCHES_W + Space::XS + DIVIDER_W + STROKE
}

/// Where the right-hand action group begins, for a bar of this width.
#[must_use]
pub fn actions_left(bar: Rect) -> f32 {
    bar.right() - Space::MD - ACTIONS_W
}

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

    let (tools_y, cy) = rows(bar);
    let wrapped = tools_y < cy;
    let mut x = bar.left() + Space::MD;

    // Tools. On a narrow window they get a row to themselves rather than being
    // squeezed, dropped behind an overflow menu, or run under the actions.
    let mut picked = None;
    for tool in Tool::ALL {
        let rect = Rect::from_center_size(pos2(x + BUTTON / 2.0, tools_y), vec2(BUTTON, BUTTON));
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

    if wrapped {
        // The row break is the separation; a divider as well would be noise.
        x = bar.left() + Space::MD;
    } else {
        x += Space::SM;
        divider_v(ui.painter(), x, row_top(cy), row_bottom(cy), palette);
        x += Space::SM + Space::XS;
    }

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
    divider_v(ui.painter(), x, row_top(cy), row_bottom(cy), palette);
    x += Space::SM + Space::XS;

    // Stroke width.
    let width_rect = Rect::from_center_size(pos2(x + STROKE / 2.0, cy), vec2(STROKE, BUTTON - 4.0));
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
    debug_assert!(
        actions_left(bar) >= controls_right(bar),
        "the toolbar overlapped itself at {}pt",
        bar.width()
    );
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

/// The top of a divider drawn beside the controls on row centred at `cy`.
fn row_top(cy: f32) -> f32 {
    cy - ROW / 2.0 + Space::XS
}

/// The bottom of a divider drawn beside the controls on row centred at `cy`.
fn row_bottom(cy: f32) -> f32 {
    cy + ROW / 2.0 - Space::XS
}
