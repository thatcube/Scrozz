//! The non-destructive annotation editor.
//!
//! The editor is an ordinary egui surface. The application hosts it in a
//! deferred viewport from its existing event loop; this module never starts an
//! eframe runtime of its own.

use crate::harness::{Scene, SceneCtx};
use crate::icons::{Icon, IconStore};
use crate::motion::Motion;
use crate::paint::{self, ControlState, Reveal, Surface};
use crate::theme::{self, Appearance, Radius, Space, Text, Theme, corner};
use egui::{
    Align, Align2, Color32, ColorImage, CursorIcon, Id, Layout, Pos2, Rect, Response, Sense,
    Stroke, StrokeKind, TextureHandle, TextureId, TextureOptions, Ui, UiBuilder, Vec2, pos2, vec2,
};
use scrozz_annotate::{
    Annotation, AnnotationId, ArrowStyle, Canvas, CanvasRotation, Color, Document, DocumentData,
    RedactStyle, Renderer, SkiaRenderer, Style, TextPreset, UndoHistory,
};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Provenance, ScaleFactor,
};
use std::sync::{Arc, Mutex};

const TOP_BAR_HEIGHT: f32 = 62.0;
const FOOTER_HEIGHT: f32 = 58.0;
const INSPECTOR_WIDTH: f32 = 226.0;
const CONTROL_SIZE: f32 = 36.0;
const MIN_SHAPE_SIZE: f64 = 2.0;

/// The two editing workspaces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorMode {
    /// Create, select, move, resize, style, and order annotations.
    #[default]
    Compose,
    /// Adjust non-destructive crop, rotation, flips, and expansion.
    Crop,
}

impl EditorMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Compose => "Compose",
            Self::Crop => "Crop",
        }
    }
}

/// A tool in the compose workspace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorTool {
    /// Select and transform existing objects.
    #[default]
    Select,
    /// Arrow.
    Arrow,
    /// Plain line.
    Line,
    /// Rectangle.
    Rectangle,
    /// Ellipse.
    Ellipse,
    /// Freehand pen.
    Freehand,
    /// Text label.
    Text,
    /// Numbered marker.
    Counter,
    /// Translucent highlight.
    Highlight,
    /// Dim everything except a region.
    Spotlight,
    /// Destructive blur redaction.
    Blur,
    /// Destructive mosaic redaction.
    Pixelate,
    /// Destructive solid redaction.
    SolidRedact,
}

impl EditorTool {
    /// Every compose tool, in toolbar order.
    pub const ALL: [Self; 13] = [
        Self::Select,
        Self::Arrow,
        Self::Line,
        Self::Rectangle,
        Self::Ellipse,
        Self::Freehand,
        Self::Text,
        Self::Counter,
        Self::Highlight,
        Self::Spotlight,
        Self::Blur,
        Self::Pixelate,
        Self::SolidRedact,
    ];

    /// Human-readable tool name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Select => "Select",
            Self::Arrow => "Arrow",
            Self::Line => "Line",
            Self::Rectangle => "Rectangle",
            Self::Ellipse => "Ellipse",
            Self::Freehand => "Freehand",
            Self::Text => "Text",
            Self::Counter => "Counter",
            Self::Highlight => "Highlight",
            Self::Spotlight => "Spotlight",
            Self::Blur => "Blur",
            Self::Pixelate => "Pixelate",
            Self::SolidRedact => "Solid redact",
        }
    }

    const fn icon(self) -> Icon {
        match self {
            Self::Select => Icon::Viewfinder,
            Self::Arrow => Icon::ArrowUpRight,
            Self::Line => Icon::Line,
            Self::Rectangle => Icon::Square,
            Self::Ellipse => Icon::Circle,
            Self::Freehand => Icon::Pencil,
            Self::Text => Icon::LetterT,
            Self::Counter => Icon::ListNumbers,
            Self::Highlight => Icon::Highlight,
            Self::Spotlight => Icon::Scan,
            Self::Blur => Icon::Droplet,
            Self::Pixelate => Icon::GridDots,
            Self::SolidRedact => Icon::LayoutGrid,
        }
    }

    const fn is_redaction(self) -> bool {
        matches!(self, Self::Blur | Self::Pixelate | Self::SolidRedact)
    }
}

/// An event emitted by an editor surface.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorEvent {
    /// The document changed and should be autosaved.
    Changed(DocumentData),
    /// The user explicitly requested a save.
    Save(DocumentData),
    /// The user wants to drag the rendered result into another application.
    DragRequested(EditorDragRequest),
    /// The viewport should close.
    CloseRequested,
}

/// Everything the host needs to begin a native drag from the editor viewport.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorDragRequest {
    /// The latest edit snapshot, persisted before the drag leaves Scrozz.
    pub data: DocumentData,
    /// The preview rectangle in viewport-local logical points.
    pub preview: LogicalRect,
    /// The pointer position in viewport-local logical points.
    pub pointer: LogicalPoint,
}

#[derive(Clone, Copy, Debug)]
struct Placement {
    rect: Rect,
    scale: f32,
}

impl Placement {
    fn canvas_to_screen(self, point: LogicalPoint) -> Pos2 {
        pos2(
            self.rect.left() + point.x as f32 * self.scale,
            self.rect.top() + point.y as f32 * self.scale,
        )
    }

    fn screen_to_canvas(self, point: Pos2) -> LogicalPoint {
        LogicalPoint::new(
            f64::from((point.x - self.rect.left()) / self.scale),
            f64::from((point.y - self.rect.top()) / self.scale),
        )
    }
}

#[derive(Clone, Copy, Debug)]
enum ResizeHandle {
    NorthWest,
    NorthEast,
    SouthEast,
    SouthWest,
}

#[derive(Clone, Debug)]
enum Gesture {
    Create {
        start: LogicalPoint,
        current: LogicalPoint,
        points: Vec<LogicalPoint>,
    },
    Move {
        id: AnnotationId,
        last: LogicalPoint,
    },
    Resize {
        id: AnnotationId,
        handle: ResizeHandle,
        original: LogicalRect,
    },
    Crop {
        start: LogicalPoint,
        current: LogicalPoint,
    },
}

/// Stateful annotation editor content suitable for an ordinary or deferred
/// viewport.
pub struct AnnotationEditor {
    document: Document,
    history: UndoHistory,
    mode: EditorMode,
    tool: EditorTool,
    selected: Option<AnnotationId>,
    working_style: Style,
    zoom: f32,
    gesture: Option<Gesture>,
    events: Vec<EditorEvent>,
    texture: Option<TextureHandle>,
    rendered: Option<(DocumentData, EditorMode)>,
    render_error: Option<String>,
    notice: Option<String>,
    close_sent: bool,
    interactive: bool,
}

impl std::fmt::Debug for AnnotationEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnnotationEditor")
            .field("mode", &self.mode)
            .field("tool", &self.tool)
            .field("selected", &self.selected)
            .field("zoom", &self.zoom)
            .field("document", &self.document.data())
            .finish_non_exhaustive()
    }
}

impl AnnotationEditor {
    /// Creates an editor around a loaded document.
    #[must_use]
    pub fn new(document: Document) -> Self {
        let history = UndoHistory::new(&document);
        Self {
            document,
            history,
            mode: EditorMode::Compose,
            tool: EditorTool::Select,
            selected: None,
            working_style: Style::stroked(),
            zoom: 1.0,
            gesture: None,
            events: Vec::new(),
            texture: None,
            rendered: None,
            render_error: None,
            notice: None,
            close_sent: false,
            interactive: true,
        }
    }

