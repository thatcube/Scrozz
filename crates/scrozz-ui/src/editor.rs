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
    Alignment, AnalysisCancellation, AspectPreset, AutomaticBackground, Background,
    BackgroundImage, Beautification, BeautificationPreset, BuiltInBackground, Document,
    DocumentData, ExactOutputSize, SensitiveRegionReview, SkiaRenderer, SmartFrameAnalysis,
    SmartFramePreset, SmartFramePresetSettings, SourceInsets, Watermark, provisional_smart_frame,
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
    /// Analyse one immutable current revision off the UI thread.
    AnalyzeSmartFrame {
        /// Stable card/editor identifier.
        id: u64,
        /// Editor revision used to reject stale results.
        revision: u64,
        /// Current annotations with framing removed.
        data: DocumentData,
        /// Cooperative cancellation handle.
        cancellation: AnalysisCancellation,
    },
    /// Add or replace a pixel-free custom preset.
    UpsertPreset {
        /// Stable card/editor identifier.
        id: u64,
        /// Validated preset.
        preset: SmartFramePreset,
    },
    /// Remove a custom preset.
    DeletePreset {
        /// Stable card/editor identifier.
        id: u64,
        /// Preset identifier.
        preset_id: String,
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
    /// Cross-capture custom presets.
    pub custom_presets: Vec<SmartFramePreset>,
}

impl EditorRequest {
    /// Creates an editor request.
    #[must_use]
    pub fn new(id: u64, title: impl Into<String>, document: Document) -> Self {
        Self {
            id,
            title: title.into(),
            document,
            custom_presets: Vec::new(),
        }
    }

