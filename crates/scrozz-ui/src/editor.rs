//! The non-destructive annotation and beautification workspace.
//!
//! The main overlay owns the capture cards, while this module owns ordinary
//! child viewports for longer-lived editing. Communication is intentionally a
//! small handle/event seam so the UI crate never needs to know about SQLite,
//! the clipboard, or a user's save directory.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};

use egui::{
    Align, Color32, ColorImage, Context, CornerRadius, CursorIcon, FontId, Frame as EguiFrame, Id,
    Layout, Margin, Rect, Response, RichText, ScrollArea, Sense, Stroke, StrokeKind, TextureHandle,
    TextureOptions, Vec2, ViewportBuilder, ViewportId, pos2, vec2,
};
use scrozz_annotate::{
    Alignment, AspectPreset, Background, BackgroundImage, Beautification, BeautificationPreset,
    BuiltInBackground, Document, DocumentData, SkiaRenderer,
};
use scrozz_core::{ColorSpace, Error, Result};
use scrozz_export::{convert_to_srgb, to_straight_rgba8};

use crate::theme::{Appearance, Theme, install_fonts, install_style};

const PREVIEW_WIDTH: u32 = 900;
const AUTOSAVE_DELAY_SECONDS: f64 = 0.28;

/// Where an editor export should go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorDestination {
    /// Put a destination-compatible representation on the system clipboard.
    Clipboard,
    /// Write a destination-compatible file to the configured/default folder.
    DefaultFolder,
}

/// Work requested by an editor viewport.
#[derive(Debug, Clone)]
pub enum EditorEvent {
    /// Persist only the mutable document data.
    Persist {
        /// Stable card/editor identifier.
        id: u64,
        /// Non-destructive annotations and framing.
        data: DocumentData,
    },
    /// Render and deliver a snapshot of the document.
    Export {
        /// Stable card/editor identifier.
        id: u64,
        /// Destination requested by the person editing.
        destination: EditorDestination,
        /// Compact edit snapshot; the worker restores immutable source pixels.
        data: DocumentData,
    },
    /// The viewport closed.
    Closed {
        /// Stable card/editor identifier.
        id: u64,
    },
}

/// A document to show in the editor.
#[derive(Debug, Clone)]
pub struct EditorRequest {
    /// Stable card/editor identifier.
    pub id: u64,
    /// Human-readable title shown in the viewport.
    pub title: String,
    /// Capture plus its restored edits.
    pub document: Document,
}

impl EditorRequest {
    /// Creates an editor request.
    #[must_use]
    pub fn new(id: u64, title: impl Into<String>, document: Document) -> Self {
        Self {
            id,
            title: title.into(),
            document,
        }
    }
}

/// Status sent back by persistence and export workers.
#[derive(Debug, Clone)]
pub enum EditorStatus {
    /// An operation completed.
    Complete(String),
    /// An operation failed and needs to be visible in the workspace.
    Failed(String),
}

#[derive(Debug)]
enum EditorCommand {
    Open(Box<EditorRequest>),
    Focus(u64),
    Status {
        id: u64,
        status: EditorStatus,
        finishes_export: bool,
    },
    PersistStatus {
        id: u64,
        status: EditorStatus,
    },
}

#[derive(Default)]
struct Shared {
    commands: VecDeque<EditorCommand>,
    events: VecDeque<EditorEvent>,
    context: Option<Context>,
}

/// Thread-safe bridge between the host application and editor viewports.
#[derive(Clone, Default)]
pub struct EditorHandle {
    shared: Arc<Mutex<Shared>>,
}

impl EditorHandle {
    /// Queues a document for editing, focusing its existing viewport when open.
    pub fn open(&self, request: EditorRequest) {
        let context = {
            let mut shared = self.shared.lock().expect("editor mutex poisoned");
            shared
                .commands
                .push_back(EditorCommand::Open(Box::new(request)));
            shared.context.clone()
        };
        if let Some(context) = context {
            context.request_repaint();
        }
    }

    /// Focuses an editor that is already open without restoring stale metadata.
    pub fn focus(&self, id: u64) {
        let context = {
            let mut shared = self.shared.lock().expect("editor mutex poisoned");
            shared.commands.push_back(EditorCommand::Focus(id));
            shared.context.clone()
        };
        if let Some(context) = context {
            context.request_repaint();
        }
    }

    /// Delivers a worker result to an open editor.
    pub fn status(&self, id: u64, status: EditorStatus) {
        let context = {
            let mut shared = self.shared.lock().expect("editor mutex poisoned");
            shared.commands.push_back(EditorCommand::Status {
                id,
                status,
                finishes_export: false,
            });
            shared.context.clone()
        };
        if let Some(context) = context {
            context.request_repaint();
        }
    }

