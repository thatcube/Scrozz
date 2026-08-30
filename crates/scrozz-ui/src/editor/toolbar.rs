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
    ExactOutputSize, GeneratedStyle,
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

/// The stroke-width slider's length, in points.
const STROKE: f32 = 104.0;
const REDACT_INTENSITY: f32 = 212.0;

/// The width the tool palette occupies, including its trailing gap.
const TOOLS_W: f32 = Tool::ALL.len() as f32 * (BUTTON + Space::HAIR);

/// The width the compact colour control occupies.
const COLOR_W: f32 = COLOR_CONTROL;
const ARROW_W: f32 = ARROW_CONTROL;

/// The space a divider needs: the gap before it, and the gap after.
const DIVIDER_W: f32 = Space::SM + Space::SM + Space::XS;

/// The width of the right-hand action group, at its widest.
///
/// Five right-aligned document actions.
const ACTIONS_W: f32 = 5.0f32.mul_add(BUTTON, 4.0 * Space::HAIR);

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
        (
            Icon::DeviceFloppy,
            "Save",
            Action::Intent(Box::new(Intent::Save)),
        ),
        (Icon::Copy, "Copy", Action::Intent(Box::new(Intent::Copy))),
        (Icon::ArrowForwardUp, "Redo", Action::Redo),
        (Icon::ArrowBackUp, "Undo", Action::Undo),
        (
            Icon::LayoutGrid,
            "Scene",
            Action::Intent(Box::new(Intent::ToggleSmartFrame)),
        ),
    ] {
        let rect = Rect::from_center_size(pos2(rx + BUTTON / 2.0, cy), vec2(BUTTON, BUTTON));
        let enabled = match action {
            Action::Undo => state.can_undo(),
            Action::Redo => state.can_redo(),
            Action::Intent(_) => true,
        };
        let selected = matches!(
            &action,
            Action::Intent(intent) if **intent == Intent::ToggleSmartFrame
        ) && state.has_smart_frame_draft();
        let flags = if enabled {
            ControlState::new().selected(selected)
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
        canvas.bottom() - BUTTON - Space::SM,
    );
    egui::Area::new(egui::Id::new("editor-zoom-control"))
        .order(egui::Order::Foreground)
        .fixed_pos(position)
        .show(ui.ctx(), |ui| {
            ui.set_opacity(1.0);
            let frame = Frame::new()
                .fill(palette.card_fill_raised)
                .stroke(Stroke::new(1.0, palette.hairline))
                .corner_radius(corner(Radius::BUTTON))
                .inner_margin(Margin::symmetric(Space::SM as i8, Space::XS as i8));
            frame.show(ui, |ui| {
                let label = format!("{:.0}%", view.scale() * 100.0);
                ui.menu_button(label, |ui| {
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
                        ui.add(
                            egui::Label::new(format!("Image {source_width} x {source_height} px"))
                                .sense(Sense::focusable_noninteractive()),
                        );
                    }

                    let mut snap = state.crop_snap_edges();
                    let snap_response = ui.checkbox(&mut snap, "Snap to edges");
                    if snap_response.changed() {
                        state.set_crop_snap_edges(snap);
                    }
                    snap_response
                        .on_hover_text("Hold Command/Ctrl while dragging to disable snapping");

                    if ui.button("Cancel").clicked() {
                        let _ = state.command(Command::CancelCrop);
                    }
                    if state.document().crop().is_some()
                        && ui.button("Revert to Original").clicked()
                    {
                        let _ = state.command(Command::RevertCrop);
                    }
                    if ui.button("Crop").clicked() {
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

// ── Scene toolbar integration ────────────────────────────────────

/// Draws the Scene controls in the given `panel` rectangle.
///
/// Returns an [`Intent`] if the user triggered something the host must act on
/// (analysis request, preset save/delete, etc.).
pub fn draw_scene_panel(
    ui: &mut Ui,
    surface: &Surface<'_>,
    state: &mut EditorState,
    panel: Rect,
) -> Option<Intent> {
    let palette = surface.palette();
    let mut intent: Option<Intent> = None;

    ui.painter().rect_filled(panel, 0.0, palette.canvas());
    ui.painter().line_segment(
        [panel.left_top(), panel.left_bottom()],
        Stroke::new(1.0, palette.divider),
    );

    // Clip to panel bounds.
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(panel));
    let child_ui = &mut child;
    ScrollArea::vertical()
        .id_salt("scene-panel")
        .show(child_ui, |ui| {
            ui.set_min_width(panel.width() - Space::MD * 2.0);
            ui.add_space(Space::SM);

            if !state.has_smart_frame_draft() {
                smart_frame_intro(ui, palette);
                ui.add_space(Space::SM);
                let label = "Apply Automatic Scene";
                let btn = ui.add_sized(
                    [ui.available_width(), 44.0],
                    egui::Button::new(
                        egui::RichText::new(label)
                            .size(14.0)
                            .color(palette.on_accent)
                            .strong(),
                    )
                    .fill(palette.accent)
                    .corner_radius(egui::CornerRadius::same(10)),
                );
                if btn.clicked() {
                    intent = Some(state.begin_smart_frame());
                }
                ui.add_space(Space::SM);
                starting_points(ui, state, palette, &mut intent);
            } else {
                draft_header(ui, state, palette, &mut intent);
                ui.add_space(Space::SM);

                section_label(ui, "CURATED BACKGROUNDS", palette);
                starting_points(ui, state, palette, &mut intent);
                ui.add_space(Space::SM);

                section_label(ui, "GENERATED FOR THIS CAPTURE", palette);
                generated_suggestions(ui, state, palette, &mut intent);

                advanced_controls(ui, state, palette, &mut intent);
                preset_library(ui, state, palette, &mut intent);

                section_rule(ui, palette);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(state.can_undo_framing(), egui::Button::new("Undo Scene"))
                        .clicked()
                    {
                        state.undo_framing();
                    }
                    if ui
                        .add_enabled(state.can_redo_framing(), egui::Button::new("Redo"))
                        .clicked()
                    {
                        state.redo_framing();
                    }
                });
            }
            ui.add_space(Space::MD);
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

fn smart_frame_intro(ui: &mut Ui, palette: &crate::theme::Palette) {
    Frame::new()
        .fill(palette.card_fill)
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(Margin::same(14))
        .stroke(Stroke::new(1.0, palette.hairline))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new("SCENE")
                    .font(egui::FontId::monospace(10.0))
                    .color(palette.accent)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Present the capture without changing it")
                    .size(17.0)
                    .color(palette.text)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Scene adds a reversible canvas around untouched capture pixels. \
                     Export flattens a copy; Remove Scene restores the source.",
                )
                .small()
                .color(palette.text_muted),
            );
        });
}

fn draft_header(
    ui: &mut Ui,
    state: &mut EditorState,
    palette: &crate::theme::Palette,
    intent: &mut Option<Intent>,
) {
    let analysis_pending = state.smart_frame_analysis_pending();
    let inset_explanation = state
        .smart_frame_inset_explanation()
        .unwrap_or("")
        .to_owned();

    Frame::new()
        .fill(palette.accent.gamma_multiply(0.10))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(Margin::same(14))
        .stroke(Stroke::new(1.0, palette.accent.gamma_multiply(0.55)))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new("SCENE")
                    .font(egui::FontId::monospace(10.0))
                    .color(palette.accent)
                    .strong(),
            );
            ui.add_space(5.0);
            ui.label(
                egui::RichText::new(if analysis_pending {
                    "Resolving automatic choices..."
                } else {
                    "Editable and reversible"
                })
                .color(palette.text)
                .strong(),
            );
            ui.label(
                egui::RichText::new(inset_explanation)
                    .small()
                    .color(palette.text_muted),
            );
            if !analysis_pending
                && ui.small_button("Reset to Automatic").clicked()
                && let Some(i) = state.reset_scene_to_auto()
            {
                *intent = Some(i);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let apply = ui.add_sized(
                    [112.0, 34.0],
                    egui::Button::new(
                        egui::RichText::new("Done")
                            .strong()
                            .color(palette.on_accent),
                    )
                    .fill(palette.accent)
                    .corner_radius(egui::CornerRadius::same(8)),
                );
                if apply.clicked() {
                    state.apply_scene();
                }
                if ui
                    .add_sized([92.0, 34.0], egui::Button::new("Cancel"))
                    .clicked()
                {
                    state.cancel_scene();
                }
            });
        });
}

