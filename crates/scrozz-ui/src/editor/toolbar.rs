//! The editor's toolbar: tool palette, colour, stroke width, and actions.
//!
//! Everything is drawn into an explicit rectangle rather than laid out by
//! egui's `Layout`, matching the rest of Scrozz's surfaces: the toolbar has to
//! stay put while the canvas underneath changes size, and a layout that reflows
//! would make every golden a hostage to text metrics.

use egui::{
    Align, Color32, Frame, Key, Layout, Margin, Popup, PopupCloseBehavior, Rect, RectAlign,
    Response, ScrollArea, Sense, Stroke, Ui, WidgetInfo, WidgetType, pos2, vec2,
};
use scrozz_annotate::{ArrowStyle, Color, Style};

use crate::icons::Icon;
use crate::paint::{
    ControlState, Reveal, Surface, divider_v, focus_ring, glass_panel, icon_button, stroke_width,
};
use crate::theme::{Elevation, Radius, Space, Text, corner};

use super::paint::CanvasView;
use super::state::{Command, CropAspect, EditorState, Intent, Tool};
use scrozz_annotate::{
    Alignment, AspectPreset, Background, Beautification, BeautificationPreset, BuiltInBackground,
    ExactOutputSize, GeneratedStyle, SourceInsets,
};

/// The height of one row of controls, in points.
const ROW: f32 = 40.0;

/// The toolbar's height when everything fits on one row, in points.
pub const HEIGHT: f32 = ROW + Space::MD;

/// The toolbar's height when its controls wrap onto a second row.
pub const HEIGHT_WRAPPED: f32 = ROW * 2.0 + Space::MD;

/// A tool or action button's side, in points.
const BUTTON: f32 = 30.0;

/// The compact current-colour control's width.
const COLOR_CONTROL: f32 = 46.0;
const ARROW_CONTROL: f32 = 46.0;

/// Stable accessibility/focus identity of the compact colour disclosure.
pub const COLOR_CONTROL_ID: &str = "scrozz-editor-colour";
/// Stable accessibility/focus identity of the arrow-style disclosure.
pub const ARROW_CONTROL_ID: &str = "scrozz-editor-arrow-style";

/// The hit target for each colour in the vertical popup.
const COLOR_ROW: f32 = 34.0;

/// The painted swatch diameter inside a popup row.
const COLOR_DOT: f32 = 22.0;

/// The popup's narrow preset rail.
const POPOVER_WIDTH: f32 = 42.0;

/// The wider custom-picker fallback.
const CUSTOM_PICKER_WIDTH: f32 = 292.0;
const ARROW_POPOVER_WIDTH: f32 = 224.0;
const ARROW_ROW: f32 = 40.0;

/// Maximum quick-palette height before its swatch rail scrolls.
const POPOVER_MAX_HEIGHT: f32 = 560.0;

/// The height of the canvas zoom readout, in points.
const ZOOM_H: f32 = 26.0;

/// The stroke-width slider's length, in points.
const STROKE: f32 = 104.0;
const REDACT_INTENSITY: f32 = 212.0;

const ANNOTATION_TOOLS: [Tool; 10] = [
    Tool::Select,
    Tool::Arrow,
    Tool::Line,
    Tool::Rectangle,
    Tool::Ellipse,
    Tool::Pen,
    Tool::Text,
    Tool::Highlight,
    Tool::Redact,
    Tool::Counter,
];

/// The width of Crop, Scene, Add Image, the composition divider, and annotations.
const TOOLS_W: f32 = (ANNOTATION_TOOLS.len() as f32 + 3.0) * (BUTTON + Space::HAIR) + DIVIDER_W;

/// The width the compact colour control occupies.
const COLOR_W: f32 = COLOR_CONTROL;
const ARROW_W: f32 = ARROW_CONTROL;

/// The space a divider needs: the gap before it, and the gap after.
const DIVIDER_W: f32 = Space::SM + Space::SM + Space::XS;

/// The width of a text action in the toolbar's commit group.
const COMMIT_BUTTON_W: f32 = 72.0;

/// The height of every text and icon control on a toolbar row.
const CONTROL_H: f32 = BUTTON;

/// The width of the right-hand action group, at its widest.
///
/// Four icon document actions, then the session's own Cancel and Done. There
/// is exactly one place in the editor to finish or abandon a session, and this
/// is it — no separate chrome bar above the canvas, and nothing inside the
/// Scene or Crop panels claiming the same job.
const ACTIONS_W: f32 = 4.0f32.mul_add(BUTTON, 3.0 * Space::HAIR)
    + DIVIDER_W
    + COMMIT_BUTTON_W
    + Space::XS
    + COMMIT_BUTTON_W;

/// The width the whole toolbar needs to sit on a single row.
///
/// Below this the controls wrap, because the alternative — forcing the window
/// to be this wide — would stop the editor fitting beside anything else on a
/// laptop display.
pub const SINGLE_ROW_W: f32 = Space::MD
    + TOOLS_W
    + DIVIDER_W
    + COLOR_W
    + Space::XS
    + DIVIDER_W
    + STROKE
    + Space::XS
    + ARROW_W
    + Space::MD
    + ACTIONS_W
    + Space::MD;

/// The width the controls need once they have wrapped onto two rows.
///
/// The wider of the two rows wins: tools on the first, everything else on the
/// second. This is the real floor under the editor window's minimum width.
pub const WRAPPED_W: f32 = {
    let tools = Space::MD + TOOLS_W + Space::MD;
    let rest = Space::MD
        + COLOR_W
        + Space::XS
        + DIVIDER_W
        + STROKE
        + Space::XS
        + ARROW_W
        + Space::MD
        + ACTIONS_W
        + Space::MD;
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
    x + COLOR_W + Space::XS + DIVIDER_W + STROKE + Space::XS + ARROW_W
}

/// Where the right-hand action group begins, for a bar of this width.
#[must_use]
pub fn actions_left(bar: Rect) -> f32 {
    bar.right() - Space::MD - ACTIONS_W
}

pub use super::state::{STROKE_MAX, STROKE_MIN};

/// The annotation quick palette, ordered like a compact artist's rail.
pub const PALETTE: [Color; 10] = [
    Color::BLACK,
    Color::ACCENT,
    Color::rgb(0xFF, 0x9F, 0x0A),
    Color::rgb(0xFF, 0xD6, 0x0A),
    Color::rgb(0x30, 0xD1, 0x58),
    Color::rgb(0x64, 0xD2, 0xFF),
    Color::rgb(0x0A, 0x84, 0xFF),
    Color::rgb(0xBF, 0x5A, 0xF2),
    Color::rgb(0xFF, 0x37, 0x5F),
    Color::WHITE,
];

/// Screen-reader names corresponding to [`PALETTE`].
pub const PALETTE_NAMES: [&str; 10] = [
    "Black", "Red", "Orange", "Yellow", "Green", "Cyan", "Blue", "Purple", "Pink", "White",
];

/// Named arrow thickness choices, backed by numeric source-unit widths.
pub const ARROW_THICKNESSES: [(&str, f64); 4] = [
    ("Thin", 2.0),
    ("Regular", 4.0),
    ("Bold", 8.0),
    ("Heavy", 14.0),
];
/// Maximum user colours retained beside the built-in palette.
pub const MAX_CUSTOM_SWATCHES: usize = 8;

/// Persistent interaction state for the anchored colour popup.
#[derive(Debug, Default)]
pub struct ColorPopover {
    open: bool,
    fallback_open: bool,
    focus_on_open: bool,
    last_rect: Option<Rect>,
    custom: Vec<Color>,
    custom_changed: bool,
    pending_replace: Option<Color>,
}

impl ColorPopover {
    /// Opens the quick palette and focuses the current colour.
    pub fn open(&mut self) {
        self.open = true;
        self.fallback_open = false;
        self.focus_on_open = true;
    }

    /// Opens the cross-platform custom-colour fallback.
    pub fn open_fallback(&mut self) {
        self.open = true;
        self.fallback_open = true;
        self.focus_on_open = true;
    }

    /// Closes every colour surface.
    pub fn close(&mut self) {
        self.open = false;
        self.fallback_open = false;
        self.focus_on_open = false;
    }

    /// Replaces persisted custom swatches, sanitising order and duplicates.
    pub fn set_custom(&mut self, colors: Vec<Color>) {
        self.custom.clear();
        for color in colors {
            if !self.custom.contains(&color) {
                self.custom.push(color);
            }
            if self.custom.len() == MAX_CUSTOM_SWATCHES {
                break;
            }
        }
        self.custom_changed = false;
    }

    /// Current custom swatches in most-recently-used order.
    #[must_use]
    pub fn custom(&self) -> &[Color] {
        &self.custom
    }

    /// Adds or replaces a custom colour and moves it to the front.
    pub fn remember(&mut self, color: Color) {
        if let Some(replaced) = self.pending_replace.take() {
            self.custom.retain(|existing| *existing != replaced);
        }
        self.custom.retain(|existing| *existing != color);
        self.custom.insert(0, color);
        self.custom.truncate(MAX_CUSTOM_SWATCHES);
        self.custom_changed = true;
    }

    fn remove(&mut self, index: usize) {
        if index < self.custom.len() {
            self.custom.remove(index);
            self.custom_changed = true;
        }
    }

    /// Takes a persistence update after an add, replace, remove, or MRU use.
    pub fn take_change(&mut self) -> Option<Vec<Color>> {
        std::mem::take(&mut self.custom_changed).then(|| self.custom.clone())
    }

    /// Whether the anchored surface is visible.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// The popup's most recently resolved screen rectangle.
    #[must_use]
    pub const fn last_rect(&self) -> Option<Rect> {
        self.last_rect
    }
}

/// Persistent interaction state for the arrow inspector popup.
#[derive(Debug, Default)]
pub struct ArrowPopover {
    open: bool,
    focus_on_open: bool,
    last_rect: Option<Rect>,
}

impl ArrowPopover {
    /// Opens the arrow style, bend, and thickness inspector.
    pub fn open(&mut self) {
        self.open = true;
        self.focus_on_open = true;
    }

    /// Closes the inspector.
    pub fn close(&mut self) {
        self.open = false;
        self.focus_on_open = false;
    }

    /// Whether the inspector is visible.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Most recently resolved screen rectangle.
    #[must_use]
    pub const fn last_rect(&self) -> Option<Rect> {
        self.last_rect
    }
}

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
        Tool::Redact => Icon::GridDots,
        Tool::Counter => Icon::ListNumbers,
        Tool::Crop => Icon::Crop,
    }
}