    /// Delivers export feedback and re-enables destination actions.
    pub fn export_status(&self, id: u64, status: EditorStatus) {
        let context = {
            let mut shared = self.shared.lock().expect("editor mutex poisoned");
            shared.commands.push_back(EditorCommand::Status {
                id,
                status,
                finishes_export: true,
            });
            shared.context.clone()
        };
        if let Some(context) = context {
            context.request_repaint();
        }
    }

    /// Delivers autosave feedback without masking a terminal export result.
    pub fn persist_status(&self, id: u64, status: EditorStatus) {
        let context = {
            let mut shared = self.shared.lock().expect("editor mutex poisoned");
            shared
                .commands
                .push_back(EditorCommand::PersistStatus { id, status });
            shared.context.clone()
        };
        if let Some(context) = context {
            context.request_repaint();
        }
    }

    /// Returns all editor work currently waiting for the application.
    #[must_use]
    pub fn drain_events(&self) -> Vec<EditorEvent> {
        let mut shared = self.shared.lock().expect("editor mutex poisoned");
        shared.events.drain(..).collect()
    }

    fn bind_context(&self, context: &Context) {
        self.shared.lock().expect("editor mutex poisoned").context = Some(context.clone());
    }

    fn drain_commands(&self) -> Vec<EditorCommand> {
        let mut shared = self.shared.lock().expect("editor mutex poisoned");
        shared.commands.drain(..).collect()
    }

    fn emit(&self, event: EditorEvent) {
        self.shared
            .lock()
            .expect("editor mutex poisoned")
            .events
            .push_back(event);
    }
}

/// Owns all open editor viewports.
pub struct EditorWorkspace {
    handle: EditorHandle,
    panels: BTreeMap<u64, EditorPanel>,
    theme: Theme,
}

impl EditorWorkspace {
    /// Installs editor styling and binds a shared handle to this egui context.
    #[must_use]
    pub fn new(context: &Context, handle: EditorHandle, appearance: Appearance) -> Self {
        install_fonts(context);
        let theme = Theme::for_appearance(appearance);
        install_style(context, &theme);
        handle.bind_context(context);
        Self {
            handle,
            panels: BTreeMap::new(),
            theme,
        }
    }

    /// Draws every open editor as an ordinary child viewport.
    pub fn ui(&mut self, context: &Context) {
        self.apply_commands(context);

        let ids: Vec<u64> = self.panels.keys().copied().collect();
        for id in ids {
            let Some(panel) = self.panels.get_mut(&id) else {
                continue;
            };
            let viewport_id = ViewportId::from_hash_of(("scrozz.editor", id));
            let builder = ViewportBuilder::default()
                .with_title(format!("{} - Scrozz", panel.title))
                .with_inner_size([1180.0, 760.0])
                .with_min_inner_size([860.0, 580.0]);
            let mut close = false;
            let mut events = Vec::new();
            let theme = self.theme;

            context.show_viewport_immediate(viewport_id, builder, |ui, _class| {
                close = ui.input(|input| input.viewport().close_requested());
                events.extend(panel.ui(ui, &theme));
            });

            for event in events {
                self.handle.emit(event);
            }
            if close {
                if let Some(event) = panel.flush_persist() {
                    self.handle.emit(event);
                }
                self.handle.emit(EditorEvent::Closed { id });
                self.panels.remove(&id);
            }
        }
    }

    /// Emits every debounced metadata write before the host stops its worker.
    pub fn flush_all(&mut self) {
        for panel in self.panels.values_mut() {
            if let Some(event) = panel.flush_persist() {
                self.handle.emit(event);
            }
        }
    }

    fn apply_commands(&mut self, context: &Context) {
        for command in self.handle.drain_commands() {
            match command {
                EditorCommand::Open(request) => {
                    let request = *request;
                    context.send_viewport_cmd_to(
                        ViewportId::from_hash_of(("scrozz.editor", request.id)),
                        egui::ViewportCommand::Focus,
                    );
                    self.panels
                        .entry(request.id)
                        .or_insert_with(|| EditorPanel::new(request));
                }
                EditorCommand::Focus(id) => {
                    if self.panels.contains_key(&id) {
                        context.send_viewport_cmd_to(
                            ViewportId::from_hash_of(("scrozz.editor", id)),
                            egui::ViewportCommand::Focus,
                        );
                    }
                }
                EditorCommand::Status {
                    id,
                    status,
                    finishes_export,
                } => {
                    if let Some(panel) = self.panels.get_mut(&id) {
                        if finishes_export {
                            panel.export_pending = false;
                            panel.export_notice = true;
                            panel.status = status;
                        } else if matches!(status, EditorStatus::Failed(_))
                            || (!panel.export_pending && !panel.export_notice)
                        {
                            panel.status = status;
                        }
                    }
                }
                EditorCommand::PersistStatus { id, status } => {
                    if let Some(panel) = self.panels.get_mut(&id) {
                        match status {
                            EditorStatus::Complete(message) => {
                                panel.persist_error = None;
                                if !panel.export_pending && !panel.export_notice {
                                    panel.status = EditorStatus::Complete(message);
                                }
                            }
                            EditorStatus::Failed(message) => {
                                panel.persist_error = Some(message);
                            }
                        }
                    }
                }
            }
        }
    }
}

