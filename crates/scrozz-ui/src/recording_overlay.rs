//! Interactive target and region selection controls for recording.

use egui::{
    Align2, DragValue, Response, RichText, Sense, Stroke, StrokeKind, Ui, WidgetInfo, WidgetType,
    vec2,
};
use scrozz_core::{CaptureTarget, LogicalRect, LogicalSize};
use scrozz_record::selection::{
    AspectRatio, LastSelectionMemory, SelectionConstraints, SelectionMode,
};

use crate::{
    harness::{RecordingFixture, Scene, SceneCtx},
    recording_controls::{
        body, button, caption, choice, heading, install_scene_theme, panel, rule, scene_theme,
        section_label,
    },
    theme::{Radius, Space, Text, Theme, corner},
};

/// Caller-owned selection values edited by [`RecordingOverlay`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingSelectionState {
    /// Click-or-drag All-in-One behavior, or region-only behavior.
    pub mode: SelectionMode,
    /// Exact-size and aspect-ratio geometry constraints.
    pub constraints: SelectionConstraints,
    /// Real target resolved by the caller from the current pointer gesture.
    pub candidate: Option<CaptureTarget>,
}

impl Default for RecordingSelectionState {
    fn default() -> Self {
        Self {
            mode: SelectionMode::AllInOne,
            constraints: SelectionConstraints::NONE,
            candidate: None,
        }
    }
}

/// Immutable context around the editable selection values.
#[derive(Debug, Clone)]
pub struct RecordingOverlayModel<'a> {
    /// Values to show and edit this pass.
    pub state: RecordingSelectionState,
    /// Available logical desktop used only to scale the preview and bound fields.
    pub desktop_bounds: LogicalRect,
    /// Current real drag region, if a platform input adapter has one.
    pub drag_preview: Option<LogicalRect>,
    /// Human-readable hint from real target enumeration.
    pub target_hint: Option<&'a str>,
    /// Persistent memory supplied by the recording domain.
    pub last_selection: &'a LastSelectionMemory,
}

/// Semantic selector action.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordingOverlayAction {
    /// Selection mode changed; any previously resolved candidate is stale.
    ModeChanged(SelectionMode),
    /// Geometry constraints changed; the caller should re-resolve its gesture.
    ConstraintsChanged(SelectionConstraints),
    /// The remembered concrete region was requested.
    ReuseLastSelection(CaptureTarget),
    /// Confirm the exact real target supplied by the caller.
    Confirm(CaptureTarget),
    /// Leave target selection without recording.
    Cancel,
}

/// Enabled and validity state for selector controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingOverlayControls {
    /// Whether a remembered region can be reused.
    pub reuse_enabled: bool,
    /// Whether the current real target can be confirmed.
    pub confirm_enabled: bool,
    /// Actionable geometry validation message.
    pub validation_error: Option<String>,
}

/// Result of drawing a [`RecordingOverlay`].
#[derive(Debug)]
pub struct RecordingOverlayResponse {
    /// Updated value model for the caller to retain.
    pub state: RecordingSelectionState,
    /// Semantic actions requested during this pass.
    pub actions: Vec<RecordingOverlayAction>,
    /// Derived enabled and validity state.
    pub controls: RecordingOverlayControls,
    /// Response for the entire selector panel.
    pub response: Response,
    /// Reuse control response.
    pub reuse_response: Response,
    /// Confirm control response.
    pub confirm_response: Response,
    /// Cancel control response.
    pub cancel_response: Response,
}

/// All-in-One recording target and exact-region selector.
pub struct RecordingOverlay<'a> {
    model: RecordingOverlayModel<'a>,
    theme: &'a Theme,
}

impl<'a> RecordingOverlay<'a> {
    /// Creates a selector from caller-owned values.
    #[must_use]
    pub const fn new(model: RecordingOverlayModel<'a>, theme: &'a Theme) -> Self {
        Self { model, theme }
    }