fn starting_points(
    ui: &mut Ui,
    state: &mut EditorState,
    palette: &crate::theme::Palette,
    intent: &mut Option<Intent>,
) {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.horizontal_wrapped(|ui| {
            for (label, background) in [
                ("Mist", BuiltInBackground::Mist),
                ("Iris", BuiltInBackground::Iris),
                ("Midnight", BuiltInBackground::Midnight),
                ("Sunrise", BuiltInBackground::Sunrise),
                ("Lagoon", BuiltInBackground::Lagoon),
                ("Sand", BuiltInBackground::Sand),
            ] {
                let selected = matches!(
                    state.document().scene(),
                    Some(scene) if scene.background == Background::BuiltIn(background)
                );
                let response = pill_button(ui, label, selected, palette);
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
        ui.spacing_mut().item_spacing.x = 4.0;
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
    ui.label(
        egui::RichText::new("Your presets")
            .color(palette.text)
            .strong(),
    );
    if presets.is_empty() {
        ui.label(
            egui::RichText::new("Save the current draft to reuse it on another capture.")
                .small()
                .color(palette.text_muted),
        );
    } else {
        let selected_name = state
            .selected_preset()
            .and_then(|id| presets.iter().find(|p| p.id == id))
            .map_or("Choose a custom preset", |p| p.name.as_str())
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
    ui.add_space(7.0);
    let mut name = state.preset_name().to_owned();
    ui.add(
        egui::TextEdit::singleline(&mut name)
            .hint_text("Preset name")
            .desired_width(ui.available_width()),
    );
    state.set_preset_name(name);

    let updates_selected = state
        .selected_preset()
        .and_then(|id| presets.iter().find(|p| p.id == id))
        .is_some_and(|p| p.name == state.preset_name().trim());
    let save_label = if updates_selected {
        "Update Scene preset"
    } else {
        "Save Scene as Preset"
    };
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                !state.preset_name().trim().is_empty(),
                egui::Button::new(save_label),
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
        if state.selected_preset().is_some() && ui.small_button("Duplicate").clicked() {
            match state.build_preset(true) {
                Ok(preset) => {
                    state.upsert_local_preset(preset.clone());
                    *intent = Some(Intent::UpsertPreset(Box::new(preset)));
                }
                Err(error) => tracing::warn!(%error, "preset duplicate failed"),
            }
        }
        if let Some(id) = state.selected_preset().map(str::to_owned)
            && ui.small_button("Delete").clicked()
        {
            state.delete_preset(&id);
            *intent = Some(Intent::DeletePreset(id));
        }
    });
}

fn advanced_controls(
    ui: &mut Ui,
    state: &mut EditorState,
    palette: &crate::theme::Palette,
    intent: &mut Option<Intent>,
) {
    let mut config = state
        .document()
        .beautification()
        .cloned()
        .unwrap_or_default();
    let before = config.clone();

    section_rule(ui, palette);
    section_label(ui, "BACKGROUND", palette);
    background_selector(ui, &mut config, palette);

    section_rule(ui, palette);
    section_label(ui, "CANVAS", palette);
    let mut padding = config.padding;
    property_status(
        ui,
        "Padding",
        config.automatic.padding,
        format!("{padding:.0} pt"),
        palette,
    );
    if ui
        .add(egui::Slider::new(&mut padding, 0.0..=220.0).show_value(false))
        .changed()
    {
        config.set_uniform_padding(padding);
    }

    ui.add_space(6.0);
    property_status(
        ui,
        "Placement",
        config.automatic.placement,
        if config.auto_balance {
            "Subtle optical balance"
        } else {
            alignment_name(config.alignment)
        },
        palette,
    );
    ui.checkbox(&mut config.auto_balance, "Automatic balance");
    if !config.auto_balance {
        alignment_row(ui, &mut config.alignment);
    }

    ui.add_space(8.0);
    ui.label(
        egui::RichText::new("Aspect ratio")
            .color(palette.text)
            .strong(),
    );
    aspect_row(ui, &mut config.aspect);
    if config.aspect != AspectPreset::Original {
        config.output_size = None;
    }

    ui.add_space(8.0);
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

    section_rule(ui, palette);
    section_label(ui, "APPEARANCE", palette);
    ui.add_enabled_ui(state.document().may_style_subject(), |ui| {
        property_status(
            ui,
            "Corners",
            config.automatic.corners,
            format!("{:.0} pt", config.corner_radius),
            palette,
        );
        ui.add(egui::Slider::new(&mut config.corner_radius, 0.0..=80.0).show_value(false));
        property_status(
            ui,
            "Shadow",
            config.automatic.shadow,
            format!("{:.0} pt", config.shadow),
            palette,
        );
        ui.add(egui::Slider::new(&mut config.shadow, 0.0..=80.0).show_value(false));
    });
    if !state.document().may_style_subject() {
        d9_outer_canvas_note(ui, palette);
        config.corner_radius = 0.0;
        config.shadow = 0.0;
        config.border_width = 0.0;
    }

    if config != before
        && let Some(i) = state.apply_scene_edit(config)
    {
        *intent = Some(i);
    }

    section_rule(ui, palette);
    ui.horizontal_wrapped(|ui| {
        if ui.button("Reset to Automatic").clicked()
            && let Some(next) = state.reset_scene_to_auto()
        {
            *intent = Some(next);
        }
        if ui
            .button("Clear canvas")
            .on_hover_text("Keep Scene spacing and controls, but use a transparent canvas.")
            .clicked()
        {
            state.clear_scene_canvas();
        }
        if ui
            .button("Remove Scene")
            .on_hover_text("Return exactly to the source without Scene.")
            .clicked()
        {
            state.remove_scene();
        }
    });
}

fn property_status(
    ui: &mut Ui,
    label: &str,
    automatic: bool,
    resolved: impl std::fmt::Display,
    palette: &crate::theme::Palette,
) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).color(palette.text).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                egui::RichText::new(if automatic {
                    format!("Automatic · {resolved}")
                } else {
                    resolved.to_string()
                })
                .small()
                .color(if automatic {
                    palette.accent
                } else {
                    palette.text_muted
                }),
            );
        });
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

