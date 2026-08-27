//! Capture history: a pure view model and the ordinary window that presents it.
//!
//! The overlay is intentionally invisible until a capture exists. History is the
//! opposite kind of surface: a normal, movable, resizable application window
//! where a person can browse for as long as they need. This module therefore
//! owns no platform hooks and never asks for click-through behavior.

use std::collections::{HashMap, VecDeque};

use egui::{
    Align, Align2, Color32, Layout, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle,
    TextureOptions, Ui, Vec2, ViewportBuilder, WindowLevel, pos2, vec2,
};
use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};
use scrozz_store::{CaptureId, MediaKind, Page, SearchQuery, Timestamp};

use crate::{
    Radius, Space, Text, Theme,
    theme::{Appearance, corner, install_fonts, install_style},
};

/// Results shown on one history page.
pub const PAGE_SIZE: u32 = 24;

const FILTER_WIDTH: f32 = 184.0;
const DETAIL_WIDTH: f32 = 310.0;
const CARD_WIDTH: f32 = 224.0;
const CARD_HEIGHT: f32 = 190.0;
const THUMB_HEIGHT: f32 = 132.0;
const DAY_MILLIS: i64 = 86_400_000;

/// A validated RGBA thumbnail produced away from the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryThumbnail {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl HistoryThumbnail {
    /// Wraps straight-alpha RGBA8 pixels.
    #[must_use]
    pub fn from_rgba(width: u32, height: u32, pixels: Vec<u8>) -> Option<Self> {
        let expected = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        (width > 0 && height > 0 && pixels.len() == expected).then_some(Self {
            width,
            height,
            pixels,
        })
    }

    /// Thumbnail width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Thumbnail height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Straight-alpha RGBA8 bytes.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

/// One capture as the history window needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    /// Durable capture identity.
    pub id: CaptureId,
    /// Capture time.
    pub created_at: Timestamp,
    /// Still, video, or GIF.
    pub media_kind: MediaKind,
    /// Whether retention may evict the source pixels.
    pub pinned: bool,
    /// Application that owned the captured window.
    pub app_name: Option<String>,
    /// Captured window title.
    pub window_title: Option<String>,
    /// Source dimensions.
    pub width: u32,
    /// Source dimensions.
    pub height: u32,
    /// Source display scale.
    pub scale: f64,
    /// Whether source pixels are still available.
    pub image_present: bool,
    /// Number of editable annotations.
    pub annotation_count: usize,
    /// Searchable OCR text, when recognition has run.
    pub ocr_text: Option<String>,
    /// A display-sized preview, when source pixels survived.
    pub thumbnail: Option<HistoryThumbnail>,
}

impl HistoryEntry {
    fn primary_label(&self) -> &str {
        self.window_title
            .as_deref()
            .or(self.app_name.as_deref())
            .unwrap_or("Untitled capture")
    }

    fn secondary_label(&self) -> String {
        let source = self.app_name.as_deref().unwrap_or("Scrozz");
        format!("{source}  ·  {} x {}", self.width, self.height)
    }
}

/// A complete answer from the history worker.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPage {
    /// Rows on this page, newest first.
    pub entries: Vec<HistoryEntry>,
    /// Matching rows across all pages.
    pub total: u64,
    /// Application choices across the complete history.
    pub apps: Vec<String>,
    /// Page offset this answer represents.
    pub offset: u32,
    /// Page size this answer represents.
    pub limit: u32,
}

/// Date ranges offered by the history filter rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateFilter {
    /// No date restriction.
    #[default]
    AnyTime,
    /// Captures from the last 24 hours.
    Today,
    /// Captures from the last seven days.
    LastSevenDays,
    /// Captures from the last 30 days.
    LastThirtyDays,
}

impl DateFilter {
    /// Every choice in display order.
    pub const ALL: &'static [Self] = &[
        Self::AnyTime,
        Self::Today,
        Self::LastSevenDays,
        Self::LastThirtyDays,
    ];

    /// Human-facing label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AnyTime => "Any time",
            Self::Today => "Last 24 hours",
            Self::LastSevenDays => "Last 7 days",
            Self::LastThirtyDays => "Last 30 days",
        }
    }

    fn cutoff(self, now: Timestamp) -> Option<Timestamp> {
        let days = match self {
            Self::AnyTime => return None,
            Self::Today => 1,
            Self::LastSevenDays => 7,
            Self::LastThirtyDays => 30,
        };
        Some(Timestamp(
            now.as_millis().saturating_sub(i64::from(days) * DAY_MILLIS),
        ))
    }
}

/// User-controlled history filters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HistoryFilters {
    /// Search across app, title, and recognised text.
    pub text: String,
    /// One media kind, or all kinds.
    pub media_kind: Option<MediaKind>,
    /// One application, or all applications.
    pub app_name: Option<String>,
    /// A relative date window.
    pub date: DateFilter,
    /// Show only captures protected from retention.
    pub pinned_only: bool,
}

impl HistoryFilters {
    /// Converts visible controls into the store's query model.
    #[must_use]
    pub fn query(&self, now: Timestamp, offset: u32, limit: u32) -> SearchQuery {
        let text = self.text.trim();
        SearchQuery {
            text: (!text.is_empty()).then(|| text.to_owned()),
            app_name: self.app_name.clone(),
            created_after: self.date.cutoff(now),
            media_kind: self.media_kind,
            pinned_only: self.pinned_only,
            page: Page::new(limit, offset),
            ..SearchQuery::default()
        }
    }
}

/// Work the window asks the application to perform.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryAction {
    /// Load or reload a page. The request identity prevents stale answers from
    /// replacing newer filters.
    Query {
        /// Monotonically increasing request identity.
        request: u64,
        /// Fully resolved store query.
        query: SearchQuery,
    },
    /// Put the capture back into the live card stack.
    Restore(CaptureId),
    /// Open the editable stored document.
    OpenEditor(CaptureId),
    /// Copy the rendered document.
    Copy(CaptureId),
    /// Save the rendered document.
    Save(CaptureId),
    /// Prepare a native drag payload from the preview being pulled.
    Drag {
        /// Capture being dragged.
        id: CaptureId,
        /// Preview rectangle in screen logical points.
        rect: LogicalRect,
        /// Pointer position in screen logical points.
        pointer: LogicalPoint,
    },
    /// Change retention protection.
    SetPinned {
        /// Capture to change.
        id: CaptureId,
        /// New state.
        pinned: bool,
    },
    /// Permanently remove one capture and its document.
    Delete(CaptureId),
}