    /// The document currently being edited.
    #[must_use]
    pub const fn document(&self) -> &Document {
        &self.document
    }

    /// A persistable snapshot of the current document.
    #[must_use]
    pub fn document_data(&self) -> DocumentData {
        self.document.data()
    }

    /// Replaces the short host-provided status shown in the footer.
    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    /// Current workspace.
    #[must_use]
    pub const fn mode(&self) -> EditorMode {
        self.mode
    }

    /// Switches workspace.
    pub fn set_mode(&mut self, mode: EditorMode) {
        self.mode = mode;
        self.gesture = None;
    }

    /// Current compose tool.
    #[must_use]
    pub const fn tool(&self) -> EditorTool {
        self.tool
    }

    /// Selects a compose tool.
    pub fn set_tool(&mut self, tool: EditorTool) {
        self.tool = tool;
        self.mode = EditorMode::Compose;
        self.gesture = None;
    }

    /// Enables or disables pointer-driven editing while retaining the real UI.
    pub fn set_interactive(&mut self, interactive: bool) {
        self.interactive = interactive;
    }

    /// Drains events emitted since the last call.
    pub fn drain_events(&mut self) -> Vec<EditorEvent> {
        std::mem::take(&mut self.events)
    }

    /// Draws one editor frame.
    pub fn show(&mut self, ui: &mut Ui, icons: &IconStore, theme: &Theme) {
        let motion = Motion::from_context(ui.ctx());
        let surface = if self.interactive {
            Surface::new(theme, icons, motion)
        } else {
            Surface::still(theme, icons, motion)
        };
        self.show_with_surface(ui, &surface);
    }

    /// Draws one editor frame with an explicit surface context.
    pub fn show_with_surface(&mut self, ui: &mut Ui, surface: &Surface<'_>) {
        self.handle_shortcuts(ui);
        if ui.input(|input| input.viewport().close_requested()) && !self.close_sent {
            self.events.push(EditorEvent::CloseRequested);
            self.close_sent = true;
        }

        let full = ui.max_rect();
        ui.painter()
            .rect_filled(full, 0.0, surface.palette().canvas());

        let top = Rect::from_min_size(full.min, vec2(full.width(), TOP_BAR_HEIGHT));
        let footer = Rect::from_min_max(
            pos2(full.left(), full.bottom() - FOOTER_HEIGHT),
            full.right_bottom(),
        );
        let inspector = Rect::from_min_max(
            pos2(full.right() - INSPECTOR_WIDTH, top.bottom()),
            pos2(full.right(), footer.top()),
        );
        let workspace = Rect::from_min_max(
            pos2(full.left(), top.bottom()),
            pos2(inspector.left(), footer.top()),
        );

        self.paint_top_bar(ui, surface, top);
        self.paint_inspector(ui, surface, inspector);
        self.paint_workspace(ui, surface, workspace);
        self.paint_footer(ui, surface, footer);
    }

    fn handle_shortcuts(&mut self, ui: &mut Ui) {
        if !self.interactive {
            return;
        }
        let (undo, redo, save, delete, escape) = ui.input(|input| {
            (
                input.modifiers.command
                    && !input.modifiers.shift
                    && input.key_pressed(egui::Key::Z),
                input.modifiers.command && input.modifiers.shift && input.key_pressed(egui::Key::Z),
                input.modifiers.command && input.key_pressed(egui::Key::S),
                input.key_pressed(egui::Key::Delete) || input.key_pressed(egui::Key::Backspace),
                input.key_pressed(egui::Key::Escape),
            )
        });
        if undo {
            self.undo();
        } else if redo {
            self.redo();
        }
        if save {
            self.events.push(EditorEvent::Save(self.document.data()));
        }
        if delete {
            self.delete_selected();
        }
        if escape {
            self.selected = None;
            self.gesture = None;
        }
    }

    fn paint_top_bar(&mut self, ui: &mut Ui, surface: &Surface<'_>, rect: Rect) {
        let palette = surface.palette();
        ui.painter()
            .rect_filled(rect, 0.0, palette.card_fill_raised);
        paint::divider_h(
            ui.painter(),
            rect.left(),
            rect.right(),
            rect.bottom() - 1.0,
            palette,
        );

        let title_pos = pos2(rect.left() + Space::LG, rect.center().y);
        ui.painter().text(
            title_pos,
            Align2::LEFT_CENTER,
            "SCROZZ",
            surface.font(Text::Title),
            palette.text,
        );

        let mut x = rect.left() + 92.0;
        for mode in [EditorMode::Compose, EditorMode::Crop] {
            let button = Rect::from_min_size(pos2(x, rect.center().y - 17.0), vec2(72.0, 34.0));
            if text_button(
                ui,
                surface,
                button,
                Id::new(("editor-mode", mode.label())),
                mode.label(),
                self.mode == mode,
                self.interactive,
            )
            .clicked()
            {
                self.set_mode(mode);
            }
            x += 74.0;
        }

        paint::divider_v(
            ui.painter(),
            x + Space::XS,
            rect.top() + Space::MD,
            rect.bottom() - Space::MD,
            palette,
        );
        x += Space::LG;

        for (label, icon, enabled) in [
            ("Undo", Icon::ArrowBackUp, self.history.can_undo()),
            ("Redo", Icon::ArrowForwardUp, self.history.can_redo()),
        ] {
            let button =
                Rect::from_min_size(pos2(x, rect.center().y - 18.0), Vec2::splat(CONTROL_SIZE));
            let response = paint::icon_button(
                ui,
                surface,
                button,
                Id::new(("editor-history", label)),
                icon,
                label,
                ControlState {
                    enabled: self.interactive && enabled,
                    ..ControlState::new()
                },
                Reveal::SHOWN,
            )
            .on_hover_text(label);
            if response.clicked() {
                match label {
                    "Undo" => self.undo(),
                    "Redo" => self.redo(),
                    _ => {}
                }
            }
            x += CONTROL_SIZE + Space::XS;
        }

        paint::divider_v(
            ui.painter(),
            x + Space::XS,
            rect.top() + Space::MD,
            rect.bottom() - Space::MD,
            palette,
        );
        x += Space::LG;

        if self.mode == EditorMode::Compose {
            for tool in EditorTool::ALL {
                let button =
                    Rect::from_min_size(pos2(x, rect.center().y - 18.0), Vec2::splat(CONTROL_SIZE));
                let response = paint::icon_button(
                    ui,
                    surface,
                    button,
                    Id::new(("editor-tool", tool.label())),
                    tool.icon(),
                    tool.label(),
                    ControlState {
                        enabled: self.interactive,
                        ..ControlState::new().selected(self.tool == tool)
                    },
                    Reveal::SHOWN,
                )
                .on_hover_text(tool.label());
                if response.clicked() {
                    self.set_tool(tool);
                }
                x += CONTROL_SIZE + Space::XS;
            }
        } else {
            self.paint_crop_actions(ui, surface, rect, x);
        }
    }