    /// Draws the selector and returns updated values and semantic requests.
    pub fn show(self, ui: &mut Ui) -> RecordingOverlayResponse {
        let mut state = self.model.state;
        let mut actions = Vec::new();
        let mut reuse_response = None;
        let mut confirm_response = None;
        let mut cancel_response = None;
        let max_width = self.model.desktop_bounds.size.width.max(1.0);
        let max_height = self.model.desktop_bounds.size.height.max(1.0);

        let inner = panel(ui, self.theme, 520.0, |ui| {
            heading(ui, self.theme, "Choose what to record");
            body(
                ui,
                self.theme,
                "Click a window or display, or drag a precise region.",
            );
            ui.add_space(Space::MD);

            section_label(ui, self.theme, "Selection behavior");
            ui.horizontal(|ui| {
                for (mode, label) in [
                    (SelectionMode::AllInOne, "All-in-One"),
                    (SelectionMode::Region, "Region only"),
                ] {
                    if choice(ui, self.theme, label, state.mode == mode, true).clicked()
                        && state.mode != mode
                    {
                        state.mode = mode;
                        state.candidate = None;
                        actions.push(RecordingOverlayAction::ModeChanged(mode));
                    }
                }
            });
            caption(
                ui,
                self.theme,
                match state.mode {
                    SelectionMode::AllInOne => {
                        "A click keeps a real window or display target; a drag makes a region."
                    }
                    SelectionMode::Region => "Only a non-empty dragged region can be confirmed.",
                },
            );

            ui.add_space(Space::LG);
            draw_preview(
                ui,
                self.theme,
                self.model.desktop_bounds,
                self.model.drag_preview.or_else(|| candidate_region(&state)),
                self.model.target_hint,
                state.candidate.as_ref(),
            );

            ui.add_space(Space::LG);
            rule(ui, self.theme);
            ui.add_space(Space::MD);
            section_label(ui, self.theme, "Region geometry");

            let mut constraints_changed = false;
            let mut exact_enabled = state.constraints.exact_size.is_some();
            let exact_toggle = ui.checkbox(
                &mut exact_enabled,
                RichText::new("Exact size")
                    .font(self.theme.font(Text::Label))
                    .color(self.theme.palette.text),
            );
            if exact_toggle.changed() {
                constraints_changed = true;
                state.constraints.exact_size = exact_enabled.then(|| {
                    suggested_exact_size(
                        self.model.drag_preview.or_else(|| candidate_region(&state)),
                        self.model.last_selection.region(),
                        max_width,
                        max_height,
                    )
                });
            }

            if let Some(size) = state.constraints.exact_size {
                let mut width = size.width;
                let mut height = size.height;
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("W")
                            .font(self.theme.font(Text::Caption))
                            .color(self.theme.palette.text_faint),
                    );
                    let width_response = ui.add(
                        DragValue::new(&mut width)
                            .range(1.0..=max_width)
                            .speed(1.0)
                            .suffix(" pt"),
                    );
                    ui.label(
                        RichText::new("H")
                            .font(self.theme.font(Text::Caption))
                            .color(self.theme.palette.text_faint),
                    );
                    let height_response = ui.add(
                        DragValue::new(&mut height)
                            .range(1.0..=max_height)
                            .speed(1.0)
                            .suffix(" pt"),
                    );
                    if width_response.changed() {
                        if let Some(ratio) = state.constraints.aspect_ratio {
                            height = (width / ratio.value()).clamp(1.0, max_height);
                        }
                        constraints_changed = true;
                    } else if height_response.changed() {
                        if let Some(ratio) = state.constraints.aspect_ratio {
                            width = (height * ratio.value()).clamp(1.0, max_width);
                        }
                        constraints_changed = true;
                    }
                });
                state.constraints.exact_size = Some(LogicalSize::new(width, height));
            }

            let mut aspect_locked = state.constraints.aspect_ratio.is_some();
            let aspect_toggle = ui.checkbox(
                &mut aspect_locked,
                RichText::new("Lock aspect ratio")
                    .font(self.theme.font(Text::Label))
                    .color(self.theme.palette.text),
            );
            if aspect_toggle.changed() {
                state.constraints.aspect_ratio = if aspect_locked {
                    current_aspect(&state).or_else(|| AspectRatio::new(16.0, 9.0).ok())
                } else {
                    None
                };
                sync_exact_to_aspect(&mut state.constraints, max_width, max_height);
                constraints_changed = true;
            }

