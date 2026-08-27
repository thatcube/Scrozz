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
    Alignment, Annotation, AnnotationId, ArrowStyle, AspectPreset, Background, BackgroundImage,
    Beautification, BeautificationPreset, BuiltInBackground, Canvas, CanvasRotation, Color,
    Document, DocumentData, RedactStyle, RenderGeometry, SkiaRenderer, Style, TextPreset,
    UndoHistory,
};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, Error, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalPoint, PhysicalSize, PixelFormat, Provenance, Result, ScaleFactor,
};
use scrozz_export::to_straight_rgba8;
use std::sync::{Arc, Mutex};

const TOP_BAR_HEIGHT: f32 = 62.0;
const FOOTER_HEIGHT: f32 = 58.0;
const COMPACT_FOOTER_HEIGHT: f32 = 94.0;
const COMPACT_LAYOUT_WIDTH: f32 = 840.0;
const INSPECTOR_WIDTH: f32 = 226.0;
const CONTROL_SIZE: f32 = 36.0;
const MIN_SHAPE_SIZE: f64 = 2.0;
const CROP_SNAP_SCREEN_DISTANCE: f64 = 10.0;

/// The two editing workspaces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorMode {
    /// Create, select, move, resize, style, and order annotations.
    #[default]
    Compose,
    /// Adjust non-destructive crop, rotation, flips, and expansion.
    Crop,
    /// Frame the transformed canvas for sharing without changing source pixels.
    Beautify,
}

impl EditorMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Compose => "Compose",
            Self::Crop => "Crop",
            Self::Beautify => "Beautify",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CropAspect {
    #[default]
    Free,
    Custom,
    Original,
    Square,
    FourThree,
    SixteenNine,
}

impl CropAspect {
    const PRESETS: [Self; 5] = [
        Self::Free,
        Self::Original,
        Self::Square,
        Self::FourThree,
        Self::SixteenNine,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Free => "Free",
            Self::Custom => "Locked",
            Self::Original => "Original",
            Self::Square => "1:1",
            Self::FourThree => "4:3",
            Self::SixteenNine => "16:9",
        }
    }
}

/// Where an editor image export should be delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorDestination {
    /// Copy a destination-compatible representation to the clipboard.
    Clipboard,
    /// Write a destination-compatible image to the configured/default folder.
    DefaultFolder,
}