    fn paint_crop_actions(&mut self, ui: &mut Ui, surface: &Surface<'_>, rect: Rect, mut x: f32) {
        let actions = [
            ("Rotate left", "CCW"),
            ("Rotate right", "CW"),
            ("Flip horizontal", "H"),
            ("Flip vertical", "V"),
            ("Revert", "Reset"),
        ];
        for (label, short) in actions {
            let width = if label == "Revert" { 62.0 } else { 46.0 };
            let button = Rect::from_min_size(pos2(x, rect.center().y - 17.0), vec2(width, 34.0));
            if text_button(
                ui,
                surface,
                button,
                Id::new(("crop-action", label)),
                short,
                false,
                self.interactive,
            )
            .on_hover_text(label)
            .clicked()
            {
                let mut canvas = *self.document.canvas();
                match label {
                    "Rotate left" => {
                        canvas.rotation = canvas.rotation.counter_clockwise();
                    }
                    "Rotate right" => {
                        canvas.rotation = canvas.rotation.clockwise();
                    }
                    "Flip horizontal" => canvas.flip_horizontal = !canvas.flip_horizontal,
                    "Flip vertical" => canvas.flip_vertical = !canvas.flip_vertical,
                    "Revert" => canvas = Canvas::default(),
                    _ => {}
                }
                self.apply_canvas(canvas);
            }
            x += width + Space::SM;
        }
    }