struct EditorPanel {
    id: u64,
    title: String,
    document: Document,
    preview: Option<TextureHandle>,
    preview_size: [usize; 2],
    preview_dirty: bool,
    preview_error: Option<String>,
    status: EditorStatus,
    export_pending: bool,
    export_notice: bool,
    persist_error: Option<String>,
    persist_pending: bool,
    changed_at: f64,
}

impl EditorPanel {
    fn new(request: EditorRequest) -> Self {
        Self {
            id: request.id,
            title: request.title,
            document: request.document,
            preview: None,
            preview_size: [1, 1],
            preview_dirty: true,
            preview_error: None,
            status: EditorStatus::Complete("Changes save automatically".to_owned()),
            export_pending: false,
            export_notice: false,
            persist_error: None,
            persist_pending: false,
            changed_at: 0.0,
        }
    }

    fn ui(&mut self, root: &mut egui::Ui, theme: &Theme) -> Vec<EditorEvent> {
        let context = root.ctx().clone();
        if self.preview_dirty {
            self.update_preview(&context);
        }

        let mut events = Vec::new();
        egui::Panel::top(Id::new(("editor.header", self.id)))
            .exact_size(72.0)
            .show_separator_line(false)
            .frame(
                EguiFrame::new()
                    .fill(theme.palette.card_fill)
                    .inner_margin(Margin::symmetric(24, 12))
                    .stroke(Stroke::new(1.0, theme.palette.hairline)),
            )
            .show(root, |ui| self.header(ui, theme));

        egui::Panel::bottom(Id::new(("editor.actions", self.id)))
            .exact_size(72.0)
            .show_separator_line(false)
            .frame(
                EguiFrame::new()
                    .fill(theme.palette.card_fill)
                    .inner_margin(Margin::symmetric(24, 14))
                    .stroke(Stroke::new(1.0, theme.palette.hairline)),
            )
            .show(root, |ui| {
                events.extend(self.actions(ui, theme));
            });

        egui::Panel::right(Id::new(("editor.controls", self.id)))
            .exact_size(344.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                EguiFrame::new()
                    .fill(theme.palette.card_fill_raised)
                    .inner_margin(Margin::same(20)),
            )
            .show(root, |ui| {
                self.controls(ui, theme);
            });

        egui::CentralPanel::default()
            .frame(EguiFrame::new().fill(theme.palette.canvas()))
            .show(root, |ui| self.preview(ui, theme));

        let now = root.input(|input| input.time);
        if self.persist_pending
            && now - self.changed_at >= AUTOSAVE_DELAY_SECONDS
            && let Some(event) = self.flush_persist()
        {
            events.push(event);
        }
        context.request_repaint_after_secs(0.1);
        events
    }