impl EditorDestination {
    const fn progress(self) -> &'static str {
        match self {
            Self::Clipboard => "Copying image…",
            Self::DefaultFolder => "Saving image…",
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

    const fn index(self) -> usize {
        match self {
            Self::Select => 0,
            Self::Arrow => 1,
            Self::Line => 2,
            Self::Rectangle => 3,
            Self::Ellipse => 4,
            Self::Freehand => 5,
            Self::Text => 6,
            Self::Counter => 7,
            Self::Highlight => 8,
            Self::Spotlight => 9,
            Self::Blur => 10,
            Self::Pixelate => 11,
            Self::SolidRedact => 12,
        }
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
    /// Render and deliver the exact current snapshot to a named destination.
    ExportRequested {
        /// Destination selected by the user.
        destination: EditorDestination,
        /// The snapshot that must be persisted and rendered transactionally.
        data: DocumentData,
    },
    /// The color-name accessibility preference changed.
    ColorNamesChanged(bool),
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
    geometry: RenderGeometry,
}

impl Placement {
    fn output_to_screen(self, point: PhysicalPoint) -> Pos2 {
        pos2(
            self.rect.left() + point.x as f32 * self.scale,
            self.rect.top() + point.y as f32 * self.scale,
        )
    }

    fn screen_to_output(self, point: Pos2) -> PhysicalPoint {
        PhysicalPoint::new(
            f64::from((point.x - self.rect.left()) / self.scale),
            f64::from((point.y - self.rect.top()) / self.scale),
        )
    }

    fn source_to_screen(self, point: LogicalPoint) -> Pos2 {
        self.output_to_screen(self.geometry.source_to_output(point))
    }

    fn screen_to_source(self, point: Pos2) -> LogicalPoint {
        self.geometry.output_to_source(self.screen_to_output(point))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResizeHandle {
    NorthWest,
    NorthEast,
    SouthEast,
    SouthWest,
    Start,
    End,
    Control,
}

#[derive(Clone, Debug)]
enum Gesture {
    Create {
        start: LogicalPoint,
        current: LogicalPoint,
        points: Vec<LogicalPoint>,
        reverse_arrow: bool,
    },
    Move {
        id: AnnotationId,
        original: LogicalRect,
        start: LogicalPoint,
        current: LogicalPoint,
    },
    Resize {
        id: AnnotationId,
        handle: ResizeHandle,
        original: LogicalRect,
        current: LogicalPoint,
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
    working_styles: [Style; EditorTool::ALL.len()],
    zoom: f32,
    gesture: Option<Gesture>,
    events: Vec<EditorEvent>,
    texture: Option<TextureHandle>,
    rendered: Option<(DocumentData, EditorMode)>,
    render_geometry: Option<RenderGeometry>,
    render_error: Option<String>,
    notice: Option<String>,
    save_pending: bool,
    save_error: Option<String>,
    export_pending: Option<EditorDestination>,
    export_notice: Option<String>,
    export_error: Option<String>,
    close_sent: bool,
    interactive: bool,
    layer_focus: bool,
    crop_aspect: CropAspect,
    crop_ratio: f64,
    blur_mode: RedactStyle,
    show_color_names: bool,
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
            working_styles: default_working_styles(),
            zoom: 1.0,
            gesture: None,
            events: Vec::new(),
            texture: None,
            rendered: None,
            render_geometry: None,
            render_error: None,
            notice: None,
            save_pending: false,
            save_error: None,
            export_pending: None,
            export_notice: None,
            export_error: None,
            close_sent: false,
            interactive: true,
            layer_focus: false,
            crop_aspect: CropAspect::Free,
            crop_ratio: 1.0,
            blur_mode: RedactStyle::Blur,
            show_color_names: true,
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

    /// Shows that the latest revision is waiting for durable acknowledgement.
    pub fn mark_save_pending(&mut self) {
        self.save_pending = true;
        self.save_error = None;
        self.notice = Some("Saving changes…".to_owned());
    }

    /// Shows that the latest revision is durable.
    pub fn mark_save_succeeded(&mut self) {
        self.save_pending = false;
        self.save_error = None;
        self.notice = Some("Changes saved.".to_owned());
    }

    /// Keeps the editor retryable after queue or store failure.
    pub fn mark_save_failed(&mut self, error: impl Into<String>) {
        self.save_pending = false;
        let error = error.into();
        self.notice = Some(format!("Save failed: {error}. Select Retry to try again."));
        self.save_error = Some(error);
    }

    /// Shows that a destination export is waiting for its terminal outcome.
    pub fn mark_export_pending(&mut self, destination: EditorDestination) {
        self.export_pending = Some(destination);
        self.export_notice = None;
        self.export_error = None;
    }

    /// Re-enables destination actions and keeps their terminal success visible.
    pub fn mark_export_succeeded(&mut self, detail: impl Into<String>) {
        self.export_pending = None;
        self.export_error = None;
        self.export_notice = Some(detail.into());
    }

    /// Re-enables destination actions and surfaces a retryable terminal failure.
    pub fn mark_export_failed(&mut self, error: impl Into<String>) {
        self.export_pending = None;
        self.export_notice = None;
        self.export_error = Some(error.into());
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
        self.layer_focus = false;
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

    /// Whether readable palette names are shown next to color controls.
    #[must_use]
    pub const fn show_color_names(&self) -> bool {
        self.show_color_names
    }

    /// Sets the editor's color-name accessibility preference.
    pub fn set_show_color_names(&mut self, show: bool) {
        self.show_color_names = show;
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
        let footer_height = if full.width() < COMPACT_LAYOUT_WIDTH {
            COMPACT_FOOTER_HEIGHT
        } else {
            FOOTER_HEIGHT
        };
        let footer = Rect::from_min_max(
            pos2(full.left(), full.bottom() - footer_height),
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
        let text_edit_focused = ui.ctx().text_edit_focused();
        let focused = ui.memory(|memory| memory.focused());
        let canvas_focused = focused == Some(Id::new("annotation-canvas"));
        let (undo, redo, save, delete, escape, previous, next, redact, arrow, modifiers) = ui
            .input(|input| {
                let arrow = [
                    egui::Key::ArrowLeft,
                    egui::Key::ArrowRight,
                    egui::Key::ArrowUp,
                    egui::Key::ArrowDown,
                ]
                .into_iter()
                .find(|key| input.key_pressed(*key));
                (
                    input.modifiers.command
                        && !input.modifiers.shift
                        && input.key_pressed(egui::Key::Z),
                    input.modifiers.command
                        && input.modifiers.shift
                        && input.key_pressed(egui::Key::Z),
                    input.modifiers.command && input.key_pressed(egui::Key::S),
                    input.key_pressed(egui::Key::Delete) || input.key_pressed(egui::Key::Backspace),
                    input.key_pressed(egui::Key::Escape),
                    input.modifiers.command && input.key_pressed(egui::Key::OpenBracket),
                    input.modifiers.command && input.key_pressed(egui::Key::CloseBracket),
                    !input.modifiers.command && input.key_pressed(egui::Key::P),
                    arrow,
                    input.modifiers,
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
        if delete && !text_edit_focused {
            self.delete_selected();
        }
        if previous && !text_edit_focused {
            self.cycle_selection(false);
        } else if next && !text_edit_focused {
            self.cycle_selection(true);
        }
        if redact && !text_edit_focused {
            self.set_tool(EditorTool::SolidRedact);
        }
        if let Some(key) = arrow
            && !text_edit_focused
            && (canvas_focused || self.layer_focus)
        {
            let amount = if modifiers.shift { 10.0 } else { 1.0 };
            let (dx, dy) = match key {
                egui::Key::ArrowLeft => (-amount, 0.0),
                egui::Key::ArrowRight => (amount, 0.0),
                egui::Key::ArrowUp => (0.0, -amount),
                egui::Key::ArrowDown => (0.0, amount),
                _ => (0.0, 0.0),
            };
            self.keyboard_adjust_selected(dx, dy, modifiers.alt);
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
        for mode in [EditorMode::Compose, EditorMode::Crop, EditorMode::Beautify] {
            let width = if mode == EditorMode::Beautify {
                78.0
            } else {
                72.0
            };
            let button = Rect::from_min_size(pos2(x, rect.center().y - 17.0), vec2(width, 34.0));
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
            x += width + 2.0;
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

        match self.mode {
            EditorMode::Compose => {
                for tool in EditorTool::ALL {
                    let button = Rect::from_min_size(
                        pos2(x, rect.center().y - 18.0),
                        Vec2::splat(CONTROL_SIZE),
                    );
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
            }
            EditorMode::Crop => self.paint_crop_actions(ui, surface, rect, x),
            EditorMode::Beautify => {}
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
            .show(&mut child, |ui| match self.mode {
                EditorMode::Compose => self.style_inspector(ui, surface),
                EditorMode::Crop => self.crop_inspector(ui, surface),
                EditorMode::Beautify => self.beautify_inspector(ui, surface),
            });
    }

    fn crop_inspector(&mut self, ui: &mut Ui, surface: &Surface<'_>) {
        section_title(ui, surface, "Canvas");
        ui.add_space(Space::SM);
        let canvas = *self.document.canvas();
        let crop = canvas
            .crop
            .unwrap_or_else(|| self.document.logical_bounds());
        let bounds = self.document.logical_bounds();
        let mut width = crop.size.width;
        let mut height = crop.size.height;
        let width_changed = ui
            .horizontal(|ui| {
                helper_text(ui, surface, "Width");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(
                        egui::DragValue::new(&mut width)
                            .range(MIN_SHAPE_SIZE..=bounds.size.width)
                            .suffix(" pt")
                            .speed(1.0),
                    )
                    .changed()
                })
                .inner
            })
            .inner;
        let height_changed = ui
            .horizontal(|ui| {
                helper_text(ui, surface, "Height");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(
                        egui::DragValue::new(&mut height)
                            .range(MIN_SHAPE_SIZE..=bounds.size.height)
                            .suffix(" pt")
                            .speed(1.0),
                    )
                    .changed()
                })
                .inner
            })
            .inner;
        if self.interactive && width_changed {
            self.apply_crop_dimensions(width, height, true);
        } else if self.interactive && height_changed {
            self.apply_crop_dimensions(width, height, false);
        }
        metric_row(
            ui,
            surface,
            "Output",
            self.document.canvas_size().width,
            "pt wide",
        );
        ui.add_space(Space::LG);

        section_title(ui, surface, "Aspect");
        ui.add_space(Space::XS);
        for aspect in CropAspect::PRESETS {
            if compact_choice(
                ui,
                surface,
                Id::new(("crop-aspect", aspect.label())),
                aspect.label(),
                self.crop_aspect == aspect,
                self.interactive,
            ) {
                self.apply_crop_aspect(aspect);
            }
        }
        let mut locked = self.crop_aspect != CropAspect::Free;
        if toggle_row(
            ui,
            surface,
            "Lock aspect",
            if self.crop_aspect == CropAspect::Custom {
                "Keep the current ratio"
            } else {
                "Keep the selected preset"
            },
            &mut locked,
            self.interactive,
        ) {
            if locked {
                let current = self
                    .document
                    .canvas()
                    .crop
                    .unwrap_or_else(|| self.document.logical_bounds());
                self.crop_ratio = current.size.width / current.size.height.max(MIN_SHAPE_SIZE);
                self.crop_aspect = CropAspect::Custom;
            } else {
                self.crop_aspect = CropAspect::Free;
            }
        }
        ui.add_space(Space::LG);

        helper_text(
            ui,
            surface,
            "Edges snap into place. Hold Command while dragging for free placement.",
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

    fn beautify_inspector(&mut self, ui: &mut Ui, surface: &Surface<'_>) {
        section_title(ui, surface, "Framing bench");
        helper_text(
            ui,
            surface,
            "Add sharing canvas around the transformed image. Source pixels stay untouched.",
        );
        ui.add_space(Space::LG);

        if !self.document.may_beautify() {
            beautification_refusal(ui, surface);
            return;
        }

        let mut config = self.document.beautification().cloned().unwrap_or_default();
        let before = config.clone();

        section_title(ui, surface, "Starting point");
        ui.add_space(Space::XS);
        for (label, value) in [
            ("None", Beautification::default()),
            ("Clean", Beautification::preset(BeautificationPreset::Clean)),
            (
                "Social",
                Beautification::preset(BeautificationPreset::Social),
            ),
            ("Story", Beautification::preset(BeautificationPreset::Story)),
            (
                "Editorial",
                Beautification::preset(BeautificationPreset::Editorial),
            ),
        ] {
            if compact_choice(
                ui,
                surface,
                Id::new(("beautification-preset", label)),
                label,
                !beautification_changed(&config, &value),
                self.interactive,
            ) {
                config = value;
            }
        }
        ui.add_space(Space::LG);

        section_title(ui, surface, "Background");
        ui.add_space(Space::SM);
        let backgrounds = [
            BuiltInBackground::Mist,
            BuiltInBackground::Iris,
            BuiltInBackground::Midnight,
            BuiltInBackground::Sunrise,
            BuiltInBackground::Lagoon,
            BuiltInBackground::Sand,
        ];
        let start = ui.cursor().min;
        for (index, background) in backgrounds.into_iter().enumerate() {
            let column = index % 2;
            let row = index / 2;
            let rect = Rect::from_min_size(
                pos2(start.x + column as f32 * 92.0, start.y + row as f32 * 34.0),
                vec2(86.0, 28.0),
            );
            if background_choice(
                ui,
                surface,
                rect,
                background,
                config.background == Background::BuiltIn(background),
                self.interactive,
            )
            .clicked()
            {
                config.background = Background::BuiltIn(background);
            }
        }
        ui.add_space(104.0);
        for (label, background) in [
            ("Transparent", Background::Transparent),
            ("Solid color", Background::Solid(Color::rgb(25, 29, 38))),
        ] {
            if compact_choice(
                ui,
                surface,
                Id::new(("beautification-background", label)),
                label,
                std::mem::discriminant(&config.background) == std::mem::discriminant(&background),
                self.interactive,
            ) {
                config.background = background;
            }
        }
        if let Background::Solid(color) = &mut config.background {
            let mut picked = to_egui(*color);
            ui.horizontal(|ui| {
                helper_text(ui, surface, "Fill color");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.color_edit_button_srgba(&mut picked).changed() && self.interactive {
                        let [r, g, b, a] = picked.to_array();
                        *color = Color::rgba(r, g, b, a);
                    }
                });
            });
            ui.add_space(Space::SM);
        }
        custom_background_drop(ui, surface, &mut config, &mut self.notice, self.interactive);
        ui.add_space(Space::LG);

        section_title(ui, surface, "Canvas");
        ui.add_space(Space::SM);
        beauty_slider(ui, "Padding", &mut config.padding, 0.0..=220.0);
        egui::ComboBox::from_id_salt("beautification-aspect")
            .selected_text(aspect_label(config.aspect))
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for aspect in [
                    AspectPreset::Original,
                    AspectPreset::Square,
                    AspectPreset::Portrait,
                    AspectPreset::Story,
                    AspectPreset::Landscape,
                    AspectPreset::Wide,
                ] {
                    ui.selectable_value(&mut config.aspect, aspect, aspect_label(aspect));
                }
            });
        ui.add_space(Space::MD);
        helper_text(ui, surface, "Placement");
        ui.add_space(Space::XS);
        alignment_matrix(ui, surface, &mut config.alignment, self.interactive);
        ui.add_space(Space::MD);
        let mut auto_balance = config.auto_balance;
        if toggle_row(
            ui,
            surface,
            "Visual balance",
            "Keep at least 35% padding",
            &mut auto_balance,
            self.interactive,
        ) {
            config.auto_balance = auto_balance;
        }
        ui.add_space(Space::LG);

        section_title(ui, surface, "Finish");
        ui.add_space(Space::SM);
        beauty_slider(ui, "Corner radius", &mut config.corner_radius, 0.0..=80.0);
        beauty_slider(ui, "Shadow", &mut config.shadow, 0.0..=80.0);
        beauty_slider(ui, "Border", &mut config.border_width, 0.0..=12.0);
        if config.border_width > 0.0 {
            let mut picked = to_egui(config.border_color);
            ui.horizontal(|ui| {
                helper_text(ui, surface, "Border color");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.color_edit_button_srgba(&mut picked).changed() && self.interactive {
                        let [r, g, b, a] = picked.to_array();
                        config.border_color = Color::rgba(r, g, b, a);
                    }
                });
            });
        }

        if self.interactive && beautification_changed(&before, &config) {
            let beautification = (!config.is_noop()).then_some(config);
            match self.document.set_beautification(beautification) {
                Ok(()) => self.checkpoint(),
                Err(error) => self.notice = Some(format!("Framing failed: {error}")),
            }
        }
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

        self.layer_list(ui, surface);
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
                            | scrozz_annotate::AnnotationKind::Spotlight
                            | scrozz_annotate::AnnotationKind::Redact
                    )
                ) || self.tool == EditorTool::Highlight
                    || self.tool == EditorTool::Spotlight
                    || self.tool.is_redaction()
                {
                    style.fill = Some(color);
                } else if (kind == Some(scrozz_annotate::AnnotationKind::Rectangle)
                    || (selected.is_none() && self.tool == EditorTool::Rectangle))
                    && style.fill.is_some()
                {
                    style.fill = Some(color);
                }
            }
        }
        ui.add_space(62.0);
        if self.show_color_names {
            helper_text(ui, surface, style.stroke.name());
        }
        let mut show_color_names = self.show_color_names;
        if toggle_row(
            ui,
            surface,
            "Color names",
            "Show the selected color in words",
            &mut show_color_names,
            self.interactive,
        ) {
            self.show_color_names = show_color_names;
            self.events
                .push(EditorEvent::ColorNamesChanged(show_color_names));
        }
        ui.add_space(Space::LG);

        if kind == Some(scrozz_annotate::AnnotationKind::Rectangle)
            || (selected.is_none() && self.tool == EditorTool::Rectangle)
        {
            let mut filled = style.fill.is_some_and(|color| !color.is_invisible());
            if toggle_row(
                ui,
                surface,
                "Filled shape",
                "Use the selected color inside",
                &mut filled,
                self.interactive,
            ) {
                style.fill = filled.then_some(style.stroke);
            }
            ui.add_space(Space::LG);
        }

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
            let selected_redaction = selected.as_ref().and_then(|object| {
                if let Annotation::Redact { style, .. } = &object.annotation {
                    Some(*style)
                } else {
                    None
                }
            });
            if selected_redaction.is_some_and(|redaction| {
                matches!(redaction, RedactStyle::Blur | RedactStyle::SmoothBlur)
            }) || (selected.is_none() && self.tool == EditorTool::Blur)
            {
                let current = selected_redaction.unwrap_or(self.blur_mode);
                for (label, mode) in [
                    ("Secure blur", RedactStyle::Blur),
                    ("Smooth blur", RedactStyle::SmoothBlur),
                ] {
                    if compact_choice(
                        ui,
                        surface,
                        Id::new(("blur-mode", label)),
                        label,
                        current == mode,
                        self.interactive,
                    ) {
                        if let Some(id) = self.selected {
                            if set_redact_style(&mut self.document, id, mode) {
                                self.checkpoint();
                            }
                        } else {
                            self.blur_mode = mode;
                        }
                    }
                }
                helper_text(
                    ui,
                    surface,
                    if current == RedactStyle::Blur {
                        "Secure blur replaces source detail with a seeded field."
                    } else {
                        "Smooth blur is cosmetic and may retain recoverable detail."
                    },
                );
                ui.add_space(Space::MD);
            }
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
                self.working_styles[self.tool.index()] = style;
            }
        }
    }