    fn paint_inspector(&mut self, ui: &mut Ui, surface: &Surface<'_>, rect: Rect) {
        let palette = surface.palette();
        ui.painter().rect_filled(rect, 0.0, palette.card_fill);
        paint::divider_v(
            ui.painter(),
            rect.left(),
            rect.top(),
            rect.bottom(),
            palette,
        );
        let inner = rect.shrink2(vec2(Space::LG, Space::LG));
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt("editor-inspector")
                .max_rect(inner)
                .layout(Layout::top_down(Align::Min)),
        );
        child.set_clip_rect(inner);
        egui::ScrollArea::vertical()
            .id_salt("editor-inspector-scroll")
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                if self.mode == EditorMode::Crop {
                    self.crop_inspector(ui, surface);
                } else {
                    self.style_inspector(ui, surface);
                }
            });
    }

    fn crop_inspector(&mut self, ui: &mut Ui, surface: &Surface<'_>) {
        section_title(ui, surface, "Canvas");
        ui.add_space(Space::SM);
        let canvas = *self.document.canvas();
        let crop = canvas
            .crop
            .unwrap_or_else(|| self.document.logical_bounds());
        metric_row(ui, surface, "Width", crop.size.width, "pt");
        metric_row(ui, surface, "Height", crop.size.height, "pt");
        metric_row(
            ui,
            surface,
            "Output",
            self.document.canvas_size().width,
            "pt wide",
        );
        ui.add_space(Space::LG);
        helper_text(
            ui,
            surface,
            "Drag across the image to set a crop. The original capture stays untouched.",
        );
        ui.add_space(Space::LG);

        let mut expand = canvas.auto_expand;
        if toggle_row(
            ui,
            surface,
            "Auto-expand",
            "Keep marks outside the crop",
            &mut expand,
            self.interactive,
        ) {
            self.apply_canvas(Canvas {
                auto_expand: expand,
                ..canvas
            });
        }
        ui.add_space(Space::MD);
        let transform = format!(
            "{} · {}{}",
            rotation_label(canvas.rotation),
            if canvas.flip_horizontal {
                "H-flipped"
            } else {
                "Natural"
            },
            if canvas.flip_vertical {
                " · V-flipped"
            } else {
                ""
            }
        );
        helper_text(ui, surface, &transform);
    }

    fn style_inspector(&mut self, ui: &mut Ui, surface: &Surface<'_>) {
        let selected = self.selected.and_then(|id| self.document.get(id)).cloned();
        let kind = selected.as_ref().map(|object| object.kind());
        let mut style = selected
            .as_ref()
            .map_or(self.default_style_for_tool(), |object| object.style);
        let original = style;

        section_title(
            ui,
            surface,
            selected.as_ref().map_or(self.tool.label(), |_| "Selection"),
        );
        if let Some(object) = &selected {
            helper_text(ui, surface, &format!("{:?}", object.kind()));
        } else {
            helper_text(ui, surface, "New objects use these settings");
        }
        ui.add_space(Space::LG);

        section_title(ui, surface, "Color");
        ui.add_space(Space::SM);
        let swatches = palette_colors();
        let start = ui.cursor().min;
        for (index, color) in swatches.into_iter().enumerate() {
            let column = index % 6;
            let row = index / 6;
            let swatch = Rect::from_min_size(
                pos2(start.x + column as f32 * 31.0, start.y + row as f32 * 31.0),
                vec2(22.0, 22.0),
            );
            let response = paint::color_swatch(
                ui,
                surface,
                swatch,
                Id::new(("editor-color", index)),
                to_egui(color),
                color.name(),
                style.stroke == color,
            )
            .on_hover_text(color.name());
            if response.clicked() && self.interactive {
                style.stroke = color;
                if matches!(
                    kind,
                    Some(
                        scrozz_annotate::AnnotationKind::Highlight
                            | scrozz_annotate::AnnotationKind::Redact
                    )
                ) || self.tool == EditorTool::Highlight
                    || self.tool.is_redaction()
                {
                    style.fill = Some(color);
                }
            }
        }
        ui.add_space(62.0);
        helper_text(ui, surface, style.stroke.name());
        ui.add_space(Space::LG);

        if !matches!(
            kind,
            Some(
                scrozz_annotate::AnnotationKind::Highlight
                    | scrozz_annotate::AnnotationKind::Spotlight
                    | scrozz_annotate::AnnotationKind::Redact
            )
        ) && self.tool != EditorTool::Highlight
            && self.tool != EditorTool::Spotlight
            && !self.tool.is_redaction()
        {
            section_title(ui, surface, "Weight");
            ui.add_space(Space::SM);
            let mut fraction = ((style.stroke_width - 1.0) / 11.0) as f32;
            let width_rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 32.0));
            let response = paint::stroke_width(
                ui,
                surface,
                width_rect,
                Id::new("editor-stroke-width"),
                fraction,
            );
            if self.interactive
                && (response.dragged() || response.clicked())
                && let Some(pointer) = response.interact_pointer_pos()
            {
                fraction = ((pointer.x - width_rect.left() - Space::MD)
                    / (width_rect.width() - Space::MD * 2.0))
                    .clamp(0.0, 1.0);
                style.stroke_width = f64::from(1.0 + fraction * 11.0);
            }
            ui.add_space(40.0);
            helper_text(ui, surface, &format!("{:.1} pt", style.stroke_width));
            ui.add_space(Space::LG);
        }

        if kind == Some(scrozz_annotate::AnnotationKind::Arrow)
            || (selected.is_none() && self.tool == EditorTool::Arrow)
        {
            section_title(ui, surface, "Arrow");
            ui.add_space(Space::SM);
            for arrow in ArrowStyle::ALL {
                let selected_style = style.arrow_style == arrow;
                if compact_choice(
                    ui,
                    surface,
                    Id::new(("arrow-style", arrow.label())),
                    arrow.label(),
                    selected_style,
                    self.interactive,
                ) {
                    style.arrow_style = arrow;
                }
            }
            ui.add_space(Space::LG);
        }

        if kind == Some(scrozz_annotate::AnnotationKind::Text)
            || (selected.is_none() && self.tool == EditorTool::Text)
        {
            section_title(ui, surface, "Type");
            ui.add_space(Space::SM);
            egui::ComboBox::from_id_salt("text-preset")
                .selected_text(style.text_preset.label())
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for preset in TextPreset::ALL {
                        ui.selectable_value(&mut style.text_preset, preset, preset.label());
                    }
                });
            let mut size = style.font_size;
            if ui
                .add(egui::Slider::new(&mut size, 10.0..=72.0).text("Size"))
                .changed()
                && self.interactive
            {
                style.font_size = size;
            }
            if let Some(id) = self.selected
                && let Some(object) = self.document.get(id)
                && let Annotation::Text { content, .. } = &object.annotation
            {
                let mut edited = content.clone();
                if ui
                    .add(
                        egui::TextEdit::multiline(&mut edited)
                            .desired_rows(2)
                            .hint_text("Annotation text"),
                    )
                    .changed()
                    && self.interactive
                {
                    if let Some(mut object) = self.document.get_mut(id)
                        && let Annotation::Text { content, .. } = object.annotation()
                    {
                        *content = edited;
                    }
                    self.checkpoint();
                }
            }
            ui.add_space(Space::LG);
        }

        if kind == Some(scrozz_annotate::AnnotationKind::Redact)
            || (selected.is_none() && self.tool.is_redaction())
        {
            section_title(ui, surface, "Redaction");
            ui.add_space(Space::SM);
            let mut strength = style.redact_strength;
            if ui
                .add(egui::Slider::new(&mut strength, 0.0..=1.0).text("Strength"))
                .changed()
                && self.interactive
            {
                style.redact_strength = strength;
            }
            helper_text(ui, surface, "Export permanently destroys these pixels.");
            ui.add_space(Space::LG);
        }

        if !self.tool.is_redaction()
            && kind != Some(scrozz_annotate::AnnotationKind::Redact)
            && kind != Some(scrozz_annotate::AnnotationKind::Spotlight)
        {
            let mut shadow = style.shadow;
            if toggle_row(
                ui,
                surface,
                "Object shadow",
                "Lift this mark from the image",
                &mut shadow,
                self.interactive,
            ) {
                style.shadow = shadow;
            }
            ui.add_space(Space::LG);
        }

        if let Some(id) = self.selected {
            section_title(ui, surface, "Arrange");
            ui.add_space(Space::SM);
            let width = (ui.available_width() - Space::SM) / 2.0;
            ui.horizontal(|ui| {
                if ui
                    .add_sized([width, 28.0], egui::Button::new("Back"))
                    .clicked()
                    && self.interactive
                    && self.document.send_to_back(id)
                {
                    self.checkpoint();
                }
                if ui
                    .add_sized([width, 28.0], egui::Button::new("Front"))
                    .clicked()
                    && self.interactive
                    && self.document.bring_to_front(id)
                {
                    self.checkpoint();
                }
            });
            if ui
                .add_sized(
                    [ui.available_width(), 28.0],
                    egui::Button::new("Delete object"),
                )
                .clicked()
                && self.interactive
            {
                self.delete_selected();
            }
        }

        if style != original && self.interactive {
            if let Some(id) = self.selected {
                if self.document.set_style(id, style) {
                    self.checkpoint();
                }
            } else {
                self.working_style = style;
            }
        }
    }

    fn paint_workspace(&mut self, ui: &mut Ui, surface: &Surface<'_>, rect: Rect) {
        let palette = surface.palette();
        let mat = if palette.is_dark() {
            Color32::from_rgb(0x10, 0x12, 0x19)
        } else {
            Color32::from_rgb(0xDD, 0xE1, 0xEC)
        };
        ui.painter().rect_filled(rect, 0.0, mat);
        paint_precision_grid(ui.painter(), rect, palette);

        let Some(texture) = self.ensure_texture(ui.ctx()) else {
            let message = self
                .render_error
                .as_deref()
                .unwrap_or("The image could not be rendered.");
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                message,
                surface.font(Text::Body),
                palette.text_muted,
            );
            return;
        };

        let placement = self.placement(rect);
        paint::soft_shadow(ui.painter(), placement.rect, Radius::CHIP, palette, 0.85);
        ui.painter().image(
            texture,
            placement.rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
        ui.painter().rect_stroke(
            placement.rect,
            corner(Radius::CHIP),
            Stroke::new(1.0, palette.thumb_border),
            StrokeKind::Inside,
        );

        if self.mode == EditorMode::Crop {
            self.paint_crop_overlay(ui, surface, placement);
        } else {
            self.paint_selection(ui, surface, placement);
            self.paint_creation_preview(ui, surface, placement);
        }

        if self.interactive {
            let response = ui
                .interact(
                    placement.rect,
                    Id::new("annotation-canvas"),
                    Sense::click_and_drag(),
                )
                .on_hover_cursor(if self.mode == EditorMode::Crop {
                    CursorIcon::Crosshair
                } else if self.tool == EditorTool::Select {
                    CursorIcon::Default
                } else {
                    CursorIcon::Crosshair
                });
            self.handle_canvas_response(&response, placement);
        }
    }

    fn paint_crop_overlay(&self, ui: &mut Ui, surface: &Surface<'_>, placement: Placement) {
        let crop = match &self.gesture {
            Some(Gesture::Crop { start, current, .. }) => normalized_rect(*start, *current),
            _ => self
                .document
                .canvas()
                .crop
                .unwrap_or_else(|| self.document.logical_bounds()),
        };
        let crop_rect = self.source_rect_to_screen(crop, placement);
        let image = placement.rect;
        let shade = Color32::from_black_alpha(132);
        for outside in [
            Rect::from_min_max(image.min, pos2(image.right(), crop_rect.top())),
            Rect::from_min_max(pos2(image.left(), crop_rect.bottom()), image.right_bottom()),
            Rect::from_min_max(
                pos2(image.left(), crop_rect.top()),
                pos2(crop_rect.left(), crop_rect.bottom()),
            ),
            Rect::from_min_max(
                pos2(crop_rect.right(), crop_rect.top()),
                pos2(image.right(), crop_rect.bottom()),
            ),
        ] {
            if outside.is_positive() {
                ui.painter().rect_filled(outside, 0.0, shade);
            }
        }
        ui.painter().rect_stroke(
            crop_rect,
            0.0,
            Stroke::new(2.0, surface.palette().accent_hi),
            StrokeKind::Inside,
        );
        for third in [1.0 / 3.0, 2.0 / 3.0] {
            let x = egui::lerp(crop_rect.x_range(), third);
            let y = egui::lerp(crop_rect.y_range(), third);
            ui.painter().line_segment(
                [pos2(x, crop_rect.top()), pos2(x, crop_rect.bottom())],
                Stroke::new(1.0, Color32::from_white_alpha(100)),
            );
            ui.painter().line_segment(
                [pos2(crop_rect.left(), y), pos2(crop_rect.right(), y)],
                Stroke::new(1.0, Color32::from_white_alpha(100)),
            );
        }
        let label = format!(
            "{:.0} × {:.0}",
            crop.size.width.abs(),
            crop.size.height.abs()
        );
        ui.painter().text(
            crop_rect.left_top() + vec2(8.0, 7.0),
            Align2::LEFT_TOP,
            label,
            surface.font(Text::Caption),
            Color32::WHITE,
        );
    }

    fn paint_selection(&self, ui: &mut Ui, surface: &Surface<'_>, placement: Placement) {
        let Some(object) = self.selected.and_then(|id| self.document.get(id)) else {
            return;
        };
        let rect = self.source_rect_to_screen(object.bounds(), placement);
        let painter = ui.painter();
        let stroke = Stroke::new(1.5, surface.palette().accent_hi);
        painter.rect_stroke(rect, 2.0, stroke, StrokeKind::Outside);
        for (_, point) in handle_points(rect) {
            painter.circle_filled(point, 5.0, surface.palette().card_fill_raised);
            painter.circle_stroke(point, 5.0, Stroke::new(2.0, surface.palette().accent_hi));
        }
    }

    fn paint_creation_preview(&self, ui: &mut Ui, surface: &Surface<'_>, placement: Placement) {
        let Some(Gesture::Create {
            start,
            current,
            points,
        }) = &self.gesture
        else {
            return;
        };
        let geometry = self.workspace_geometry();
        let to_screen = |point| placement.canvas_to_screen(geometry.source_to_canvas(point));
        let stroke = Stroke::new(2.0, to_egui(self.working_style.stroke));
        match self.tool {
            EditorTool::Arrow | EditorTool::Line => {
                ui.painter()
                    .line_segment([to_screen(*start), to_screen(*current)], stroke);
            }
            EditorTool::Freehand => {
                for pair in points.windows(2) {
                    ui.painter()
                        .line_segment([to_screen(pair[0]), to_screen(pair[1])], stroke);
                }
            }
            _ => {
                let rect = self.source_rect_to_screen(normalized_rect(*start, *current), placement);
                ui.painter()
                    .rect_stroke(rect, 2.0, stroke, StrokeKind::Inside);
            }
        }
        let _ = surface;
    }

    fn handle_canvas_response(&mut self, response: &Response, placement: Placement) {
        let geometry = self.workspace_geometry();
        let pointer_source = |position: Pos2| {
            let canvas = placement.screen_to_canvas(position);
            geometry.canvas_to_source(canvas)
        };

        if response.drag_started()
            && let Some(origin) = response.ctx.input(|input| input.pointer.press_origin())
        {
            let source = pointer_source(origin);
            if self.mode == EditorMode::Crop {
                self.gesture = Some(Gesture::Crop {
                    start: source,
                    current: source,
                });
            } else if self.tool == EditorTool::Select {
                self.begin_select_gesture(origin, source, placement);
            } else {
                self.gesture = Some(Gesture::Create {
                    start: source,
                    current: source,
                    points: vec![source],
                });
            }
        }

        if response.dragged()
            && let Some(position) = response.interact_pointer_pos()
        {
            let source = pointer_source(position);
            match &mut self.gesture {
                Some(Gesture::Create {
                    current, points, ..
                }) => {
                    *current = source;
                    if self.tool == EditorTool::Freehand {
                        points.push(source);
                    }
                }
                Some(Gesture::Move { id, last }) => {
                    let dx = source.x - last.x;
                    let dy = source.y - last.y;
                    self.document.translate(*id, dx, dy);
                    *last = source;
                }
                Some(Gesture::Resize {
                    id,
                    handle,
                    original,
                }) => {
                    let bounds = resized_rect(*original, *handle, source);
                    self.document.set_bounds(*id, bounds);
                }
                Some(Gesture::Crop { current, .. }) => *current = source,
                None => {}
            }
        }

        if response.drag_stopped() {
            self.finish_gesture();
        } else if response.clicked()
            && let Some(position) = response.interact_pointer_pos()
        {
            self.handle_canvas_click(pointer_source(position));
        }
    }

    fn begin_select_gesture(&mut self, pointer: Pos2, source: LogicalPoint, placement: Placement) {
        if let Some(id) = self.selected
            && let Some(object) = self.document.get(id)
        {
            let screen_bounds = self.source_rect_to_screen(object.bounds(), placement);
            if let Some(handle) = handle_points(screen_bounds)
                .into_iter()
                .find(|(_, point)| point.distance(pointer) <= 9.0)
                .map(|(handle, _)| handle)
            {
                self.gesture = Some(Gesture::Resize {
                    id,
                    handle,
                    original: object.bounds(),
                });
                return;
            }
        }
        self.selected = self.document.hit_test(source);
        self.gesture = self.selected.map(|id| Gesture::Move { id, last: source });
    }

    fn handle_canvas_click(&mut self, source: LogicalPoint) {
        match self.mode {
            EditorMode::Crop => {}
            EditorMode::Compose => match self.tool {
                EditorTool::Select => self.selected = self.document.hit_test(source),
                EditorTool::Text => {
                    let id = self.document.add(
                        Annotation::Text {
                            at: source,
                            content: "Add a note".to_owned(),
                        },
                        self.default_style_for_tool(),
                    );
                    self.selected = Some(id);
                    self.tool = EditorTool::Select;
                    self.checkpoint();
                }
                EditorTool::Counter => {
                    let id = self.document.add(
                        Annotation::Counter {
                            at: source,
                            index: 0,
                        },
                        self.default_style_for_tool(),
                    );
                    self.selected = Some(id);
                    self.checkpoint();
                }
                _ => {}
            },
        }
    }

    fn finish_gesture(&mut self) {
        let Some(gesture) = self.gesture.take() else {
            return;
        };
        match gesture {
            Gesture::Create {
                start,
                current,
                points,
            } => {
                if let Some(annotation) = self.annotation_for_gesture(start, current, points) {
                    let id = self.document.add(annotation, self.default_style_for_tool());
                    self.selected = Some(id);
                    self.checkpoint();
                }
            }
            Gesture::Move { .. } | Gesture::Resize { .. } => self.checkpoint(),
            Gesture::Crop { start, current } => {
                let mut crop = normalized_rect(start, current);
                crop = clamp_rect(crop, self.document.logical_bounds());
                if crop.size.width >= MIN_SHAPE_SIZE && crop.size.height >= MIN_SHAPE_SIZE {
                    let canvas = Canvas {
                        crop: Some(crop),
                        ..*self.document.canvas()
                    };
                    self.apply_canvas(canvas);
                }
            }
        }
    }

    fn annotation_for_gesture(
        &self,
        start: LogicalPoint,
        current: LogicalPoint,
        points: Vec<LogicalPoint>,
    ) -> Option<Annotation> {
        let rect = normalized_rect(start, current);
        match self.tool {
            EditorTool::Arrow => Some(Annotation::Arrow {
                from: start,
                to: current,
            }),
            EditorTool::Line => Some(Annotation::Line {
                from: start,
                to: current,
            }),
            EditorTool::Rectangle if rect_is_drawable(rect) => Some(Annotation::Rectangle(rect)),
            EditorTool::Ellipse if rect_is_drawable(rect) => Some(Annotation::Ellipse(rect)),
            EditorTool::Freehand if points.len() > 1 => Some(Annotation::Freehand(points)),
            EditorTool::Highlight if rect_is_drawable(rect) => Some(Annotation::Highlight(rect)),
            EditorTool::Spotlight if rect_is_drawable(rect) => Some(Annotation::Spotlight(rect)),
            EditorTool::Blur if rect_is_drawable(rect) => Some(Annotation::Redact {
                area: rect,
                style: RedactStyle::Blur,
            }),
            EditorTool::Pixelate if rect_is_drawable(rect) => Some(Annotation::Redact {
                area: rect,
                style: RedactStyle::Pixelate,
            }),
            EditorTool::SolidRedact if rect_is_drawable(rect) => Some(Annotation::Redact {
                area: rect,
                style: RedactStyle::Solid,
            }),
            _ => None,
        }
    }

    fn paint_footer(&mut self, ui: &mut Ui, surface: &Surface<'_>, rect: Rect) {
        let palette = surface.palette();
        ui.painter()
            .rect_filled(rect, 0.0, palette.card_fill_raised);
        paint::divider_h(ui.painter(), rect.left(), rect.right(), rect.top(), palette);

        let status = self.notice.clone().unwrap_or_else(|| match self.mode {
            EditorMode::Compose => {
                format!("{} objects · {}", self.document.len(), self.tool.label())
            }
            EditorMode::Crop => {
                let size = self.document.canvas_size();
                format!(
                    "{:.0} × {:.0} pt · non-destructive",
                    size.width, size.height
                )
            }
        });
        ui.painter().text(
            pos2(rect.left() + Space::LG, rect.center().y),
            Align2::LEFT_CENTER,
            status,
            surface.font(Text::Caption),
            palette.text_muted,
        );

        let zoom_area = Rect::from_center_size(
            pos2(rect.center().x - 35.0, rect.center().y),
            vec2(190.0, 32.0),
        );
        if text_button(
            ui,
            surface,
            Rect::from_min_size(zoom_area.min, vec2(32.0, 32.0)),
            Id::new("zoom-out"),
            "−",
            false,
            self.interactive,
        )
        .clicked()
        {
            self.zoom = (self.zoom - 0.1).clamp(0.35, 3.0);
        }
        let mut zoom = self.zoom;
        let slider = Rect::from_min_size(
            pos2(zoom_area.left() + 38.0, zoom_area.top()),
            vec2(78.0, 32.0),
        );
        let mut zoom_ui = ui.new_child(
            UiBuilder::new()
                .id_salt("zoom-slider")
                .max_rect(slider)
                .layout(Layout::left_to_right(Align::Center)),
        );
        if zoom_ui
            .add_sized(
                slider.size(),
                egui::Slider::new(&mut zoom, 0.35..=3.0).show_value(false),
            )
            .changed()
            && self.interactive
        {
            self.zoom = zoom;
        }
        ui.painter().text(
            pos2(zoom_area.left() + 124.0, zoom_area.center().y),
            Align2::LEFT_CENTER,
            format!("{:.0}%", self.zoom * 100.0),
            surface.font(Text::Caption),
            palette.text_muted,
        );
        if text_button(
            ui,
            surface,
            Rect::from_min_size(
                pos2(zoom_area.right() - 32.0, zoom_area.top()),
                vec2(32.0, 32.0),
            ),
            Id::new("zoom-in"),
            "+",
            false,
            self.interactive,
        )
        .clicked()
        {
            self.zoom = (self.zoom + 0.1).clamp(0.35, 3.0);
        }

        let save = Rect::from_min_size(
            pos2(rect.right() - 96.0, rect.center().y - 18.0),
            vec2(80.0, 36.0),
        );
        if paint::pill_button_with_state(
            ui,
            surface,
            save,
            Id::new("editor-save"),
            Icon::DeviceFloppy,
            "Save",
            true,
            ControlState {
                enabled: self.interactive,
                ..ControlState::new()
            },
            Reveal::SHOWN,
        )
        .clicked()
        {
            self.events.push(EditorEvent::Save(self.document.data()));
        }

        let drag = Rect::from_min_size(
            pos2(save.left() - 116.0, rect.center().y - 18.0),
            vec2(104.0, 36.0),
        );
        let response = paint::pill_drag_source_with_state(
            ui,
            surface,
            drag,
            Id::new("editor-drag"),
            Icon::GripVertical,
            "Drag me",
            false,
            ControlState {
                enabled: self.interactive,
                ..ControlState::new()
            },
            Reveal::SHOWN,
        )
        .on_hover_cursor(CursorIcon::Grab)
        .on_hover_text("Drag the edited image into another app");
        if response.drag_started()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let canvas = self.document.canvas_size();
            let scale = 240.0 / canvas.width.max(canvas.height).max(1.0);
            let preview_size = vec2(
                (canvas.width * scale).max(1.0) as f32,
                (canvas.height * scale).max(1.0) as f32,
            );
            let preview = Rect::from_center_size(pointer, preview_size);
            self.events
                .push(EditorEvent::DragRequested(EditorDragRequest {
                    data: self.document.data(),
                    preview: LogicalRect::new(
                        LogicalPoint::new(f64::from(preview.left()), f64::from(preview.top())),
                        LogicalSize::new(f64::from(preview.width()), f64::from(preview.height())),
                    ),
                    pointer: LogicalPoint::new(f64::from(pointer.x), f64::from(pointer.y)),
                }));
        }
    }

    fn placement(&self, workspace: Rect) -> Placement {
        let size = self.workspace_geometry().output_size();
        let available = workspace.shrink2(vec2(44.0, 34.0));
        let fit = (available.width() / size.width.max(1.0) as f32)
            .min(available.height() / size.height.max(1.0) as f32);
        let scale = (fit * self.zoom).clamp(0.02, 12.0);
        let image_size = vec2(size.width as f32 * scale, size.height as f32 * scale);
        Placement {
            rect: Rect::from_center_size(workspace.center(), image_size),
            scale,
        }
    }

    fn source_rect_to_screen(&self, rect: LogicalRect, placement: Placement) -> Rect {
        let geometry = self.workspace_geometry();
        let points = source_corners(rect)
            .map(|point| placement.canvas_to_screen(geometry.source_to_canvas(point)));
        Rect::from_points(&points)
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) -> Option<TextureId> {
        let data = self.document.data();
        let key = (data, self.mode);
        if self.rendered.as_ref() != Some(&key) {
            let renderer = SkiaRenderer::new();
            let rendered = match self.mode {
                EditorMode::Compose => renderer.render(&self.document),
                EditorMode::Crop => renderer.render_canvas(&self.document, self.workspace_canvas()),
            };
            match rendered {
                Ok(frame) => {
                    let image = frame_to_image(&frame);
                    self.texture = Some(ctx.load_texture(
                        "annotation-editor-canvas",
                        image,
                        TextureOptions::LINEAR,
                    ));
                    self.rendered = Some(key);
                    self.render_error = None;
                }
                Err(error) => {
                    self.texture = None;
                    self.rendered = None;
                    self.render_error = Some(error.to_string());
                }
            }
        }
        self.texture.as_ref().map(TextureHandle::id)
    }

    fn workspace_canvas(&self) -> Canvas {
        match self.mode {
            EditorMode::Compose => *self.document.canvas(),
            EditorMode::Crop => Canvas {
                crop: None,
                ..*self.document.canvas()
            },
        }
    }

    fn workspace_geometry(&self) -> scrozz_annotate::CanvasGeometry {
        self.document
            .canvas_geometry_for(self.workspace_canvas())
            .expect("workspace canvas derives from the validated document canvas")
    }

    fn default_style_for_tool(&self) -> Style {
        match self.tool {
            EditorTool::Highlight => Style {
                stroke: self.working_style.stroke,
                fill: Some(self.working_style.stroke.scaled_alpha(0.7)),
                ..Style::highlighter()
            },
            EditorTool::Spotlight => Style::spotlight(),
            tool if tool.is_redaction() => Style {
                redact_strength: self.working_style.redact_strength,
                ..Style::redaction()
            },
            _ => self.working_style,
        }
    }

    fn checkpoint(&mut self) {
        self.history.checkpoint(&self.document);
        self.events.push(EditorEvent::Changed(self.document.data()));
    }

    fn apply_canvas(&mut self, canvas: Canvas) {
        if self.document.set_canvas(canvas).is_ok() {
            self.checkpoint();
        }
    }

    fn undo(&mut self) {
        if self.history.undo(&mut self.document).unwrap_or(false) {
            if self
                .selected
                .is_some_and(|selected| self.document.get(selected).is_none())
            {
                self.selected = None;
            }
            self.events.push(EditorEvent::Changed(self.document.data()));
        }
    }

    fn redo(&mut self) {
        if self.history.redo(&mut self.document).unwrap_or(false) {
            self.events.push(EditorEvent::Changed(self.document.data()));
        }
    }

    fn delete_selected(&mut self) {
        if let Some(id) = self.selected.take()
            && self.document.remove(id).is_some()
        {
            self.checkpoint();
        }
    }
}