/// Draws the toolbar, returning an intent if an action button was pressed.
pub fn draw(
    ui: &mut Ui,
    surface: &Surface<'_>,
    state: &mut EditorState,
    color_popover: &mut ColorPopover,
    arrow_popover: &mut ArrowPopover,
    bar: Rect,
    input_blocked: bool,
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
    let mut intent = None;

    // Composition comes first: these controls change the capture or its canvas,
    // while everything after the divider is an annotation layered on the source.
    let crop_rect = Rect::from_center_size(pos2(x + BUTTON / 2.0, tools_y), vec2(BUTTON, BUTTON));
    let crop = icon_button(
        ui,
        surface,
        crop_rect,
        ui.id().with(("editor-tool", Tool::Crop)),
        Icon::Crop,
        "Crop",
        ControlState::new().selected(state.tool() == Tool::Crop),
        Reveal::SHOWN,
    );
    if crop.clicked() && !input_blocked {
        state.set_tool(Tool::Crop);
    }
    crop.on_hover_text("Crop  (C)");
    x += BUTTON + Space::HAIR;

    let scene_rect = Rect::from_center_size(pos2(x + BUTTON / 2.0, tools_y), vec2(BUTTON, BUTTON));
    let scene = icon_button(
        ui,
        surface,
        scene_rect,
        ui.id().with("editor-scene"),
        Icon::LayoutGrid,
        "Scene",
        ControlState::new().selected(state.has_smart_frame_draft()),
        Reveal::SHOWN,
    );
    if scene.clicked() && !input_blocked {
        intent = Some(Intent::ToggleSmartFrame);
    }
    scene.on_hover_text("Scene");
    x += BUTTON + Space::HAIR;

    let image_rect = Rect::from_center_size(pos2(x + BUTTON / 2.0, tools_y), vec2(BUTTON, BUTTON));
    let image = icon_button(
        ui,
        surface,
        image_rect,
        ui.id().with("editor-add-image"),
        Icon::DeviceDesktop,
        "Add Image",
        ControlState::new(),
        Reveal::SHOWN,
    );
    if image.clicked() && !input_blocked {
        intent = Some(Intent::AddImage);
    }
    image.on_hover_text("Add Image");
    x += BUTTON + Space::HAIR + Space::SM;
    divider_v(
        ui.painter(),
        x,
        row_top(tools_y),
        row_bottom(tools_y),
        palette,
    );
    x += Space::SM + Space::XS;

    // Annotation tools. On a narrow window this whole composition-and-tools
    // group gets its own row rather than being squeezed or dropped.
    let mut picked = None;
    for tool in ANNOTATION_TOOLS {
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
        if response.clicked() && !input_blocked {
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

    let redact_context = state.tool() == Tool::Redact || state.selection_is_redaction();
    if redact_context {
        color_popover.close();
        arrow_popover.close();
        let mut intensity = (state.redact_intensity() * 100.0).round();
        let label_width = 58.0;
        ui.painter().text(
            pos2(x, cy - 4.0),
            egui::Align2::LEFT_CENTER,
            "Intensity",
            Text::Label.font(),
            palette.text,
        );
        let rect = Rect::from_min_size(
            pos2(x + label_width, cy - 16.0),
            vec2(REDACT_INTENSITY - label_width, 25.0),
        );
        let response = ui.put(
            rect,
            egui::Slider::new(&mut intensity, 0.0..=100.0)
                .suffix("%")
                .step_by(1.0),
        );
        response.widget_info(|| {
            WidgetInfo::labeled(
                WidgetType::Slider,
                true,
                format!("Intensity: {intensity:.0}%. Low 0%, Medium 50%, High 100%"),
            )
        });
        if response.changed() && !input_blocked {
            state.set_redact_intensity(intensity / 100.0);
        }
        for (fraction, align, label) in [
            (0.0, egui::Align2::LEFT_CENTER, "Low"),
            (0.5, egui::Align2::CENTER_CENTER, "Medium"),
            (1.0, egui::Align2::RIGHT_CENTER, "High"),
        ] {
            ui.painter().text(
                pos2(rect.left() + rect.width() * fraction, cy + 13.0),
                align,
                label,
                Text::Caption.font(),
                palette.text_muted,
            );
        }
        response.on_hover_text(
            "Redact intensity: Low 0%, Medium 50%, High 100%. Every level is irreversible.",
        );
        x += REDACT_INTENSITY + Space::XS;
    } else {
        // Colour. One stable-width control; the palette lives in a foreground
        // popup so opening it cannot move any toolbar or canvas geometry.
        let color_rect = Rect::from_center_size(
            pos2(x + COLOR_CONTROL / 2.0, cy),
            vec2(COLOR_CONTROL, BUTTON),
        );
        let color_button = color_control(
            ui,
            surface,
            color_rect,
            egui::Id::new(COLOR_CONTROL_ID),
            state.stroke_color(),
            color_popover.open,
        );
        if color_button.clicked() && (!input_blocked || color_popover.open) {
            if color_popover.open {
                color_popover.close();
            } else {
                color_popover.open();
            }
        }
        let custom = draw_color_popover(ui, surface, state, color_popover, &color_button);
        if custom {
            return Some(Intent::CustomColor);
        }
        x += COLOR_CONTROL;

        x += Space::XS;
        divider_v(ui.painter(), x, row_top(cy), row_bottom(cy), palette);
        x += Space::SM + Space::XS;

        // Stroke width.
        let width_rect =
            Rect::from_center_size(pos2(x + STROKE / 2.0, cy), vec2(STROKE, BUTTON - 4.0));
        let fraction = width_fraction(state.stroke_width());
        let response = stroke_width(
            ui,
            surface,
            width_rect,
            ui.id().with("editor-stroke"),
            fraction,
            state.stroke_width(),
        );
        if !input_blocked
            && (response.dragged() || response.clicked())
            && let Some(pos) = response.interact_pointer_pos()
        {
            let inner_l = width_rect.left() + Space::MD;
            let inner_r = width_rect.right() - Space::MD;
            let f = ((pos.x - inner_l) / (inner_r - inner_l)).clamp(0.0, 1.0);
            state.set_stroke_width(width_for_fraction(f));
        }
        if !input_blocked
            && let Some(width) = stroke_keyboard_width(ui, &response, state.stroke_width())
        {
            state.set_stroke_width(width);
        }
        response.on_hover_text(format!(
            "{} · {:.0} pt",
            thickness_name(state.stroke_width()),
            state.stroke_width()
        ));
        x += STROKE + Space::XS;
    }

    let arrow_context = state.tool() == Tool::Arrow || state.selection_is_arrow();
    if arrow_context {
        let arrow_rect = Rect::from_center_size(
            pos2(x + ARROW_CONTROL / 2.0, cy),
            vec2(ARROW_CONTROL, BUTTON),
        );
        let arrow_button = arrow_control(
            ui,
            surface,
            arrow_rect,
            state.arrow_style(),
            arrow_popover.open,
        );
        if arrow_button.clicked() && (!input_blocked || arrow_popover.open) {
            if arrow_popover.open {
                arrow_popover.close();
            } else {
                arrow_popover.open();
            }
        }
        draw_arrow_popover(ui, surface, state, arrow_popover, &arrow_button);
    } else {
        arrow_popover.close();
    }

    // The session's own two decisions, right-aligned and furthest from the
    // tools: Done commits the edited pixels, Cancel abandons them. Both are
    // unconditional, and both live here and nowhere else.
    let done_rect = Rect::from_center_size(
        pos2(bar.right() - Space::MD - COMMIT_BUTTON_W / 2.0, cy),
        vec2(COMMIT_BUTTON_W, CONTROL_H),
    );
    let done = crate::paint::text_button(
        ui,
        surface,
        done_rect,
        ui.id().with("editor-done"),
        "Done",
        true,
        ControlState::new(),
    );
    if done.clicked() && !input_blocked {
        intent = Some(Intent::Commit);
    }
    done.on_hover_text("Finish editing and update this capture");

    let cancel_rect = Rect::from_center_size(
        pos2(done_rect.left() - Space::XS - COMMIT_BUTTON_W / 2.0, cy),
        vec2(COMMIT_BUTTON_W, CONTROL_H),
    );
    let cancel = crate::paint::text_button(
        ui,
        surface,
        cancel_rect,
        ui.id().with("editor-cancel"),
        "Cancel",
        false,
        ControlState::new(),
    );
    if cancel.clicked() && !input_blocked {
        intent = Some(Intent::Discard);
    }
    cancel.on_hover_text("Discard this editing session");

    let commit_left = cancel_rect.left() - Space::SM;
    divider_v(
        ui.painter(),
        commit_left,
        row_top(cy),
        row_bottom(cy),
        palette,
    );

    // Document actions, right-aligned. Laid out from the right so the group
    // stays anchored to the commit pair as the toolbar grows and shrinks.
    let mut rx = commit_left - Space::SM - Space::XS - BUTTON;
    debug_assert!(
        actions_left(bar) >= controls_right(bar),
        "the toolbar overlapped itself at {}pt",
        bar.width()
    );
    for (icon, label, action) in [
        (
            Icon::DeviceFloppy,
            "Save",
            Action::Intent(Box::new(Intent::Save)),
        ),
        (Icon::Copy, "Copy", Action::Intent(Box::new(Intent::Copy))),
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
        if response.clicked() && enabled && !input_blocked {
            match action {
                Action::Undo => {
                    let _ = state.command(super::state::Command::Undo);
                }
                Action::Redo => {
                    let _ = state.command(super::state::Command::Redo);
                }
                Action::Intent(next) => intent = Some(*next),
            }
        }
        response.on_hover_text(shortcut_hint(label));
        rx -= BUTTON + Space::HAIR;
    }

    intent
}

/// Draws viewport-only controls over the canvas.
pub fn draw_view_controls(
    ui: &Ui,
    surface: &Surface<'_>,
    state: &mut EditorState,
    canvas: Rect,
    view: &CanvasView,
) {
    draw_zoom_control(ui, surface, state, canvas, view);
    if state.crop_mode() {
        draw_crop_controls(ui, surface, state, canvas);
    }
}

fn draw_zoom_control(
    ui: &Ui,
    surface: &Surface<'_>,
    state: &mut EditorState,
    canvas: Rect,
    view: &CanvasView,
) {
    let palette = surface.palette();
    let position = pos2(
        canvas.left() + Space::SM,
        canvas.bottom() - ZOOM_H - Space::SM,
    );
    egui::Area::new(egui::Id::new("editor-zoom-control"))
        .order(egui::Order::Foreground)
        .fixed_pos(position)
        .show(ui.ctx(), |ui| {
            ui.set_opacity(1.0);
            // Opaque, not the usual translucent card: this sits over whatever
            // the capture happens to contain, and a readout that dissolves
            // into a bright screenshot is the one thing it must never do.
            let frame = Frame::new()
                .fill(palette.flatten(palette.card_fill_raised))
                .stroke(Stroke::new(1.0, palette.hairline))
                .corner_radius(corner(Radius::BUTTON))
                .inner_margin(Margin::symmetric(Space::XS as i8, 0));
            frame.show(ui, |ui| {
                // The frame *is* the indicator, so the control inside it wears
                // no chrome of its own until it is hovered or focused.
                {
                    let widgets = &mut ui.style_mut().visuals.widgets;
                    widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
                    widgets.inactive.bg_stroke = Stroke::NONE;
                    widgets.hovered.bg_stroke = Stroke::NONE;
                    widgets.open.bg_stroke = Stroke::NONE;
                    for state in [
                        &mut widgets.inactive,
                        &mut widgets.hovered,
                        &mut widgets.active,
                        &mut widgets.open,
                    ] {
                        state.corner_radius = corner(Radius::CHIP);
                        state.fg_stroke = Stroke::new(1.0, palette.text);
                    }
                }
                ui.spacing_mut().button_padding = vec2(Space::XS, Space::XS);
                let label = egui::RichText::new(format!("{:.0}%", view.scale() * 100.0))
                    .font(surface.font(Text::Label));
                ui.menu_button(label, |ui| {
                    ui.style_mut().visuals.widgets = crate::theme::widget_visuals(palette);
                    if ui
                        .selectable_label(state.is_fit_zoom() && state.pan() == (0.0, 0.0), "Fit")
                        .on_hover_text("Fit the whole image in the editor")
                        .clicked()
                    {
                        let _ = state.command(Command::ZoomReset);
                        ui.close();
                    }
                    ui.separator();
                    for percent in [25, 50, 75, 100, 125, 200, 400, 800] {
                        let zoom = percent as f32 / 100.0;
                        if ui
                            .selectable_label(
                                !state.is_fit_zoom() && (view.scale() - zoom).abs() < 0.001,
                                format!("{percent}%"),
                            )
                            .clicked()
                        {
                            state.zoom_about(zoom, (0.0, 0.0));
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text("Zoom level. Pinch or use Command/Ctrl +/-");
            });
        });
}

fn draw_crop_controls(ui: &Ui, surface: &Surface<'_>, state: &mut EditorState, canvas: Rect) {
    let palette = surface.palette();
    let zoom_reserve = 76.0;
    let width = (canvas.width() - zoom_reserve - Space::LG * 2.0).clamp(1.0, 920.0);
    let panel_center = canvas.left() + zoom_reserve + (canvas.width() - zoom_reserve) / 2.0;
    let content = ui.ctx().content_rect();
    let offset = vec2(
        panel_center - content.center().x,
        canvas.bottom() - content.bottom() - Space::SM,
    );
    egui::Area::new(egui::Id::new("editor-crop-controls"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_BOTTOM, offset)
        .show(ui.ctx(), |ui| {
            ui.set_opacity(1.0);
            ui.set_width(width);
            ui.set_max_width(width);
            let frame = Frame::new()
                .fill(palette.card_fill_raised)
                .stroke(Stroke::new(1.0, palette.hairline))
                .corner_radius(corner(Radius::BAR))
                .inner_margin(Margin::same(Space::SM as i8));
            frame.show(ui, |ui| {
                let inner_width = (width - Space::SM * 2.0).max(1.0);
                ui.set_width(inner_width);
                ui.set_max_width(inner_width);
                ui.horizontal_wrapped(|ui| {
                    ui.label("Crop");
                    let mut aspect = state.crop_aspect().unwrap_or_default();
                    let aspect_response = egui::ComboBox::from_id_salt("crop-aspect")
                        .selected_text(aspect.label())
                        .show_ui(ui, |ui| {
                            for choice in CropAspect::ALL {
                                ui.selectable_value(&mut aspect, choice, choice.label());
                            }
                        });
                    aspect_response.response.widget_info(|| {
                        WidgetInfo::labeled(
                            WidgetType::ComboBox,
                            true,
                            format!("Aspect ratio: {}", aspect.label()),
                        )
                    });
                    if state.crop_aspect() != Some(aspect) {
                        state.set_crop_aspect(aspect);
                    }

                    if let Some(((width_px, height_px), (source_width, source_height))) =
                        state.crop_pixel_sizes()
                    {
                        let scale = state.document().source().frame.scale.get();
                        let mut width_px = width_px;
                        let mut height_px = height_px;
                        let width_response = ui.add(
                            egui::DragValue::new(&mut width_px)
                                .range(1..=source_width)
                                .speed(1.0)
                                .prefix("W ")
                                .suffix(" px"),
                        );
                        width_response.widget_info(|| {
                            WidgetInfo::labeled(
                                WidgetType::DragValue,
                                true,
                                format!("Crop width: {width_px} pixels"),
                            )
                        });
                        width_response
                            .clone()
                            .on_hover_text("Crop width in source pixels");
                        if width_response.changed() {
                            state.set_crop_width(f64::from(width_px) / scale);
                        }
                        let height_response = ui.add(
                            egui::DragValue::new(&mut height_px)
                                .range(1..=source_height)
                                .speed(1.0)
                                .prefix("H ")
                                .suffix(" px"),
                        );
                        height_response.widget_info(|| {
                            WidgetInfo::labeled(
                                WidgetType::DragValue,
                                true,
                                format!("Crop height: {height_px} pixels"),
                            )
                        });
                        height_response
                            .clone()
                            .on_hover_text("Crop height in source pixels");
                        if height_response.changed() {
                            state.set_crop_height(f64::from(height_px) / scale);
                        }
                        if ui
                            .button("Swap")
                            .on_hover_text("Swap crop width and height")
                            .clicked()
                        {
                            state.swap_crop_dimensions();
                        }
                        let output_label = format!("Output {width_px} x {height_px} px");
                        ui.add(
                            egui::Label::new(format!("Output {width_px} x {height_px} px"))
                                .sense(Sense::focusable_noninteractive()),
                        )
                        .widget_info(|| {
                            WidgetInfo::labeled(WidgetType::Label, true, output_label.clone())
                        });
                        let source_label = format!("Source {source_width} x {source_height} px");
                        ui.add(
                            egui::Label::new(format!("Source {source_width} x {source_height} px"))
                                .sense(Sense::focusable_noninteractive()),
                        )
                        .widget_info(|| {
                            WidgetInfo::labeled(WidgetType::Label, true, source_label.clone())
                        });
                    }
                });

                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("Rotate left")
                        .on_hover_text("Rotate the crop result 90 degrees counter-clockwise")
                        .clicked()
                    {
                        let _ = state.command(Command::RotateCropLeft);
                    }
                    if ui
                        .button("Rotate right")
                        .on_hover_text("Rotate the crop result 90 degrees clockwise")
                        .clicked()
                    {
                        let _ = state.command(Command::RotateCropRight);
                    }
                    if ui
                        .button("Flip horizontal")
                        .on_hover_text("Mirror the crop result left to right")
                        .clicked()
                    {
                        let _ = state.command(Command::FlipCropHorizontal);
                    }
                    if ui
                        .button("Flip vertical")
                        .on_hover_text("Mirror the crop result top to bottom")
                        .clicked()
                    {
                        let _ = state.command(Command::FlipCropVertical);
                    }

                    // No percentage read-out here: the canvas keeps exactly
                    // one zoom indicator, in its own corner, and a second copy
                    // in this bar is the duplicate that used to disagree with
                    // it while being harder to read.
                    if ui.button("Zoom -").clicked() {
                        let _ = state.command(Command::ZoomOut);
                    }
                    if ui.button("Zoom +").clicked() {
                        let _ = state.command(Command::ZoomIn);
                    }
                    if ui.button("Fit").clicked() {
                        let _ = state.command(Command::ZoomReset);
                    }

                    let mut snap = state.crop_snap_edges();
                    let snap_response = ui.checkbox(&mut snap, "Snap to edges");
                    if snap_response.changed() {
                        state.set_crop_snap_edges(snap);
                    }
                    snap_response
                        .on_hover_text("Hold Command/Ctrl while dragging to disable snapping");

                    // Named for what they act on. The editor session's own
                    // Cancel and Done live in the toolbar, and two controls a
                    // few points apart both saying "Cancel" while meaning
                    // different things is not a bar worth shipping.
                    if ui.button("Cancel Crop").clicked() {
                        let _ = state.command(Command::CancelCrop);
                    }
                    if state.document().crop().is_some()
                        && ui.button("Revert to Original").clicked()
                    {
                        let _ = state.command(Command::RevertCrop);
                    }
                    if ui.button("Apply Crop").clicked() {
                        let _ = state.command(Command::ApplyCrop);
                    }
                });
            });
        });
}

const POPOVER_ALIGNS: [RectAlign; 4] = [
    RectAlign::BOTTOM_START,
    RectAlign::TOP_START,
    RectAlign::BOTTOM_END,
    RectAlign::TOP_END,
];

fn color_control(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    id: egui::Id,
    color: Color,
    open: bool,
) -> Response {
    let label = format!("Colour: {}. Open colour palette", color_name(color));
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, open, label.clone()));
    let palette = surface.palette();
    let painter = ui.painter();
    if response.has_focus() {
        focus_ring(painter, rect, Radius::BUTTON, palette);
    }
    if response.hovered() || response.is_pointer_button_down_on() || open {
        painter.rect_filled(
            rect,
            corner(Radius::BUTTON),
            if response.is_pointer_button_down_on() {
                palette.active
            } else {
                palette.hover
            },
        );
    }
    let center = pos2(rect.left() + Space::MD, rect.center().y);
    paint_color_dot(painter, center, 9.0, to_egui(color), palette, false);
    let chevron = pos2(rect.right() - Space::MD, rect.center().y);
    let ink = palette.text_muted;
    painter.line_segment(
        [chevron + vec2(-3.0, -1.5), chevron + vec2(0.0, 1.5)],
        Stroke::new(1.5, ink),
    );
    painter.line_segment(
        [chevron + vec2(0.0, 1.5), chevron + vec2(3.0, -1.5)],
        Stroke::new(1.5, ink),
    );
    response
}

const ARROW_STYLES: [(ArrowStyle, &str); 4] = [
    (ArrowStyle::Bold, "Bold"),
    (ArrowStyle::Curved, "Curved"),
    (ArrowStyle::Sketch, "Sketch"),
    (ArrowStyle::Double, "Double"),
];

fn arrow_style_name(style: ArrowStyle) -> &'static str {
    ARROW_STYLES
        .iter()
        .find_map(|(candidate, label)| (*candidate == style).then_some(*label))
        .unwrap_or("Bold")
}

fn arrow_control(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    style: ArrowStyle,
    open: bool,
) -> Response {
    let label = format!("Arrow style: {}", arrow_style_name(style));
    let response = ui.interact(rect, egui::Id::new(ARROW_CONTROL_ID), Sense::click());
    response.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, open, label.clone()));
    let palette = surface.palette();
    if response.hovered() || response.is_pointer_button_down_on() || open {
        ui.painter().rect_filled(
            rect,
            corner(Radius::BUTTON),
            if response.is_pointer_button_down_on() {
                palette.active
            } else {
                palette.hover
            },
        );
    }
    if response.has_focus() {
        focus_ring(ui.painter(), rect, Radius::BUTTON, palette);
    }
    paint_arrow_glyph(
        ui.painter(),
        Rect::from_center_size(rect.center() - vec2(4.0, 0.0), vec2(22.0, 14.0)),
        style,
        palette.text,
    );
    let chevron = pos2(rect.right() - Space::SM, rect.center().y);
    ui.painter().line_segment(
        [chevron + vec2(-3.0, -1.5), chevron + vec2(0.0, 1.5)],
        Stroke::new(1.5, palette.text_muted),
    );
    ui.painter().line_segment(
        [chevron + vec2(0.0, 1.5), chevron + vec2(3.0, -1.5)],
        Stroke::new(1.5, palette.text_muted),
    );
    response.on_hover_text(label)
}