    fn layer_list(&mut self, ui: &mut Ui, surface: &Surface<'_>) {
        section_title(ui, surface, "Layers");
        ui.add_space(Space::XS);
        if self.document.is_empty() {
            helper_text(ui, surface, "Draw an object to add a layer.");
            return;
        }

        let layers = self
            .document
            .annotations()
            .iter()
            .rev()
            .enumerate()
            .map(|(index, object)| {
                (
                    object.id,
                    layer_label(object, index + 1, self.document.len()),
                )
            })
            .collect::<Vec<_>>();
        let mut chosen = None;
        let mut layer_focus = false;
        let list = ui.vertical(|ui| {
            for (id, label) in &layers {
                let selected = self.selected == Some(*id);
                let response =
                    ui.add_enabled(self.interactive, egui::Button::selectable(selected, label));
                response.ctx.accesskit_node_builder(response.id, |node| {
                    node.set_role(egui::accesskit::Role::ListItem);
                    node.set_label(label.as_str());
                    node.set_selected(selected);
                });
                layer_focus |= response.has_focus();
                if response.clicked() {
                    chosen = Some(*id);
                }
            }
        });
        list.response
            .ctx
            .accesskit_node_builder(list.response.id, |node| {
                node.set_role(egui::accesskit::Role::List);
                node.set_label("Annotation layers");
            });
        if let Some(id) = chosen {
            self.selected = Some(id);
            self.tool = EditorTool::Select;
        }
        self.layer_focus = layer_focus;
        helper_text(
            ui,
            surface,
            "Tab to a layer; arrows move, Option+arrows resize.",
        );
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

        let Some((texture, geometry)) = self.ensure_texture(ui.ctx()) else {
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

        let placement = self.placement(rect, geometry);
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

        match self.mode {
            EditorMode::Compose => {
                self.paint_selection(ui, surface, placement);
                self.paint_creation_preview(ui, surface, placement);
            }
            EditorMode::Crop => self.paint_crop_overlay(ui, surface, placement),
            EditorMode::Beautify => {}
        }

        if self.interactive && self.mode != EditorMode::Beautify {
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
        let handles = self.selection_handle_points(object, placement);
        if let Annotation::Arrow { from, to } = object.annotation
            && let Some(control) = handles
                .iter()
                .find_map(|(handle, point)| (*handle == ResizeHandle::Control).then_some(*point))
        {
            let from = placement.source_to_screen(from);
            let to = placement.source_to_screen(to);
            ui.painter().line_segment(
                [from, control],
                Stroke::new(1.0, surface.palette().accent_hi.gamma_multiply(0.55)),
            );
            ui.painter().line_segment(
                [control, to],
                Stroke::new(1.0, surface.palette().accent_hi.gamma_multiply(0.55)),
            );
        }
        let points = handles.iter().map(|(_, point)| *point).collect::<Vec<_>>();
        let rect = Rect::from_points(&points);
        let painter = ui.painter();
        let stroke = Stroke::new(1.5, surface.palette().accent_hi);
        painter.rect_stroke(rect, 2.0, stroke, StrokeKind::Outside);
        for (handle, point) in handles {
            if handle == ResizeHandle::Control {
                let control = Rect::from_center_size(point, Vec2::splat(9.0));
                painter.rect_filled(control, 2.0, surface.palette().card_fill_raised);
                painter.rect_stroke(
                    control,
                    2.0,
                    Stroke::new(2.0, surface.palette().accent_hi),
                    StrokeKind::Outside,
                );
            } else {
                painter.circle_filled(point, 5.0, surface.palette().card_fill_raised);
                painter.circle_stroke(point, 5.0, Stroke::new(2.0, surface.palette().accent_hi));
            }
        }
    }

    fn paint_creation_preview(&self, ui: &mut Ui, surface: &Surface<'_>, placement: Placement) {
        let Some(Gesture::Create {
            start,
            current,
            points,
            ..
        }) = &self.gesture
        else {
            return;
        };
        let to_screen = |point| placement.source_to_screen(point);
        let stroke = Stroke::new(2.0, to_egui(self.default_style_for_tool().stroke));
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
        let pointer_source = |position: Pos2| placement.screen_to_source(position);

        if response.drag_started()
            && let Some(origin) = response.ctx.input(|input| input.pointer.press_origin())
        {
            let mut source = pointer_source(origin);
            if self.mode == EditorMode::Crop {
                let bypass_snap = response.ctx.input(|input| input.modifiers.command);
                source = snap_crop_point(
                    source,
                    self.document.logical_bounds(),
                    CROP_SNAP_SCREEN_DISTANCE
                        / (f64::from(placement.scale) * placement.geometry.scale.get()),
                    bypass_snap,
                );
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
                    reverse_arrow: self.tool == EditorTool::Arrow
                        && response.ctx.input(|input| input.modifiers.alt),
                });
            }
        }

        if response.dragged()
            && let Some(position) = response.interact_pointer_pos()
        {
            let mut source = pointer_source(position);
            let crop_bounds = self.document.logical_bounds();
            let crop_ratio = self.crop_ratio();
            let bypass_snap = response.ctx.input(|input| input.modifiers.command);
            let snap_tolerance = CROP_SNAP_SCREEN_DISTANCE
                / (f64::from(placement.scale) * placement.geometry.scale.get());
            match &mut self.gesture {
                Some(Gesture::Create {
                    current, points, ..
                }) => {
                    *current = source;
                    if self.tool == EditorTool::Freehand {
                        points.push(source);
                    }
                }
                Some(Gesture::Move { current, .. }) | Some(Gesture::Resize { current, .. }) => {
                    *current = source
                }
                Some(Gesture::Crop { start, current }) => {
                    source = snap_crop_point(source, crop_bounds, snap_tolerance, bypass_snap);
                    *current = constrain_crop_point(*start, source, crop_bounds, crop_ratio);
                }
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
            && let Some(handle) = self
                .selection_handle_points(object, placement)
                .into_iter()
                .find(|(_, point)| point.distance(pointer) <= 9.0)
                .map(|(handle, _)| handle)
        {
            self.gesture = Some(Gesture::Resize {
                id,
                handle,
                original: object.bounds(),
                current: source,
            });
            return;
        }
        self.selected = self.document.hit_test(source);
        self.gesture = self.selected.and_then(|id| {
            self.document.get(id).map(|object| Gesture::Move {
                id,
                original: object.bounds(),
                start: source,
                current: source,
            })
        });
    }

    fn handle_canvas_click(&mut self, source: LogicalPoint) {
        match self.mode {
            EditorMode::Crop | EditorMode::Beautify => {}
            EditorMode::Compose => match self.tool {
                EditorTool::Select => self.selected = self.document.hit_test(source),
                EditorTool::Text => {
                    let Some(id) = self.add_annotation(
                        Annotation::Text {
                            at: source,
                            content: "Add a note".to_owned(),
                        },
                        self.default_style_for_tool(),
                    ) else {
                        return;
                    };
                    self.selected = Some(id);
                    self.tool = EditorTool::Select;
                    self.checkpoint();
                }
                EditorTool::Counter => {
                    let Some(id) = self.add_annotation(
                        Annotation::Counter {
                            at: source,
                            index: 0,
                        },
                        self.default_style_for_tool(),
                    ) else {
                        return;
                    };
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
                reverse_arrow,
            } => {
                if let Some(annotation) =
                    self.annotation_for_gesture(start, current, points, reverse_arrow)
                {
                    if let Some(id) = self.add_annotation(annotation, self.default_style_for_tool())
                    {
                        self.selected = Some(id);
                        self.checkpoint();
                    }
                }
            }
            Gesture::Move {
                id,
                original: _,
                start,
                current,
            } => {
                let dx = current.x - start.x;
                let dy = current.y - start.y;
                if dx != 0.0 || dy != 0.0 {
                    self.document.translate(id, dx, dy);
                    self.checkpoint();
                }
            }
            Gesture::Resize {
                id,
                handle,
                original,
                current,
            } => {
                let changed = match handle {
                    ResizeHandle::Start | ResizeHandle::End => {
                        set_point_annotation_endpoint(&mut self.document, id, handle, current)
                    }
                    ResizeHandle::Control => set_curve_control(&mut self.document, id, current),
                    _ => {
                        let bounds = resized_rect(original, handle, current);
                        bounds != original && self.document.set_bounds(id, bounds)
                    }
                };
                if changed {
                    self.checkpoint();
                }
            }
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

    fn add_annotation(&mut self, annotation: Annotation, style: Style) -> Option<AnnotationId> {
        match self.document.add(annotation, style) {
            Ok(id) => Some(id),
            Err(error) => {
                self.notice = Some(format!("Could not add annotation: {error}"));
                None
            }
        }
    }

    fn annotation_for_gesture(
        &self,
        start: LogicalPoint,
        current: LogicalPoint,
        points: Vec<LogicalPoint>,
        reverse_arrow: bool,
    ) -> Option<Annotation> {
        let rect = normalized_rect(start, current);
        match self.tool {
            EditorTool::Arrow => Some(Annotation::Arrow {
                from: if reverse_arrow { current } else { start },
                to: if reverse_arrow { start } else { current },
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
                style: self.blur_mode,
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
        let compact = rect.width() < COMPACT_LAYOUT_WIDTH;
        ui.painter()
            .rect_filled(rect, 0.0, palette.card_fill_raised);
        paint::divider_h(ui.painter(), rect.left(), rect.right(), rect.top(), palette);

        let status = if let Some(destination) = self.export_pending {
            destination.progress().to_owned()
        } else if let Some(error) = &self.export_error {
            format!("Export failed: {error}")
        } else if let Some(notice) = &self.export_notice {
            notice.clone()
        } else if let Some(notice) = &self.notice {
            notice.clone()
        } else {
            match self.mode {
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
                EditorMode::Beautify => "Framing preview · source unchanged".to_owned(),
            }
        };
        ui.painter().text(
            pos2(
                rect.left() + Space::LG,
                if compact {
                    rect.top() + 22.0
                } else {
                    rect.center().y
                },
            ),
            Align2::LEFT_CENTER,
            status,
            surface.font(Text::Caption),
            palette.text_muted,
        );

        if !compact {
            let zoom_area = Rect::from_center_size(
                pos2(rect.center().x - 135.0, rect.center().y),
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
        }

        let action_center_y = if compact {
            rect.bottom() - 28.0
        } else {
            rect.center().y
        };
        let save_width = if compact { 72.0 } else { 80.0 };
        let save = Rect::from_min_size(
            pos2(
                rect.right() - Space::MD - save_width,
                action_center_y - 18.0,
            ),
            vec2(save_width, 36.0),
        );
        let save_label = if self.save_error.is_some() {
            "Retry"
        } else if self.save_pending {
            "Saving"
        } else {
            "Save"
        };
        if paint::pill_button_with_state(
            ui,
            surface,
            save,
            Id::new("editor-save"),
            Icon::DeviceFloppy,
            save_label,
            true,
            ControlState {
                enabled: self.interactive && !self.save_pending,
                ..ControlState::new()
            },
            Reveal::SHOWN,
        )
        .clicked()
        {
            self.events.push(EditorEvent::Save(self.document.data()));
        }

        let action_gap = if compact { Space::SM } else { Space::MD };
        let drag_width = if compact { 76.0 } else { 104.0 };
        let drag = Rect::from_min_size(
            pos2(
                save.left() - action_gap - drag_width,
                action_center_y - 18.0,
            ),
            vec2(drag_width, 36.0),
        );
        let response = paint::pill_drag_source_with_state(
            ui,
            surface,
            drag,
            Id::new("editor-drag"),
            Icon::GripVertical,
            if compact { "Drag" } else { "Drag me" },
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
            let canvas = self
                .render_geometry
                .map(RenderGeometry::output_size)
                .unwrap_or_else(|| {
                    let logical = self.document.canvas_size();
                    PhysicalSize::new(logical.width, logical.height)
                });
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

        let export_enabled = self.interactive && self.export_pending.is_none();
        let save_image_width = if compact { 84.0 } else { 124.0 };
        let save_image = Rect::from_min_size(
            pos2(
                drag.left() - action_gap - save_image_width,
                action_center_y - 18.0,
            ),
            vec2(save_image_width, 36.0),
        );
        if paint::pill_button_with_state(
            ui,
            surface,
            save_image,
            Id::new("editor-save-image"),
            Icon::DeviceFloppy,
            if compact { "Export" } else { "Save image" },
            false,
            ControlState {
                enabled: export_enabled,
                ..ControlState::new()
            },
            Reveal::SHOWN,
        )
        .on_hover_text("Export the edited image to the default folder")
        .clicked()
        {
            let destination = EditorDestination::DefaultFolder;
            self.mark_export_pending(destination);
            self.events.push(EditorEvent::ExportRequested {
                destination,
                data: self.document.data(),
            });
        }

        let copy_image_width = if compact { 72.0 } else { 88.0 };
        let copy_image = Rect::from_min_size(
            pos2(
                save_image.left() - action_gap - copy_image_width,
                action_center_y - 18.0,
            ),
            vec2(copy_image_width, 36.0),
        );
        if paint::pill_button_with_state(
            ui,
            surface,
            copy_image,
            Id::new("editor-copy-image"),
            Icon::Copy,
            "Copy",
            false,
            ControlState {
                enabled: export_enabled,
                ..ControlState::new()
            },
            Reveal::SHOWN,
        )
        .on_hover_text("Copy the edited image")
        .clicked()
        {
            let destination = EditorDestination::Clipboard;
            self.mark_export_pending(destination);
            self.events.push(EditorEvent::ExportRequested {
                destination,
                data: self.document.data(),
            });
        }
    }

    fn placement(&self, workspace: Rect, geometry: RenderGeometry) -> Placement {
        let size = geometry.output_size();
        let available = workspace.shrink2(vec2(44.0, 34.0));
        let fit = (available.width() / size.width.max(1.0) as f32)
            .min(available.height() / size.height.max(1.0) as f32);
        let scale = (fit * self.zoom).clamp(0.02, 12.0);
        let image_size = vec2(size.width as f32 * scale, size.height as f32 * scale);
        Placement {
            rect: Rect::from_center_size(workspace.center(), image_size),
            scale,
            geometry,
        }
    }

    fn source_rect_to_screen(&self, rect: LogicalRect, placement: Placement) -> Rect {
        let points = source_corners(rect).map(|point| placement.source_to_screen(point));
        Rect::from_points(&points)
    }

    fn selection_handle_points(
        &self,
        object: &scrozz_annotate::AnnotationObject,
        placement: Placement,
    ) -> Vec<(ResizeHandle, Pos2)> {
        let point_handles = match &object.annotation {
            Annotation::Arrow { from, to } | Annotation::Line { from, to } => Some((*from, *to)),
            _ => None,
        };
        if let Some((mut from, mut to)) = point_handles {
            let mut control = object.curve_control();
            match self.gesture.as_ref() {
                Some(Gesture::Move {
                    id, start, current, ..
                }) if *id == object.id => {
                    let dx = current.x - start.x;
                    let dy = current.y - start.y;
                    from = LogicalPoint::new(from.x + dx, from.y + dy);
                    to = LogicalPoint::new(to.x + dx, to.y + dy);
                    control = control.map(|point| LogicalPoint::new(point.x + dx, point.y + dy));
                }
                Some(Gesture::Resize {
                    id,
                    handle: ResizeHandle::Start,
                    current,
                    ..
                }) if *id == object.id => from = *current,
                Some(Gesture::Resize {
                    id,
                    handle: ResizeHandle::End,
                    current,
                    ..
                }) if *id == object.id => to = *current,
                Some(Gesture::Resize {
                    id,
                    handle: ResizeHandle::Control,
                    current,
                    ..
                }) if *id == object.id => control = Some(*current),
                _ => {}
            }
            let mut handles = vec![
                (ResizeHandle::Start, placement.source_to_screen(from)),
                (ResizeHandle::End, placement.source_to_screen(to)),
            ];
            if let Some(control) = control {
                handles.push((ResizeHandle::Control, placement.source_to_screen(control)));
            }
            return handles;
        }

        source_handle_points(self.gesture_bounds(object.id, object.bounds()))
            .map(|(handle, point)| (handle, placement.source_to_screen(point)))
            .into()
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) -> Option<(TextureId, RenderGeometry)> {
        let data = self.document.data();
        let key = (data, self.mode);
        if self.rendered.as_ref() != Some(&key) {
            let renderer = SkiaRenderer::new();
            let rendered = match self.mode {
                EditorMode::Compose | EditorMode::Beautify => renderer
                    .render_at_with_geometry(&self.document, self.document.source.frame.scale),
                EditorMode::Crop => {
                    renderer.render_canvas_with_geometry(&self.document, self.workspace_canvas())
                }
            };
            match rendered {
                Ok(rendered_frame) => {
                    let image = frame_to_image(&rendered_frame.frame);
                    self.texture = Some(ctx.load_texture(
                        "annotation-editor-canvas",
                        image,
                        TextureOptions::LINEAR,
                    ));
                    self.render_geometry = Some(rendered_frame.geometry);
                    self.rendered = Some(key);
                    self.render_error = None;
                }
                Err(error) => {
                    self.texture = None;
                    self.rendered = None;
                    self.render_geometry = None;
                    self.render_error = Some(error.to_string());
                }
            }
        }
        self.texture
            .as_ref()
            .zip(self.render_geometry)
            .map(|(texture, geometry)| (texture.id(), geometry))
    }

    fn gesture_bounds(&self, id: AnnotationId, fallback: LogicalRect) -> LogicalRect {
        match self.gesture.as_ref() {
            Some(Gesture::Move {
                id: moving,
                original,
                start,
                current,
            }) if *moving == id => {
                translated_rect(*original, current.x - start.x, current.y - start.y)
            }
            Some(Gesture::Resize {
                id: resizing,
                handle,
                original,
                current,
            }) if *resizing == id => resized_rect(*original, *handle, *current),
            _ => fallback,
        }
    }

    fn workspace_canvas(&self) -> Canvas {
        match self.mode {
            EditorMode::Compose | EditorMode::Beautify => *self.document.canvas(),
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

    fn crop_ratio(&self) -> Option<f64> {
        match self.crop_aspect {
            CropAspect::Free => None,
            CropAspect::Custom => Some(self.crop_ratio),
            CropAspect::Original => {
                let size = self.document.logical_size();
                Some(size.width / size.height.max(MIN_SHAPE_SIZE))
            }
            CropAspect::Square => Some(1.0),
            CropAspect::FourThree => Some(4.0 / 3.0),
            CropAspect::SixteenNine => Some(16.0 / 9.0),
        }
    }

    fn apply_crop_aspect(&mut self, aspect: CropAspect) {
        self.crop_aspect = aspect;
        let Some(ratio) = self.crop_ratio() else {
            return;
        };
        self.crop_ratio = ratio;
        let canvas = *self.document.canvas();
        let bounds = self.document.logical_bounds();
        let crop = canvas.crop.unwrap_or(bounds);
        let fitted = fit_ratio_within(crop, ratio);
        self.apply_canvas(Canvas {
            crop: Some(clamp_rect(fitted, bounds)),
            ..canvas
        });
    }

    fn apply_crop_dimensions(&mut self, mut width: f64, mut height: f64, width_changed: bool) {
        let canvas = *self.document.canvas();
        let bounds = self.document.logical_bounds();
        let crop = canvas.crop.unwrap_or(bounds);
        if let Some(ratio) = self.crop_ratio() {
            if width_changed {
                height = width / ratio;
            } else {
                width = height * ratio;
            }
        }
        let available_width =
            (bounds.origin.x + bounds.size.width - crop.origin.x).max(MIN_SHAPE_SIZE);
        let available_height =
            (bounds.origin.y + bounds.size.height - crop.origin.y).max(MIN_SHAPE_SIZE);
        if let Some(ratio) = self.crop_ratio() {
            let scale = (available_width / width)
                .min(available_height / height)
                .min(1.0);
            width *= scale;
            height *= scale;
            self.crop_ratio = ratio;
        }
        let resized = LogicalRect::new(
            crop.origin,
            LogicalSize::new(
                width.clamp(MIN_SHAPE_SIZE, available_width),
                height.clamp(MIN_SHAPE_SIZE, available_height),
            ),
        );
        self.apply_canvas(Canvas {
            crop: Some(resized),
            ..canvas
        });
    }

    fn default_style_for_tool(&self) -> Style {
        self.working_styles[self.tool.index()]
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

    fn cycle_selection(&mut self, forward: bool) {
        let objects = self.document.annotations();
        if objects.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|id| objects.iter().position(|object| object.id == id));
        let index = match (current, forward) {
            (Some(index), true) => (index + 1) % objects.len(),
            (Some(0), false) | (None, false) => objects.len() - 1,
            (Some(index), false) => index - 1,
            (None, true) => 0,
        };
        self.selected = Some(objects[index].id);
        self.tool = EditorTool::Select;
    }

    fn keyboard_adjust_selected(&mut self, dx: f64, dy: f64, resize: bool) {
        let Some(id) = self.selected else {
            return;
        };
        let changed = if resize {
            let Some(object) = self.document.get(id).cloned() else {
                return;
            };
            match object.annotation {
                Annotation::Text { .. } | Annotation::Counter { .. } => {
                    let mut style = object.style;
                    let delta = if dx != 0.0 { dx } else { dy };
                    style.font_size = (style.effective_font_size() + delta).max(1.0);
                    self.document.set_style(id, style)
                }
                Annotation::Arrow { mut to, .. } | Annotation::Line { mut to, .. } => {
                    let original = to;
                    to.x += dx;
                    to.y += dy;
                    if to == original {
                        return;
                    }
                    let mut object = self.document.get_mut(id).expect("selected object exists");
                    match object.annotation() {
                        Annotation::Arrow { to: current_to, .. }
                        | Annotation::Line { to: current_to, .. } => {
                            *current_to = to;
                            true
                        }
                        _ => unreachable!("annotation kind changed while selected"),
                    }
                }
                _ => {
                    let mut bounds = object.bounds();
                    bounds.size.width = (bounds.size.width + dx).max(1.0);
                    bounds.size.height = (bounds.size.height + dy).max(1.0);
                    self.document.set_bounds(id, bounds)
                }
            }
        } else {
            self.document.translate(id, dx, dy)
        };
        if changed {
            self.checkpoint();
        }
    }
}

fn set_curve_control(document: &mut Document, id: AnnotationId, point: LogicalPoint) -> bool {
    let Some(mut object) = document.get_mut(id) else {
        return false;
    };
    if !matches!(object.annotation(), Annotation::Arrow { .. }) {
        return false;
    }
    let style = object.style();
    if style.arrow_style != ArrowStyle::Curved || style.curve_control == Some(point) {
        return false;
    }
    style.curve_control = Some(point);
    true
}

fn set_redact_style(document: &mut Document, id: AnnotationId, style: RedactStyle) -> bool {
    let Some(mut object) = document.get_mut(id) else {
        return false;
    };
    let Annotation::Redact { style: current, .. } = object.annotation() else {
        return false;
    };
    if *current == style {
        return false;
    }
    *current = style;
    true
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

fn beauty_slider(ui: &mut Ui, label: &str, value: &mut f64, range: std::ops::RangeInclusive<f64>) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(
                egui::Slider::new(value, range)
                    .suffix(" pt")
                    .fixed_decimals(0),
            );
        });
    });
    ui.add_space(Space::SM);
}

fn aspect_label(aspect: AspectPreset) -> &'static str {
    match aspect {
        AspectPreset::Original => "Original canvas",
        AspectPreset::Square => "Square · 1:1",
        AspectPreset::Portrait => "Portrait · 4:5",
        AspectPreset::Story => "Story · 9:16",
        AspectPreset::Landscape => "Landscape · 16:9",
        AspectPreset::Wide => "Header · 3:1",
    }
}

fn built_in_label(background: BuiltInBackground) -> &'static str {
    match background {
        BuiltInBackground::Mist => "Mist",
        BuiltInBackground::Iris => "Iris",
        BuiltInBackground::Midnight => "Midnight",
        BuiltInBackground::Sunrise => "Sunrise",
        BuiltInBackground::Lagoon => "Lagoon",
        BuiltInBackground::Sand => "Sand",
    }
}

fn built_in_swatch(background: BuiltInBackground) -> Color32 {
    match background {
        BuiltInBackground::Mist => Color32::from_rgb(201, 211, 224),
        BuiltInBackground::Iris => Color32::from_rgb(126, 105, 224),
        BuiltInBackground::Midnight => Color32::from_rgb(25, 43, 72),
        BuiltInBackground::Sunrise => Color32::from_rgb(239, 159, 150),
        BuiltInBackground::Lagoon => Color32::from_rgb(51, 154, 166),
        BuiltInBackground::Sand => Color32::from_rgb(213, 193, 166),
    }
}

fn background_choice(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    background: BuiltInBackground,
    selected: bool,
    enabled: bool,
) -> Response {
    let response = ui.interact(
        rect,
        Id::new(("beautification-built-in", built_in_label(background))),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let palette = surface.palette();
    let fill = if selected {
        palette.accent.gamma_multiply(0.18)
    } else if response.hovered() && enabled {
        palette.hover
    } else {
        palette.chip_fill
    };
    ui.painter().rect_filled(rect, corner(Radius::BUTTON), fill);
    ui.painter().rect_stroke(
        rect,
        corner(Radius::BUTTON),
        Stroke::new(
            if selected { 2.0 } else { 1.0 },
            if selected {
                palette.accent
            } else {
                palette.hairline
            },
        ),
        StrokeKind::Inside,
    );
    ui.painter().circle_filled(
        pos2(rect.left() + 13.0, rect.center().y),
        5.0,
        built_in_swatch(background),
    );
    ui.painter().text(
        pos2(rect.left() + 24.0, rect.center().y),
        Align2::LEFT_CENTER,
        built_in_label(background),
        surface.font(Text::Caption),
        palette.text,
    );
    response
}

fn alignment_matrix(ui: &mut Ui, surface: &Surface<'_>, alignment: &mut Alignment, enabled: bool) {
    let size = 72.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter()
        .rect_filled(rect, corner(Radius::BUTTON), surface.palette().chip_fill);
    ui.painter().rect_stroke(
        rect,
        corner(Radius::BUTTON),
        Stroke::new(1.0, surface.palette().hairline),
        StrokeKind::Inside,
    );
    let choices = [
        Alignment::TopLeft,
        Alignment::Top,
        Alignment::TopRight,
        Alignment::Left,
        Alignment::Center,
        Alignment::Right,
        Alignment::BottomLeft,
        Alignment::Bottom,
        Alignment::BottomRight,
    ];
    let cell = size / 3.0;
    for (index, choice) in choices.into_iter().enumerate() {
        let center = pos2(
            rect.left() + cell * (index % 3) as f32 + cell / 2.0,
            rect.top() + cell * (index / 3) as f32 + cell / 2.0,
        );
        let hit = Rect::from_center_size(center, Vec2::splat(cell));
        let cell_response = ui.interact(
            hit,
            response.id.with(index),
            if enabled {
                Sense::click()
            } else {
                Sense::hover()
            },
        );
        if cell_response.clicked() {
            *alignment = choice;
        }
        ui.painter().circle_filled(
            center,
            if *alignment == choice { 5.0 } else { 2.5 },
            if *alignment == choice {
                surface.palette().accent
            } else {
                surface.palette().text_muted
            },
        );
    }
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_role(egui::accesskit::Role::Group);
        node.set_label("Image placement");
    });
}

fn custom_background_drop(
    ui: &mut Ui,
    surface: &Surface<'_>,
    config: &mut Beautification,
    notice: &mut Option<String>,
    enabled: bool,
) {
    let hovering = enabled && ui.input(|input| !input.raw.hovered_files.is_empty());
    let frame = egui::Frame::new()
        .fill(if hovering {
            surface.palette().accent.gamma_multiply(0.14)
        } else {
            surface.palette().chip_fill
        })
        .corner_radius(corner(Radius::BUTTON))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .stroke(Stroke::new(
            if hovering { 2.0 } else { 1.0 },
            if hovering {
                surface.palette().accent
            } else {
                surface.palette().hairline
            },
        ));
    let response = frame
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(if matches!(config.background, Background::Image(_)) {
                        "CUSTOM"
                    } else {
                        "DROP"
                    })
                    .font(surface.font(Text::Caption))
                    .color(surface.palette().accent)
                    .strong(),
                );
                helper_text(ui, surface, "PNG, JPEG, or WebP");
            });
        })
        .response
        .on_hover_cursor(CursorIcon::Copy);
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_role(egui::accesskit::Role::Group);
        node.set_label("Drop a PNG, JPEG, or WebP custom background");
    });

    if !enabled {
        return;
    }
    let dropped = ui.input(|input| input.raw.dropped_files.first().cloned());
    let Some(file) = dropped else {
        return;
    };
    match file
        .bytes()
        .map_err(Error::Codec)
        .and_then(|bytes| background_from_bytes(&bytes))
    {
        Ok(image) => {
            config.background = Background::Image(image);
            *notice = Some(format!(
                "Loaded custom background {}.",
                file.path().file_name().map_or_else(
                    || "image".to_owned(),
                    |name| name.to_string_lossy().into_owned()
                )
            ));
        }
        Err(error) => *notice = Some(format!("Background failed: {error}")),
    }
}