fn text_button(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    id: Id,
    label: &str,
    selected: bool,
    enabled: bool,
) -> Response {
    let response = ui.interact(
        rect,
        id,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let palette = surface.palette();
    let hovered = enabled && surface.interactive && response.hovered();
    let fill = if selected {
        palette.accent
    } else if hovered {
        palette.hover
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, corner(Radius::BUTTON), fill);
    if selected {
        ui.painter().rect_stroke(
            rect,
            corner(Radius::BUTTON),
            Stroke::new(1.0, palette.accent_hi),
            StrokeKind::Inside,
        );
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        surface.font(Text::Label),
        if selected {
            palette.on_accent
        } else if enabled {
            palette.text_muted
        } else {
            palette.text_faint
        },
    );
    response
}

fn compact_choice(
    ui: &mut Ui,
    surface: &Surface<'_>,
    id: Id,
    label: &str,
    selected: bool,
    enabled: bool,
) -> bool {
    let rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 28.0));
    let response = text_button(ui, surface, rect, id, label, selected, enabled);
    ui.add_space(30.0);
    response.clicked()
}

fn section_title(ui: &mut Ui, surface: &Surface<'_>, title: &str) {
    ui.painter().text(
        ui.cursor().min,
        Align2::LEFT_TOP,
        title,
        surface.font(Text::Label),
        surface.palette().text,
    );
    ui.add_space(19.0);
}