    fn header(&self, ui: &mut egui::Ui, theme: &Theme) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("FRAMING BENCH")
                        .font(FontId::monospace(10.0))
                        .color(theme.palette.accent)
                        .strong(),
                );
                ui.add_space(3.0);
                ui.label(
                    RichText::new(&self.title)
                        .font(FontId::proportional(20.0))
                        .color(theme.palette.text)
                        .strong(),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let source = &self.document.source.frame;
                chip(
                    ui,
                    &format!(
                        "{} x {}  /  {}x",
                        source.width(),
                        source.height(),
                        trim_scale(source.scale.get())
                    ),
                    theme,
                );
                ui.add_space(8.0);
                chip(ui, "SOURCE UNTOUCHED", theme);
            });
        });
    }

    fn preview(&self, ui: &mut egui::Ui, theme: &Theme) {
        let available = ui.available_size();
        let stage = Rect::from_min_size(ui.min_rect().min, available);
        ui.painter().rect_filled(stage, 0.0, theme.palette.canvas());

        let inset = stage.shrink2(vec2(42.0, 34.0));
        let plate = inset.shrink2(vec2(8.0, 8.0));
        ui.painter().rect_filled(
            inset,
            CornerRadius::same(18),
            theme.palette.card_fill.gamma_multiply(0.75),
        );
        ui.painter().rect_stroke(
            inset,
            CornerRadius::same(18),
            Stroke::new(1.0, theme.palette.hairline),
            StrokeKind::Inside,
        );

        if let Some(error) = &self.preview_error {
            ui.painter().text(
                plate.center(),
                egui::Align2::CENTER_CENTER,
                error,
                FontId::proportional(14.0),
                danger(theme),
            );
            return;
        }
        let Some(texture) = &self.preview else {
            return;
        };

        let image_size = Vec2::new(self.preview_size[0] as f32, self.preview_size[1] as f32);
        let fit = (plate.width() / image_size.x)
            .min(plate.height() / image_size.y)
            .min(1.0);
        let painted_size = image_size * fit;
        let image_rect = Rect::from_center_size(plate.center(), painted_size);
        ui.painter().image(
            texture.id(),
            image_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    fn controls(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        if !self.document.may_beautify() {
            d9_refusal(ui, theme);
            return;
        }

        let mut config = self.document.beautification().cloned().unwrap_or_default();
        let before = config.clone();

        ScrollArea::vertical()
            .id_salt(("beautify.scroll", self.id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                section_label(ui, "STARTING POINT", theme);
                preset_row(ui, &mut config, theme);
                section_rule(ui, theme);

                section_label(ui, "BACKGROUND", theme);
                background_controls(ui, &mut config, theme, &mut self.status);
                section_rule(ui, theme);

                section_label(ui, "CANVAS", theme);
                measure_slider(ui, "Padding", &mut config.padding, 0.0..=220.0);
                ui.add_space(8.0);
                aspect_row(ui, &mut config.aspect);
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Placement")
                                .color(theme.palette.text)
                                .strong(),
                        );
                        ui.label(
                            RichText::new("Anchor the capture in extra canvas")
                                .small()
                                .color(theme.palette.text_muted),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        alignment_matrix(ui, &mut config.alignment, theme);
                    });
                });
                ui.add_space(10.0);
                ui.checkbox(&mut config.auto_balance, "Visually balance the subject")
                    .on_hover_text(
                        "Offsets the capture using deterministic visual salience, while retaining \
                         at least 35% of the requested padding.",
                    );
                section_rule(ui, theme);

                section_label(ui, "FINISH", theme);
                measure_slider(ui, "Corners", &mut config.corner_radius, 0.0..=80.0);
                measure_slider(ui, "Shadow", &mut config.shadow, 0.0..=80.0);
                measure_slider(ui, "Border", &mut config.border_width, 0.0..=12.0);
                if config.border_width > 0.0 {
                    color_control(ui, "Border colour", &mut config.border_color);
                }
                ui.add_space(12.0);
            });

        if beautification_changed(&before, &config) {
            let beautification = (config != Beautification::default()).then_some(config);
            match self.document.set_beautification(beautification) {
                Ok(()) => self.changed(ui.input(|input| input.time)),
                Err(error) => self.status = EditorStatus::Failed(error.to_string()),
            }
        }
    }

    fn actions(&mut self, ui: &mut egui::Ui, theme: &Theme) -> Vec<EditorEvent> {
        let mut events = Vec::new();
        ui.horizontal(|ui| {
            self.status_label(ui, theme);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let copy = action_button(ui, "Copy image", true, !self.export_pending, theme);
                if copy.clicked() {
                    if let Some(event) = self.flush_persist() {
                        events.push(event);
                    }
                    events.push(EditorEvent::Export {
                        id: self.id,
                        destination: EditorDestination::Clipboard,
                        data: self.document.data(),
                    });
                    self.export_pending = true;
                    self.export_notice = false;
                    self.status = EditorStatus::Complete("Copying image...".to_owned());
                }
                ui.add_space(8.0);
                let save =
                    action_button(ui, "Save to Pictures", false, !self.export_pending, theme);
                if save.clicked() {
                    if let Some(event) = self.flush_persist() {
                        events.push(event);
                    }
                    events.push(EditorEvent::Export {
                        id: self.id,
                        destination: EditorDestination::DefaultFolder,
                        data: self.document.data(),
                    });
                    self.export_pending = true;
                    self.export_notice = false;
                    self.status = EditorStatus::Complete("Saving image...".to_owned());
                }
            });
        });
        events
    }

    fn status_label(&self, ui: &mut egui::Ui, theme: &Theme) {
        let (message, failed) = match (&self.status, &self.persist_error) {
            (EditorStatus::Failed(activity), Some(persist)) => (
                format!("{activity} · Changes were not saved: {persist}"),
                true,
            ),
            (_, Some(persist)) => (format!("Changes were not saved: {persist}"), true),
            (EditorStatus::Complete(message), None) => (message.clone(), false),
            (EditorStatus::Failed(message), None) => (message.clone(), true),
        };
        let text = RichText::new(message).color(if failed {
            danger(theme)
        } else {
            theme.palette.text_muted
        });
        ui.label(if failed { text.strong() } else { text });
    }

    fn changed(&mut self, now: f64) {
        self.preview_dirty = true;
        self.persist_pending = true;
        self.changed_at = now;
        if !self.export_pending {
            self.export_notice = false;
        }
        self.status = EditorStatus::Complete("Saving changes...".to_owned());
    }

    fn flush_persist(&mut self) -> Option<EditorEvent> {
        if !self.persist_pending {
            return None;
        }
        self.persist_pending = false;
        Some(EditorEvent::Persist {
            id: self.id,
            data: self.document.data(),
        })
    }

    fn update_preview(&mut self, context: &Context) {
        self.preview_dirty = false;
        match SkiaRenderer.render_to_width(&self.document, PREVIEW_WIDTH) {
            Ok(frame) => {
                let straight = match to_straight_rgba8(&frame).and_then(|image| {
                    if matches!(
                        frame.color_space,
                        ColorSpace::DisplayP3 | ColorSpace::Rec2020
                    ) {
                        convert_to_srgb(&image, frame.color_space)
                    } else {
                        Ok(image)
                    }
                }) {
                    Ok(image) => image,
                    Err(error) => {
                        self.preview_error = Some(error.to_string());
                        return;
                    }
                };
                let size = [straight.width as usize, straight.height as usize];
                let image = ColorImage::from_rgba_unmultiplied(size, &straight.data);
                if let Some(texture) = &mut self.preview {
                    texture.set(image, TextureOptions::LINEAR);
                } else {
                    self.preview = Some(context.load_texture(
                        format!("scrozz.editor.preview.{}", self.id),
                        image,
                        TextureOptions::LINEAR,
                    ));
                }
                self.preview_size = size;
                self.preview_error = None;
            }
            Err(error) => {
                self.preview_error = Some(error.to_string());
            }
        }
    }
}