fn background_selector(ui: &mut Ui, config: &mut Beautification, palette: &crate::theme::Palette) {
    let choices = [
        ("Automatic", 0u8),
        ("Blurred source", 1),
        ("Color", 2),
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
        Background::BuiltIn(background) => match background {
            BuiltInBackground::Mist => "Mist",
            BuiltInBackground::Iris => "Iris",
            BuiltInBackground::Midnight => "Midnight",
            BuiltInBackground::Sunrise => "Sunrise",
            BuiltInBackground::Lagoon => "Lagoon",
            BuiltInBackground::Sand => "Sand",
        },
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
    ui.label(
        egui::RichText::new("Desktop and Image appear here after the host supplies pixels.")
            .small()
            .color(palette.text_muted),
    );
}

fn aspect_row(ui: &mut Ui, aspect: &mut AspectPreset) {
    let choices = [
        ("Original", AspectPreset::Original),
        ("16:9", AspectPreset::Landscape),
        ("1:1", AspectPreset::Square),
        ("9:16", AspectPreset::Story),
    ];
    egui::ComboBox::from_id_salt("scene-aspect")
        .selected_text(
            choices
                .iter()
                .find(|(_, a)| a == aspect)
                .map_or("Custom", |(l, _)| l),
        )
        .show_ui(ui, |ui| {
            for (label, candidate) in &choices {
                if ui.selectable_label(*aspect == *candidate, *label).clicked() {
                    *aspect = *candidate;
                }
            }
        });
}

fn section_rule(ui: &mut Ui, palette: &crate::theme::Palette) {
    ui.add_space(10.0);
    ui.painter().line_segment(
        [
            pos2(ui.cursor().left(), ui.cursor().top()),
            pos2(ui.cursor().left() + ui.available_width(), ui.cursor().top()),
        ],
        Stroke::new(1.0, palette.hairline),
    );
    ui.add_space(10.0);
}

fn section_label(ui: &mut Ui, text: &str, palette: &crate::theme::Palette) {
    ui.label(
        egui::RichText::new(text)
            .font(egui::FontId::monospace(10.0))
            .color(palette.text_muted)
            .strong(),
    );
    ui.add_space(6.0);
}

fn d9_outer_canvas_note(ui: &mut Ui, palette: &crate::theme::Palette) {
    let danger = if palette.is_dark() {
        Color32::from_rgb(0xFF, 0x7A, 0x70)
    } else {
        Color32::from_rgb(0xB4, 0x23, 0x18)
    };
    ui.add_space(8.0);
    Frame::new()
        .fill(danger.gamma_multiply(0.10))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(Margin::same(16))
        .stroke(Stroke::new(1.0, danger.gamma_multiply(0.7)))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new("NATIVE APPEARANCE")
                    .font(egui::FontId::monospace(10.0))
                    .color(danger)
                    .strong(),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Only the outer presentation canvas changes.")
                    .color(palette.text)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "The captured silhouette, corners, and shadow stay byte-stable. \
                     Scene can change only background, padding, placement, ratio, and \
                     output size.",
                )
                .color(palette.text_muted),
            );
        });
}