fn helper_text(ui: &mut Ui, surface: &Surface<'_>, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .font(surface.font(Text::Caption))
            .color(surface.palette().text_muted),
    );
}

fn metric_row(ui: &mut Ui, surface: &Surface<'_>, label: &str, value: f64, suffix: &str) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(label)
                .font(surface.font(Text::Caption))
                .color(surface.palette().text_muted),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                egui::RichText::new(format!("{value:.0} {suffix}"))
                    .font(surface.font(Text::Label))
                    .color(surface.palette().text),
            );
        });
    });
    ui.add_space(Space::XS);
}

fn toggle_row(
    ui: &mut Ui,
    surface: &Surface<'_>,
    title: &str,
    subtitle: &str,
    value: &mut bool,
    enabled: bool,
) -> bool {
    let rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 44.0));
    let response = ui.interact(
        rect,
        Id::new(("editor-toggle", title)),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if response.clicked() {
        *value = !*value;
    }
    ui.painter().text(
        rect.left_top(),
        Align2::LEFT_TOP,
        title,
        surface.font(Text::Label),
        surface.palette().text,
    );
    ui.painter().text(
        rect.left_bottom() - vec2(0.0, 15.0),
        Align2::LEFT_BOTTOM,
        subtitle,
        surface.font(Text::Caption),
        surface.palette().text_muted,
    );
    let track =
        Rect::from_center_size(pos2(rect.right() - 20.0, rect.center().y), vec2(34.0, 20.0));
    ui.painter().rect_filled(
        track,
        corner(Radius::pill(track.height())),
        if *value {
            surface.palette().accent
        } else {
            surface.palette().chip_fill
        },
    );
    let knob_x = if *value {
        track.right() - 10.0
    } else {
        track.left() + 10.0
    };
    ui.painter().circle_filled(
        pos2(knob_x, track.center().y),
        7.0,
        if *value {
            surface.palette().on_accent
        } else {
            surface.palette().text_muted
        },
    );
    ui.add_space(46.0);
    response.clicked()
}