            ui.add_enabled_ui(aspect_locked, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (width, height, label) in [
                        (16.0, 9.0, "16:9"),
                        (4.0, 3.0, "4:3"),
                        (1.0, 1.0, "1:1"),
                        (9.0, 16.0, "9:16"),
                    ] {
                        let selected = state
                            .constraints
                            .aspect_ratio
                            .is_some_and(|ratio| (ratio.value() - width / height).abs() < 1.0e-6);
                        if choice(ui, self.theme, label, selected, aspect_locked).clicked()
                            && let Ok(ratio) = AspectRatio::new(width, height)
                        {
                            state.constraints.aspect_ratio = Some(ratio);
                            sync_exact_to_aspect(&mut state.constraints, max_width, max_height);
                            constraints_changed = true;
                        }
                    }
                });
            });

            if constraints_changed {
                state.candidate = None;
                actions.push(RecordingOverlayAction::ConstraintsChanged(
                    state.constraints,
                ));
            }

            let validation_error = state
                .constraints
                .validate()
                .err()
                .map(|error| error.to_string());
            if let Some(error) = &validation_error {
                ui.add_space(Space::SM);
                ui.colored_label(self.theme.palette.recording, error);
            }

            ui.add_space(Space::LG);
            rule(ui, self.theme);
            ui.add_space(Space::MD);
            let reuse_enabled = self.model.last_selection.region().is_some();
            ui.horizontal(|ui| {
                let reuse = button(ui, self.theme, "Use last region", false, reuse_enabled);
                if reuse.clicked()
                    && let Some(region) = self.model.last_selection.region()
                {
                    let target = CaptureTarget::Region(region);
                    state.candidate = Some(target.clone());
                    actions.push(RecordingOverlayAction::ReuseLastSelection(target));
                }
                reuse_response = Some(reuse);

                let cancel = button(ui, self.theme, "Cancel", false, true);
                if cancel.clicked() {
                    actions.push(RecordingOverlayAction::Cancel);
                }
                cancel_response = Some(cancel);

                let confirm_enabled =
                    validation_error.is_none() && candidate_is_valid(state.candidate.as_ref());
                let confirm = button(ui, self.theme, "Record selection", true, confirm_enabled);
                if confirm.clicked()
                    && let Some(target) = state.candidate.clone()
                {
                    actions.push(RecordingOverlayAction::Confirm(target));
                }
                confirm_response = Some(confirm);
            });
        });

        let validation_error = state
            .constraints
            .validate()
            .err()
            .map(|error| error.to_string());
        let controls = RecordingOverlayControls {
            reuse_enabled: self.model.last_selection.region().is_some(),
            confirm_enabled: validation_error.is_none()
                && candidate_is_valid(state.candidate.as_ref()),
            validation_error,
        };

        RecordingOverlayResponse {
            state,
            actions,
            controls,
            response: inner.response,
            reuse_response: reuse_response.expect("the selector always draws reuse"),
            confirm_response: confirm_response.expect("the selector always draws confirm"),
            cancel_response: cancel_response.expect("the selector always draws cancel"),
        }
    }
}

fn candidate_region(state: &RecordingSelectionState) -> Option<LogicalRect> {
    match state.candidate.as_ref() {
        Some(CaptureTarget::Region(region)) => Some(*region),
        _ => None,
    }
}

fn current_aspect(state: &RecordingSelectionState) -> Option<AspectRatio> {
    state
        .constraints
        .exact_size
        .or_else(|| candidate_region(state).map(|region| region.size))
        .and_then(|size| AspectRatio::new(size.width, size.height).ok())
}

fn suggested_exact_size(
    preview: Option<LogicalRect>,
    remembered: Option<LogicalRect>,
    max_width: f64,
    max_height: f64,
) -> LogicalSize {
    preview.or(remembered).map_or_else(
        || LogicalSize::new(max_width.min(1920.0), max_height.min(1080.0)),
        |region| region.size,
    )
}

fn sync_exact_to_aspect(constraints: &mut SelectionConstraints, max_width: f64, max_height: f64) {
    let (Some(mut size), Some(ratio)) = (constraints.exact_size, constraints.aspect_ratio) else {
        return;
    };
    size.height = size.width / ratio.value();
    if size.height > max_height {
        size.height = max_height;
        size.width = size.height * ratio.value();
    }
    if size.width > max_width {
        size.width = max_width;
        size.height = size.width / ratio.value();
    }
    constraints.exact_size = Some(LogicalSize::new(size.width, size.height));
}