fn paint_arrow_glyph(painter: &egui::Painter, rect: Rect, style: ArrowStyle, color: Color32) {
    let left = rect.left_center();
    let right = rect.right_center();
    let bend = if style == ArrowStyle::Curved {
        -4.0
    } else {
        0.0
    };
    let middle = rect.center() + vec2(0.0, bend);
    let stroke = Stroke::new(
        if style == ArrowStyle::Sketch {
            1.5
        } else {
            2.5
        },
        color,
    );
    painter.line_segment([left, middle], stroke);
    painter.line_segment([middle, right - vec2(4.0, 0.0)], stroke);
    let head = vec![
        right,
        right + vec2(-6.0, -4.0),
        right + vec2(-5.0, 0.0),
        right + vec2(-6.0, 4.0),
    ];
    painter.add(egui::Shape::convex_polygon(head, color, Stroke::NONE));
    if style == ArrowStyle::Double {
        let head = vec![
            left,
            left + vec2(6.0, -4.0),
            left + vec2(5.0, 0.0),
            left + vec2(6.0, 4.0),
        ];
        painter.add(egui::Shape::convex_polygon(head, color, Stroke::NONE));
    }
}

fn draw_arrow_popover(
    ui: &mut Ui,
    surface: &Surface<'_>,
    state: &mut EditorState,
    popover: &mut ArrowPopover,
    anchor: &Response,
) {
    let was_open = popover.open;
    let mut open = popover.open;
    let palette = surface.palette();
    let popup_id = anchor.id.with("inspector");
    let shown = Popup::from_response(anchor)
        .id(popup_id)
        .open_bool(&mut open)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .align(RectAlign::BOTTOM_END)
        .align_alternatives(&POPOVER_ALIGNS)
        .gap(Space::SM)
        .width(ARROW_POPOVER_WIDTH)
        .layout(Layout::top_down(Align::Min))
        .frame(Frame::new().inner_margin(Margin::same(Space::SM as i8)))
        .show(|popup_ui| {
            popup_ui.set_opacity(1.0);
            paint_popover_panel(popup_ui, palette);
            popup_ui.set_min_width(ARROW_POPOVER_WIDTH);
            popup_ui.label(egui::RichText::new("Arrow style").font(Text::Label.font()));
            for (index, (style, label)) in ARROW_STYLES.into_iter().enumerate() {
                let (_, rect) = popup_ui.allocate_space(vec2(ARROW_POPOVER_WIDTH, ARROW_ROW));
                let response = popup_ui.interact(
                    rect,
                    popup_ui.id().with(("arrow-style", index)),
                    Sense::click(),
                );
                response.widget_info(|| {
                    WidgetInfo::selected(
                        WidgetType::RadioButton,
                        true,
                        state.arrow_style() == style,
                        format!("{label} arrow"),
                    )
                });
                let selected = state.arrow_style() == style;
                if selected || response.hovered() {
                    popup_ui.painter().rect_filled(
                        rect,
                        corner(Radius::BUTTON),
                        if selected {
                            palette.accent
                        } else {
                            palette.hover
                        },
                    );
                }
                if response.has_focus() {
                    focus_ring(popup_ui.painter(), rect, Radius::BUTTON, palette);
                }
                paint_arrow_glyph(
                    popup_ui.painter(),
                    Rect::from_min_size(
                        rect.left_center() + vec2(Space::SM, -7.0),
                        vec2(34.0, 14.0),
                    ),
                    style,
                    if selected {
                        palette.on_accent
                    } else {
                        palette.text
                    },
                );
                popup_ui.painter().text(
                    pos2(rect.left() + 52.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    Text::Button.font(),
                    if selected {
                        palette.on_accent
                    } else {
                        palette.text
                    },
                );
                if activate_response(popup_ui, &response) {
                    state.set_arrow_style(style);
                }
                if popover.focus_on_open && state.arrow_style() == style {
                    response.request_focus();
                }
            }

            popup_ui.separator();
            popup_ui.label(egui::RichText::new("Thickness").font(Text::Label.font()));
            egui::Grid::new("arrow-thickness-grid")
                .num_columns(2)
                .spacing(vec2(Space::SM, Space::XS))
                .show(popup_ui, |ui| {
                    for (label, width) in ARROW_THICKNESSES {
                        let selected = (state.stroke_width() - width).abs() < 0.01;
                        let response =
                            ui.selectable_label(selected, format!("{label}  {width:.0} pt"));
                        response.widget_info(|| {
                            WidgetInfo::selected(
                                WidgetType::RadioButton,
                                true,
                                selected,
                                format!("{label} thickness, {width:.0} points"),
                            )
                        });
                        if response.clicked() {
                            state.set_stroke_width(width);
                        }
                        if label == "Regular" {
                            ui.end_row();
                        }
                    }
                });

            popup_ui.add_space(Space::XS);
            let mut bend = (state.arrow_bend() * 100.0).round() as i32;
            let response = popup_ui.add(
                egui::Slider::new(&mut bend, -75..=75)
                    .text("Bend")
                    .suffix("%"),
            );
            if response.changed() {
                state.set_arrow_bend(f64::from(bend) / 100.0);
            }
        });

    popover.focus_on_open = false;
    if let Some(shown) = shown {
        popover.last_rect = Some(shown.response.rect);
    }
    popover.open = open;
    if !open && was_open {
        anchor.request_focus();
    }
}

fn draw_color_popover(
    ui: &mut Ui,
    surface: &Surface<'_>,
    state: &mut EditorState,
    popover: &mut ColorPopover,
    anchor: &Response,
) -> bool {
    let was_open = popover.open;
    let mut open = popover.open;
    let fallback = popover.fallback_open;
    let mut close_requested = false;
    let mut custom_requested = false;
    let mut remember_requested = false;
    let mut chosen = None;
    let mut chosen_custom = None;
    let mut remove_custom = None;
    let mut next_focus = None;
    let selected = PALETTE
        .iter()
        .position(|color| *color == state.stroke_color())
        .unwrap_or(PALETTE.len());
    let palette = surface.palette();
    // The margin still belongs to the container; the panel itself is painted
    // after overriding egui's ambient Area fade so foreground and background
    // appear together on the same deterministic frame.
    let frame = Frame::new().inner_margin(Margin::same(Space::XS as i8));
    let popup_id = anchor.id.with("palette");
    let width = if fallback {
        CUSTOM_PICKER_WIDTH
    } else {
        POPOVER_WIDTH
    };
    let shown = Popup::from_response(anchor)
        .id(popup_id)
        .open_bool(&mut open)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .align(RectAlign::BOTTOM_START)
        .align_alternatives(&POPOVER_ALIGNS)
        .gap(Space::SM)
        .width(width)
        .layout(Layout::top_down(Align::Center))
        .frame(frame)
        .show(|popup_ui| {
            // Scrozz motion comes from named tokens, not egui's ambient Area
            // fade. An immediate native-menu appearance is deterministic and
            // does not leave a frozen-time golden at zero opacity.
            popup_ui.set_opacity(1.0);
            paint_popover_panel(popup_ui, palette);
            popup_ui.set_min_width(width);
            if fallback {
                remember_requested = draw_custom_fallback(
                    popup_ui,
                    state,
                    &mut close_requested,
                    popover.focus_on_open,
                );
                return;
            }

            let max_height = (popup_ui.ctx().content_rect().height() - Space::XL * 2.0)
                .clamp(COLOR_ROW * 3.0, POPOVER_MAX_HEIGHT);
            ScrollArea::vertical()
                .max_height(max_height)
                .auto_shrink([false, true])
                .show(popup_ui, |popup_ui| {
                    popup_ui.spacing_mut().item_spacing.y = 0.0;
                    for (index, (color, name)) in PALETTE.into_iter().zip(PALETTE_NAMES).enumerate()
                    {
                        let response = preset_swatch(
                            popup_ui,
                            surface,
                            popup_ui.id().with(("colour-preset", index)),
                            color,
                            name,
                            state.stroke_color() == color,
                        );
                        if activate_response(popup_ui, &response) {
                            chosen = Some(color);
                            close_requested = true;
                        }
                    }

                    if !popover.custom.is_empty() {
                        popup_ui.add_space(Space::XS);
                        let separator = Rect::from_center_size(
                            pos2(popup_ui.max_rect().center().x, popup_ui.cursor().top()),
                            vec2(COLOR_DOT, 1.0),
                        );
                        popup_ui.painter().line_segment(
                            [separator.left_center(), separator.right_center()],
                            Stroke::new(1.0, palette.divider),
                        );
                        popup_ui.add_space(Space::XS);
                        for (index, color) in popover.custom.iter().copied().enumerate() {
                            let name = format!(
                                "Custom #{:02X}{:02X}{:02X}{:02X}",
                                color.r, color.g, color.b, color.a
                            );
                            let response = preset_swatch(
                                popup_ui,
                                surface,
                                popup_ui.id().with(("custom-colour", index)),
                                color,
                                &name,
                                state.stroke_color() == color,
                            );
                            if activate_response(popup_ui, &response) {
                                chosen = Some(color);
                                chosen_custom = Some(index);
                                close_requested = true;
                            }
                        }
                        if let Some(index) = popover
                            .custom
                            .iter()
                            .position(|color| *color == state.stroke_color())
                        {
                            let remove = custom_remove_button(
                                popup_ui,
                                surface,
                                popup_ui.id().with("remove-custom-colour"),
                            );
                            if activate_response(popup_ui, &remove) {
                                remove_custom = Some(index);
                            }
                        }
                    }

                    popup_ui.add_space(Space::XS);
                    let separator = Rect::from_center_size(
                        pos2(popup_ui.max_rect().center().x, popup_ui.cursor().top()),
                        vec2(COLOR_DOT, 1.0),
                    );
                    popup_ui.painter().line_segment(
                        [separator.left_center(), separator.right_center()],
                        Stroke::new(1.0, palette.divider),
                    );
                    popup_ui.add_space(Space::XS);
                    let replacing = popover
                        .custom
                        .iter()
                        .position(|color| *color == state.stroke_color());
                    let custom = custom_color_button(
                        popup_ui,
                        surface,
                        popup_ui.id().with("custom-colour-picker"),
                        replacing.is_some(),
                    );
                    if activate_response(popup_ui, &custom) {
                        popover.pending_replace = replacing.map(|index| popover.custom[index]);
                        custom_requested = true;
                    }

                    if popover.focus_on_open {
                        next_focus = Some(selected);
                    }

                    if let Some(index) = next_focus {
                        let id = if index < PALETTE.len() {
                            popup_ui.id().with(("colour-preset", index))
                        } else {
                            popup_ui.id().with("custom-colour-picker")
                        };
                        popup_ui.memory_mut(|memory| memory.request_focus(id));
                    }
                });
        });

    popover.focus_on_open = false;
    if let Some(shown) = shown {
        popover.last_rect = Some(shown.response.rect);
    }
    if let Some(color) = chosen
        && color != state.stroke_color()
    {
        state.set_stroke_color(color);
    }
    if let Some(index) = chosen_custom {
        popover.pending_replace = None;
        popover.remember(state.stroke_color());
        let _ = index;
    }
    if let Some(index) = remove_custom {
        popover.remove(index);
    }
    if remember_requested {
        popover.remember(state.stroke_color());
    }
    if close_requested {
        open = false;
    }
    popover.open = open;
    if !open {
        popover.fallback_open = false;
        if was_open {
            anchor.request_focus();
        }
    }
    custom_requested
}

fn paint_popover_panel(ui: &Ui, palette: &crate::theme::Palette) {
    let rect = ui.max_rect().expand(Space::XS);
    let rounding = corner(Radius::BAR);
    if let Some((ambient, _)) = Elevation::Lifted.shadows(palette) {
        ui.painter().add(ambient.as_shape(rect, rounding));
    }
    ui.painter()
        .rect_filled(rect, rounding, palette.card_fill_raised);
    ui.painter().rect_stroke(
        rect,
        rounding,
        Stroke::new(1.0, palette.hairline),
        egui::StrokeKind::Inside,
    );
}

fn draw_custom_fallback(
    ui: &mut Ui,
    state: &mut EditorState,
    close: &mut bool,
    focus_on_open: bool,
) -> bool {
    let mut remember = false;
    ui.with_layout(Layout::top_down(Align::Min), |ui| {
        ui.label(
            egui::RichText::new("Custom colour")
                .font(Text::Label.font())
                .color(ui.visuals().text_color()),
        );
        ui.add_space(Space::XS);
        let mut color = to_egui(state.stroke_color());
        let visual_changed = egui::color_picker::color_picker_color32(
            ui,
            &mut color,
            egui::color_picker::Alpha::OnlyBlend,
        );
        ui.add_space(Space::XS);
        let [mut red, mut green, mut blue, mut alpha] = color.to_srgba_unmultiplied();
        let red_response = ui.add(egui::Slider::new(&mut red, 0..=255).text("Red"));
        let channels_changed = red_response.changed()
            | ui.add(egui::Slider::new(&mut green, 0..=255).text("Green"))
                .changed()
            | ui.add(egui::Slider::new(&mut blue, 0..=255).text("Blue"))
                .changed()
            | ui.add(egui::Slider::new(&mut alpha, 0..=255).text("Opacity"))
                .changed();
        if focus_on_open {
            red_response.request_focus();
        }
        if visual_changed || channels_changed {
            let color = Color::rgba(red, green, blue, alpha);
            if color != state.stroke_color() {
                state.set_stroke_color(color);
            }
        }
        ui.add_space(Space::SM);
        if ui.button("Done").clicked() {
            *close = true;
            remember = true;
        }
    });
    remember
}

fn activate_response(ui: &Ui, response: &Response) -> bool {
    response.clicked()
        || (response.has_focus()
            && ui.input_mut(|input| {
                input.consume_key(egui::Modifiers::NONE, Key::Enter)
                    || input.consume_key(egui::Modifiers::NONE, Key::Space)
            }))
}

fn preset_swatch(
    ui: &mut Ui,
    surface: &Surface<'_>,
    id: egui::Id,
    color: Color,
    name: &str,
    selected: bool,
) -> Response {
    let (_, rect) = ui.allocate_space(vec2(POPOVER_WIDTH, COLOR_ROW));
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| {
        WidgetInfo::selected(
            WidgetType::RadioButton,
            true,
            selected,
            format!("{name} colour"),
        )
    });
    let palette = surface.palette();
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, corner(Radius::BUTTON), palette.hover);
    }
    if response.has_focus() {
        focus_ring(
            ui.painter(),
            rect.shrink2(vec2(Space::XS, Space::HAIR)),
            Radius::BUTTON,
            palette,
        );
    }
    paint_color_dot(
        ui.painter(),
        rect.center(),
        COLOR_DOT / 2.0,
        to_egui(color),
        palette,
        selected,
    );
    response
}