fn sensitive_suggestions(ui: &mut Ui, state: &EditorState, palette: &crate::theme::Palette) {
    section_label(ui, "PRIVACY REVIEW", palette);
    match state.sensitive_review() {
        Some(review) if !review.suggestions.is_empty() => {
            ui.label(
                egui::RichText::new(format!(
                    "{} suggestion{} awaiting review",
                    review.suggestions.len(),
                    if review.suggestions.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                ))
                .color(palette.text),
            );
            ui.label(
                egui::RichText::new("Smart Frame never redacts suggested regions automatically.")
                    .small()
                    .color(palette.text_muted),
            );
        }
        _ => {
            ui.label(
                egui::RichText::new("No reviewed sensitive-region suggestions")
                    .small()
                    .color(palette.text_muted),
            );
        }
    }
}

fn pill_button(
    ui: &mut Ui,
    label: &str,
    selected: bool,
    palette: &crate::theme::Palette,
) -> Response {
    let fill = if selected {
        palette.accent.gamma_multiply(0.25)
    } else {
        palette.chip_fill
    };
    let stroke = if selected {
        Stroke::new(1.0, palette.accent)
    } else {
        Stroke::new(1.0, palette.hairline)
    };
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(if selected {
            palette.accent
        } else {
            palette.text
        }))
        .fill(fill)
        .stroke(stroke)
        .corner_radius(egui::CornerRadius::same(8)),
    )
}