fn section_label(ui: &mut egui::Ui, label: &str, theme: &Theme) {
    ui.label(
        RichText::new(label)
            .font(FontId::monospace(10.0))
            .color(theme.palette.text_muted)
            .strong(),
    );
    ui.add_space(8.0);
}

fn section_rule(ui: &mut egui::Ui, theme: &Theme) {
    ui.add_space(16.0);
    ui.separator();
    ui.add_space(14.0);
    let _ = theme;
}

fn chip(ui: &mut egui::Ui, text: &str, theme: &Theme) {
    EguiFrame::new()
        .fill(theme.palette.card_fill_raised)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(9, 5))
        .stroke(Stroke::new(1.0, theme.palette.hairline))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .font(FontId::monospace(10.0))
                    .color(theme.palette.text_muted),
            );
        });
}

fn preset_row(ui: &mut egui::Ui, config: &mut Beautification, theme: &Theme) {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.horizontal_wrapped(|ui| {
            if pill_button(ui, "None", *config == Beautification::default(), theme).clicked() {
                *config = Beautification::default();
            }
            for (label, preset) in [
                ("Clean", BeautificationPreset::Clean),
                ("Social", BeautificationPreset::Social),
                ("Story", BeautificationPreset::Story),
                ("Editorial", BeautificationPreset::Editorial),
            ] {
                let selected = *config == Beautification::preset(preset);
                if pill_button(ui, label, selected, theme).clicked() {
                    *config = Beautification::preset(preset);
                }
            }
        });
    });
}

fn background_controls(
    ui: &mut egui::Ui,
    config: &mut Beautification,
    theme: &Theme,
    status: &mut EditorStatus,
) {
    ui.horizontal_wrapped(|ui| {
        for background in [
            BuiltInBackground::Mist,
            BuiltInBackground::Iris,
            BuiltInBackground::Midnight,
            BuiltInBackground::Sunrise,
            BuiltInBackground::Lagoon,
            BuiltInBackground::Sand,
        ] {
            let label = built_in_label(background);
            let selected = config.background == Background::BuiltIn(background);
            if swatch_button(ui, label, background, selected, theme).clicked() {
                config.background = Background::BuiltIn(background);
            }
        }
    });
    ui.add_space(10.0);
    ui.horizontal_wrapped(|ui| {
        if pill_button(
            ui,
            "Transparent",
            config.background == Background::Transparent,
            theme,
        )
        .clicked()
        {
            config.background = Background::Transparent;
        }
        if pill_button(
            ui,
            "Solid",
            matches!(config.background, Background::Solid(_)),
            theme,
        )
        .clicked()
        {
            config.background = Background::Solid(scrozz_annotate::Color::rgba(25, 29, 38, 255));
        }
    });
    if let Background::Solid(color) = &mut config.background {
        color_control(ui, "Fill colour", color);
    }
    ui.add_space(10.0);
    custom_background_drop(ui, config, theme, status);
}

fn custom_background_drop(
    ui: &mut egui::Ui,
    config: &mut Beautification,
    theme: &Theme,
    status: &mut EditorStatus,
) {
    let hovering = ui.input(|input| !input.raw.hovered_files.is_empty());
    let fill = if hovering {
        theme.palette.accent.gamma_multiply(0.14)
    } else {
        theme.palette.card_fill
    };
    let response = EguiFrame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 10))
        .stroke(Stroke::new(
            if hovering { 2.0 } else { 1.0 },
            if hovering {
                theme.palette.accent
            } else {
                theme.palette.hairline
            },
        ))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let marker = if matches!(config.background, Background::Image(_)) {
                    "CUSTOM"
                } else {
                    "DROP"
                };
                ui.label(
                    RichText::new(marker)
                        .font(FontId::monospace(10.0))
                        .color(theme.palette.accent)
                        .strong(),
                );
                ui.label(
                    RichText::new("Drop a PNG, JPEG, or WebP background")
                        .small()
                        .color(theme.palette.text_muted),
                );
            });
        })
        .response;
    response.on_hover_cursor(CursorIcon::Copy);

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
            *status = EditorStatus::Complete(format!(
                "Loaded custom background {}",
                file.path().file_name().map_or_else(
                    || "image".to_owned(),
                    |name| name.to_string_lossy().into_owned()
                )
            ));
        }
        Err(error) => *status = EditorStatus::Failed(error.to_string()),
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