/// Pure history state shared by the live viewport and the screenshot harness.
pub struct HistoryViewModel {
    visible: bool,
    now: Timestamp,
    last_date_refresh: Timestamp,
    filters: HistoryFilters,
    entries: Vec<HistoryEntry>,
    total: u64,
    apps: Vec<String>,
    offset: u32,
    limit: u32,
    selected: Option<CaptureId>,
    loading: bool,
    error: Option<String>,
    notice: Option<String>,
    delete_armed: Option<CaptureId>,
    request: u64,
    actions: VecDeque<HistoryAction>,
    native_drag_intents: VecDeque<CaptureId>,
    textures: HashMap<String, TextureHandle>,
}

impl std::fmt::Debug for HistoryViewModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryViewModel")
            .field("visible", &self.visible)
            .field("now", &self.now)
            .field("filters", &self.filters)
            .field("entries", &self.entries.len())
            .field("total", &self.total)
            .field("offset", &self.offset)
            .field("selected", &self.selected)
            .field("loading", &self.loading)
            .field("error", &self.error)
            .finish()
    }
}

impl HistoryViewModel {
    /// A closed history window using `now` for all relative labels and date
    /// filters. Injecting this value is what keeps golden images frozen.
    #[must_use]
    pub fn new(now: Timestamp) -> Self {
        Self {
            visible: false,
            now,
            last_date_refresh: now,
            filters: HistoryFilters::default(),
            entries: Vec::new(),
            total: 0,
            apps: Vec::new(),
            offset: 0,
            limit: PAGE_SIZE,
            selected: None,
            loading: false,
            error: None,
            notice: None,
            delete_armed: None,
            request: 0,
            actions: VecDeque::new(),
            native_drag_intents: VecDeque::new(),
            textures: HashMap::new(),
        }
    }

    /// A visible model with an already-loaded first page.
    ///
    /// Used by deterministic scenes and by hosts that restore a cached page.
    #[must_use]
    pub fn loaded(now: Timestamp, page: HistoryPage) -> Self {
        let mut model = Self::new(now);
        model.visible = true;
        model.request = 1;
        model.apply_page(1, page);
        model
    }

    /// Selects a visible capture when it exists on the current page.
    pub fn select(&mut self, id: &CaptureId) {
        if self.entries.iter().any(|entry| entry.id == *id) {
            self.selected = Some(id.clone());
            self.delete_armed = None;
        }
    }