    /// Supplies persisted custom presets.
    #[must_use]
    pub fn with_presets(mut self, presets: Vec<SmartFramePreset>) -> Self {
        self.custom_presets = presets;
        self
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
    SmartFrameAnalyzed {
        id: u64,
        revision: u64,
        result: Box<std::result::Result<SmartFrameAnalysis, String>>,
    },
    PresetsUpdated {
        id: u64,
        presets: Vec<SmartFramePreset>,
        status: EditorStatus,
    },
    SensitiveReview {
        id: u64,
        review: SensitiveRegionReview,
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

    /// Delivers an asynchronous Smart Frame analysis.
    pub fn smart_frame_analyzed(
        &self,
        id: u64,
        revision: u64,
        result: std::result::Result<SmartFrameAnalysis, String>,
    ) {
        let context = {
            let mut shared = self.shared.lock().expect("editor mutex poisoned");
            shared
                .commands
                .push_back(EditorCommand::SmartFrameAnalyzed {
                    id,
                    revision,
                    result: Box::new(result),
                });
            shared.context.clone()
        };
        if let Some(context) = context {
            context.request_repaint();
        }
    }

    /// Replaces the custom-preset list after a durable mutation.
    pub fn presets_updated(&self, id: u64, presets: Vec<SmartFramePreset>, status: EditorStatus) {
        let context = {
            let mut shared = self.shared.lock().expect("editor mutex poisoned");
            shared.commands.push_back(EditorCommand::PresetsUpdated {
                id,
                presets,
                status,
            });
            shared.context.clone()
        };
        if let Some(context) = context {
            context.request_repaint();
        }
    }

    /// Supplies reviewed-but-unconfirmed sensitive-region suggestions.
    pub fn sensitive_review(&self, id: u64, review: SensitiveRegionReview) {
        let context = {
            let mut shared = self.shared.lock().expect("editor mutex poisoned");
            shared
                .commands
                .push_back(EditorCommand::SensitiveReview { id, review });
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
                panel.cancel_smart_frame();
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
                EditorCommand::SmartFrameAnalyzed {
                    id,
                    revision,
                    result,
                } => {
                    if let Some(panel) = self.panels.get_mut(&id) {
                        panel.finish_smart_frame_analysis(revision, *result);
                    }
                }
                EditorCommand::PresetsUpdated {
                    id,
                    presets,
                    status,
                } => {
                    if let Some(panel) = self.panels.get_mut(&id) {
                        panel.custom_presets = presets;
                        panel.status = status;
                    }
                }
                EditorCommand::SensitiveReview { id, review } => {
                    if let Some(panel) = self.panels.get_mut(&id)
                        && review.revision == panel.revision
                    {
                        panel.sensitive_review = Some(review);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct SmartFrameDraft {
    before: Option<Beautification>,
    analysis_revision: u64,
    cancellation: AnalysisCancellation,
    analysis_pending: bool,
    edited_after_request: bool,
    inset_explanation: String,
    analysis_scope: AnalysisScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnalysisScope {
    All,
    AutomaticOnly,
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
    revision: u64,
    next_analysis_generation: u64,
    smart_frame: Option<SmartFrameDraft>,
    undo: Vec<Option<Beautification>>,
    redo: Vec<Option<Beautification>>,
    advanced_open: bool,
    custom_presets: Vec<SmartFramePreset>,
    selected_preset: Option<String>,
    preset_name: String,
    sensitive_review: Option<SensitiveRegionReview>,
    confirm_revert: bool,
}

impl EditorPanel {
    fn new(request: EditorRequest) -> Self {
        let EditorRequest {
            id,
            title,
            document,
            custom_presets,
        } = request;
        Self {
            id,
            title,
            document,
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
            revision: 0,
            next_analysis_generation: 1,
            smart_frame: None,
            undo: Vec::new(),
            redo: Vec::new(),
            advanced_open: false,
            custom_presets,
            selected_preset: None,
            preset_name: String::new(),
            sensitive_review: None,
            confirm_revert: false,
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
                events.extend(self.controls(ui, theme));
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
        let show_metadata = ui.available_width() >= 920.0;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("SMART FRAME STUDIO")
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
            if show_metadata {
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
            }
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

    fn controls(&mut self, ui: &mut egui::Ui, theme: &Theme) -> Vec<EditorEvent> {
        let mut events = Vec::new();
        let compact = ui.available_height() < 520.0;
        ScrollArea::vertical()
            .id_salt(("beautify.scroll", self.id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.smart_frame.is_none() {
                    smart_frame_intro(ui, theme);
                    ui.add_space(12.0);
                    let existing = self.document.beautification().cloned();
                    let label = if existing.is_some() {
                        "Edit Smart Frame"
                    } else {
                        "Smart Frame"
                    };
                    if smart_frame_button(ui, label, theme).clicked() {
                        if let Some(existing) = existing {
                            self.begin_with(existing);
                        } else {
                            events.push(self.begin_smart_frame());
                        }
                    }
                    ui.add_space(14.0);
                    section_label(ui, "STARTING POINTS", theme);
                    self.starting_points(ui, theme);
                    if !compact {
                        self.preset_library(ui, theme, &mut events, false);
                    }
                    if self.document.beautification().is_some() {
                        section_rule(ui, theme);
                        if self.confirm_revert {
                            EguiFrame::new()
                                .fill(danger(theme).gamma_multiply(0.09))
                                .corner_radius(CornerRadius::same(8))
                                .inner_margin(Margin::same(10))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new("Remove applied framing?")
                                            .color(theme.palette.text)
                                            .strong(),
                                    );
                                    ui.horizontal(|ui| {
                                        if ui
                                            .button(
                                                RichText::new("Revert")
                                                    .color(danger(theme))
                                                    .strong(),
                                            )
                                            .clicked()
                                        {
                                            self.revert_framing(ui.input(|input| input.time));
                                            self.confirm_revert = false;
                                        }
                                        if ui.button("Keep framing").clicked() {
                                            self.confirm_revert = false;
                                        }
                                    });
                                });
                        } else if ui
                            .button(RichText::new("Revert framing").color(danger(theme)))
                            .clicked()
                        {
                            self.confirm_revert = true;
                        }
                    }
                    return;
                }

                self.draft_header(ui, theme, &mut events);
                section_rule(ui, theme);
                section_label(ui, "STARTING POINTS", theme);
                self.starting_points(ui, theme);
                if !compact {
                    self.preset_library(ui, theme, &mut events, true);
                    section_rule(ui, theme);
                }

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.advanced_open, "Advanced controls");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new("SOURCE SAFE")
                                .font(FontId::monospace(9.0))
                                .color(theme.palette.text_muted),
                        );
                    });
                });
                if self.advanced_open {
                    self.advanced_controls(ui, theme, &mut events);
                }
                if !compact {
                    section_rule(ui, theme);
                    self.sensitive_suggestions(ui, theme);
                }
            });
        events
    }

    fn begin_smart_frame(&mut self) -> EditorEvent {
        let before = self.document.beautification().cloned();
        let generation = self.next_analysis_generation;
        self.next_analysis_generation = self.next_analysis_generation.saturating_add(1);
        let cancellation = AnalysisCancellation::default();
        let provisional = provisional_smart_frame(
            self.document.logical_size(),
            self.document.source.frame.scale.get(),
            self.document.source.provenance,
            self.document.source.frame.color_space,
        );
        self.document
            .set_beautification(Some(provisional))
            .expect("the provisional recipe is provenance-safe");
        self.smart_frame = Some(SmartFrameDraft {
            before,
            analysis_revision: generation,
            cancellation: cancellation.clone(),
            analysis_pending: true,
            edited_after_request: false,
            inset_explanation: "Analysing this revision...".to_owned(),
            analysis_scope: AnalysisScope::All,
        });
        self.preview_dirty = true;
        self.advanced_open = false;
        self.confirm_revert = false;
        self.status =
            EditorStatus::Complete("Smart Frame draft - review before applying".to_owned());
        let mut data = self.document.data();
        data.beautification = None;
        EditorEvent::AnalyzeSmartFrame {
            id: self.id,
            revision: generation,
            data,
            cancellation,
        }
    }

    fn begin_with(&mut self, mut config: Beautification) {
        let before = self.smart_frame.as_ref().map_or_else(
            || self.document.beautification().cloned(),
            |draft| draft.before.clone(),
        );
        self.cancel_analysis();
        self.confirm_revert = false;
        if !self.document.may_style_subject() {
            config.inset = SourceInsets::default();
            config.corner_radius = 0.0;
            config.shadow = 0.0;
            config.border_width = 0.0;
        }
        match self.document.set_beautification(Some(config)) {
            Ok(()) => {
                self.smart_frame = Some(SmartFrameDraft {
                    before,
                    analysis_revision: 0,
                    cancellation: AnalysisCancellation::default(),
                    analysis_pending: false,
                    edited_after_request: true,
                    inset_explanation: "Preset values are editable until Apply".to_owned(),
                    analysis_scope: AnalysisScope::All,
                });
                self.preview_dirty = true;
                self.status =
                    EditorStatus::Complete("Smart Frame draft - review before applying".to_owned());
            }
            Err(error) => self.status = EditorStatus::Failed(error.to_string()),
        }
    }

    fn restart_analysis(&mut self) -> Option<EditorEvent> {
        let before = self.smart_frame.as_ref()?.before.clone();
        self.cancel_analysis();
        let generation = self.next_analysis_generation;
        self.next_analysis_generation = self.next_analysis_generation.saturating_add(1);
        let cancellation = AnalysisCancellation::default();
        let provisional = provisional_smart_frame(
            self.document.logical_size(),
            self.document.source.frame.scale.get(),
            self.document.source.provenance,
            self.document.source.frame.color_space,
        );
        self.document
            .set_beautification(Some(provisional))
            .expect("the provisional recipe is provenance-safe");
        self.smart_frame = Some(SmartFrameDraft {
            before,
            analysis_revision: generation,
            cancellation: cancellation.clone(),
            analysis_pending: true,
            edited_after_request: false,
            inset_explanation: "Analysing this revision...".to_owned(),
            analysis_scope: AnalysisScope::All,
        });
        self.preview_dirty = true;
        let mut data = self.document.data();
        data.beautification = None;
        Some(EditorEvent::AnalyzeSmartFrame {
            id: self.id,
            revision: generation,
            data,
            cancellation,
        })
    }

    fn finish_smart_frame_analysis(
        &mut self,
        revision: u64,
        result: std::result::Result<SmartFrameAnalysis, String>,
    ) {
        let Some(draft) = self.smart_frame.as_mut() else {
            return;
        };
        if draft.analysis_revision != revision
            || draft.edited_after_request
            || draft.cancellation.is_cancelled()
        {
            return;
        }
        draft.analysis_pending = false;
        match result {
            Ok(analysis) => {
                draft.inset_explanation = analysis.inset_explanation;
                let beautification = match draft.analysis_scope {
                    AnalysisScope::All => analysis.beautification,
                    AnalysisScope::AutomaticOnly => {
                        let mut current =
                            self.document.beautification().cloned().unwrap_or_default();
                        current.background = analysis.beautification.background;
                        current.smart_frame = analysis.beautification.smart_frame;
                        current
                    }
                };
                match self.document.set_beautification(Some(beautification)) {
                    Ok(()) => {
                        self.preview_dirty = true;
                        self.status =
                            EditorStatus::Complete("Smart Frame is ready to review".to_owned());
                    }
                    Err(error) => self.status = EditorStatus::Failed(error.to_string()),
                }
            }
            Err(error) if error.to_lowercase().contains("cancel") => {}
            Err(error) => {
                draft.inset_explanation =
                    "Inset left at zero because analysis did not complete".to_owned();
                self.status = EditorStatus::Failed(error);
            }
        }
    }

    fn cancel_analysis(&mut self) {
        if let Some(draft) = &self.smart_frame {
            draft.cancellation.cancel();
        }
    }

    fn request_automatic_background_analysis(&mut self) -> Option<EditorEvent> {
        let draft = self.smart_frame.as_mut()?;
        draft.cancellation.cancel();
        let generation = self.next_analysis_generation;
        self.next_analysis_generation = self.next_analysis_generation.saturating_add(1);
        let cancellation = AnalysisCancellation::default();
        draft.analysis_revision = generation;
        draft.cancellation = cancellation.clone();
        draft.analysis_pending = true;
        draft.edited_after_request = false;
        draft.analysis_scope = AnalysisScope::AutomaticOnly;
        draft.inset_explanation = "Resolving a capture-aware background...".to_owned();
        let mut data = self.document.data();
        data.beautification = None;
        Some(EditorEvent::AnalyzeSmartFrame {
            id: self.id,
            revision: generation,
            data,
            cancellation,
        })
    }

    fn mark_draft_edited(&mut self) {
        if let Some(draft) = &mut self.smart_frame {
            draft.cancellation.cancel();
            draft.analysis_pending = false;
            draft.edited_after_request = true;
        }
        self.preview_dirty = true;
    }

    fn apply_smart_frame(&mut self, now: f64) {
        let Some(draft) = self.smart_frame.take() else {
            return;
        };
        draft.cancellation.cancel();
        let after = self.document.beautification().cloned();
        if after != draft.before {
            self.undo.push(draft.before);
            self.redo.clear();
            self.revision = self.revision.saturating_add(1);
            self.changed(now);
        } else {
            self.status = EditorStatus::Complete("No framing changes to apply".to_owned());
        }
    }

    fn cancel_smart_frame(&mut self) {
        let Some(draft) = self.smart_frame.take() else {
            return;
        };
        draft.cancellation.cancel();
        if self.document.set_beautification(draft.before).is_ok() {
            self.preview_dirty = true;
            self.status = EditorStatus::Complete("Smart Frame draft cancelled".to_owned());
        }
    }

    fn revert_framing(&mut self, now: f64) {
        self.cancel_smart_frame();
        let before = self.document.beautification().cloned();
        if before.is_some() && self.document.set_beautification(None).is_ok() {
            self.undo.push(before);
            self.redo.clear();
            self.revision = self.revision.saturating_add(1);
            self.changed(now);
        }
    }

    fn undo_framing(&mut self, now: f64) {
        self.cancel_smart_frame();
        let Some(previous) = self.undo.pop() else {
            return;
        };
        let current = self.document.beautification().cloned();
        if self.document.set_beautification(previous).is_ok() {
            self.redo.push(current);
            self.revision = self.revision.saturating_add(1);
            self.changed(now);
        }
    }

    fn redo_framing(&mut self, now: f64) {
        self.cancel_smart_frame();
        let Some(next) = self.redo.pop() else {
            return;
        };
        let current = self.document.beautification().cloned();
        if self.document.set_beautification(next).is_ok() {
            self.undo.push(current);
            self.revision = self.revision.saturating_add(1);
            self.changed(now);
        }
    }

    fn draft_header(&mut self, ui: &mut egui::Ui, theme: &Theme, events: &mut Vec<EditorEvent>) {
        let Some(draft) = &self.smart_frame else {
            return;
        };
        let analysis_pending = draft.analysis_pending;
        let inset_explanation = draft.inset_explanation.clone();
        EguiFrame::new()
            .fill(theme.palette.accent.gamma_multiply(0.10))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(14))
            .stroke(Stroke::new(1.0, theme.palette.accent.gamma_multiply(0.55)))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("SMART FRAME DRAFT")
                        .font(FontId::monospace(10.0))
                        .color(theme.palette.accent)
                        .strong(),
                );
                ui.add_space(5.0);
                ui.label(
                    RichText::new(if analysis_pending {
                        "Balancing this revision..."
                    } else {
                        "Ready to refine"
                    })
                    .color(theme.palette.text)
                    .strong(),
                );
                ui.label(
                    RichText::new(inset_explanation)
                        .small()
                        .color(theme.palette.text_muted),
                );
                if !analysis_pending
                    && ui.small_button("Refresh automatic choices").clicked()
                    && let Some(event) = self.restart_analysis()
                {
                    events.push(event);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_sized(
                            [112.0, 34.0],
                            egui::Button::new(
                                RichText::new("Apply")
                                    .strong()
                                    .color(theme.palette.on_accent),
                            )
                            .fill(theme.palette.accent)
                            .corner_radius(CornerRadius::same(8)),
                        )
                        .clicked()
                    {
                        self.apply_smart_frame(ui.input(|input| input.time));
                    }
                    if ui
                        .add_sized([92.0, 34.0], egui::Button::new("Cancel"))
                        .clicked()
                    {
                        self.cancel_smart_frame();
                    }
                });
            });
    }

    fn starting_points(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.horizontal_wrapped(|ui| {
                for (label, preset) in [
                    ("Clean", BeautificationPreset::Clean),
                    ("Social", BeautificationPreset::Social),
                    ("Story", BeautificationPreset::Story),
                    ("Editorial", BeautificationPreset::Editorial),
                ] {
                    let candidate = Beautification::preset(preset);
                    let selected = self.document.beautification() == Some(&candidate);
                    if pill_button(ui, label, selected, theme).clicked() {
                        self.begin_with(candidate);
                    }
                }
            });
        });
    }

    fn preset_library(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        events: &mut Vec<EditorEvent>,
        allow_save: bool,
    ) {
        ui.add_space(12.0);
        if self.custom_presets.is_empty() && !allow_save {
            return;
        }
        ui.label(
            RichText::new("Your presets")
                .color(theme.palette.text)
                .strong(),
        );
        if self.custom_presets.is_empty() {
            ui.label(
                RichText::new("Save the current draft to reuse it on another capture.")
                    .small()
                    .color(theme.palette.text_muted),
            );
        } else {
            let selected_name = self
                .selected_preset
                .as_deref()
                .and_then(|id| self.custom_presets.iter().find(|preset| preset.id == id))
                .map_or("Choose a custom preset", |preset| preset.name.as_str());
            let presets = self.custom_presets.clone();
            egui::ComboBox::from_id_salt(("editor.custom-preset", self.id))
                .selected_text(selected_name)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for preset in presets {
                        if ui
                            .selectable_label(
                                self.selected_preset.as_deref() == Some(preset.id.as_str()),
                                &preset.name,
                            )
                            .clicked()
                        {
                            self.selected_preset = Some(preset.id.clone());
                            self.preset_name = preset.name.clone();
                            let automatic = matches!(
                                preset.settings.background,
                                scrozz_annotate::PresetBackground::Automatic
                            );
                            self.begin_with(preset.settings.to_beautification());
                            if automatic
                                && let Some(event) = self.request_automatic_background_analysis()
                            {
                                events.push(event);
                            }
                        }
                    }
                });
        }
        if !allow_save {
            return;
        }
        ui.add_space(7.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.preset_name)
                .hint_text("Preset name")
                .desired_width(ui.available_width()),
        );
        let updates_selected = self
            .selected_preset
            .as_deref()
            .and_then(|id| self.custom_presets.iter().find(|preset| preset.id == id))
            .is_some_and(|preset| preset.name == self.preset_name.trim());
        let save_label = if updates_selected {
            "Update preset"
        } else {
            "Save new preset"
        };
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !self.preset_name.trim().is_empty(),
                    egui::Button::new(save_label),
                )
                .clicked()
            {
                match self.build_preset(false) {
                    Ok(preset) => {
                        self.upsert_local_preset(preset.clone());
                        events.push(EditorEvent::UpsertPreset {
                            id: self.id,
                            preset,
                        });
                    }
                    Err(error) => self.status = EditorStatus::Failed(error.to_string()),
                }
            }
            if self.selected_preset.is_some() && ui.small_button("Duplicate").clicked() {
                match self.build_preset(true) {
                    Ok(preset) => {
                        self.upsert_local_preset(preset.clone());
                        events.push(EditorEvent::UpsertPreset {
                            id: self.id,
                            preset,
                        });
                    }
                    Err(error) => self.status = EditorStatus::Failed(error.to_string()),
                }
            }
            if let Some(id) = self.selected_preset.clone()
                && ui.small_button("Delete").clicked()
            {
                self.custom_presets.retain(|preset| preset.id != id);
                self.selected_preset = None;
                events.push(EditorEvent::DeletePreset {
                    id: self.id,
                    preset_id: id,
                });
            }
        });
    }

    fn build_preset(&mut self, duplicate: bool) -> Result<SmartFramePreset> {
        let name = if duplicate {
            format!("{} copy", self.preset_name.trim())
        } else {
            self.preset_name.trim().to_owned()
        };
        let beauty = self
            .document
            .beautification()
            .ok_or_else(|| Error::InvalidRequest("start a Smart Frame draft first".to_owned()))?;
        let mut settings = SmartFramePresetSettings::from_beautification(beauty)?;
        let selected = self
            .selected_preset
            .as_deref()
            .and_then(|id| self.custom_presets.iter().find(|preset| preset.id == id));
        if let Some(selected) = selected {
            settings.extensions = selected.settings.extensions.clone();
        }
        let existing_id = (!duplicate)
            .then(|| {
                selected
                    .filter(|preset| preset.name == name)
                    .map(|preset| preset.id.clone())
            })
            .flatten();
        let id = existing_id.unwrap_or_else(|| unique_preset_id(&name, &self.custom_presets));
        let mut preset = SmartFramePreset::new(id, name, settings)?;
        if let Some(selected) = selected {
            preset.extensions = selected.extensions.clone();
        }
        Ok(preset)
    }

    fn upsert_local_preset(&mut self, preset: SmartFramePreset) {
        self.selected_preset = Some(preset.id.clone());
        if let Some(existing) = self
            .custom_presets
            .iter_mut()
            .find(|existing| existing.id == preset.id)
        {
            *existing = preset;
        } else {
            self.custom_presets.push(preset);
        }
        self.custom_presets
            .sort_by_key(|preset| preset.name.to_lowercase());
        self.status = EditorStatus::Complete("Saved custom Smart Frame preset".to_owned());
    }

    fn advanced_controls(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        events: &mut Vec<EditorEvent>,
    ) {
        let mut config = self.document.beautification().cloned().unwrap_or_default();
        let before = config.clone();
        section_rule(ui, theme);
        section_label(ui, "BACKGROUND", theme);
        background_controls(
            ui,
            &mut config,
            theme,
            &mut self.status,
            self.document.source.frame.color_space,
        );
        section_rule(ui, theme);
        section_label(ui, "CANVAS", theme);
        measure_slider(ui, "Padding", &mut config.padding, 0.0..=220.0);
        let mut uniform_inset = config
            .inset
            .left
            .max(config.inset.top)
            .max(config.inset.right)
            .max(config.inset.bottom);
        let inset_response = measure_slider(ui, "Inset", &mut uniform_inset, 0.0..=160.0);
        if inset_response.changed() {
            config.inset = SourceInsets::uniform(uniform_inset);
        }
        if let Some(metadata) = &config.smart_frame {
            ui.label(
                RichText::new(metadata.inset_decision.explanation())
                    .small()
                    .color(theme.palette.text_muted),
            );
        }
        ui.add_space(8.0);
        output_size_row(ui, &mut config, &mut self.status);
        ui.add_space(8.0);
        aspect_row(ui, &mut config.aspect);
        if config.aspect != AspectPreset::Original {
            config.output_size = None;
        }
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Alignment")
                        .color(theme.palette.text)
                        .strong(),
                );
                ui.label(
                    RichText::new(if config.auto_balance {
                        "Auto Balance positions the subject"
                    } else {
                        "Choose an anchor in the extra canvas"
                    })
                    .small()
                    .color(theme.palette.text_muted),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                alignment_matrix(ui, &mut config.alignment, theme);
            });
        });
        ui.add_space(10.0);
        ui.checkbox(&mut config.auto_balance, "Auto Balance")
            .on_hover_text(
                "Uses the stored visual focus for stable placement and retains a safe edge inset.",
            );
        section_rule(ui, theme);
        section_label(ui, "SUBJECT", theme);
        ui.add_enabled_ui(self.document.may_style_subject(), |ui| {
            measure_slider(ui, "Corners", &mut config.corner_radius, 0.0..=80.0);
            measure_slider(ui, "Shadow", &mut config.shadow, 0.0..=80.0);
            measure_slider(ui, "Border", &mut config.border_width, 0.0..=12.0);
            if config.border_width > 0.0 {
                color_control(ui, "Border colour", &mut config.border_color);
            }
        });
        if !self.document.may_style_subject() {
            d9_outer_canvas_note(ui, theme);
            config.inset = SourceInsets::default();
            config.corner_radius = 0.0;
            config.shadow = 0.0;
            config.border_width = 0.0;
        }
        section_rule(ui, theme);
        section_label(ui, "WATERMARK", theme);
        let mut enabled = config.watermark.is_some();
        if ui.checkbox(&mut enabled, "Show watermark").changed() {
            config.watermark = enabled.then(Watermark::default);
        }
        if let Some(watermark) = &mut config.watermark {
            ui.add(
                egui::TextEdit::singleline(&mut watermark.text)
                    .hint_text("Your text")
                    .desired_width(ui.available_width()),
            );
            measure_slider(ui, "Text size", &mut watermark.font_size, 8.0..=36.0);
            color_control(ui, "Text colour", &mut watermark.color);
        }

        let automatic_selected = !matches!(before.background, Background::Automatic(_))
            && matches!(config.background, Background::Automatic(_));
        if beautification_changed(&before, &config) {
            match self.document.set_beautification(Some(config)) {
                Ok(()) => {
                    self.mark_draft_edited();
                    if automatic_selected
                        && let Some(event) = self.request_automatic_background_analysis()
                    {
                        events.push(event);
                    }
                }
                Err(error) => self.status = EditorStatus::Failed(error.to_string()),
            }
        }
    }

    fn sensitive_suggestions(&self, ui: &mut egui::Ui, theme: &Theme) {
        section_label(ui, "PRIVACY REVIEW", theme);
        match &self.sensitive_review {
            Some(review) if !review.suggestions.is_empty() => {
                ui.label(
                    RichText::new(format!(
                        "{} suggestion{} awaiting review",
                        review.suggestions.len(),
                        if review.suggestions.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ))
                    .color(theme.palette.text),
                );
                ui.label(
                    RichText::new("Smart Frame never redacts suggested regions automatically.")
                        .small()
                        .color(theme.palette.text_muted),
                );
            }
            _ => {
                ui.label(
                    RichText::new("No reviewed sensitive-region suggestions")
                        .small()
                        .color(theme.palette.text_muted),
                );
            }
        }
    }

    fn actions(&mut self, ui: &mut egui::Ui, theme: &Theme) -> Vec<EditorEvent> {
        let mut events = Vec::new();
        ui.horizontal(|ui| {
            self.status_label(ui, theme);
            ui.add_space(10.0);
            if ui
                .add_enabled(!self.undo.is_empty(), egui::Button::new("Undo framing"))
                .clicked()
            {
                self.undo_framing(ui.input(|input| input.time));
            }
            if ui
                .add_enabled(!self.redo.is_empty(), egui::Button::new("Redo"))
                .clicked()
            {
                self.redo_framing(ui.input(|input| input.time));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let destinations_enabled = !self.export_pending && self.smart_frame.is_none();
                let copy = action_button(ui, "Copy image", true, destinations_enabled, theme);
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
                    action_button(ui, "Save to Pictures", false, destinations_enabled, theme);
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

fn smart_frame_intro(ui: &mut egui::Ui, theme: &Theme) {
    EguiFrame::new()
        .fill(theme.palette.card_fill)
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(14))
        .stroke(Stroke::new(1.0, theme.palette.hairline))
        .show(ui, |ui| {
            ui.label(
                RichText::new("SMART FRAME")
                    .font(FontId::monospace(10.0))
                    .color(theme.palette.accent)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("Balanced framing in one action")
                    .size(17.0)
                    .color(theme.palette.text)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Adapts spacing, finish, and a capture-aware background. Auto Balance starts on.",
                )
                .small()
                .color(theme.palette.text_muted),
            );
        });
}

fn smart_frame_button(ui: &mut egui::Ui, label: &str, theme: &Theme) -> Response {
    ui.add_sized(
        [ui.available_width(), 44.0],
        egui::Button::new(
            RichText::new(label)
                .size(14.0)
                .color(theme.palette.on_accent)
                .strong(),
        )
        .fill(theme.palette.accent)
        .stroke(Stroke::new(1.0, theme.palette.accent))
        .corner_radius(CornerRadius::same(10)),
    )
}

fn unique_preset_id(name: &str, presets: &[SmartFramePreset]) -> String {
    let base = name
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let base = if base.is_empty() {
        "smart-frame".to_owned()
    } else {
        base
    };
    if presets.iter().all(|preset| preset.id != base) {
        return base;
    }
    for suffix in 2..=10_000 {
        let candidate = format!("{base}-{suffix}");
        if presets.iter().all(|preset| preset.id != candidate) {
            return candidate;
        }
    }
    format!("{base}-{}", presets.len().saturating_add(2))
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
    source_color_space: ColorSpace,
) {
    if pill_button(
        ui,
        "Automatic",
        matches!(config.background, Background::Automatic(_)),
        theme,
    )
    .on_hover_text("Uses a resolved, contrast-checked palette from this capture.")
    .clicked()
    {
        config.background =
            Background::Automatic(AutomaticBackground::fallback(source_color_space));
    }
    ui.add_space(8.0);
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

fn output_size_row(ui: &mut egui::Ui, config: &mut Beautification, status: &mut EditorStatus) {
    const PRESETS: &[(&str, Option<ExactOutputSize>)] = &[
        ("Flexible canvas", None),
        (
            "Square post - 1080 x 1080 (social)",
            Some(ExactOutputSize::new(1080, 1080)),
        ),
        (
            "Portrait post - 1080 x 1350 (social)",
            Some(ExactOutputSize::new(1080, 1350)),
        ),
        (
            "Vertical story - 1080 x 1920 (social)",
            Some(ExactOutputSize::new(1080, 1920)),
        ),
        (
            "Video thumbnail - 1280 x 720",
            Some(ExactOutputSize::new(1280, 720)),
        ),
        (
            "Wide header - 1500 x 500",
            Some(ExactOutputSize::new(1500, 500)),
        ),
    ];
    let selected = PRESETS
        .iter()
        .find(|(_, size)| *size == config.output_size)
        .map_or_else(
            || {
                config.output_size.map_or_else(
                    || "Flexible canvas".to_owned(),
                    |size| format!("Custom - {} x {}", size.width, size.height),
                )
            },
            |(label, _)| (*label).to_owned(),
        );
    egui::ComboBox::from_id_salt(("editor.output-size", ui.id()))
        .selected_text(selected)
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for (label, size) in PRESETS {
                if ui
                    .selectable_label(config.output_size == *size, *label)
                    .clicked()
                {
                    config.output_size = *size;
                    if size.is_some() {
                        config.aspect = AspectPreset::Original;
                    }
                }
            }
        });
    let mut custom = config
        .output_size
        .unwrap_or(ExactOutputSize::new(1080, 1080));
    ui.horizontal(|ui| {
        ui.label("Custom");
        let width = ui.add(
            egui::DragValue::new(&mut custom.width)
                .range(1..=16_384)
                .suffix(" px"),
        );
        ui.label("x");
        let height = ui.add(
            egui::DragValue::new(&mut custom.height)
                .range(1..=16_384)
                .suffix(" px"),
        );
        if width.changed() || height.changed() {
            let candidate = Some(custom);
            let mut checked = config.clone();
            checked.output_size = candidate;
            checked.aspect = AspectPreset::Original;
            match checked.validate() {
                Ok(()) => {
                    config.output_size = candidate;
                    config.aspect = AspectPreset::Original;
                }
                Err(error) => *status = EditorStatus::Failed(error.to_string()),
            }
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
) -> Response {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(
                egui::Slider::new(value, range)
                    .suffix(" pt")
                    .fixed_decimals(0)
                    .show_value(true),
            )
        })
        .inner
    })
    .inner
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

fn d9_outer_canvas_note(ui: &mut egui::Ui, theme: &Theme) {
    ui.add_space(8.0);
    EguiFrame::new()
        .fill(danger(theme).gamma_multiply(0.10))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(16))
        .stroke(Stroke::new(1.0, danger(theme).gamma_multiply(0.7)))
        .show(ui, |ui| {
            ui.label(
                RichText::new("NATIVE WINDOW PRESERVED")
                    .font(FontId::monospace(10.0))
                    .color(danger(theme))
                    .strong(),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("Only the outer presentation canvas changes.")
                    .color(theme.palette.text)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Inset, corners, shadow, and border stay disabled so the captured window remains \
                     byte-stable. Background, padding, placement, and output size remain available.",
                )
                .color(theme.palette.text_muted),
            );
        });
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
    /// Builds the legacy editor fixture with the advanced inspector visible.
    #[must_use]
    pub fn new() -> Self {
        Self::expanded()
    }

    /// Editor before Smart Frame is activated.
    #[must_use]
    pub fn untouched() -> Self {
        Self::with_state(EditorPreviewState::Untouched)
    }

    /// One-click result with progressive controls closed.
    #[must_use]
    pub fn one_click() -> Self {
        Self::with_state(EditorPreviewState::OneClick)
    }

    /// Smart Frame draft with the complete advanced inspector.
    #[must_use]
    pub fn expanded() -> Self {
        Self::with_state(EditorPreviewState::Expanded)
    }

    fn with_state(state: EditorPreviewState) -> Self {
        let mut panel = EditorPanel::new(EditorRequest::new(
            1,
            "Launch notes - region capture",
            editor_fixture_document(),
        ));
        if state != EditorPreviewState::Untouched {
            let EditorEvent::AnalyzeSmartFrame {
                revision,
                cancellation,
                ..
            } = panel.begin_smart_frame()
            else {
                unreachable!("Smart Frame always requests analysis");
            };
            let result = scrozz_annotate::analyze_smart_frame(
                &panel.document.source.frame,
                panel.document.source.provenance,
                &cancellation,
            )
            .map_err(|error| error.to_string());
            panel.finish_smart_frame_analysis(revision, result);
            panel.advanced_open = state == EditorPreviewState::Expanded;
        }
        Self {
            panel: Mutex::new(panel),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorPreviewState {
    Untouched,
    OneClick,
    Expanded,
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
    Document::new(source)
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
    fn window_document_allows_only_an_outer_presentation_canvas() {
        let mut document = document(Provenance::Window);
        assert!(document.may_beautify());
        assert!(!document.may_style_subject());
        assert!(
            document
                .clone()
                .set_beautification(Some(Beautification::preset(BeautificationPreset::Clean)))
                .is_err()
        );
        assert!(
            document
                .set_beautification(Some(Beautification::padded(
                    32.0,
                    Background::Automatic(AutomaticBackground::default())
                )))
                .is_ok()
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

    #[test]
    fn smart_frame_starts_as_an_immediate_default_on_draft() {
        let mut panel = EditorPanel::new(EditorRequest::new(
            3,
            "Capture",
            document(Provenance::Region),
        ));
        let before = panel.document.data();

        let EditorEvent::AnalyzeSmartFrame {
            revision,
            data,
            cancellation,
            ..
        } = panel.begin_smart_frame()
        else {
            panic!("analysis event");
        };

        assert_eq!(revision, 1);
        assert!(!cancellation.is_cancelled());
        assert!(
            data.beautification.is_none(),
            "analysis receives no old frame"
        );
        let draft = panel.document.beautification().expect("visible draft");
        assert!(draft.auto_balance, "Smart Frame's main value starts on");
        assert!(matches!(draft.background, Background::Automatic(_)));
        assert_eq!(panel.revision, 0, "opening a draft is revision-neutral");

        panel.cancel_smart_frame();
        assert_eq!(panel.document.data(), before);
        assert_eq!(panel.revision, 0);
        assert!(panel.flush_persist().is_none());
    }

    #[test]
    fn apply_is_one_undoable_revision_and_cancel_restores_exactly() {
        let mut panel = EditorPanel::new(EditorRequest::new(
            4,
            "Capture",
            document(Provenance::Region),
        ));
        let before = panel.document.data();
        panel.begin_with(Beautification::preset(BeautificationPreset::Social));
        panel.apply_smart_frame(1.0);

        assert_eq!(panel.revision, 1);
        assert_eq!(panel.undo.len(), 1);
        assert!(panel.persist_pending);
        assert!(panel.document.beautification().is_some());

        panel.undo_framing(2.0);
        assert_eq!(panel.document.data().beautification, before.beautification);
        assert_eq!(panel.revision, 2);
        panel.redo_framing(3.0);
        assert_eq!(
            panel.document.beautification(),
            Some(&Beautification::preset(BeautificationPreset::Social))
        );
    }

    #[test]
    fn a_stale_analysis_never_replaces_a_newer_draft() {
        let mut panel = EditorPanel::new(EditorRequest::new(
            5,
            "Capture",
            document(Provenance::Region),
        ));
        let EditorEvent::AnalyzeSmartFrame {
            revision: stale, ..
        } = panel.begin_smart_frame()
        else {
            panic!("analysis event");
        };
        panel.cancel_smart_frame();
        let EditorEvent::AnalyzeSmartFrame {
            revision: current, ..
        } = panel.begin_smart_frame()
        else {
            panic!("analysis event");
        };
        assert_ne!(stale, current);
        let expected = panel.document.beautification().cloned();
        panel.finish_smart_frame_analysis(
            stale,
            Ok(SmartFrameAnalysis {
                beautification: Beautification::preset(BeautificationPreset::Story),
                inset_explanation: "stale".to_owned(),
            }),
        );
        assert_eq!(panel.document.beautification(), expected.as_ref());
    }

    #[test]
    fn a_manual_preset_choice_cancels_the_in_flight_analysis() {
        let mut panel = EditorPanel::new(EditorRequest::new(
            8,
            "Capture",
            document(Provenance::Region),
        ));
        let EditorEvent::AnalyzeSmartFrame {
            revision,
            cancellation,
            ..
        } = panel.begin_smart_frame()
        else {
            panic!("analysis event");
        };
        panel.begin_with(Beautification::preset(BeautificationPreset::Editorial));
        assert!(cancellation.is_cancelled());

        panel.finish_smart_frame_analysis(
            revision,
            Ok(SmartFrameAnalysis {
                beautification: Beautification::preset(BeautificationPreset::Story),
                inset_explanation: "stale".to_owned(),
            }),
        );
        assert_eq!(
            panel.document.beautification(),
            Some(&Beautification::preset(BeautificationPreset::Editorial))
        );
    }

    #[test]
    fn window_smart_frame_preserves_the_subject_controls() {
        let mut panel = EditorPanel::new(EditorRequest::new(
            6,
            "Window",
            document(Provenance::Window),
        ));
        let _ = panel.begin_smart_frame();
        let beauty = panel.document.beautification().expect("outer frame");
        assert!(beauty.padding > 0.0);
        assert!(beauty.preserves_subject_pixels());
        assert!(panel.document.may_beautify());
        assert!(!panel.document.may_style_subject());
    }

    #[test]
    fn renaming_a_selected_preset_saves_a_new_preset_instead_of_overwriting() {
        let mut panel = EditorPanel::new(EditorRequest::new(
            7,
            "Capture",
            document(Provenance::Region),
        ));
        panel.begin_with(Beautification::preset(BeautificationPreset::Clean));
        let original = SmartFramePreset::new(
            "quiet",
            "Quiet",
            SmartFramePresetSettings::from_beautification(panel.document.beautification().unwrap())
                .unwrap(),
        )
        .unwrap();
        panel.custom_presets.push(original);
        panel.selected_preset = Some("quiet".to_owned());
        panel.preset_name = "Quiet for docs".to_owned();

        let renamed = panel.build_preset(false).unwrap();
        assert_ne!(renamed.id, "quiet");
        assert_eq!(renamed.name, "Quiet for docs");
    }
}