fn swatch_button(
    ui: &mut egui::Ui,
    label: &str,
    background: BuiltInBackground,
    selected: bool,
    theme: &Theme,
) -> Response {
    let desired = vec2(92.0, 34.0);
    let (rect, response) = ui.allocate_exact_size(desired, Sense::click());
    let fill = if selected {
        theme.palette.accent.gamma_multiply(0.18)
    } else {
        theme.palette.card_fill
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(7),
        fill,
        Stroke::new(
            if selected { 2.0 } else { 1.0 },
            if selected {
                theme.palette.accent
            } else {
                theme.palette.hairline
            },
        ),
        StrokeKind::Inside,
    );
    ui.painter().circle_filled(
        pos2(rect.left() + 15.0, rect.center().y),
        5.0,
        built_in_swatch(background),
    );
    ui.painter().text(
        pos2(rect.left() + 27.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        theme.palette.text,
    );
    response
}

fn pill_button(ui: &mut egui::Ui, label: &str, selected: bool, theme: &Theme) -> Response {
    let button = egui::Button::new(RichText::new(label).size(12.0).color(if selected {
        theme.palette.accent
    } else {
        theme.palette.text
    }))
    .fill(if selected {
        theme.palette.accent.gamma_multiply(0.14)
    } else {
        theme.palette.card_fill
    })
    .stroke(Stroke::new(
        1.0,
        if selected {
            theme.palette.accent
        } else {
            theme.palette.hairline
        },
    ))
    .corner_radius(CornerRadius::same(7));
    ui.add(button)
}

fn aspect_row(ui: &mut egui::Ui, aspect: &mut AspectPreset) {
    egui::ComboBox::from_id_salt(("editor.aspect", ui.id()))
        .selected_text(aspect_label(*aspect))
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for candidate in [
                AspectPreset::Original,
                AspectPreset::Square,
                AspectPreset::Portrait,
                AspectPreset::Story,
                AspectPreset::Landscape,
                AspectPreset::Wide,
            ] {
                ui.selectable_value(aspect, candidate, aspect_label(candidate));
            }
        });
}

fn aspect_label(aspect: AspectPreset) -> &'static str {
    match aspect {
        AspectPreset::Original => "Original canvas",
        AspectPreset::Square => "Square post - 1:1",
        AspectPreset::Portrait => "Portrait post - 4:5",
        AspectPreset::Story => "Story / reel - 9:16",
        AspectPreset::Landscape => "Landscape - 16:9",
        AspectPreset::Wide => "Social header - 3:1",
    }
}

fn alignment_matrix(ui: &mut egui::Ui, alignment: &mut Alignment, theme: &Theme) {
    let size = 64.0;
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(7),
        theme.palette.card_fill,
        Stroke::new(1.0, theme.palette.hairline),
        StrokeKind::Inside,
    );
    let cells = [
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
    for (index, candidate) in cells.into_iter().enumerate() {
        let column = (index % 3) as f32;
        let row = (index / 3) as f32;
        let center = pos2(
            rect.left() + cell * (column + 0.5),
            rect.top() + cell * (row + 0.5),
        );
        let hit = Rect::from_center_size(center, vec2(cell, cell));
        let id = response.id.with(index);
        let cell_response = ui.interact(hit, id, Sense::click());
        if cell_response.clicked() {
            *alignment = candidate;
        }
        let selected = *alignment == candidate;
        ui.painter().circle_filled(
            center,
            if selected { 5.0 } else { 2.5 },
            if selected {
                theme.palette.accent
            } else {
                theme.palette.text_muted
            },
        );
    }
}

fn measure_slider(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f64,
    range: std::ops::RangeInclusive<f64>,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(
                egui::Slider::new(value, range)
                    .suffix(" pt")
                    .fixed_decimals(0)
                    .show_value(true),
            );
        });
    });
}

fn color_control(ui: &mut egui::Ui, label: &str, color: &mut scrozz_annotate::Color) {
    let mut picked = Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a);
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.color_edit_button_srgba(&mut picked).changed() {
                let [r, g, b, a] = picked.to_array();
                *color = scrozz_annotate::Color::rgba(r, g, b, a);
            }
        });
    });
}