    /// Whether the ordinary viewport should exist.
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.visible
    }

    /// Opens and refreshes history.
    pub fn open(&mut self, now: Timestamp) {
        self.visible = true;
        self.now = now;
        self.last_date_refresh = now;
        self.reload_from_start();
    }

    /// Closes the viewport without discarding its filters.
    pub fn close(&mut self) {
        self.visible = false;
        self.delete_armed = None;
    }

    /// Current filters.
    #[must_use]
    pub const fn filters(&self) -> &HistoryFilters {
        &self.filters
    }

    /// Replaces filters and starts from the first page.
    pub fn set_filters(&mut self, filters: HistoryFilters) {
        if self.filters != filters {
            self.filters = filters;
            self.reload_from_start();
        }
    }

    /// Advances relative labels and refreshes rolling date filters once a minute.
    ///
    /// Deterministic scenes never call this method, so their injected clock
    /// remains frozen.
    pub fn advance_clock(&mut self, now: Timestamp) {
        self.now = now;
        if self.visible
            && self.filters.date != DateFilter::AnyTime
            && now
                .as_millis()
                .saturating_sub(self.last_date_refresh.as_millis())
                >= 60_000
        {
            self.reload_from_start();
        }
    }

    /// Reloads the first page after the underlying history changes.
    pub fn refresh_from_start(&mut self, now: Timestamp) {
        if self.visible {
            self.now = now;
            self.reload_from_start();
        }
    }

    /// Reloads the current page after an operation may have changed its rows.
    pub fn refresh_current(&mut self, now: Timestamp) {
        if self.visible {
            self.now = now;
            self.queue_query();
        }
    }

    /// Applies a worker answer only if it is still the newest request.
    pub fn apply_page(&mut self, request: u64, page: HistoryPage) {
        if request != self.request {
            return;
        }
        let limit = page.limit.max(1);
        if page.entries.is_empty() && page.offset > 0 && u64::from(page.offset) >= page.total {
            self.total = page.total;
            self.apps = page.apps;
            self.entries.clear();
            self.textures.clear();
            self.selected = None;
            self.offset = if page.total == 0 {
                0
            } else {
                let final_index = u32::try_from(page.total.saturating_sub(1)).unwrap_or(u32::MAX);
                (final_index / limit) * limit
            };
            self.limit = limit;
            if page.total == 0 {
                self.loading = false;
                self.error = None;
            } else {
                self.queue_query();
            }
            return;
        }
        self.loading = false;
        self.error = None;
        self.offset = page.offset;
        self.limit = limit;
        self.total = page.total;
        self.apps = page.apps;
        self.entries = page.entries;
        self.textures.clear();
        if self
            .selected
            .as_ref()
            .is_some_and(|id| !self.entries.iter().any(|entry| &entry.id == id))
        {
            self.selected = None;
        }
    }

    /// Records a failed query without presenting it as an empty history.
    pub fn apply_query_error(&mut self, request: u64, error: impl Into<String>) {
        if request == self.request {
            self.loading = false;
            self.error = Some(error.into());
        }
    }

    /// Applies a completed pin change.
    pub fn pinned(&mut self, id: &CaptureId, pinned: bool) {
        if self.filters.pinned_only && !pinned {
            self.entries.retain(|entry| entry.id != *id);
            self.selected = None;
        }
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == *id) {
            entry.pinned = pinned;
        }
        self.notice = Some(if pinned {
            "Capture pinned".to_owned()
        } else {
            "Capture unpinned".to_owned()
        });
        // A page query runs independently from the mutation worker. Start a new
        // generation so an older answer cannot put the pre-mutation row back.
        self.reload_current_page();
    }

    /// Applies a completed deletion, then asks for a page that fills the gap.
    pub fn deleted(&mut self, id: &CaptureId) {
        self.entries.retain(|entry| entry.id != *id);
        self.selected = None;
        self.delete_armed = None;
        self.notice = Some("Capture deleted".to_owned());
        self.reload_current_page();
    }

    /// Shows feedback for copy, save, restore, drag, or editor actions.
    pub fn completed(&mut self, detail: impl Into<String>) {
        self.notice = Some(detail.into());
    }

    /// Shows an operation failure while preserving the loaded page.
    pub fn operation_failed(&mut self, error: impl Into<String>) {
        self.notice = Some(error.into());
    }

    /// Takes every action queued by the most recent UI frame.
    pub fn drain_actions(&mut self) -> Vec<HistoryAction> {
        self.actions.drain(..).collect()
    }

    /// Takes drag gestures that need their initiating native event retained
    /// before this viewport callback returns to the event loop.
    pub fn drain_native_drag_intents(&mut self) -> Vec<CaptureId> {
        self.native_drag_intents.drain(..).collect()
    }

    /// Cancels an unstarted drag when its native initiating event was unavailable.
    pub fn cancel_native_drag(&mut self, id: &CaptureId, error: impl Into<String>) {
        self.actions.retain(
            |action| !matches!(action, HistoryAction::Drag { id: queued, .. } if queued == id),
        );
        self.operation_failed(error);
    }

    fn reload_from_start(&mut self) {
        self.last_date_refresh = self.now;
        self.offset = 0;
        self.selected = None;
        self.delete_armed = None;
        self.queue_query();
    }

    fn reload_current_page(&mut self) {
        if self.offset > 0 && self.offset as u64 >= self.total.saturating_sub(1) {
            self.offset = self.offset.saturating_sub(self.limit);
        }
        self.queue_query();
    }

    fn queue_query(&mut self) {
        self.request = self.request.saturating_add(1);
        self.loading = true;
        self.error = None;
        self.actions.push_back(HistoryAction::Query {
            request: self.request,
            query: self.filters.query(self.now, self.offset, self.limit),
        });
    }

    fn previous_page(&mut self) {
        if self.offset > 0 {
            self.offset = self.offset.saturating_sub(self.limit);
            self.selected = None;
            self.queue_query();
        }
    }

    fn next_page(&mut self) {
        if u64::from(self.offset.saturating_add(self.limit)) < self.total {
            self.offset = self.offset.saturating_add(self.limit);
            self.selected = None;
            self.queue_query();
        }
    }

    fn selected_entry(&self) -> Option<HistoryEntry> {
        let id = self.selected.as_ref()?;
        self.entries.iter().find(|entry| &entry.id == id).cloned()
    }

    /// Draws one frame of the history window.
    pub fn ui(&mut self, ui: &mut Ui) {
        let theme = theme_for(ui);
        let palette = theme.palette;
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, palette.canvas());

        ui.spacing_mut().item_spacing = vec2(Space::SM, Space::SM);
        egui::Frame::new().inner_margin(Space::MD).show(ui, |ui| {
            ui.vertical(|ui| {
                self.header(ui, &theme);
                ui.add_space(Space::SM);
                ui.separator();
                ui.add_space(Space::SM);
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        vec2(FILTER_WIDTH, ui.available_height()),
                        Layout::top_down(Align::Min),
                        |ui| self.filter_rail(ui, &theme),
                    );
                    ui.separator();

                    let detail_open = self.selected_entry().is_some();
                    let reserved = if detail_open {
                        DETAIL_WIDTH + Space::LG
                    } else {
                        0.0
                    };
                    let grid_width = (ui.available_width() - reserved).max(CARD_WIDTH);
                    ui.allocate_ui_with_layout(
                        vec2(grid_width, ui.available_height()),
                        Layout::top_down(Align::Min),
                        |ui| self.results(ui, &theme),
                    );

                    if let Some(entry) = self.selected_entry() {
                        ui.separator();
                        ui.allocate_ui_with_layout(
                            vec2(DETAIL_WIDTH, ui.available_height()),
                            Layout::top_down(Align::Min),
                            |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt("history-detail-scroll")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| self.detail(ui, &theme, &entry));
                            },
                        );
                    }
                });
            });
        });
    }

    fn header(&mut self, ui: &mut Ui, theme: &Theme) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Capture history")
                        .font(theme.font(Text::Display))
                        .color(theme.palette.text),
                );
                let label = if self.total == 1 {
                    "1 capture".to_owned()
                } else {
                    format!("{} captures", self.total)
                };
                ui.label(
                    RichText::new(label)
                        .font(theme.font(Text::Caption))
                        .color(theme.palette.text_muted),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    self.close();
                }
                ui.add_sized(
                    [300.0, 36.0],
                    egui::TextEdit::singleline(&mut self.filters.text)
                        .hint_text("Search titles, apps, and text...")
                        .font(theme.font(Text::Body)),
                );
                if ui
                    .add(egui::Button::new("Search").min_size(vec2(72.0, 36.0)))
                    .clicked()
                {
                    self.reload_from_start();
                }
            });
        });
    }

    fn filter_rail(&mut self, ui: &mut Ui, theme: &Theme) {
        section_label(ui, theme, "MEDIA");
        if selectable(ui, theme, "All captures", self.filters.media_kind.is_none()) {
            self.filters.media_kind = None;
            self.reload_from_start();
        }
        for &kind in MediaKind::all() {
            if selectable(
                ui,
                theme,
                kind.plural(),
                self.filters.media_kind == Some(kind),
            ) {
                self.filters.media_kind = Some(kind);
                self.reload_from_start();
            }
        }

        ui.add_space(Space::LG);
        section_label(ui, theme, "WHEN");
        egui::ComboBox::from_id_salt("history-date")
            .selected_text(self.filters.date.label())
            .width(FILTER_WIDTH - Space::LG)
            .show_ui(ui, |ui| {
                for &date in DateFilter::ALL {
                    if ui
                        .selectable_value(&mut self.filters.date, date, date.label())
                        .changed()
                    {
                        self.reload_from_start();
                    }
                }
            });

        ui.add_space(Space::LG);
        section_label(ui, theme, "APPLICATION");
        egui::ComboBox::from_id_salt("history-app")
            .selected_text(self.filters.app_name.as_deref().unwrap_or("Every app"))
            .width(FILTER_WIDTH - Space::LG)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_value(&mut self.filters.app_name, None, "Every app")
                    .changed()
                {
                    self.reload_from_start();
                }
                for app in self.apps.clone() {
                    if ui
                        .selectable_value(&mut self.filters.app_name, Some(app.clone()), &app)
                        .changed()
                    {
                        self.reload_from_start();
                    }
                }
            });

        ui.add_space(Space::LG);
        if ui
            .checkbox(&mut self.filters.pinned_only, "Pinned only")
            .changed()
        {
            self.reload_from_start();
        }
    }

    fn results(&mut self, ui: &mut Ui, theme: &Theme) {
        if let Some(error) = &self.error {
            state_panel(
                ui,
                theme,
                "History could not be loaded",
                error,
                Some("Try again"),
            );
            if ui.button("Try again").clicked() {
                self.queue_query();
            }
            return;
        }
        if self.loading && self.entries.is_empty() {
            state_panel(
                ui,
                theme,
                "Looking through your captures...",
                "Large libraries can take a moment the first time.",
                None,
            );
            return;
        }
        if self.entries.is_empty() {
            let filtered = self.filters != HistoryFilters::default();
            state_panel(
                ui,
                theme,
                if filtered {
                    "No captures match these filters"
                } else {
                    "Your captures will collect here"
                },
                if filtered {
                    "Clear a filter or try a broader search."
                } else {
                    "Take a screenshot and it will remain editable from history."
                },
                None,
            );
            return;
        }

        let columns = ((ui.available_width() + Space::MD) / (CARD_WIDTH + Space::MD))
            .floor()
            .max(1.0) as usize;
        egui::ScrollArea::vertical()
            .id_salt("history-grid-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("history-grid")
                    .num_columns(columns)
                    .spacing(vec2(Space::MD, Space::MD))
                    .show(ui, |ui| {
                        for index in 0..self.entries.len() {
                            let entry = self.entries[index].clone();
                            self.capture_card(ui, theme, &entry);
                            if (index + 1) % columns == 0 {
                                ui.end_row();
                            }
                        }
                    });
                ui.add_space(Space::LG);
                self.pagination(ui, theme);
            });
    }

    fn capture_card(&mut self, ui: &mut Ui, theme: &Theme, entry: &HistoryEntry) {
        let (rect, response) =
            ui.allocate_exact_size(vec2(CARD_WIDTH, CARD_HEIGHT), Sense::click());
        let selected = self.selected.as_ref() == Some(&entry.id);
        let palette = theme.palette;
        let fill = if selected {
            palette.card_fill_raised
        } else {
            palette.card_fill
        };
        ui.painter().rect_filled(rect, corner(Radius::CARD), fill);
        ui.painter().rect_stroke(
            rect,
            corner(Radius::CARD),
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

        let thumb_rect = Rect::from_min_max(
            rect.min + vec2(Space::XS, Space::XS),
            pos2(rect.right() - Space::XS, rect.top() + THUMB_HEIGHT),
        );
        self.paint_thumbnail(ui, theme, entry, thumb_rect);
        self.paint_capture_badges(ui, theme, entry, thumb_rect);

        let text_x = rect.left() + Space::MD;
        let title_y = thumb_rect.bottom() + Space::SM;
        ui.painter().text(
            pos2(text_x, title_y),
            Align2::LEFT_TOP,
            truncate(entry.primary_label(), 28),
            theme.font(Text::Label),
            palette.text,
        );
        ui.painter().text(
            pos2(text_x, title_y + 22.0),
            Align2::LEFT_TOP,
            relative_time(entry.created_at, self.now),
            theme.font(Text::Caption),
            palette.text_muted,
        );
        if entry.pinned {
            ui.painter().text(
                pos2(rect.right() - Space::MD, title_y + 22.0),
                Align2::RIGHT_TOP,
                "PINNED",
                theme.font(Text::Caption),
                palette.accent,
            );
        }

        if response.clicked() {
            self.selected = Some(entry.id.clone());
            self.delete_armed = None;
        }
    }

    fn paint_capture_badges(&self, ui: &Ui, theme: &Theme, entry: &HistoryEntry, rect: Rect) {
        if let Some(app) = entry.app_name.as_deref() {
            let label = truncate(app, 16);
            let width =
                (44.0 + label.chars().count() as f32 * 6.5).min(rect.width() - 2.0 * Space::SM);
            let badge = Rect::from_min_size(
                pos2(rect.left() + Space::SM, rect.bottom() - 30.0),
                vec2(width, 24.0),
            );
            ui.painter()
                .rect_filled(badge, badge.height() / 2.0, Color32::from_black_alpha(196));
            let icon = pos2(badge.left() + 12.0, badge.center().y);
            ui.painter().circle_filled(icon, 8.0, theme.palette.accent);
            ui.painter().text(
                icon,
                Align2::CENTER_CENTER,
                app_monogram(app),
                theme.font(Text::Caption),
                theme.palette.on_accent,
            );
            ui.painter().text(
                pos2(badge.left() + 25.0, badge.center().y),
                Align2::LEFT_CENTER,
                label,
                theme.font(Text::Caption),
                Color32::WHITE,
            );
        }

        if entry.media_kind != MediaKind::Screenshot {
            let label = match entry.media_kind {
                MediaKind::Screenshot => "",
                MediaKind::Video => "VIDEO",
                MediaKind::Gif => "GIF",
            };
            let width = if entry.media_kind == MediaKind::Video {
                54.0
            } else {
                38.0
            };
            let badge = Rect::from_min_size(
                pos2(rect.right() - width - Space::SM, rect.top() + Space::SM),
                vec2(width, 22.0),
            );
            ui.painter()
                .rect_filled(badge, badge.height() / 2.0, Color32::from_black_alpha(196));
            ui.painter().text(
                badge.center(),
                Align2::CENTER_CENTER,
                label,
                theme.font(Text::Caption),
                Color32::WHITE,
            );
        }
    }

    fn paint_thumbnail(&mut self, ui: &mut Ui, theme: &Theme, entry: &HistoryEntry, rect: Rect) {
        ui.painter()
            .rect_filled(rect, corner(Radius::THUMB), theme.palette.card_fill_raised);
        let Some(thumbnail) = &entry.thumbnail else {
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                if entry.image_present {
                    "Preview unavailable"
                } else {
                    "Source image evicted\nEdits are still saved"
                },
                theme.font(Text::Caption),
                theme.palette.text_muted,
            );
            return;
        };

        let texture = self.textures.entry(entry.id.0.clone()).or_insert_with(|| {
            ui.ctx().load_texture(
                format!("history/{}", entry.id.0),
                egui::ColorImage::from_rgba_unmultiplied(
                    [thumbnail.width as usize, thumbnail.height as usize],
                    thumbnail.pixels(),
                ),
                TextureOptions::LINEAR,
            )
        });
        let fitted = fit_rect(rect, vec2(thumbnail.width as f32, thumbnail.height as f32));
        ui.painter().image(
            texture.id(),
            fitted,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    fn pagination(&mut self, ui: &mut Ui, theme: &Theme) {
        let first = u64::from(self.offset) + 1;
        let last = (u64::from(self.offset) + self.entries.len() as u64).min(self.total);
        ui.horizontal(|ui| {
            let previous = ui.add_enabled(self.offset > 0, egui::Button::new("Previous"));
            if previous.clicked() {
                self.previous_page();
            }
            ui.label(
                RichText::new(format!("{first}-{last} of {}", self.total))
                    .font(theme.font(Text::Caption))
                    .color(theme.palette.text_muted),
            );
            let next = ui.add_enabled(
                u64::from(self.offset.saturating_add(self.limit)) < self.total,
                egui::Button::new("Next"),
            );
            if next.clicked() {
                self.next_page();
            }
        });
    }

    fn detail(&mut self, ui: &mut Ui, theme: &Theme, entry: &HistoryEntry) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Capture")
                    .font(theme.font(Text::Title))
                    .color(theme.palette.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .small_button("x")
                    .on_hover_text("Close details")
                    .clicked()
                {
                    self.selected = None;
                    self.delete_armed = None;
                }
            });
        });
        ui.add_space(Space::SM);
        let (preview, drag) =
            ui.allocate_exact_size(vec2(ui.available_width(), 208.0), Sense::click_and_drag());
        self.paint_thumbnail(ui, theme, entry, preview);
        if drag.drag_started()
            && let Some(pointer) = drag.interact_pointer_pos()
        {
            self.native_drag_intents.push_back(entry.id.clone());
            let viewport_origin = ui.ctx().input(|input| {
                input
                    .viewport()
                    .inner_rect
                    .map_or(pos2(0.0, 0.0), |rect| rect.min)
            });
            self.actions.push_back(HistoryAction::Drag {
                id: entry.id.clone(),
                rect: LogicalRect::new(
                    LogicalPoint::new(
                        f64::from(viewport_origin.x + preview.left()),
                        f64::from(viewport_origin.y + preview.top()),
                    ),
                    LogicalSize::new(f64::from(preview.width()), f64::from(preview.height())),
                ),
                pointer: LogicalPoint::new(
                    f64::from(viewport_origin.x + pointer.x),
                    f64::from(viewport_origin.y + pointer.y),
                ),
            });
        }
        ui.add_space(Space::MD);
        ui.label(
            RichText::new(entry.primary_label())
                .font(theme.font(Text::Title))
                .color(theme.palette.text),
        );
        ui.label(
            RichText::new(entry.secondary_label())
                .font(theme.font(Text::Caption))
                .color(theme.palette.text_muted),
        );
        ui.label(
            RichText::new(relative_time(entry.created_at, self.now))
                .font(theme.font(Text::Caption))
                .color(theme.palette.text_muted),
        );

        let mut facts = Vec::new();
        if entry.annotation_count > 0 {
            facts.push(format!("{} annotations", entry.annotation_count));
        }
        if entry.ocr_text.as_ref().is_some_and(|text| !text.is_empty()) {
            facts.push("searchable text".to_owned());
        }
        if !entry.image_present {
            facts.push("source image evicted".to_owned());
        }
        if !facts.is_empty() {
            ui.add_space(Space::SM);
            ui.label(
                RichText::new(facts.join("  ·  "))
                    .font(theme.font(Text::Caption))
                    .color(theme.palette.text_muted),
            );
        }

        ui.add_space(Space::LG);
        let enabled = entry.image_present;
        if ui
            .add_enabled(
                enabled,
                egui::Button::new("Open editor").min_size(vec2(ui.available_width(), 36.0)),
            )
            .clicked()
        {
            self.actions
                .push_back(HistoryAction::OpenEditor(entry.id.clone()));
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(enabled, egui::Button::new("Restore"))
                .clicked()
            {
                self.actions
                    .push_back(HistoryAction::Restore(entry.id.clone()));
            }
            if ui.add_enabled(enabled, egui::Button::new("Copy")).clicked() {
                self.actions
                    .push_back(HistoryAction::Copy(entry.id.clone()));
            }
            if ui.add_enabled(enabled, egui::Button::new("Save")).clicked() {
                self.actions
                    .push_back(HistoryAction::Save(entry.id.clone()));
            }
        });
        ui.label(
            RichText::new("Drag the preview into another app")
                .font(theme.font(Text::Caption))
                .color(theme.palette.text_faint),
        );
        if ui
            .button(if entry.pinned { "Unpin" } else { "Pin" })
            .clicked()
        {
            self.actions.push_back(HistoryAction::SetPinned {
                id: entry.id.clone(),
                pinned: !entry.pinned,
            });
        }

        ui.add_space(Space::XL);
        ui.vertical(|ui| {
            let armed = self.delete_armed.as_ref() == Some(&entry.id);
            let label = if armed {
                "Delete permanently"
            } else {
                "Delete capture..."
            };
            let danger = if theme.palette.is_dark() {
                Color32::from_rgb(0xFF, 0x8A, 0x8A)
            } else {
                Color32::from_rgb(0xB8, 0x23, 0x36)
            };
            if ui
                .add(egui::Button::new(RichText::new(label).color(danger)))
                .clicked()
            {
                if armed {
                    self.actions
                        .push_back(HistoryAction::Delete(entry.id.clone()));
                } else {
                    self.delete_armed = Some(entry.id.clone());
                }
            }
            if armed {
                ui.label(
                    RichText::new("This removes the image, edits, and searchable text.")
                        .font(theme.font(Text::Caption))
                        .color(theme.palette.text_muted),
                );
            }
            if let Some(notice) = &self.notice {
                ui.label(
                    RichText::new(notice)
                        .font(theme.font(Text::Caption))
                        .color(theme.palette.accent),
                );
            }
        });
    }
}