fn custom_color_button(
    ui: &mut Ui,
    surface: &Surface<'_>,
    id: egui::Id,
    replacing: bool,
) -> Response {
    let (_, rect) = ui.allocate_space(vec2(POPOVER_WIDTH, COLOR_ROW));
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| {
        WidgetInfo::labeled(
            WidgetType::Button,
            true,
            if replacing {
                "Replace selected custom colour"
            } else {
                "Add custom colour"
            },
        )
    });
    let palette = surface.palette();
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, corner(Radius::BUTTON), palette.hover);
    }
    if response.has_focus() {
        focus_ring(
            ui.painter(),
            rect.shrink2(vec2(Space::XS, Space::HAIR)),
            Radius::BUTTON,
            palette,
        );
    }
    paint_spectrum(ui.painter(), rect.center(), COLOR_DOT / 2.0);
    response
}

fn custom_remove_button(ui: &mut Ui, surface: &Surface<'_>, id: egui::Id) -> Response {
    let (_, rect) = ui.allocate_space(vec2(POPOVER_WIDTH, COLOR_ROW));
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| {
        WidgetInfo::labeled(WidgetType::Button, true, "Remove selected custom colour")
    });
    let palette = surface.palette();
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, corner(Radius::BUTTON), palette.hover);
    }
    if response.has_focus() {
        focus_ring(
            ui.painter(),
            rect.shrink2(vec2(Space::XS, Space::HAIR)),
            Radius::BUTTON,
            palette,
        );
    }
    let center = rect.center();
    ui.painter().line_segment(
        [center + vec2(-5.0, 0.0), center + vec2(5.0, 0.0)],
        Stroke::new(2.0, palette.text_muted),
    );
    response
}