fn paint_precision_grid(painter: &egui::Painter, rect: Rect, palette: &crate::theme::Palette) {
    let color = if palette.is_dark() {
        Color32::from_white_alpha(8)
    } else {
        Color32::from_black_alpha(10)
    };
    let step = 24.0;
    let mut x = rect.left() + step;
    while x < rect.right() {
        let mut y = rect.top() + step;
        while y < rect.bottom() {
            painter.circle_filled(pos2(x, y), 0.8, color);
            y += step;
        }
        x += step;
    }
}

fn frame_to_image(frame: &Frame) -> ColorImage {
    let mut rgba = Vec::with_capacity((frame.width() * frame.height() * 4) as usize);
    for y in 0..frame.height() as usize {
        let start = y * frame.stride;
        let end = start + frame.width() as usize * 4;
        rgba.extend_from_slice(&frame.data[start..end]);
    }
    ColorImage::from_rgba_premultiplied([frame.width() as usize, frame.height() as usize], &rgba)
}

fn to_egui(color: Color) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a)
}

fn palette_colors() -> [Color; 12] {
    [
        Color::WHITE,
        Color::rgb(200, 205, 214),
        Color::rgb(25, 27, 33),
        Color::rgb(255, 74, 85),
        Color::rgb(255, 145, 52),
        Color::rgb(255, 211, 69),
        Color::rgb(61, 207, 142),
        Color::rgb(61, 139, 255),
        Color::rgb(121, 103, 255),
        Color::rgb(185, 91, 255),
        Color::rgb(255, 88, 177),
        Color::rgb(116, 124, 138),
    ]
}

fn normalized_rect(a: LogicalPoint, b: LogicalPoint) -> LogicalRect {
    LogicalRect::from_corners(
        LogicalPoint::new(a.x.min(b.x), a.y.min(b.y)),
        LogicalPoint::new(a.x.max(b.x), a.y.max(b.y)),
    )
}

fn rect_is_drawable(rect: LogicalRect) -> bool {
    rect.size.width >= MIN_SHAPE_SIZE && rect.size.height >= MIN_SHAPE_SIZE
}

fn clamp_rect(rect: LogicalRect, bounds: LogicalRect) -> LogicalRect {
    let left = rect.origin.x.max(bounds.origin.x);
    let top = rect.origin.y.max(bounds.origin.y);
    let right = (rect.origin.x + rect.size.width)
        .min(bounds.origin.x + bounds.size.width)
        .max(left);
    let bottom = (rect.origin.y + rect.size.height)
        .min(bounds.origin.y + bounds.size.height)
        .max(top);
    LogicalRect::from_corners(
        LogicalPoint::new(left, top),
        LogicalPoint::new(right, bottom),
    )
}

fn source_corners(rect: LogicalRect) -> [LogicalPoint; 4] {
    let right = rect.origin.x + rect.size.width;
    let bottom = rect.origin.y + rect.size.height;
    [
        rect.origin,
        LogicalPoint::new(right, rect.origin.y),
        LogicalPoint::new(right, bottom),
        LogicalPoint::new(rect.origin.x, bottom),
    ]
}

fn handle_points(rect: Rect) -> [(ResizeHandle, Pos2); 4] {
    [
        (ResizeHandle::NorthWest, rect.left_top()),
        (ResizeHandle::NorthEast, rect.right_top()),
        (ResizeHandle::SouthEast, rect.right_bottom()),
        (ResizeHandle::SouthWest, rect.left_bottom()),
    ]
}

fn resized_rect(original: LogicalRect, handle: ResizeHandle, pointer: LogicalPoint) -> LogicalRect {
    let left = original.origin.x;
    let top = original.origin.y;
    let right = left + original.size.width;
    let bottom = top + original.size.height;
    match handle {
        ResizeHandle::NorthWest => normalized_rect(pointer, LogicalPoint::new(right, bottom)),
        ResizeHandle::NorthEast => normalized_rect(LogicalPoint::new(left, bottom), pointer),
        ResizeHandle::SouthEast => normalized_rect(LogicalPoint::new(left, top), pointer),
        ResizeHandle::SouthWest => normalized_rect(LogicalPoint::new(right, top), pointer),
    }
}