/// The ordinary desktop window used for capture history.
pub const WINDOW_TITLE: &str = "Scrozz Capture History";

/// The ordinary desktop window used for capture history.
#[must_use]
pub fn viewport_builder() -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title(WINDOW_TITLE)
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([820.0, 560.0])
        .with_resizable(true)
        .with_decorations(true)
        .with_taskbar(true)
        .with_active(true)
        .with_window_level(WindowLevel::Normal)
}

/// Stable identity of the history viewport.
#[must_use]
pub fn viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("scrozz-capture-history")
}

/// Installs the shared type and widget system for a standalone history render.
pub fn setup(ctx: &egui::Context, appearance: Appearance) {
    let theme = Theme::for_appearance(appearance);
    install_fonts(ctx);
    install_style(ctx, &theme);
}

fn theme_for(ui: &Ui) -> Theme {
    Theme::for_appearance(if ui.visuals().dark_mode {
        Appearance::Dark
    } else {
        Appearance::Light
    })
}

fn section_label(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme.font(Text::Caption))
            .color(theme.palette.text_faint),
    );
}

fn selectable(ui: &mut Ui, theme: &Theme, label: &str, selected: bool) -> bool {
    ui.add_sized(
        [FILTER_WIDTH - Space::LG, 30.0],
        egui::Button::new(
            RichText::new(label)
                .font(theme.font(Text::Body))
                .color(if selected {
                    theme.palette.on_accent
                } else {
                    theme.palette.text_muted
                }),
        )
        .selected(selected),
    )
    .clicked()
}