fn paint_color_dot(
    painter: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    color: Color32,
    palette: &crate::theme::Palette,
    selected: bool,
) {
    if selected {
        painter.circle_stroke(
            center,
            radius + 4.0,
            Stroke::new(4.0, palette.card_fill_raised),
        );
        painter.circle_stroke(center, radius + 4.0, Stroke::new(2.0, palette.focus_ring));
    }
    painter.circle_filled(center, radius, color);
    painter.circle_stroke(center, radius, Stroke::new(1.0, palette.text_muted));
    if selected {
        let ink = contrasting_ink(color);
        painter.line_segment(
            [center + vec2(-4.0, 0.0), center + vec2(-1.0, 3.0)],
            Stroke::new(2.0, ink),
        );
        painter.line_segment(
            [center + vec2(-1.0, 3.0), center + vec2(5.0, -4.0)],
            Stroke::new(2.0, ink),
        );
    }
}

fn paint_spectrum(painter: &egui::Painter, center: egui::Pos2, radius: f32) {
    const HUES: [Color32; 12] = [
        Color32::from_rgb(255, 59, 48),
        Color32::from_rgb(255, 149, 0),
        Color32::from_rgb(255, 204, 0),
        Color32::from_rgb(52, 199, 89),
        Color32::from_rgb(0, 199, 190),
        Color32::from_rgb(50, 173, 230),
        Color32::from_rgb(0, 122, 255),
        Color32::from_rgb(88, 86, 214),
        Color32::from_rgb(175, 82, 222),
        Color32::from_rgb(255, 45, 85),
        Color32::from_rgb(255, 55, 95),
        Color32::from_rgb(255, 59, 48),
    ];
    let mut mesh = egui::epaint::Mesh::default();
    let center_index = mesh.vertices.len() as u32;
    mesh.colored_vertex(center, Color32::WHITE);
    for (index, color) in HUES.into_iter().enumerate() {
        let angle = std::f32::consts::TAU * index as f32 / (HUES.len() - 1) as f32;
        mesh.colored_vertex(center + vec2(angle.cos(), angle.sin()) * radius, color);
    }
    for index in 0..HUES.len() - 1 {
        mesh.add_triangle(
            center_index,
            center_index + index as u32 + 1,
            center_index + index as u32 + 2,
        );
    }
    painter.add(egui::Shape::mesh(mesh));
    painter.circle_stroke(
        center,
        radius,
        Stroke::new(1.0, Color32::from_black_alpha(70)),
    );
}

fn contrasting_ink(color: Color32) -> Color32 {
    let luminance =
        0.299 * f32::from(color.r()) + 0.587 * f32::from(color.g()) + 0.114 * f32::from(color.b());
    if luminance > 150.0 {
        Color32::from_rgb(20, 20, 24)
    } else {
        Color32::WHITE
    }
}

fn color_name(color: Color) -> String {
    PALETTE
        .iter()
        .position(|preset| *preset == color)
        .map_or_else(
            || format!("#{:02X}{:02X}{:02X}", color.r, color.g, color.b),
            |index| PALETTE_NAMES[index].to_owned(),
        )
}

/// Converts an egui colour back into the annotation style model.
#[must_use]
pub fn from_egui(color: Color32) -> Color {
    let [red, green, blue, alpha] = color.to_srgba_unmultiplied();
    Color::rgba(red, green, blue, alpha)
}

/// Whether the focused colour disclosure owns this frame's keyboard activation.
pub(super) fn inspector_control_activation(ui: &Ui) -> bool {
    ui.memory(|memory| {
        memory.has_focus(egui::Id::new(COLOR_CONTROL_ID))
            || memory.has_focus(egui::Id::new(ARROW_CONTROL_ID))
    }) && ui.input(|input| input.key_pressed(Key::Enter) || input.key_pressed(Key::Space))
}

enum Action {
    Undo,
    Redo,
    Intent(Box<Intent>),
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

fn thickness_name(width: f64) -> &'static str {
    ARROW_THICKNESSES
        .iter()
        .min_by(|(_, left), (_, right)| (width - *left).abs().total_cmp(&(width - *right).abs()))
        .map_or("Regular", |(name, _)| *name)
}