fn candidate_is_valid(candidate: Option<&CaptureTarget>) -> bool {
    match candidate {
        Some(CaptureTarget::Region(region)) => {
            let values = [
                region.origin.x,
                region.origin.y,
                region.size.width,
                region.size.height,
            ];
            values.iter().all(|value| value.is_finite()) && !region.is_empty()
        }
        Some(CaptureTarget::Display(id)) => !id.0.is_empty(),
        Some(CaptureTarget::Window(id)) => !id.0.is_empty(),
        Some(CaptureTarget::AllDisplays) => true,
        None => false,
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn draw_preview(
    ui: &mut Ui,
    theme: &Theme,
    desktop: LogicalRect,
    region: Option<LogicalRect>,
    target_hint: Option<&str>,
    candidate: Option<&CaptureTarget>,
) {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), 168.0), Sense::hover());
    response.widget_info(|| {
        WidgetInfo::labeled(
            WidgetType::Image,
            true,
            target_hint.unwrap_or("Recording target preview"),
        )
    });
    let painter = ui.painter();
    painter.rect_filled(rect, corner(Radius::THUMB), theme.palette.chip_fill);
    painter.rect_stroke(
        rect,
        corner(Radius::THUMB),
        Stroke::new(1.0, theme.palette.thumb_border),
        StrokeKind::Inside,
    );

    let inner = rect.shrink(Space::MD);
    if let Some(region) = region
        && desktop.size.width > 0.0
        && desktop.size.height > 0.0
    {
        let sx = f64::from(inner.width()) / desktop.size.width;
        let sy = f64::from(inner.height()) / desktop.size.height;
        let scale = sx.min(sy);
        let desktop_width = (desktop.size.width * scale) as f32;
        let desktop_height = (desktop.size.height * scale) as f32;
        let desktop_rect =
            egui::Rect::from_center_size(inner.center(), vec2(desktop_width, desktop_height));
        let x = desktop_rect.left() + ((region.origin.x - desktop.origin.x) * scale) as f32;
        let y = desktop_rect.top() + ((region.origin.y - desktop.origin.y) * scale) as f32;
        let preview = egui::Rect::from_min_size(
            egui::pos2(x, y),
            vec2(
                (region.size.width * scale) as f32,
                (region.size.height * scale) as f32,
            ),
        )
        .intersect(desktop_rect);
        painter.rect_stroke(
            desktop_rect,
            corner(Radius::CHIP),
            Stroke::new(1.0, theme.palette.text_faint),
            StrokeKind::Inside,
        );
        painter.rect_filled(
            preview,
            corner(Radius::CHIP),
            theme.palette.accent.linear_multiply(0.22),
        );
        painter.rect_stroke(
            preview,
            corner(Radius::CHIP),
            Stroke::new(2.0, theme.palette.accent_hi),
            StrokeKind::Inside,
        );
        painter.text(
            preview.center(),
            Align2::CENTER_CENTER,
            format!("{:.0} × {:.0}", region.size.width, region.size.height),
            theme.font(Text::Label),
            theme.palette.text,
        );
        return;
    }

    let summary = target_hint.unwrap_or(match candidate {
        Some(CaptureTarget::Display(_)) => "Display selected",
        Some(CaptureTarget::Window(_)) => "Window selected",
        Some(CaptureTarget::AllDisplays) => "All displays selected",
        Some(CaptureTarget::Region(_)) => "Region selected",
        None => "Move the pointer over a target, or drag a region",
    });
    painter.text(
        inner.center(),
        Align2::CENTER_CENTER,
        summary,
        theme.font(Text::Body),
        theme.palette.text_muted,
    );
}

/// Real selection-overlay renderer used by the deterministic harness.
#[derive(Debug, Default)]
pub struct RecordingOverlayScene;

impl Scene for RecordingOverlayScene {
    fn name(&self) -> &str {
        "recording-selection-overlay"
    }

    fn setup(&self, ctx: &egui::Context) {
        install_scene_theme(ctx);
    }

    fn ui(&self, ui: &mut Ui, ctx: &SceneCtx<'_>) {
        let Some(RecordingFixture::Selection(fixture)) = ctx.fixture.recording.as_ref() else {
            return;
        };
        let theme = scene_theme(ctx.theme);
        ui.vertical_centered(|ui| {
            ui.add_space(Space::XXL);
            RecordingOverlay::new(
                RecordingOverlayModel {
                    state: fixture.state.clone(),
                    desktop_bounds: fixture.desktop_bounds,
                    drag_preview: fixture.drag_preview,
                    target_hint: fixture.target_hint.as_deref(),
                    last_selection: &fixture.last_selection,
                },
                &theme,
            )
            .show(ui);
        });
    }
}