fn background_from_bytes(bytes: &[u8]) -> Result<BackgroundImage> {
    let frame = scrozz_export::decode(bytes)?;
    let image = to_straight_rgba8(&frame)?;
    BackgroundImage::new(image.width, image.height, image.data, frame.color_space)
}

fn beautification_changed(before: &Beautification, after: &Beautification) -> bool {
    match (&before.background, &after.background) {
        (Background::Image(before_image), Background::Image(after_image)) => {
            let background_changed = before_image.width() != after_image.width()
                || before_image.height() != after_image.height()
                || before_image.color_space() != after_image.color_space()
                || !std::ptr::eq(before_image.pixels(), after_image.pixels());
            if background_changed {
                return true;
            }
            let mut before = before.clone();
            let mut after = after.clone();
            before.background = Background::Transparent;
            after.background = Background::Transparent;
            before != after
        }
        _ => before != after,
    }
}

fn beautification_refusal(ui: &mut Ui, surface: &Surface<'_>) {
    let danger = editor_danger(surface);
    let rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 132.0));
    ui.painter()
        .rect_filled(rect, corner(Radius::CARD), danger.gamma_multiply(0.10));
    ui.painter().rect_stroke(
        rect,
        corner(Radius::CARD),
        Stroke::new(1.0, danger.gamma_multiply(0.7)),
        StrokeKind::Inside,
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt("beautification-refusal")
            .max_rect(rect.shrink(12.0))
            .layout(Layout::top_down(Align::Min)),
    );
    child.label(
        egui::RichText::new("Window shape locked")
            .font(surface.font(Text::Label))
            .color(danger)
            .strong(),
    );
    child.add_space(Space::SM);
    helper_text(
        &mut child,
        surface,
        "The system already supplied this window's true corners and shadow. Framing it again would make the capture subtly wrong.",
    );
    ui.add_space(140.0);
}