fn stroke_keyboard_width(ui: &Ui, response: &Response, current: f64) -> Option<f64> {
    let (mut decrease, mut increase) = (false, false);
    if response.has_focus() {
        (decrease, increase) = ui.input_mut(|input| {
            (
                input.consume_key(egui::Modifiers::NONE, Key::ArrowLeft)
                    || input.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                input.consume_key(egui::Modifiers::NONE, Key::ArrowRight)
                    || input.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
            )
        });
    }
    ui.input(|input| {
        decrease |= input
            .accesskit_action_requests(response.id, egui::accesskit::Action::Decrement)
            .next()
            .is_some();
        increase |= input
            .accesskit_action_requests(response.id, egui::accesskit::Action::Increment)
            .next()
            .is_some();
    });
    if increase == decrease {
        return None;
    }
    if increase {
        ARROW_THICKNESSES
            .into_iter()
            .find(|(_, width)| *width > current + 0.01)
            .or_else(|| ARROW_THICKNESSES.last().copied())
            .map(|(_, width)| width)
    } else {
        ARROW_THICKNESSES
            .into_iter()
            .rev()
            .find(|(_, width)| *width < current - 0.01)
            .or_else(|| ARROW_THICKNESSES.first().copied())
            .map(|(_, width)| width)
    }
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
// ── Scene inspector ──────────────────────────────────────────────
//
// One spacing rhythm, one radius family, one control height. Every row below
// is built from the same four slots — label, control, value, Automatic — so a
// column of them lines up without anyone measuring, and a new property cannot
// invent its own geometry.

/// Padding between the inspector's own edges and its content.
const PANEL_PAD: f32 = Space::LG;

/// The height every inspector control shares.
const PANEL_ROW_H: f32 = crate::theme::CONTROL_H;

/// The width of a metric row's leading label.
const PANEL_LABEL_W: f32 = 74.0;

/// The width of a metric row's resolved-value read-out.
const PANEL_VALUE_W: f32 = 58.0;

/// The width of a metric row's Automatic toggle.
const PANEL_AUTO_W: f32 = 46.0;

/// The height a slider is laid out at inside a metric row.
const PANEL_SLIDER_H: f32 = 18.0;

/// Draws the Scene controls in the given `panel` rectangle.
///
/// Returns an [`Intent`] if the user triggered something the host must act on
/// (analysis request, preset save/delete, etc.). Finishing or abandoning the
/// *editing session* is not one of them: Cancel and Done live in the toolbar,
/// and this panel never grows a second pair.
pub fn draw_scene_panel(
    ui: &mut Ui,
    surface: &Surface<'_>,
    state: &mut EditorState,
    panel: Rect,
) -> Option<Intent> {
    let palette = surface.palette();
    let mut intent: Option<Intent> = None;

    ui.painter()
        .rect_filled(panel, 0.0, palette.flatten(palette.card_fill));
    ui.painter().line_segment(
        [panel.left_top(), panel.left_bottom()],
        Stroke::new(1.0, palette.divider),
    );

    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(panel));
    let child_ui = &mut child;
    ScrollArea::vertical()
        .id_salt("scene-panel")
        .show(child_ui, |ui| {
            ui.set_min_width(panel.width());
            ui.set_max_width(panel.width());
            Frame::new()
                .inner_margin(Margin {
                    left: PANEL_PAD as i8,
                    right: PANEL_PAD as i8,
                    top: PANEL_PAD as i8,
                    bottom: (PANEL_PAD * 2.0) as i8,
                })
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = vec2(Space::SM, Space::SM);
                    ui.spacing_mut().interact_size.y = PANEL_ROW_H;
                    scene_header(ui, surface, state, &mut intent);
                    if !state.has_smart_frame_draft() {
                        return;
                    }
                    section_rule(ui, palette);
                    background_section(ui, state, palette, &mut intent);
                    section_rule(ui, palette);
                    frame_section(ui, state, palette, &mut intent);
                    section_rule(ui, palette);
                    advanced_section(ui, state, palette, &mut intent);
                });
        });

    intent
}

/// Legacy name retained for hosts and harnesses compiled against Smart Frame.
pub fn draw_smart_frame_panel(
    ui: &mut Ui,
    surface: &Surface<'_>,
    state: &mut EditorState,
    panel: Rect,
) -> Option<Intent> {
    draw_scene_panel(ui, surface, state, panel)
}

/// Title, current state, and the two controls that scope the whole Scene.
fn scene_header(
    ui: &mut Ui,
    surface: &Surface<'_>,
    state: &mut EditorState,
    intent: &mut Option<Intent>,
) {
    let palette = surface.palette();
    let has_draft = state.has_smart_frame_draft();
    ui.horizontal(|ui| {
        ui.add(
            egui::Label::new(
                egui::RichText::new("Scene")
                    .font(surface.font(Text::Title))
                    .color(palette.text),
            )
            .selectable(false),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if has_draft {
                if ghost_button(ui, palette, "Remove")
                    .on_hover_text("Return exactly to the source, with no Scene")
                    .clicked()
                {
                    state.remove_scene();
                }
                // Scene keeps its own history, separate from the document's
                // (D-Scene): undoing a background choice must not also undo an
                // arrow. These are that history, and nothing else in the
                // editor offers it.
                if inspector_icon_button(
                    ui,
                    surface,
                    Icon::ArrowForwardUp,
                    "Redo Scene change",
                    "scene-redo",
                    state.can_redo_framing(),
                )
                .clicked()
                {
                    state.redo_framing();
                }
                if inspector_icon_button(
                    ui,
                    surface,
                    Icon::ArrowBackUp,
                    "Undo Scene change",
                    "scene-undo",
                    state.can_undo_framing(),
                )
                .clicked()
                {
                    state.undo_framing();
                }
            }
        });
    });

    if !has_draft {
        ui.add_space(Space::SM);
        ui.label(
            egui::RichText::new("Present this capture on a canvas of its own.")
                .color(palette.text_muted),
        );
        ui.add_space(Space::MD);
        if accent_button(ui, palette, "Add a Scene", ui.available_width()).clicked() {
            *intent = Some(state.begin_smart_frame());
        }
        return;
    }

    ui.add_space(Space::SM);
    ui.horizontal(|ui| {
        let pending = state.smart_frame_analysis_pending();
        let automatic = state
            .document()
            .scene()
            .is_some_and(|scene| scene.automatic.any());
        let (text, accent) = if pending {
            ("Resolving…", true)
        } else if automatic {
            ("Automatic", true)
        } else {
            ("Edited", false)
        };
        status_chip(ui, palette, text, accent);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(scene_size_summary(state)).color(palette.text_muted),
                )
                .selectable(false),
            );
        });
    });
    if let Some(explanation) = state
        .smart_frame_inset_explanation()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
    {
        ui.add_space(Space::XS);
        ui.label(
            egui::RichText::new(explanation)
                .small()
                .color(palette.text_muted),
        );
    }
}

/// `1432 × 978 → 1636 × 1182`, in output pixels.
fn scene_size_summary(state: &EditorState) -> String {
    let document = state.document();
    let scale = document.source().frame.scale.get();
    let px = |value: f64| (value * scale).round().max(1.0) as u64;
    let content = document.content_size();
    let output = document.output_logical_size();
    format!(
        "{} × {} → {} × {}",
        px(content.width),
        px(content.height),
        px(output.width),
        px(output.height)
    )
}

fn background_section(
    ui: &mut Ui,
    state: &mut EditorState,
    palette: &crate::theme::Palette,
    intent: &mut Option<Intent>,
) {
    let automatic = state
        .document()
        .scene()
        .is_some_and(|scene| scene.automatic.background);
    label_row(
        ui,
        palette,
        "Background",
        &background_summary(state),
        automatic,
    );

    if !automatic
        && accent_button(ui, palette, "Automatic background", ui.available_width()).clicked()
    {
        let mut scene = state.document().scene().cloned().unwrap_or_default();
        scene.automatic.background = true;
        if let Some(next) = state.apply_scene_edit(scene) {
            *intent = Some(next);
        } else if let Some(next) = state.request_automatic_background_analysis() {
            *intent = Some(next);
        }
    }

    caption(ui, palette, "Curated");
    starting_points(ui, state, palette, intent);
    caption(ui, palette, "Generated from this capture");
    generated_suggestions(ui, state, palette, intent);
    caption(ui, palette, "Source");

    let mut config = state.document().scene().cloned().unwrap_or_default();
    let before = config.clone();
    ui.horizontal(|ui| {
        background_selector(ui, &mut config, palette);
    });
    if ui
        .add_sized(
            [ui.available_width(), PANEL_ROW_H],
            egui::Button::new("Choose an image…"),
        )
        .on_hover_text("Use a picture of your own as the Scene canvas")
        .clicked()
    {
        *intent = Some(Intent::AddImage);
    }
    if config != before
        && let Some(next) = state.apply_scene_edit(config)
    {
        *intent = Some(next);
    }
}

fn background_summary(state: &EditorState) -> String {
    let Some(scene) = state.document().scene() else {
        return "None".to_owned();
    };
    match &scene.background {
        Background::Automatic(background) => match background.style {
            GeneratedStyle::Balanced => "Balanced".to_owned(),
            GeneratedStyle::Soft => "Soft".to_owned(),
            GeneratedStyle::Vibrant => "Vibrant".to_owned(),
            GeneratedStyle::Neutral => "Neutral".to_owned(),
        },
        Background::BuiltIn(background) => built_in_name(*background).to_owned(),
        Background::BlurredSource { .. } => "Blurred source".to_owned(),
        Background::Solid(_) => "Colour".to_owned(),
        Background::Gradient { .. } => "Gradient".to_owned(),
        Background::Transparent => "Clear".to_owned(),
        Background::Desktop(_) => "Desktop".to_owned(),
        Background::Image(_) => "Image".to_owned(),
    }
}

const fn built_in_name(background: BuiltInBackground) -> &'static str {
    match background {
        BuiltInBackground::Mist => "Mist",
        BuiltInBackground::Iris => "Iris",
        BuiltInBackground::Midnight => "Midnight",
        BuiltInBackground::Sunrise => "Sunrise",
        BuiltInBackground::Lagoon => "Lagoon",
        BuiltInBackground::Sand => "Sand",
    }
}