fn action_button(
    ui: &mut egui::Ui,
    label: &str,
    primary: bool,
    enabled: bool,
    theme: &Theme,
) -> Response {
    ui.add_enabled_ui(enabled, |ui| {
        ui.add_sized(
            [142.0, 40.0],
            egui::Button::new(RichText::new(label).strong().color(if primary {
                theme.palette.on_accent
            } else {
                theme.palette.text
            }))
            .fill(if primary {
                theme.palette.accent
            } else {
                theme.palette.card_fill_raised
            })
            .stroke(Stroke::new(
                1.0,
                if primary {
                    theme.palette.accent
                } else {
                    theme.palette.hairline
                },
            ))
            .corner_radius(CornerRadius::same(8)),
        )
    })
    .inner
}

fn d9_refusal(ui: &mut egui::Ui, theme: &Theme) {
    ui.add_space(8.0);
    EguiFrame::new()
        .fill(danger(theme).gamma_multiply(0.10))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(16))
        .stroke(Stroke::new(1.0, danger(theme).gamma_multiply(0.7)))
        .show(ui, |ui| {
            ui.label(
                RichText::new("WINDOW SHAPE LOCKED")
                    .font(FontId::monospace(10.0))
                    .color(danger(theme))
                    .strong(),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("Beautification is unavailable for window captures.")
                    .color(theme.palette.text)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "The operating system already supplied the window's true corners and shadow. \
                     Adding another frame would make the capture subtly wrong (decision D9).",
                )
                .color(theme.palette.text_muted),
            );
        });
    ui.add_space(16.0);
    ui.label(
        RichText::new("The source stays unchanged and remains available for export.")
            .small()
            .color(theme.palette.text_muted),
    );
}

fn danger(theme: &Theme) -> Color32 {
    if theme.palette.is_dark() {
        Color32::from_rgb(0xFF, 0x7A, 0x70)
    } else {
        Color32::from_rgb(0xB4, 0x23, 0x18)
    }
}

fn trim_scale(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// Deterministic full editor used by the screenshot harness.
pub struct EditorPreviewScene {
    panel: Mutex<EditorPanel>,
}

impl EditorPreviewScene {
    /// Builds the annotated editor fixture.
    #[must_use]
    pub fn new() -> Self {
        Self {
            panel: Mutex::new(EditorPanel::new(EditorRequest::new(
                1,
                "Launch notes - region capture",
                editor_fixture_document(),
            ))),
        }
    }
}

impl Default for EditorPreviewScene {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::harness::Scene for EditorPreviewScene {
    fn name(&self) -> &str {
        "beautification-editor"
    }

    fn setup(&self, context: &Context) {
        install_fonts(context);
        let mut panel = self.panel.lock().expect("editor preview mutex poisoned");
        // A harness renderer creates a fresh egui context for each render. Never
        // retain a texture handle from the previous context: it can have the
        // same numeric id while referring to no texture in the new renderer.
        panel.preview = None;
        panel.preview_dirty = true;
        panel.update_preview(context);
    }

    fn ui(&self, ui: &mut egui::Ui, context: &crate::harness::SceneCtx<'_>) {
        let appearance = match context.theme {
            egui::Theme::Dark => Appearance::Dark,
            egui::Theme::Light => Appearance::Light,
        };
        let theme = Theme::for_appearance(appearance);
        let _events = self
            .panel
            .lock()
            .expect("editor preview mutex poisoned")
            .ui(ui, &theme);
    }
}

fn editor_fixture_document() -> Document {
    use scrozz_core::{
        Capture, CaptureTarget, Frame, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize,
        PixelFormat, Provenance, ScaleFactor,
    };

    const WIDTH: u32 = 960;
    const HEIGHT: u32 = 600;
    let mut pixels = Vec::with_capacity(WIDTH as usize * HEIGHT as usize * 4);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let u = x as f32 / WIDTH as f32;
            let v = y as f32 / HEIGHT as f32;
            let glow = (1.0 - ((u - 0.72).powi(2) + (v - 0.34).powi(2)).sqrt()).max(0.0);
            let panel = x > 108 && x < 852 && y > 84 && y < 516;
            let (r, g, b) = if panel {
                (
                    (26.0 + 33.0 * glow) as u8,
                    (31.0 + 43.0 * glow) as u8,
                    (49.0 + 72.0 * glow) as u8,
                )
            } else {
                (
                    (9.0 + 14.0 * u) as u8,
                    (12.0 + 14.0 * v) as u8,
                    (23.0 + 20.0 * glow) as u8,
                )
            };
            pixels.extend_from_slice(&[r, g, b, 255]);
        }
    }
    let source = Capture {
        frame: Frame {
            data: pixels,
            size: PhysicalSize::new(f64::from(WIDTH), f64::from(HEIGHT)),
            stride: WIDTH as usize * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::DisplayP3,
            scale: ScaleFactor::new(2.0),
        },
        provenance: Provenance::Region,
        target: CaptureTarget::Region(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(f64::from(WIDTH) / 2.0, f64::from(HEIGHT) / 2.0),
        )),
    };
    let mut document = Document::new(source);
    document
        .set_beautification(Some(Beautification::preset(BeautificationPreset::Social)))
        .expect("region fixture may be beautified");
    document
}