fn editor_danger(surface: &Surface<'_>) -> Color32 {
    if surface.palette().is_dark() {
        Color32::from_rgb(0xFF, 0x7A, 0x70)
    } else {
        Color32::from_rgb(0xB4, 0x23, 0x18)
    }
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

fn layer_label(
    object: &scrozz_annotate::AnnotationObject,
    visual_index: usize,
    total: usize,
) -> String {
    let name = match &object.annotation {
        Annotation::Text { content, .. } => {
            let content = content.trim().replace('\n', " ");
            if content.is_empty() {
                "Empty text".to_owned()
            } else {
                format!("Text: {}", content.chars().take(28).collect::<String>())
            }
        }
        Annotation::Arrow { .. } => "Arrow".to_owned(),
        Annotation::Line { .. } => "Line".to_owned(),
        Annotation::Rectangle(_) => "Rectangle".to_owned(),
        Annotation::Ellipse(_) => "Ellipse".to_owned(),
        Annotation::Freehand(_) => "Freehand".to_owned(),
        Annotation::Counter { index, .. } => format!("Step {index}"),
        Annotation::Highlight(_) => "Highlight".to_owned(),
        Annotation::Spotlight(_) => "Spotlight".to_owned(),
        Annotation::Redact { style, .. } => format!("{} redaction", redact_style_label(*style)),
        _ => "Annotation".to_owned(),
    };
    format!("{visual_index} of {total}, {name}")
}

fn redact_style_label(style: RedactStyle) -> &'static str {
    match style {
        RedactStyle::Blur => "Secure blur",
        RedactStyle::SmoothBlur => "Smooth blur",
        RedactStyle::Pixelate => "Pixelate",
        RedactStyle::Solid => "Solid",
    }
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

fn snap_crop_point(
    point: LogicalPoint,
    bounds: LogicalRect,
    tolerance: f64,
    bypass: bool,
) -> LogicalPoint {
    let left = bounds.origin.x;
    let top = bounds.origin.y;
    let right = left + bounds.size.width;
    let bottom = top + bounds.size.height;
    let mut point = LogicalPoint::new(point.x.clamp(left, right), point.y.clamp(top, bottom));
    if !bypass {
        if (point.x - left).abs() <= tolerance {
            point.x = left;
        } else if (right - point.x).abs() <= tolerance {
            point.x = right;
        }
        if (point.y - top).abs() <= tolerance {
            point.y = top;
        } else if (bottom - point.y).abs() <= tolerance {
            point.y = bottom;
        }
    }
    point
}

fn constrain_crop_point(
    start: LogicalPoint,
    pointer: LogicalPoint,
    bounds: LogicalRect,
    ratio: Option<f64>,
) -> LogicalPoint {
    let Some(ratio) = ratio.filter(|ratio| ratio.is_finite() && *ratio > 0.0) else {
        return pointer;
    };
    let x_direction: f64 = if pointer.x < start.x { -1.0 } else { 1.0 };
    let y_direction: f64 = if pointer.y < start.y { -1.0 } else { 1.0 };
    let mut width = (pointer.x - start.x).abs();
    let mut height = (pointer.y - start.y).abs();
    if height <= f64::EPSILON || width / height > ratio {
        height = width / ratio;
    } else {
        width = height * ratio;
    }

    let max_width = if x_direction < 0.0 {
        start.x - bounds.origin.x
    } else {
        bounds.origin.x + bounds.size.width - start.x
    };
    let max_height = if y_direction < 0.0 {
        start.y - bounds.origin.y
    } else {
        bounds.origin.y + bounds.size.height - start.y
    };
    let scale = (max_width / width.max(f64::EPSILON))
        .min(max_height / height.max(f64::EPSILON))
        .min(1.0);
    LogicalPoint::new(
        x_direction.mul_add(width * scale, start.x),
        y_direction.mul_add(height * scale, start.y),
    )
}

fn fit_ratio_within(rect: LogicalRect, ratio: f64) -> LogicalRect {
    let current = rect.size.width / rect.size.height.max(f64::EPSILON);
    let size = if current > ratio {
        LogicalSize::new(rect.size.height * ratio, rect.size.height)
    } else {
        LogicalSize::new(rect.size.width, rect.size.width / ratio)
    };
    LogicalRect::new(
        LogicalPoint::new(
            rect.origin.x + (rect.size.width - size.width) / 2.0,
            rect.origin.y + (rect.size.height - size.height) / 2.0,
        ),
        size,
    )
}

fn source_handle_points(rect: LogicalRect) -> [(ResizeHandle, LogicalPoint); 4] {
    let right = rect.origin.x + rect.size.width;
    let bottom = rect.origin.y + rect.size.height;
    [
        (ResizeHandle::NorthWest, rect.origin),
        (
            ResizeHandle::NorthEast,
            LogicalPoint::new(right, rect.origin.y),
        ),
        (ResizeHandle::SouthEast, LogicalPoint::new(right, bottom)),
        (
            ResizeHandle::SouthWest,
            LogicalPoint::new(rect.origin.x, bottom),
        ),
    ]
}

fn source_corners(rect: LogicalRect) -> [LogicalPoint; 4] {
    source_handle_points(rect).map(|(_, point)| point)
}

fn resized_rect(original: LogicalRect, handle: ResizeHandle, pointer: LogicalPoint) -> LogicalRect {
    let left = original.origin.x;
    let top = original.origin.y;
    let right = left + original.size.width;
    let bottom = top + original.size.height;
    match handle {
        ResizeHandle::NorthWest => LogicalRect::from_corners(
            LogicalPoint::new(
                pointer.x.min(right - MIN_SHAPE_SIZE),
                pointer.y.min(bottom - MIN_SHAPE_SIZE),
            ),
            LogicalPoint::new(right, bottom),
        ),
        ResizeHandle::NorthEast => LogicalRect::from_corners(
            LogicalPoint::new(left, bottom),
            LogicalPoint::new(
                pointer.x.max(left + MIN_SHAPE_SIZE),
                pointer.y.min(bottom - MIN_SHAPE_SIZE),
            ),
        ),
        ResizeHandle::SouthEast => LogicalRect::from_corners(
            LogicalPoint::new(left, top),
            LogicalPoint::new(
                pointer.x.max(left + MIN_SHAPE_SIZE),
                pointer.y.max(top + MIN_SHAPE_SIZE),
            ),
        ),
        ResizeHandle::SouthWest => LogicalRect::from_corners(
            LogicalPoint::new(right, top),
            LogicalPoint::new(
                pointer.x.min(right - MIN_SHAPE_SIZE),
                pointer.y.max(top + MIN_SHAPE_SIZE),
            ),
        ),
        ResizeHandle::Start | ResizeHandle::End | ResizeHandle::Control => original,
    }
}

fn set_point_annotation_endpoint(
    document: &mut Document,
    id: AnnotationId,
    handle: ResizeHandle,
    point: LogicalPoint,
) -> bool {
    let Some(mut object) = document.get_mut(id) else {
        return false;
    };
    let endpoint = match (object.annotation(), handle) {
        (Annotation::Arrow { from, .. } | Annotation::Line { from, .. }, ResizeHandle::Start) => {
            from
        }
        (Annotation::Arrow { to, .. } | Annotation::Line { to, .. }, ResizeHandle::End) => to,
        _ => return false,
    };
    if *endpoint == point {
        return false;
    }
    *endpoint = point;
    true
}

fn translated_rect(rect: LogicalRect, dx: f64, dy: f64) -> LogicalRect {
    LogicalRect::new(
        LogicalPoint::new(rect.origin.x + dx, rect.origin.y + dy),
        rect.size,
    )
}

fn rotation_label(rotation: CanvasRotation) -> &'static str {
    match rotation {
        CanvasRotation::None => "0°",
        CanvasRotation::Clockwise90 => "90°",
        CanvasRotation::HalfTurn => "180°",
        CanvasRotation::CounterClockwise90 => "270°",
    }
}