/// The two spacings, and the subject's own treatment.
///
/// *Inner* is the capture's own margin, held back so its content sits centred
/// inside the Scene. *Outer* is the space between that content and the Scene
/// background. They are different distances and they get different rows.
fn frame_section(
    ui: &mut Ui,
    state: &mut EditorState,
    palette: &crate::theme::Palette,
    intent: &mut Option<Intent>,
) {
    let mut config = state.document().scene().cloned().unwrap_or_default();
    let before = config.clone();
    let stylable = state.document().may_style_subject();
    let content = state.document().content_size();
    let inset_limit = SourceInsets::limit_for(content.width)
        .min(SourceInsets::limit_for(content.height))
        .max(1.0);
    // Set when a property is handed *back* to Automatic, which is the one
    // inspector edit that needs the capture looked at again rather than just
    // stored.
    let mut resolve_again = false;

    label_row(ui, palette, "Frame", "", false);

    if stylable {
        // One slider for four edges. An automatic inset can be asymmetric —
        // the detector measures each edge on its own — and touching this
        // control replaces it with a uniform one, which is exactly what fixing
        // a value by hand means everywhere else in this panel.
        let mut inner = config.inset.largest();
        let row = metric_row(
            ui,
            palette,
            "Inner",
            &mut inner,
            0.0..=inset_limit,
            config.automatic.inset,
            true,
        );
        if row.changed {
            config.inset = SourceInsets::uniform(inner);
        }
        if let Some(automatic) = row.automatic {
            config.automatic.inset = automatic;
            resolve_again |= automatic;
        }
        ui.add(
            egui::Label::new(
                egui::RichText::new(
                    "Holds back the capture's own outer margin. Nothing is discarded.",
                )
                .small()
                .color(palette.text_muted),
            )
            .selectable(false),
        );
    }

    let mut padding = config.padding;
    let row = metric_row(
        ui,
        palette,
        "Outer",
        &mut padding,
        0.0..=220.0,
        config.automatic.padding,
        true,
    );
    if row.changed {
        config.set_uniform_padding(padding);
    }
    if let Some(automatic) = row.automatic {
        config.automatic.padding = automatic;
        resolve_again |= automatic;
    }

    if stylable {
        let mut corners = config.corner_radius;
        let row = metric_row(
            ui,
            palette,
            "Corners",
            &mut corners,
            0.0..=80.0,
            config.automatic.corners,
            true,
        );
        if row.changed {
            config.corner_radius = corners;
        }
        if let Some(automatic) = row.automatic {
            config.automatic.corners = automatic;
            resolve_again |= automatic;
        }

        let mut shadow = config.shadow;
        let row = metric_row(
            ui,
            palette,
            "Shadow",
            &mut shadow,
            0.0..=80.0,
            config.automatic.shadow,
            true,
        );
        if row.changed {
            config.shadow = shadow;
        }
        if let Some(automatic) = row.automatic {
            config.automatic.shadow = automatic;
            resolve_again |= automatic;
        }
    } else {
        native_subject_note(ui, palette);
        config.preserve_native_subject();
    }

    label_row(
        ui,
        palette,
        "Placement",
        if config.auto_balance {
            "Optically balanced"
        } else {
            alignment_name(config.alignment)
        },
        config.automatic.placement,
    );
    ui.horizontal(|ui| {
        ui.checkbox(&mut config.auto_balance, "Optical balance");
    });
    if !config.auto_balance {
        alignment_row(ui, &mut config.alignment);
    }

    if config != before {
        let edit = state.apply_scene_edit(config);
        // A property handed back to Automatic has no resolved value until the
        // capture is analysed again, so asking for that analysis is part of
        // the same edit rather than something the user has to trigger.
        *intent = if resolve_again {
            state.request_scene_automatic_analysis().or(edit)
        } else {
            edit
        };
    }
}

/// Everything that is a deliberate choice rather than a first impression.
fn advanced_section(
    ui: &mut Ui,
    state: &mut EditorState,
    palette: &crate::theme::Palette,
    intent: &mut Option<Intent>,
) {
    egui::CollapsingHeader::new(
        egui::RichText::new("Output and presets")
            .color(palette.text)
            .strong(),
    )
    .id_salt("scene-advanced")
    .default_open(false)
    .show_unindented(ui, |ui| {
        ui.spacing_mut().item_spacing = vec2(Space::SM, Space::SM);
        output_controls(ui, state, palette, intent);
        section_rule(ui, palette);
        preset_library(ui, state, palette, intent);
        section_rule(ui, palette);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_sized(
                    [ui.available_width(), PANEL_ROW_H],
                    egui::Button::new("Reset to Automatic"),
                )
                .on_hover_text("Resolve every Scene value from this capture again")
                .clicked()
                && let Some(next) = state.reset_scene_to_auto()
            {
                *intent = Some(next);
            }
        });
        if ui
            .add_sized(
                [ui.available_width(), PANEL_ROW_H],
                egui::Button::new("Clear canvas"),
            )
            .on_hover_text("Keep Scene spacing and controls, but use a transparent canvas")
            .clicked()
        {
            state.clear_scene_canvas();
        }
    });
}

fn output_controls(
    ui: &mut Ui,
    state: &mut EditorState,
    palette: &crate::theme::Palette,
    intent: &mut Option<Intent>,
) {
    let mut config = state.document().scene().cloned().unwrap_or_default();
    let before = config.clone();

    label_row(ui, palette, "Ratio", aspect_name(config.aspect), false);
    aspect_row(ui, &mut config.aspect);
    if config.aspect != AspectPreset::Original {
        config.output_size = None;
    }

    let mut exact = config.output_size.is_some();
    ui.checkbox(&mut exact, "Minimum output size");
    if exact {
        let default = config.output_size.unwrap_or_else(|| {
            let scale = state.document().source().frame.scale.get();
            let size = config.output_size(state.document().content_size());
            ExactOutputSize {
                width: (size.width * scale).round().max(1.0) as u32,
                height: (size.height * scale).round().max(1.0) as u32,
            }
        });
        let mut width = default.width;
        let mut height = default.height;
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut width)
                    .range(1..=16_384)
                    .prefix("W "),
            );
            ui.add(
                egui::DragValue::new(&mut height)
                    .range(1..=16_384)
                    .prefix("H "),
            );
        });
        config.output_size = Some(ExactOutputSize { width, height });
    } else {
        config.output_size = None;
    }

    if config != before
        && let Some(next) = state.apply_scene_edit(config)
    {
        *intent = Some(next);
    }
}

const fn aspect_name(aspect: AspectPreset) -> &'static str {
    match aspect {
        AspectPreset::Original => "Original",
        AspectPreset::Square => "1:1",
        AspectPreset::Portrait => "4:5",
        AspectPreset::Story => "9:16",
        AspectPreset::Landscape => "16:9",
        AspectPreset::Wide => "3:1",
    }
}

// ── Inspector primitives ─────────────────────────────────────────

/// What one [`metric_row`] decided this frame.
struct MetricRow {
    /// The slider moved.
    changed: bool,
    /// The Automatic toggle was pressed, and this is its new state.
    automatic: Option<bool>,
}

/// Label, slider, value and Automatic, at fixed widths.
fn metric_row(
    ui: &mut Ui,
    palette: &crate::theme::Palette,
    label: &str,
    value: &mut f64,
    range: std::ops::RangeInclusive<f64>,
    automatic: bool,
    supports_auto: bool,
) -> MetricRow {
    let mut outcome = MetricRow {
        changed: false,
        automatic: None,
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = Space::SM;
        ui.add_sized(
            [PANEL_LABEL_W, PANEL_ROW_H],
            egui::Label::new(egui::RichText::new(label).color(palette.text))
                .selectable(false)
                .halign(Align::LEFT),
        );
        let auto_w = if supports_auto {
            PANEL_AUTO_W + Space::SM
        } else {
            0.0
        };
        let slider_w =
            (ui.available_width() - PANEL_VALUE_W - auto_w - Space::SM).clamp(40.0, 400.0);
        // Shorter than the row on purpose: egui derives the handle radius
        // from the height it is given, so a full-height slider grows a handle
        // that dwarfs the value beside it.
        let response = ui.add_sized(
            [slider_w, PANEL_SLIDER_H],
            egui::Slider::new(value, range).show_value(false),
        );
        response.widget_info(|| {
            WidgetInfo::labeled(WidgetType::Slider, true, format!("{label}: {value:.0} pt"))
        });
        outcome.changed = response.changed();
        value_box(ui, palette, &format!("{value:.0} pt"));
        if supports_auto {
            let toggle = auto_toggle(ui, palette, automatic);
            if toggle.clicked() {
                outcome.automatic = Some(!automatic);
            }
        }
    });
    outcome
}

/// A section's name and the value it currently resolves to.
fn label_row(
    ui: &mut Ui,
    palette: &crate::theme::Palette,
    label: &str,
    resolved: &str,
    automatic: bool,
) {
    ui.horizontal(|ui| {
        ui.add(
            egui::Label::new(egui::RichText::new(label).color(palette.text).strong())
                .selectable(false),
        );
        if resolved.is_empty() {
            return;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(
                egui::Label::new(egui::RichText::new(resolved).small().color(if automatic {
                    palette.on_accent_wash()
                } else {
                    palette.text_muted
                }))
                .selectable(false),
            );
        });
    });
}

/// A read-only numeric read-out, on the one recessed surface.
fn value_box(ui: &mut Ui, palette: &crate::theme::Palette, text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(PANEL_VALUE_W, PANEL_ROW_H), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, corner(Radius::CHIP), palette.well());
    painter.rect_stroke(
        rect,
        corner(Radius::CHIP),
        Stroke::new(1.0, palette.hairline),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        Text::Caption.font(),
        palette.text,
    );
}

/// The per-property Automatic switch.
fn auto_toggle(ui: &mut Ui, palette: &crate::theme::Palette, on: bool) -> Response {
    let (fill, ink, edge) = if on {
        (
            palette.accent_wash(),
            palette.on_accent_wash(),
            palette.accent,
        )
    } else {
        (palette.control_fill(), palette.text_muted, palette.hairline)
    };
    let response = ui.add_sized(
        [PANEL_AUTO_W, PANEL_ROW_H],
        egui::Button::new(egui::RichText::new("Auto").small().color(ink))
            .fill(fill)
            .stroke(Stroke::new(1.0, edge))
            .corner_radius(corner(Radius::CHIP)),
    );
    response.widget_info(|| {
        WidgetInfo::labeled(
            WidgetType::Button,
            true,
            if on {
                "Automatic: on"
            } else {
                "Automatic: off"
            },
        )
    });
    if on {
        response.on_hover_text("Resolved from this capture. Press to fix the current value.")
    } else {
        response.on_hover_text("Fixed. Press to resolve this from the capture again.")
    }
}

/// A full-width primary action.
fn accent_button(
    ui: &mut Ui,
    palette: &crate::theme::Palette,
    label: &str,
    width: f32,
) -> Response {
    ui.add_sized(
        [width, PANEL_ROW_H + 6.0],
        egui::Button::new(egui::RichText::new(label).color(palette.on_accent).strong())
            .fill(palette.accent)
            .corner_radius(corner(Radius::BUTTON)),
    )
}

/// One square icon action on an inspector row.
fn inspector_icon_button(
    ui: &mut Ui,
    surface: &Surface<'_>,
    icon: Icon,
    label: &str,
    salt: &'static str,
    enabled: bool,
) -> Response {
    let (rect, _) = ui.allocate_exact_size(vec2(PANEL_ROW_H, PANEL_ROW_H), Sense::hover());
    let flags = if enabled {
        ControlState::new()
    } else {
        ControlState::disabled()
    };
    let response = icon_button(
        ui,
        surface,
        rect,
        ui.id().with(salt),
        icon,
        label,
        flags,
        Reveal::SHOWN,
    );
    response.on_hover_text(label)
}

/// A quiet text action that carries no fill until it is hovered.
fn ghost_button(ui: &mut Ui, palette: &crate::theme::Palette, label: &str) -> Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(palette.text_muted))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::NONE)
            .corner_radius(corner(Radius::CHIP))
            .min_size(vec2(0.0, PANEL_ROW_H)),
    )
}