fn state_panel(ui: &mut Ui, theme: &Theme, title: &str, detail: &str, _action: Option<&str>) {
    ui.with_layout(Layout::top_down_justified(Align::Center), |ui| {
        ui.add_space(120.0);
        ui.label(
            RichText::new(title)
                .font(theme.font(Text::Display))
                .color(theme.palette.text),
        );
        ui.label(
            RichText::new(detail)
                .font(theme.font(Text::Subtitle))
                .color(theme.palette.text_muted),
        );
    });
}

fn fit_rect(bounds: Rect, source: Vec2) -> Rect {
    if source.x <= 0.0 || source.y <= 0.0 {
        return bounds;
    }
    let scale = (bounds.width() / source.x)
        .min(bounds.height() / source.y)
        .max(0.0);
    Rect::from_center_size(bounds.center(), source * scale)
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

fn app_monogram(name: &str) -> String {
    name.chars()
        .find(|character| character.is_alphanumeric())
        .map(|character| character.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_owned())
}

/// A frozen relative-time label used by both live history and goldens.
#[must_use]
pub fn relative_time(then: Timestamp, now: Timestamp) -> String {
    let elapsed = now.as_millis().saturating_sub(then.as_millis()).max(0);
    let minutes = elapsed / 60_000;
    if minutes < 1 {
        return "Just now".to_owned();
    }
    if minutes < 60 {
        return format!("{minutes} min ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} hr ago");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days} days ago");
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months} months ago");
    }
    format!("{} years ago", months / 12)
}