#[cfg(test)]
mod tests {
    use scrozz_core::{
        Capture, CaptureTarget, Frame, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize,
        PixelFormat, Provenance, ScaleFactor,
    };

    use super::*;

    fn document(provenance: Provenance) -> Document {
        Document::new(Capture {
            frame: Frame {
                data: vec![128; 80 * 48 * 4],
                stride: 80 * 4,
                size: PhysicalSize::new(80.0, 48.0),
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            target: CaptureTarget::Region(LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(80.0, 48.0),
            )),
            provenance,
        })
    }

    #[test]
    fn handle_preserves_non_destructive_document_data() {
        let handle = EditorHandle::default();
        let document = document(Provenance::Region);
        let source = document.source.frame.data.clone();
        handle.open(EditorRequest::new(42, "Capture", document.clone()));

        let commands = handle.drain_commands();
        let [EditorCommand::Open(request)] = commands.as_slice() else {
            panic!("open command");
        };
        assert_eq!(request.document.source.frame.data, source);
        assert_eq!(document.source.frame.data, source);
    }

    #[test]
    fn window_document_reports_d9_instead_of_becoming_beautifiable() {
        let document = document(Provenance::Window);
        assert!(!document.may_beautify());
        assert!(
            document
                .clone()
                .set_beautification(Some(Beautification::preset(BeautificationPreset::Clean)))
                .is_err()
        );
    }

    #[test]
    fn editor_events_round_trip_through_handle() {
        let handle = EditorHandle::default();
        handle.emit(EditorEvent::Closed { id: 7 });
        assert!(matches!(
            handle.drain_events().as_slice(),
            [EditorEvent::Closed { id: 7 }]
        ));
    }

    #[test]
    fn reopening_an_active_editor_focuses_without_replacing_unsaved_work() {
        let context = Context::default();
        let handle = EditorHandle::default();
        let mut workspace = EditorWorkspace::new(&context, handle.clone(), Appearance::Dark);
        handle.open(EditorRequest::new(
            42,
            "Capture",
            document(Provenance::Region),
        ));
        workspace.apply_commands(&context);

        let panel = workspace.panels.get_mut(&42).expect("open panel");
        panel
            .document
            .set_beautification(Some(Beautification::preset(BeautificationPreset::Story)))
            .unwrap();
        panel.persist_pending = true;

        handle.open(EditorRequest::new(
            42,
            "Stale history snapshot",
            document(Provenance::Region),
        ));
        workspace.apply_commands(&context);

        let panel = workspace.panels.get(&42).expect("same panel");
        assert_eq!(
            panel.document.beautification(),
            Some(&Beautification::preset(BeautificationPreset::Story))
        );
        assert!(panel.persist_pending, "reopen must retain pending autosave");
        assert_eq!(panel.title, "Capture");
    }

    #[test]
    fn workspace_flush_emits_pending_metadata_before_shutdown() {
        let context = Context::default();
        let handle = EditorHandle::default();
        let mut workspace = EditorWorkspace::new(&context, handle.clone(), Appearance::Dark);
        handle.open(EditorRequest::new(
            9,
            "Capture",
            document(Provenance::Region),
        ));
        workspace.apply_commands(&context);
        let panel = workspace.panels.get_mut(&9).expect("open panel");
        panel
            .document
            .set_beautification(Some(Beautification::preset(BeautificationPreset::Clean)))
            .unwrap();
        panel.persist_pending = true;

        workspace.flush_all();

        let events = handle.drain_events();
        let [EditorEvent::Persist { id, data }] = events.as_slice() else {
            panic!("one persistence event");
        };
        assert_eq!(*id, 9);
        assert_eq!(
            data.beautification,
            Some(Beautification::preset(BeautificationPreset::Clean))
        );
        assert!(!workspace.panels.get(&9).unwrap().persist_pending);
    }

    #[test]
    fn export_status_is_terminal_and_is_not_hidden_by_autosave() {
        let context = Context::default();
        let handle = EditorHandle::default();
        let mut workspace = EditorWorkspace::new(&context, handle.clone(), Appearance::Dark);
        handle.open(EditorRequest::new(
            9,
            "Capture",
            document(Provenance::Region),
        ));
        workspace.apply_commands(&context);
        workspace.panels.get_mut(&9).unwrap().export_pending = true;

        handle.status(9, EditorStatus::Complete("saved changes".to_owned()));
        workspace.apply_commands(&context);
        assert!(workspace.panels.get(&9).unwrap().export_pending);

        handle.export_status(9, EditorStatus::Failed("export failed".to_owned()));
        handle.persist_status(9, EditorStatus::Complete("saved changes".to_owned()));
        workspace.apply_commands(&context);
        let panel = workspace.panels.get(&9).unwrap();
        assert!(!panel.export_pending);
        assert!(panel.export_notice);
        assert!(matches!(
            &panel.status,
            EditorStatus::Failed(message) if message == "export failed"
        ));
    }
}