/// The draft's current disposition, as one small badge.
fn status_chip(ui: &mut Ui, palette: &crate::theme::Palette, text: &str, accent: bool) {
    let (fill, ink) = if accent {
        (palette.accent_wash(), palette.on_accent_wash())
    } else {
        (palette.control_fill(), palette.text)
    };
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), Text::Caption.font(), ink);
    let (rect, response) = ui.allocate_exact_size(
        vec2(galley.size().x + Space::MD, PANEL_ROW_H - 6.0),
        Sense::hover(),
    );
    response.widget_info(|| {
        WidgetInfo::labeled(WidgetType::Label, true, format!("Scene status: {text}"))
    });
    let painter = ui.painter();
    painter.rect_filled(rect, corner(Radius::pill(rect.height())), fill);
    painter.galley(rect.center() - galley.size() / 2.0, galley, ink);
}

/// A small all-caps group heading inside a section.
fn caption(ui: &mut Ui, palette: &crate::theme::Palette, text: &str) {
    ui.add_space(Space::XS);
    ui.add(
        egui::Label::new(egui::RichText::new(text).small().color(palette.text_faint))
            .selectable(false),
    );
}

fn starting_points(
    ui: &mut Ui,
    state: &mut EditorState,
    palette: &crate::theme::Palette,
    intent: &mut Option<Intent>,
) {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(Space::XS, Space::XS);
        ui.horizontal_wrapped(|ui| {
            for background in [
                BuiltInBackground::Mist,
                BuiltInBackground::Iris,
                BuiltInBackground::Midnight,
                BuiltInBackground::Sunrise,
                BuiltInBackground::Lagoon,
                BuiltInBackground::Sand,
            ] {
                let selected = matches!(
                    state.document().scene(),
                    Some(scene) if scene.background == Background::BuiltIn(background)
                );
                let response = pill_button(ui, built_in_name(background), selected, palette);
                if response.clicked() {
                    let mut scene = state
                        .document()
                        .scene()
                        .cloned()
                        .unwrap_or_else(|| Beautification::preset(BeautificationPreset::Clean));
                    scene.background = Background::BuiltIn(background);
                    scene.automatic.background = false;
                    let has_automatic_properties = scene.automatic.any();
                    state.begin_with(scene);
                    if has_automatic_properties
                        && let Some(i) = state.request_scene_automatic_analysis()
                    {
                        *intent = Some(i);
                    }
                }
            }
        });
    });
}

fn generated_suggestions(
    ui: &mut Ui,
    state: &mut EditorState,
    palette: &crate::theme::Palette,
    intent: &mut Option<Intent>,
) {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(Space::XS, Space::XS);
        ui.horizontal_wrapped(|ui| {
            for (label, style) in [
                ("Balanced", GeneratedStyle::Balanced),
                ("Soft", GeneratedStyle::Soft),
                ("Vibrant", GeneratedStyle::Vibrant),
                ("Neutral", GeneratedStyle::Neutral),
            ] {
                let selected = matches!(
                    state.document().scene().map(|scene| &scene.background),
                    Some(Background::Automatic(background)) if background.style == style
                );
                if pill_button(ui, label, selected, palette).clicked() {
                    *intent = state.set_generated_scene_style(style);
                }
            }
        });
    });
}

fn preset_library(
    ui: &mut Ui,
    state: &mut EditorState,
    palette: &crate::theme::Palette,
    intent: &mut Option<Intent>,
) {
    let presets = state.custom_presets().to_vec();
    let allow_save = state.has_smart_frame_draft();

    if presets.is_empty() && !allow_save {
        return;
    }
    label_row(ui, palette, "Presets", "", false);
    if !presets.is_empty() {
        let selected_name = state
            .selected_preset()
            .and_then(|id| presets.iter().find(|p| p.id == id))
            .map_or("Choose a preset", |p| p.name.as_str())
            .to_owned();
        egui::ComboBox::from_id_salt("smart-frame-custom-preset")
            .selected_text(&selected_name)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for preset in &presets {
                    let is_selected = state.selected_preset() == Some(preset.id.as_str());
                    if ui.selectable_label(is_selected, &preset.name).clicked() {
                        state.set_selected_preset(Some(preset.id.clone()));
                        state.set_preset_name(preset.name.clone());
                        state.begin_with(preset.settings.to_beautification());
                        if preset.settings.automatic.any()
                            && let Some(i) = state.request_scene_automatic_analysis()
                        {
                            *intent = Some(i);
                        }
                    }
                }
            });
    }
    if !allow_save {
        return;
    }
    let mut name = state.preset_name().to_owned();
    ui.add_sized(
        [ui.available_width(), PANEL_ROW_H],
        egui::TextEdit::singleline(&mut name).hint_text("Preset name"),
    );
    state.set_preset_name(name);

    let updates_selected = state
        .selected_preset()
        .and_then(|id| presets.iter().find(|p| p.id == id))
        .is_some_and(|p| p.name == state.preset_name().trim());
    let save_label = if updates_selected { "Update" } else { "Save" };
    ui.horizontal(|ui| {
        let slots = 1.0 + f32::from(u8::from(state.selected_preset().is_some())) * 2.0;
        let width = ((ui.available_width() - Space::SM * (slots - 1.0)) / slots).max(40.0);
        if ui
            .add_enabled(
                !state.preset_name().trim().is_empty(),
                egui::Button::new(save_label).min_size(vec2(width, PANEL_ROW_H)),
            )
            .clicked()
        {
            match state.build_preset(false) {
                Ok(preset) => {
                    state.upsert_local_preset(preset.clone());
                    *intent = Some(Intent::UpsertPreset(Box::new(preset)));
                }
                Err(error) => tracing::warn!(%error, "preset build failed"),
            }
        }
        if state.selected_preset().is_some() {
            if ui
                .add(egui::Button::new("Duplicate").min_size(vec2(width, PANEL_ROW_H)))
                .clicked()
            {
                match state.build_preset(true) {
                    Ok(preset) => {
                        state.upsert_local_preset(preset.clone());
                        *intent = Some(Intent::UpsertPreset(Box::new(preset)));
                    }
                    Err(error) => tracing::warn!(%error, "preset duplicate failed"),
                }
            }
            if let Some(id) = state.selected_preset().map(str::to_owned)
                && ui
                    .add(egui::Button::new("Delete").min_size(vec2(width, PANEL_ROW_H)))
                    .clicked()
            {
                state.delete_preset(&id);
                *intent = Some(Intent::DeletePreset(id));
            }
        }
    });
}

fn alignment_name(alignment: Alignment) -> &'static str {
    match alignment {
        Alignment::TopLeft => "Top left",
        Alignment::Top => "Top",
        Alignment::TopRight => "Top right",
        Alignment::Left => "Left",
        Alignment::Center => "Center",
        Alignment::Right => "Right",
        Alignment::BottomLeft => "Bottom left",
        Alignment::Bottom => "Bottom",
        Alignment::BottomRight => "Bottom right",
    }
}

fn alignment_row(ui: &mut Ui, alignment: &mut Alignment) {
    egui::ComboBox::from_id_salt("scene-alignment")
        .selected_text(alignment_name(*alignment))
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for candidate in [
                Alignment::TopLeft,
                Alignment::Top,
                Alignment::TopRight,
                Alignment::Left,
                Alignment::Center,
                Alignment::Right,
                Alignment::BottomLeft,
                Alignment::Bottom,
                Alignment::BottomRight,
            ] {
                ui.selectable_value(alignment, candidate, alignment_name(candidate));
            }
        });
}

fn background_selector(ui: &mut Ui, config: &mut Beautification, _palette: &crate::theme::Palette) {
    let choices = [
        ("Automatic", 0u8),
        ("Blurred source", 1),
        ("Colour", 2),
        ("Gradient", 3),
        ("Clear", 4),
    ];
    let current = match &config.background {
        Background::Automatic(_) => 0,
        Background::BlurredSource { .. } => 1,
        Background::Solid(_) => 2,
        Background::Gradient { .. } => 3,
        Background::Transparent => 4,
        Background::Desktop(_) | Background::Image(_) | Background::BuiltIn(_) => 5,
    };
    let current_label = match &config.background {
        Background::Desktop(_) => "Desktop",
        Background::Image(_) => "Image",
        Background::BuiltIn(background) => built_in_name(*background),
        _ => choices[current as usize].0,
    };
    egui::ComboBox::from_id_salt("scene-background")
        .selected_text(current_label)
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for &(label, idx) in &choices {
                if ui.selectable_label(current == idx, label).clicked() {
                    config.background = match idx {
                        0 => Background::Automatic(scrozz_annotate::AutomaticBackground::fallback(
                            scrozz_core::ColorSpace::Unknown,
                        )),
                        1 => Background::BlurredSource {
                            blur_radius: 28,
                            tint: Color::rgba(20, 24, 32, 72),
                        },
                        2 => Background::Solid(scrozz_annotate::Color::WHITE),
                        3 => Background::Gradient {
                            start: scrozz_annotate::Color::WHITE,
                            end: scrozz_annotate::Color::rgb(0x00, 0x00, 0x00),
                        },
                        4 => Background::Transparent,
                        _ => unreachable!(),
                    };
                }
            }
        });
}

fn aspect_row(ui: &mut Ui, aspect: &mut AspectPreset) {
    let choices = [
        AspectPreset::Original,
        AspectPreset::Landscape,
        AspectPreset::Square,
        AspectPreset::Portrait,
        AspectPreset::Story,
        AspectPreset::Wide,
    ];
    egui::ComboBox::from_id_salt("scene-aspect")
        .selected_text(aspect_name(*aspect))
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for candidate in choices {
                if ui
                    .selectable_label(*aspect == candidate, aspect_name(candidate))
                    .clicked()
                {
                    *aspect = candidate;
                }
            }
        });
}

fn section_rule(ui: &mut Ui, palette: &crate::theme::Palette) {
    ui.add_space(Space::MD);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment(
        [
            pos2(rect.left(), rect.center().y),
            pos2(rect.right(), rect.center().y),
        ],
        Stroke::new(1.0, palette.hairline),
    );
    ui.add_space(Space::MD);
}

/// The one thing a native window capture will not do, said once.
fn native_subject_note(ui: &mut Ui, palette: &crate::theme::Palette) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(
                "This window keeps its own silhouette, corners and shadow. \
                 Only the canvas around it changes.",
            )
            .small()
            .color(palette.text_muted),
        )
        .selectable(false),
    );
}

fn pill_button(
    ui: &mut Ui,
    label: &str,
    selected: bool,
    palette: &crate::theme::Palette,
) -> Response {
    let (fill, ink, edge) = if selected {
        (
            palette.accent_wash(),
            palette.on_accent_wash(),
            palette.accent,
        )
    } else {
        (palette.control_fill(), palette.text, palette.hairline)
    };
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(ink))
            .fill(fill)
            .stroke(Stroke::new(1.0, edge))
            .corner_radius(corner(Radius::CHIP))
            .min_size(vec2(0.0, PANEL_ROW_H)),
    )
}