/// The real history surface used by the screenshot harness.
#[derive(Debug, Clone, Copy, Default)]
pub struct HistoryScene;

impl crate::harness::Scene for HistoryScene {
    fn name(&self) -> &str {
        "capture-history"
    }

    fn setup(&self, ctx: &egui::Context) {
        install_fonts(ctx);
    }

    fn ui(&self, ui: &mut Ui, ctx: &crate::harness::SceneCtx<'_>) {
        let now = Timestamp(crate::harness::FROZEN_EPOCH_UNIX * 1_000);
        let page = fixture_page(ctx.fixture.scenario, ctx.seed, now);
        let selected = page.entries.get(1).map(|entry| entry.id.clone());
        let mut model = HistoryViewModel::loaded(now, page);
        if ctx.fixture.scenario == crate::harness::Scenario::HistoryDetail
            && let Some(id) = selected
        {
            model.select(&id);
        }
        model.ui(ui);
    }
}

fn fixture_page(scenario: crate::harness::Scenario, seed: u64, now: Timestamp) -> HistoryPage {
    let entries = if scenario == crate::harness::Scenario::HistoryEmpty {
        Vec::new()
    } else {
        const TITLES: &[(&str, &str)] = &[
            ("Design handoff — checkout flow", "Figma"),
            ("Invoice 1048 — Northwind", "Preview"),
            ("Release checklist", "Notion"),
            ("Dashboard performance", "Safari"),
            ("Color exploration", "Figma"),
            ("Build passed", "Terminal"),
            ("Customer notes", "Slack"),
            ("Map references", "Safari"),
        ];
        const AGES: &[i64] = &[
            2 * 60_000,
            47 * 60_000,
            3 * 3_600_000,
            DAY_MILLIS,
            3 * DAY_MILLIS,
            8 * DAY_MILLIS,
            40 * DAY_MILLIS,
            400 * DAY_MILLIS,
        ];
        TITLES
            .iter()
            .enumerate()
            .map(|(index, &(title, app))| {
                let media_kind = match index {
                    3 => MediaKind::Video,
                    5 => MediaKind::Gif,
                    _ => MediaKind::Screenshot,
                };
                let present = index != 6;
                HistoryEntry {
                    id: CaptureId(format!("fixture-{index:02}")),
                    created_at: Timestamp(now.0.saturating_sub(AGES[index])),
                    media_kind,
                    pinned: matches!(index, 1 | 4),
                    app_name: Some(app.to_owned()),
                    window_title: Some(title.to_owned()),
                    width: if index % 3 == 0 { 1920 } else { 1440 },
                    height: if index % 3 == 0 { 1080 } else { 900 },
                    scale: 2.0,
                    image_present: present,
                    annotation_count: usize::from(index == 1) * 3,
                    ocr_text: (index == 1).then(|| "Invoice total $1,248.00".to_owned()),
                    thumbnail: present.then(|| fixture_thumbnail(seed, index as u64)),
                }
            })
            .collect()
    };
    let total = entries.len() as u64;
    HistoryPage {
        entries,
        total,
        apps: vec![
            "Figma".to_owned(),
            "Notion".to_owned(),
            "Preview".to_owned(),
            "Safari".to_owned(),
            "Slack".to_owned(),
            "Terminal".to_owned(),
        ],
        offset: 0,
        limit: PAGE_SIZE,
    }
}