fn default_working_styles() -> [Style; EditorTool::ALL.len()] {
    let mut styles = [Style::stroked(); EditorTool::ALL.len()];
    styles[EditorTool::Highlight.index()] = Style::highlighter();
    styles[EditorTool::Spotlight.index()] = Style::spotlight();
    styles[EditorTool::Blur.index()] = Style::redaction();
    styles[EditorTool::Pixelate.index()] = Style::redaction();
    styles[EditorTool::SolidRedact.index()] = Style::redaction();
    styles
}

/// Real annotation-editor scene used by the deterministic screenshot harness.
pub struct EditorScene {
    mode: EditorMode,
}

struct EditorSceneState {
    editor: AnnotationEditor,
    icons: IconStore,
}

impl EditorScene {
    /// Compose-workspace scene.
    #[must_use]
    pub const fn composing() -> Self {
        Self {
            mode: EditorMode::Compose,
        }
    }

    /// Crop-workspace scene.
    #[must_use]
    pub const fn cropping() -> Self {
        Self {
            mode: EditorMode::Crop,
        }
    }

    /// Beautification-workspace scene.
    #[must_use]
    pub const fn beautifying() -> Self {
        Self {
            mode: EditorMode::Beautify,
        }
    }

    fn state_id(&self) -> Id {
        Id::new(("annotation-editor-scene", self.mode.label()))
    }
}