fn rotation_label(rotation: CanvasRotation) -> &'static str {
    match rotation {
        CanvasRotation::None => "0°",
        CanvasRotation::Clockwise90 => "90°",
        CanvasRotation::HalfTurn => "180°",
        CanvasRotation::CounterClockwise90 => "270°",
    }
}

/// Real annotation-editor scene used by the deterministic screenshot harness.
pub struct EditorScene {
    cropping: bool,
}

struct EditorSceneState {
    editor: AnnotationEditor,
    icons: IconStore,
}

impl EditorScene {
    /// Compose-workspace scene.
    #[must_use]
    pub const fn composing() -> Self {
        Self { cropping: false }
    }

    /// Crop-workspace scene.
    #[must_use]
    pub const fn cropping() -> Self {
        Self { cropping: true }
    }

    fn state_id(&self) -> Id {
        Id::new(("annotation-editor-scene", self.cropping))
    }
}

impl Scene for EditorScene {
    fn name(&self) -> &str {
        if self.cropping {
            "annotation-editor-crop"
        } else {
            "annotation-editor-compose"
        }
    }

    fn setup(&self, ctx: &egui::Context) {
        theme::install_fonts(ctx);
        let state = Arc::new(Mutex::new(EditorSceneState {
            editor: demo_editor(self.cropping),
            icons: IconStore::try_new(ctx).expect("embedded editor icons must rasterize"),
        }));
        ctx.data_mut(|data| data.insert_temp(self.state_id(), state));
    }

    fn ui(&self, ui: &mut Ui, ctx: &SceneCtx<'_>) {
        let state: Arc<Mutex<EditorSceneState>> = ui
            .ctx()
            .data_mut(|data| data.get_temp(self.state_id()))
            .expect("editor scene state is installed during setup");
        let mut state = state
            .lock()
            .expect("editor scene state mutex is not poisoned");
        let EditorSceneState { editor, icons } = &mut *state;
        let theme = Theme::for_appearance(match ctx.theme {
            egui::Theme::Dark => Appearance::Dark,
            egui::Theme::Light => Appearance::Light,
        });
        let surface = Surface::still(
            &theme,
            icons,
            Motion::at_ms(ctx.clock.as_millis()).with_reduce_motion(ctx.reduce_motion),
        );
        editor.show_with_surface(ui, &surface);
    }
}

fn demo_editor(cropping: bool) -> AnnotationEditor {
    let mut document = Document::new(demo_capture());
    if cropping {
        let _ = document.set_canvas(Canvas {
            crop: Some(LogicalRect::new(
                LogicalPoint::new(54.0, 32.0),
                LogicalSize::new(532.0, 314.0),
            )),
            auto_expand: true,
            ..Canvas::default()
        });
    } else {
        document.add(
            Annotation::Highlight(LogicalRect::new(
                LogicalPoint::new(176.0, 151.0),
                LogicalSize::new(222.0, 22.0),
            )),
            Style::highlighter(),
        );
        document.add(
            Annotation::Rectangle(LogicalRect::new(
                LogicalPoint::new(456.0, 268.0),
                LogicalSize::new(112.0, 48.0),
            )),
            Style::stroked()
                .with_stroke(Color::rgb(255, 74, 85))
                .with_stroke_width(4.5),
        );
        document.add(
            Annotation::Arrow {
                from: LogicalPoint::new(414.0, 230.0),
                to: LogicalPoint::new(498.0, 270.0),
            },
            Style {
                stroke: Color::rgb(61, 139, 255),
                stroke_width: 5.0,
                arrow_style: ArrowStyle::Curved,
                shadow: true,
                ..Style::default()
            },
        );
        document.add(
            Annotation::Text {
                at: LogicalPoint::new(282.0, 202.0),
                content: "Share this view".to_owned(),
            },
            Style {
                stroke: Color::rgb(61, 139, 255),
                fill: Some(Color::rgb(61, 139, 255)),
                font_size: 19.0,
                text_preset: TextPreset::RoundedBoxed,
                shadow: true,
                ..Style::default()
            },
        );
        document.add(
            Annotation::Counter {
                at: LogicalPoint::new(117.0, 121.0),
                index: 0,
            },
            Style {
                stroke: Color::WHITE,
                fill: Some(Color::rgb(121, 103, 255)),
                font_size: 16.0,
                shadow: true,
                ..Style::default()
            },
        );
    }
    let mut editor = AnnotationEditor::new(document);
    if cropping {
        editor.set_mode(EditorMode::Crop);
    } else {
        editor.selected = editor
            .document
            .annotations()
            .iter()
            .find(|object| object.kind() == scrozz_annotate::AnnotationKind::Arrow)
            .map(|object| object.id);
        editor.tool = EditorTool::Select;
    }
    editor
}

fn demo_capture() -> Capture {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 380;
    let stride = WIDTH as usize * 4;
    let mut data = vec![0_u8; stride * HEIGHT as usize];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let mut pixel = [245, 247, 251, 255];
            if y < 52 {
                pixel = [29, 32, 43, 255];
            } else if x < 142 {
                pixel = [232, 235, 243, 255];
            } else if (174..606).contains(&x) && (82..338).contains(&y) {
                pixel = [255, 255, 255, 255];
            }
            if (22..118).contains(&x) && (82..102).contains(&y) {
                pixel = [122, 119, 255, 255];
            }
            if (176..402).contains(&x)
                && ((118..128).contains(&y) || (152..162).contains(&y) || (186..196).contains(&y))
            {
                pixel = [191, 197, 211, 255];
            }
            if (176..554).contains(&x)
                && ((134..140).contains(&y)
                    || (168..174).contains(&y)
                    || (220..226).contains(&y)
                    || (238..244).contains(&y))
            {
                pixel = [225, 228, 236, 255];
            }
            if (456..568).contains(&x) && (268..316).contains(&y) {
                pixel = [77, 70, 224, 255];
            }
            let at = y as usize * stride + x as usize * 4;
            data[at..at + 4].copy_from_slice(&pixel);
        }
    }
    Capture {
        frame: Frame {
            data,
            size: PhysicalSize::new(f64::from(WIDTH), f64::from(HEIGHT)),
            stride,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::new(1.0),
        },
        provenance: Provenance::Region,
        target: CaptureTarget::Region(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(f64::from(WIDTH), f64::from(HEIGHT)),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_exposes_every_annotation_kind_and_both_redaction_modes() {
        assert_eq!(EditorTool::ALL.len(), 13);
        assert!(EditorTool::ALL.contains(&EditorTool::Line));
        assert!(EditorTool::ALL.contains(&EditorTool::Spotlight));
        assert!(EditorTool::ALL.contains(&EditorTool::Blur));
        assert!(EditorTool::ALL.contains(&EditorTool::Pixelate));
        assert!(EditorTool::ALL.contains(&EditorTool::SolidRedact));
    }

    #[test]
    fn crop_scene_retains_source_bytes() {
        let editor = demo_editor(true);
        let source = demo_capture();
        assert_eq!(editor.document.source.frame.data, source.frame.data);
        assert!(editor.document.canvas().crop.is_some());
    }

    #[test]
    fn crop_workspace_keeps_the_full_source_available() {
        let editor = demo_editor(true);
        assert_eq!(
            editor.document.canvas_size(),
            LogicalSize::new(532.0, 314.0)
        );
        assert_eq!(
            editor.workspace_geometry().output_size(),
            editor.document.logical_size()
        );
    }
}