fn fixture_thumbnail(seed: u64, tag: u64) -> HistoryThumbnail {
    const WIDTH: u32 = 176;
    const HEIGHT: u32 = 108;
    let shifted = seed.rotate_left((tag as u32 * 7) % 63);
    let base = [
        48 + (shifted & 0x3f) as u8,
        62 + ((shifted >> 8) & 0x4f) as u8,
        86 + ((shifted >> 16) & 0x5f) as u8,
    ];
    let accent = [
        120 + ((shifted >> 24) & 0x6f) as u8,
        90 + ((shifted >> 32) & 0x6f) as u8,
        150 + ((shifted >> 40) & 0x5f) as u8,
    ];
    let mut pixels = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let mix = ((x + y / 2 + tag as u32 * 13) % WIDTH) as f32 / WIDTH as f32;
            for channel in 0..3 {
                pixels.push(
                    (f32::from(base[channel]) * (1.0 - mix) + f32::from(accent[channel]) * mix)
                        .round() as u8,
                );
            }
            pixels.push(255);
        }
    }
    HistoryThumbnail::from_rgba(WIDTH, HEIGHT, pixels)
        .expect("the fixture thumbnail has exact RGBA geometry")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Timestamp = Timestamp(1_735_689_600_000);

    #[test]
    fn filters_resolve_to_one_store_query() {
        let filters = HistoryFilters {
            text: "  invoice  ".to_owned(),
            media_kind: Some(MediaKind::Screenshot),
            app_name: Some("Preview".to_owned()),
            date: DateFilter::LastSevenDays,
            pinned_only: true,
        };
        let query = filters.query(NOW, 48, PAGE_SIZE);
        assert_eq!(query.text.as_deref(), Some("invoice"));
        assert_eq!(query.app_name.as_deref(), Some("Preview"));
        assert_eq!(query.media_kind, Some(MediaKind::Screenshot));
        assert!(query.pinned_only);
        assert_eq!(query.page, Page::new(PAGE_SIZE, 48));
        assert_eq!(query.created_after, Some(Timestamp(NOW.0 - 7 * DAY_MILLIS)));
    }

    #[test]
    fn stale_pages_cannot_replace_new_filters() {
        let mut model = HistoryViewModel::new(NOW);
        model.open(NOW);
        let first = model.request;
        model.set_filters(HistoryFilters {
            text: "new".to_owned(),
            ..HistoryFilters::default()
        });
        let second = model.request;
        assert!(second > first);

        model.apply_page(
            first,
            HistoryPage {
                entries: Vec::new(),
                total: 99,
                apps: Vec::new(),
                offset: 0,
                limit: PAGE_SIZE,
            },
        );
        assert_eq!(model.total, 0);
        assert!(model.loading);
    }

    #[test]
    fn frozen_relative_labels_do_not_read_the_wall_clock() {
        assert_eq!(relative_time(NOW, NOW), "Just now");
        assert_eq!(
            relative_time(Timestamp(NOW.0 - 2 * 60_000), NOW),
            "2 min ago"
        );
        assert_eq!(
            relative_time(Timestamp(NOW.0 - 8 * DAY_MILLIS), NOW),
            "8 days ago"
        );
    }

    #[test]
    fn thumbnail_geometry_is_validated() {
        assert!(HistoryThumbnail::from_rgba(2, 2, vec![0; 16]).is_some());
        assert!(HistoryThumbnail::from_rgba(2, 2, vec![0; 15]).is_none());
        assert!(HistoryThumbnail::from_rgba(0, 2, Vec::new()).is_none());
    }

    #[test]
    fn opening_queues_a_query_with_a_generation() {
        let mut model = HistoryViewModel::new(NOW);
        model.open(NOW);
        assert!(model.visible);
        assert!(matches!(
            model.drain_actions().as_slice(),
            [HistoryAction::Query {
                request: 1,
                query: _
            }]
        ));
    }

    #[test]
    fn pinning_updates_the_loaded_capture_and_refreshes_the_page() {
        let page = fixture_page(crate::harness::Scenario::HistoryGrid, 7, NOW);
        let id = page.entries[0].id.clone();
        let mut model = HistoryViewModel::loaded(NOW, page);

        model.pinned(&id, true);

        assert!(
            model
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .expect("entry")
                .pinned
        );
        assert!(matches!(
            model.drain_actions().as_slice(),
            [HistoryAction::Query { request: 2, .. }]
        ));
    }

    #[test]
    fn an_in_flight_page_cannot_undo_a_completed_pin() {
        let page = fixture_page(crate::harness::Scenario::HistoryGrid, 7, NOW);
        let stale_page = page.clone();
        let id = page.entries[0].id.clone();
        let mut model = HistoryViewModel::loaded(NOW, page);

        model.refresh_current(NOW);
        assert!(matches!(
            model.drain_actions().as_slice(),
            [HistoryAction::Query { request: 2, .. }]
        ));
        model.pinned(&id, true);
        assert!(matches!(
            model.drain_actions().as_slice(),
            [HistoryAction::Query { request: 3, .. }]
        ));
        model.apply_page(2, stale_page);

        assert!(
            model
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .expect("entry")
                .pinned
        );
        assert!(model.loading);
    }

    #[test]
    fn unpinning_inside_the_pinned_filter_removes_and_refills_the_row() {
        let page = fixture_page(crate::harness::Scenario::HistoryGrid, 7, NOW);
        let id = page
            .entries
            .iter()
            .find(|entry| entry.pinned)
            .expect("pinned fixture")
            .id
            .clone();
        let mut model = HistoryViewModel::loaded(NOW, page);
        model.filters.pinned_only = true;

        model.pinned(&id, false);

        assert!(model.entries.iter().all(|entry| entry.id != id));
        assert!(matches!(
            model.drain_actions().as_slice(),
            [HistoryAction::Query { request: 2, .. }]
        ));
    }

    #[test]
    fn the_live_clock_refreshes_rolling_date_queries_once_a_minute() {
        let page = fixture_page(crate::harness::Scenario::HistoryGrid, 7, NOW);
        let mut model = HistoryViewModel::loaded(NOW, page);
        model.set_filters(HistoryFilters {
            date: DateFilter::Today,
            ..HistoryFilters::default()
        });
        model.drain_actions();

        model.advance_clock(Timestamp(NOW.0 + 59_999));
        assert!(model.drain_actions().is_empty());
        model.advance_clock(Timestamp(NOW.0 + 60_000));

        assert!(matches!(
            model.drain_actions().as_slice(),
            [HistoryAction::Query { query, .. }]
                if query.created_after == Some(Timestamp(NOW.0 + 60_000 - DAY_MILLIS))
        ));
    }

    #[test]
    fn deleting_the_only_item_on_a_later_page_moves_back_and_refills_it() {
        let mut entry = fixture_page(crate::harness::Scenario::HistoryGrid, 7, NOW)
            .entries
            .remove(0);
        entry.id = CaptureId("last-on-page".to_owned());
        let mut model = HistoryViewModel::loaded(
            NOW,
            HistoryPage {
                entries: vec![entry.clone()],
                total: 25,
                apps: vec!["Figma".to_owned()],
                offset: PAGE_SIZE,
                limit: PAGE_SIZE,
            },
        );
        model.select(&entry.id);

        model.deleted(&entry.id);

        assert_eq!(model.offset, 0);
        assert!(model.selected.is_none());
        assert!(matches!(
            model.drain_actions().as_slice(),
            [HistoryAction::Query {
                request: 2,
                query
            }] if query.page == Page::new(PAGE_SIZE, 0)
        ));
    }

    #[test]
    fn next_page_queues_the_expected_offset() {
        let mut page = fixture_page(crate::harness::Scenario::HistoryGrid, 7, NOW);
        page.total = 50;
        let mut model = HistoryViewModel::loaded(NOW, page);

        model.next_page();

        assert!(matches!(
            model.drain_actions().as_slice(),
            [HistoryAction::Query {
                request: 2,
                query
            }] if query.page == Page::new(PAGE_SIZE, PAGE_SIZE)
        ));
    }

    #[test]
    fn stale_query_errors_cannot_cover_a_newer_page() {
        let mut model = HistoryViewModel::new(NOW);
        model.open(NOW);
        model.set_filters(HistoryFilters {
            text: "newer".to_owned(),
            ..HistoryFilters::default()
        });

        model.apply_query_error(1, "old request failed");

        assert!(model.error.is_none());
        assert!(model.loading);
    }

    #[test]
    fn a_concurrently_shrunk_later_page_requeries_the_last_valid_offset() {
        let mut model = HistoryViewModel::new(NOW);
        model.open(NOW);
        model.drain_actions();
        model.request = 4;

        model.apply_page(
            4,
            HistoryPage {
                entries: Vec::new(),
                total: 7,
                apps: vec!["Preview".to_owned()],
                offset: PAGE_SIZE,
                limit: PAGE_SIZE,
            },
        );

        assert_eq!(model.offset, 0);
        assert!(model.loading);
        assert!(matches!(
            model.drain_actions().as_slice(),
            [HistoryAction::Query {
                request: 5,
                query
            }] if query.page == Page::new(PAGE_SIZE, 0)
        ));
    }

    #[test]
    fn application_badges_use_a_stable_monogram() {
        assert_eq!(app_monogram("Figma"), "F");
        assert_eq!(app_monogram("  preview"), "P");
        assert_eq!(app_monogram("---"), "?");
    }

    #[test]
    fn history_uses_an_ordinary_movable_desktop_viewport() {
        let viewport = viewport_builder();
        assert_eq!(viewport.decorations, Some(true));
        assert_eq!(viewport.taskbar, Some(true));
        assert_eq!(viewport.resizable, Some(true));
        assert_eq!(viewport.active, Some(true));
        assert_eq!(viewport.window_level, Some(WindowLevel::Normal));
        assert_eq!(
            viewport.position, None,
            "the window manager chooses its position"
        );
        assert_ne!(viewport.transparent, Some(true));
    }
}