impl Scene for EditorScene {
    fn name(&self) -> &str {
        match self.mode {
            EditorMode::Compose => "annotation-editor-compose",
            EditorMode::Crop => "annotation-editor-crop",
            EditorMode::Beautify => "annotation-editor-beautify",
        }
    }

    fn setup(&self, ctx: &egui::Context) {
        theme::install_fonts(ctx);
        let state = Arc::new(Mutex::new(EditorSceneState {
            editor: demo_editor(self.mode),
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

fn demo_editor(mode: EditorMode) -> AnnotationEditor {
    let mut document = Document::new(demo_capture());
    if mode == EditorMode::Crop {
        let _ = document.set_canvas(Canvas {
            crop: Some(LogicalRect::new(
                LogicalPoint::new(54.0, 32.0),
                LogicalSize::new(532.0, 314.0),
            )),
            auto_expand: true,
            ..Canvas::default()
        });
    } else {
        document
            .add(
                Annotation::Highlight(LogicalRect::new(
                    LogicalPoint::new(176.0, 151.0),
                    LogicalSize::new(222.0, 22.0),
                )),
                Style::highlighter(),
            )
            .expect("demo annotation id is available");
        document
            .add(
                Annotation::Rectangle(LogicalRect::new(
                    LogicalPoint::new(456.0, 268.0),
                    LogicalSize::new(112.0, 48.0),
                )),
                Style::stroked()
                    .with_stroke(Color::rgb(255, 74, 85))
                    .with_stroke_width(4.5),
            )
            .expect("demo annotation id is available");
        document
            .add(
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
            )
            .expect("demo annotation id is available");
        document
            .add(
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
            )
            .expect("demo annotation id is available");
        document
            .add(
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
            )
            .expect("demo annotation id is available");
        if mode == EditorMode::Beautify {
            document
                .set_beautification(Some(Beautification::preset(BeautificationPreset::Social)))
                .expect("the demo capture accepts beautification");
        }
    }
    let mut editor = AnnotationEditor::new(document);
    if mode == EditorMode::Compose {
        editor.selected = editor
            .document
            .annotations()
            .iter()
            .find(|object| object.kind() == scrozz_annotate::AnnotationKind::Arrow)
            .map(|object| object.id);
        editor.tool = EditorTool::Select;
    } else {
        editor.set_mode(mode);
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
        let editor = demo_editor(EditorMode::Crop);
        let source = demo_capture();
        assert_eq!(editor.document.source.frame.data, source.frame.data);
        assert!(editor.document.canvas().crop.is_some());
    }

    #[test]
    fn crop_workspace_keeps_the_full_source_available() {
        let editor = demo_editor(EditorMode::Crop);
        assert_eq!(
            editor.document.canvas_size(),
            LogicalSize::new(532.0, 314.0)
        );
        assert_eq!(
            editor.workspace_geometry().output_size(),
            editor.document.logical_size()
        );
    }

    #[test]
    fn transformed_resize_handles_keep_their_source_corner_identity() {
        let mut editor = demo_editor(EditorMode::Compose);
        editor
            .document
            .set_canvas(Canvas {
                rotation: CanvasRotation::Clockwise90,
                ..Canvas::default()
            })
            .unwrap();
        let bounds = LogicalRect::new(LogicalPoint::new(40.0, 30.0), LogicalSize::new(120.0, 70.0));
        let geometry = SkiaRenderer::new()
            .render_at_with_geometry(&editor.document, editor.document.source.frame.scale)
            .unwrap()
            .geometry;
        let size = geometry.output_size();
        let placement = Placement {
            rect: Rect::from_min_size(Pos2::ZERO, vec2(size.width as f32, size.height as f32)),
            scale: 1.0,
            geometry,
        };
        let object = scrozz_annotate::AnnotationObject {
            id: AnnotationId(99),
            annotation: Annotation::Rectangle(bounds),
            style: Style::default(),
        };
        let handles = editor.selection_handle_points(&object, placement);
        let (visual_north_west, _) = handles
            .into_iter()
            .map(|pair @ (_, point)| (pair, point.x + point.y))
            .min_by(|(_, left), (_, right)| left.total_cmp(right))
            .expect("four handles");

        assert!(
            matches!(visual_north_west.0, ResizeHandle::SouthWest),
            "a clockwise rotation moves the source south-west corner to visual north-west"
        );
        let moved_canvas = LogicalPoint::new(
            f64::from(visual_north_west.1.x + 12.0),
            f64::from(visual_north_west.1.y + 8.0),
        );
        let moved_source =
            geometry.output_to_source(PhysicalPoint::new(moved_canvas.x, moved_canvas.y));
        let resized = resized_rect(bounds, visual_north_west.0, moved_source);

        assert_eq!(
            resized.origin.y, bounds.origin.y,
            "the opposite source north-east anchor keeps its top edge"
        );
        assert_eq!(
            resized.origin.x + resized.size.width,
            bounds.origin.x + bounds.size.width,
            "the opposite source north-east anchor keeps its right edge"
        );
    }

    #[test]
    fn resize_handles_never_cross_or_move_the_opposite_source_anchor() {
        let original =
            LogicalRect::new(LogicalPoint::new(20.0, 30.0), LogicalSize::new(80.0, 50.0));
        let far_past = [
            (ResizeHandle::NorthWest, LogicalPoint::new(140.0, 110.0)),
            (ResizeHandle::NorthEast, LogicalPoint::new(-20.0, 110.0)),
            (ResizeHandle::SouthEast, LogicalPoint::new(-20.0, -10.0)),
            (ResizeHandle::SouthWest, LogicalPoint::new(140.0, -10.0)),
        ];

        for (handle, pointer) in far_past {
            let resized = resized_rect(original, handle, pointer);
            assert!(resized.size.width >= MIN_SHAPE_SIZE);
            assert!(resized.size.height >= MIN_SHAPE_SIZE);
            let points = source_handle_points(resized);
            let opposite = match handle {
                ResizeHandle::NorthWest => ResizeHandle::SouthEast,
                ResizeHandle::NorthEast => ResizeHandle::SouthWest,
                ResizeHandle::SouthEast => ResizeHandle::NorthWest,
                ResizeHandle::SouthWest => ResizeHandle::NorthEast,
                ResizeHandle::Start | ResizeHandle::End | ResizeHandle::Control => {
                    unreachable!("corner test")
                }
            };
            let anchored = points
                .into_iter()
                .find_map(|(candidate, point)| (candidate == opposite).then_some(point))
                .expect("every corner has an opposite");
            let expected = source_handle_points(original)
                .into_iter()
                .find_map(|(candidate, point)| (candidate == opposite).then_some(point))
                .expect("original corner exists");
            assert_eq!(anchored, expected, "{handle:?} moved its opposite anchor");
        }
    }

    #[test]
    fn horizontal_and_vertical_point_resizes_keep_the_opposite_endpoint_anchored() {
        for annotation in [
            Annotation::Line {
                from: LogicalPoint::new(20.0, 40.0),
                to: LogicalPoint::new(100.0, 40.0),
            },
            Annotation::Arrow {
                from: LogicalPoint::new(60.0, 15.0),
                to: LogicalPoint::new(60.0, 95.0),
            },
        ] {
            let mut editor = demo_editor(EditorMode::Compose);
            let id = editor
                .document
                .add(annotation.clone(), Style::default())
                .unwrap();
            let original = editor.document.get(id).unwrap().annotation.clone();
            let moved = LogicalPoint::new(170.0, 130.0);
            editor.gesture = Some(Gesture::Resize {
                id,
                handle: ResizeHandle::Start,
                original: editor.document.get(id).unwrap().bounds(),
                current: moved,
            });
            editor.finish_gesture();

            match (&original, &editor.document.get(id).unwrap().annotation) {
                (Annotation::Line { to: before, .. }, Annotation::Line { from, to: after })
                | (Annotation::Arrow { to: before, .. }, Annotation::Arrow { from, to: after }) => {
                    assert_eq!(*from, moved);
                    assert_eq!(after, before, "the opposite endpoint moved");
                }
                _ => unreachable!("annotation kind must be preserved"),
            }
        }
    }

    #[test]
    fn transform_gestures_defer_document_rerender_until_release() {
        let mut editor = demo_editor(EditorMode::Compose);
        let id = editor.document.annotations()[0].id;
        let original = editor.document.get(id).unwrap().bounds();
        let before = editor.document.data();
        editor.gesture = Some(Gesture::Move {
            id,
            original,
            start: LogicalPoint::new(10.0, 10.0),
            current: LogicalPoint::new(55.0, 35.0),
        });

        assert_eq!(
            editor.document.data(),
            before,
            "pointer frames use an overlay and must not invalidate the raster cache"
        );
        assert_ne!(editor.gesture_bounds(id, original), original);

        editor.finish_gesture();
        assert_eq!(
            editor.document.get(id).unwrap().bounds().origin,
            LogicalPoint::new(original.origin.x + 45.0, original.origin.y + 25.0)
        );
    }

    #[test]
    fn spotlight_and_each_redaction_tool_retain_independent_working_styles() {
        let mut editor = demo_editor(EditorMode::Compose);
        editor.working_styles[EditorTool::Spotlight.index()].opacity = 0.31;
        editor.working_styles[EditorTool::Blur.index()].redact_strength = 0.22;
        editor.working_styles[EditorTool::Pixelate.index()].redact_strength = 0.84;
        editor.working_styles[EditorTool::SolidRedact.index()].stroke = Color::WHITE;

        editor.set_tool(EditorTool::Spotlight);
        assert_eq!(editor.default_style_for_tool().opacity, 0.31);
        editor.set_tool(EditorTool::Blur);
        assert_eq!(editor.default_style_for_tool().redact_strength, 0.22);
        editor.set_tool(EditorTool::Pixelate);
        assert_eq!(editor.default_style_for_tool().redact_strength, 0.84);
        editor.set_tool(EditorTool::SolidRedact);
        assert_eq!(editor.default_style_for_tool().stroke, Color::WHITE);
    }

    #[test]
    fn focused_text_edit_receives_delete_before_object_deletion() {
        let mut editor = demo_editor(EditorMode::Compose);
        let selected = editor.selected.expect("demo selects an arrow");
        let ctx = egui::Context::default();
        let mut text = "editable".to_owned();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let response =
                ui.add(egui::TextEdit::singleline(&mut text).id_salt("focused-annotation-text"));
            response.request_focus();
        });
        output.textures_delta.clear();
        assert!(ctx.text_edit_focused());

        let input = egui::RawInput {
            focused: true,
            events: vec![egui::Event::Key {
                key: egui::Key::Delete,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| editor.handle_shortcuts(ui));
        output.textures_delta.clear();

        assert!(
            editor.document.get(selected).is_some(),
            "Delete belongs to the focused text editor, not the selected canvas object"
        );
    }

    #[test]
    fn keyboard_move_resize_and_selection_are_document_edits() {
        let mut editor = demo_editor(EditorMode::Compose);
        let selected = editor.selected.expect("demo selects an arrow");
        let before = editor.document.get(selected).unwrap().bounds();

        editor.keyboard_adjust_selected(10.0, -4.0, false);
        let moved = editor.document.get(selected).unwrap().bounds();
        assert_eq!(moved.origin.x, before.origin.x + 10.0);
        assert_eq!(moved.origin.y, before.origin.y - 4.0);

        editor.keyboard_adjust_selected(6.0, 3.0, true);
        let resized = editor.document.get(selected).unwrap().bounds();
        assert_eq!(resized.origin, moved.origin);
        assert_eq!(resized.size.width, moved.size.width + 6.0);
        assert_eq!(resized.size.height, moved.size.height + 3.0);

        editor.cycle_selection(true);
        assert_ne!(editor.selected, Some(selected));
        assert!(editor.history.can_undo());
        assert!(
            editor
                .events
                .iter()
                .any(|event| matches!(event, EditorEvent::Changed(_)))
        );
    }

    #[test]
    fn layer_labels_expose_order_kind_and_text() {
        let editor = demo_editor(EditorMode::Compose);
        let text = editor
            .document
            .annotations()
            .iter()
            .find(|object| matches!(object.annotation, Annotation::Text { .. }))
            .expect("demo has text");
        let label = layer_label(text, 2, editor.document.len());
        assert_eq!(label, "2 of 5, Text: Share this view");
    }

    #[test]
    fn crop_snapping_has_a_command_bypass_and_ratio_constraint() {
        let bounds = LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(200.0, 120.0));
        let near_edge = LogicalPoint::new(7.0, 114.0);
        assert_eq!(
            snap_crop_point(near_edge, bounds, 10.0, false),
            LogicalPoint::new(0.0, 120.0)
        );
        assert_eq!(snap_crop_point(near_edge, bounds, 10.0, true), near_edge);

        let start = LogicalPoint::new(20.0, 20.0);
        let constrained =
            constrain_crop_point(start, LogicalPoint::new(150.0, 100.0), bounds, Some(1.0));
        assert!(((constrained.x - start.x) - (constrained.y - start.y)).abs() < 1e-9);
        assert!(constrained.x <= 200.0 && constrained.y <= 120.0);
    }

    #[test]
    fn option_inverts_new_arrows_and_p_selects_redact() {
        let mut editor = demo_editor(EditorMode::Compose);
        editor.set_tool(EditorTool::Arrow);
        let start = LogicalPoint::new(10.0, 20.0);
        let end = LogicalPoint::new(80.0, 90.0);
        let arrow = editor
            .annotation_for_gesture(start, end, vec![start], true)
            .expect("arrow");
        assert_eq!(
            arrow,
            Annotation::Arrow {
                from: end,
                to: start
            }
        );

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            focused: true,
            events: vec![egui::Event::Key {
                key: egui::Key::P,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| editor.handle_shortcuts(ui));
        output.textures_delta.clear();
        assert_eq!(editor.tool(), EditorTool::SolidRedact);
    }

    #[test]
    fn curved_arrow_selection_exposes_an_editable_control_handle() {
        let mut editor = demo_editor(EditorMode::Compose);
        let id = editor.selected.expect("demo selects curved arrow");
        let object = editor.document.get(id).unwrap().clone();
        let geometry = SkiaRenderer::new()
            .render_at_with_geometry(&editor.document, ScaleFactor::IDENTITY)
            .unwrap()
            .geometry;
        let placement = Placement {
            rect: Rect::from_min_size(
                Pos2::ZERO,
                vec2(geometry.output_width as f32, geometry.output_height as f32),
            ),
            scale: 1.0,
            geometry,
        };
        assert!(
            editor
                .selection_handle_points(&object, placement)
                .iter()
                .any(|(handle, _)| *handle == ResizeHandle::Control)
        );

        let control = LogicalPoint::new(470.0, 170.0);
        assert!(set_curve_control(&mut editor.document, id, control));
        assert_eq!(
            editor.document.get(id).unwrap().style.curve_control,
            Some(control)
        );
    }

    #[test]
    fn color_name_preference_is_user_controlled() {
        let mut editor = demo_editor(EditorMode::Compose);
        assert!(editor.show_color_names());
        editor.set_show_color_names(false);
        assert!(!editor.show_color_names());
    }
}
