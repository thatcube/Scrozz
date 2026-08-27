//! Non-destructive recording playback, trimming, and export controls.

use std::time::Duration;

use egui::{
    ComboBox, ProgressBar, Response, RichText, Sense, Slider, Stroke, StrokeKind, Ui, vec2,
};
use scrozz_record::{
    edit::{ChannelBehavior, EditOutput, EditPlan, PlaybackState, VideoDocument},
    settings::{Quality, ResolutionCap},
    transcode::{TranscodeFailure, TranscodeOutput, TranscodeStatus},
};

use crate::{
    harness::{EditorExportFixture, RecordingFixture, Scene, SceneCtx},
    recording_controls::{
        body, button, caption, format_duration, heading, install_scene_theme, panel, rule,
        scene_theme, section_label,
    },
    theme::{Radius, Space, Text, Theme, corner},
};

/// Read-only transcoder state supplied by the caller.
#[derive(Debug, Clone, Copy)]
pub enum TranscodeView<'a> {
    /// No export has started.
    Idle,
    /// Active export and its last observed normalized progress.
    Running {
        /// Completed fraction in `0.0..=1.0`.
        progress: f32,
    },
    /// Export finished, with output metadata when already observed.
    Finished {
        /// Complete transcoder output.
        output: Option<&'a TranscodeOutput>,
    },
    /// Export failed, with structured failure when already observed.
    Failed {
        /// Failure and any salvageable output.
        failure: Option<&'a TranscodeFailure>,
    },
    /// Export cancellation completed.
    Cancelled,
}

impl<'a> TranscodeView<'a> {
    /// Adapts the core job status plus the caller's last event values.
    #[must_use]
    pub fn from_status(
        status: TranscodeStatus,
        progress: f32,
        output: Option<&'a TranscodeOutput>,
        failure: Option<&'a TranscodeFailure>,
    ) -> Self {
        match status {
            TranscodeStatus::Running { .. } => Self::Running { progress },
            TranscodeStatus::Finished => Self::Finished { output },
            TranscodeStatus::Failed => Self::Failed { failure },
            TranscodeStatus::Cancelled => Self::Cancelled,
        }
    }

    /// Whether a transcoder currently owns the plan.
    #[must_use]
    pub const fn is_running(self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

/// Immutable source plus value edit plan shown by [`VideoEditor`].
#[derive(Debug, Clone, Copy)]
pub struct VideoEditorModel<'a> {
    /// Source recording document and playback cursor.
    pub document: &'a VideoDocument,
    /// Edit plan to expose this pass.
    pub plan: EditPlan,
    /// Current export state.
    pub transcode: TranscodeView<'a>,
}

/// Semantic action requested by the video editor.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoEditorAction {
    /// Start virtual playback.
    Play,
    /// Pause virtual playback.
    Pause,
    /// Move the document playback cursor.
    Seek(Duration),
    /// Adopt the updated non-destructive edit plan.
    PlanChanged(EditPlan),
    /// Start transcoding this validated plan.
    Export(EditPlan),
    /// Cancel the active transcode job.
    CancelExport,
    /// Reveal complete output in the platform file browser.
    RevealOutput,
    /// Reveal salvageable partial output in the platform file browser.
    RevealPartialOutput,
}

/// Derived editor control state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoEditorControls {
    /// Whether playback can change.
    pub transport_enabled: bool,
    /// Whether seek and trim values can change.
    pub timeline_enabled: bool,
    /// Whether source audio controls are applicable and editable.
    pub audio_enabled: bool,
    /// Whether stereo-to-mono can change.
    pub mono_enabled: bool,
    /// Whether export can start.
    pub export_enabled: bool,
    /// Whether an active export can be cancelled.
    pub cancel_export_enabled: bool,
    /// Plan validation message preventing export.
    pub validation_error: Option<String>,
}

/// Result of drawing a [`VideoEditor`].
#[derive(Debug)]
pub struct VideoEditorResponse {
    /// Updated plan for the caller to retain.
    pub plan: EditPlan,
    /// Semantic requests emitted this pass.
    pub actions: Vec<VideoEditorAction>,
    /// Derived enabled and validity state.
    pub controls: VideoEditorControls,
    /// Response for the full editor panel.
    pub response: Response,
    /// Play/pause control.
    pub transport_response: Response,
    /// Seek control.
    pub seek_response: Response,
    /// Export control.
    pub export_response: Response,
    /// Cancel-export control, present only while exporting.
    pub cancel_export_response: Option<Response>,
    /// Reveal-output control, present for complete or partial output.
    pub reveal_response: Option<Response>,
}

/// Real, non-destructive video editor surface.
pub struct VideoEditor<'a> {
    model: VideoEditorModel<'a>,
    theme: &'a Theme,
}

impl<'a> VideoEditor<'a> {
    /// Creates an editor from a source document, edit plan, and export state.
    #[must_use]
    pub const fn new(model: VideoEditorModel<'a>, theme: &'a Theme) -> Self {
        Self { model, theme }
    }

    /// Draws transport, trim, edit, and export controls.
    ///
    /// Playback and transcoding remain caller responsibilities.
    pub fn show(self, ui: &mut Ui) -> VideoEditorResponse {
        let document = self.model.document;
        let metadata = document.metadata();
        let duration = document.duration();
        let mut plan = self.model.plan;
        let mut actions = Vec::new();
        let mut plan_changed = false;
        let running = self.model.transcode.is_running();
        let gif = !plan.output.supports_audio();
        if gif && !plan.audio.mute {
            plan.audio.mute = true;
            plan_changed = true;
        }

        let mut transport_response = None;
        let mut seek_response = None;
        let mut export_response = None;
        let mut cancel_export_response = None;
        let mut reveal_response = None;

        let inner = panel(ui, self.theme, 760.0, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    heading(ui, self.theme, "Video editor");
                    caption(
                        ui,
                        self.theme,
                        format!(
                            "{} × {}  •  {:.2} fps{}",
                            metadata.width,
                            metadata.height,
                            metadata.fps,
                            if metadata.audio_channels == 0 {
                                "  •  no audio"
                            } else {
                                ""
                            }
                        ),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} / {}",
                            format_duration(document.position()),
                            format_duration(duration)
                        ))
                        .font(self.theme.font(Text::Label))
                        .color(self.theme.palette.text),
                    );
                });
            });

            ui.add_space(Space::LG);
            draw_trim_track(ui, self.theme, duration, document.position(), plan);
            ui.add_space(Space::SM);

            ui.horizontal(|ui| {
                let label = if document.playback() == PlaybackState::Playing {
                    "Pause"
                } else {
                    "Play"
                };
                let transport = button(ui, self.theme, label, false, !running);
                if transport.clicked() {
                    actions.push(if document.playback() == PlaybackState::Playing {
                        VideoEditorAction::Pause
                    } else {
                        VideoEditorAction::Play
                    });
                }
                transport_response = Some(transport);

                let mut position = document.position().as_secs_f64();
                let seek = ui.add_enabled(
                    !running,
                    Slider::new(&mut position, 0.0..=duration.as_secs_f64())
                        .show_value(false)
                        .text("Playback position"),
                );
                if seek.changed()
                    && let Ok(position) = Duration::try_from_secs_f64(position)
                {
                    actions.push(VideoEditorAction::Seek(position));
                }
                seek_response = Some(seek);
            });

            ui.add_space(Space::LG);
            rule(ui, self.theme);
            ui.add_space(Space::MD);
            section_label(ui, self.theme, "Trim");

            let mut trim_start = plan.trim.start.as_secs_f64();
            let mut trim_end = plan.trim.end.as_secs_f64();
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("In {}", format_duration(plan.trim.start)))
                        .font(self.theme.font(Text::Caption))
                        .color(self.theme.palette.text_muted),
                );
                let start = ui.add_enabled(
                    !running,
                    Slider::new(&mut trim_start, 0.0..=duration.as_secs_f64())
                        .show_value(false)
                        .text("Trim in"),
                );
                if start.changed()
                    && let Ok(value) = Duration::try_from_secs_f64(trim_start)
                {
                    plan.trim.start = value;
                    plan_changed = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Out {}", format_duration(plan.trim.end)))
                        .font(self.theme.font(Text::Caption))
                        .color(self.theme.palette.text_muted),
                );
                let end = ui.add_enabled(
                    !running,
                    Slider::new(&mut trim_end, 0.0..=duration.as_secs_f64())
                        .show_value(false)
                        .text("Trim out"),
                );
                if end.changed()
                    && let Ok(value) = Duration::try_from_secs_f64(trim_end)
                {
                    plan.trim.end = value;
                    plan_changed = true;
                }
            });

            ui.add_space(Space::LG);
            rule(ui, self.theme);
            ui.add_space(Space::MD);
            section_label(ui, self.theme, "Export");

            ui.columns(2, |columns| {
                columns[0].label(
                    RichText::new("Quality")
                        .font(self.theme.font(Text::Caption))
                        .color(self.theme.palette.text_faint),
                );
                columns[0].add_enabled_ui(!running, |ui| {
                    ComboBox::from_id_salt("video-editor-quality")
                        .selected_text(quality_label(plan.quality))
                        .show_ui(ui, |ui| {
                            for quality in Quality::ALL {
                                if ui
                                    .selectable_value(
                                        &mut plan.quality,
                                        quality,
                                        quality_label(quality),
                                    )
                                    .changed()
                                {
                                    plan_changed = true;
                                }
                            }
                        });
                });

                columns[1].label(
                    RichText::new("Resolution cap")
                        .font(self.theme.font(Text::Caption))
                        .color(self.theme.palette.text_faint),
                );
                columns[1].add_enabled_ui(!running, |ui| {
                    ComboBox::from_id_salt("video-editor-resolution")
                        .selected_text(resolution_label(plan.resolution))
                        .show_ui(ui, |ui| {
                            for resolution in ResolutionCap::ALL {
                                if ui
                                    .selectable_value(
                                        &mut plan.resolution,
                                        resolution,
                                        resolution_label(resolution),
                                    )
                                    .changed()
                                {
                                    plan_changed = true;
                                }
                            }
                        });
                });
            });

            let mut gif_enabled = !plan.output.supports_audio();
            let gif_response = ui.add_enabled(
                !running,
                egui::Checkbox::new(
                    &mut gif_enabled,
                    RichText::new("Export as GIF")
                        .font(self.theme.font(Text::Label))
                        .color(self.theme.palette.text),
                ),
            );
            if gif_response.changed() {
                if gif_enabled {
                    if let Ok(gif_plan) = EditPlan::gif(document) {
                        plan.output = gif_plan.output;
                        plan.audio.mute = true;
                    }
                } else {
                    plan.output = EditOutput::Video;
                }
                plan_changed = true;
            }

            if metadata.audio_channels == 0 {
                caption(ui, self.theme, "This source has no audio track.");
            } else if !plan.output.supports_audio() {
                caption(
                    ui,
                    self.theme,
                    "GIF output has no audio; audio edits are disabled.",
                );
            } else {
                ui.add_space(Space::SM);
                section_label(ui, self.theme, "Audio");
                let mut volume = plan.audio.volume;
                let volume_response = ui.add_enabled(
                    !running && !plan.audio.mute,
                    Slider::new(
                        &mut volume,
                        0.0..=scrozz_record::edit::AudioEdit::MAX_VOLUME,
                    )
                    .text("Volume")
                    .suffix("×"),
                );
                if volume_response.changed() {
                    plan.audio.volume = volume;
                    plan_changed = true;
                }

                let mute_response = ui.add_enabled(
                    !running,
                    egui::Checkbox::new(
                        &mut plan.audio.mute,
                        RichText::new("Mute")
                            .font(self.theme.font(Text::Label))
                            .color(self.theme.palette.text),
                    ),
                );
                plan_changed |= mute_response.changed();

                if metadata.audio_channels > 1 {
                    let mut mono = plan.audio.channels == ChannelBehavior::StereoToMono;
                    let mono_response = ui.add_enabled(
                        !running && !plan.audio.mute,
                        egui::Checkbox::new(
                            &mut mono,
                            RichText::new("Stereo to mono")
                                .font(self.theme.font(Text::Label))
                                .color(self.theme.palette.text),
                        ),
                    );
                    if mono_response.changed() {
                        plan.audio.channels = if mono {
                            ChannelBehavior::StereoToMono
                        } else {
                            ChannelBehavior::Preserve
                        };
                        plan_changed = true;
                    }
                }
            }

            ui.add_space(Space::MD);
            draw_transcode_status(
                ui,
                self.theme,
                self.model.transcode,
                &mut actions,
                &mut reveal_response,
            );

            let validation_error = document
                .validate_plan(&plan)
                .err()
                .map(|error| error.to_string());
            if let Some(error) = &validation_error {
                ui.add_space(Space::SM);
                ui.colored_label(self.theme.palette.recording, error);
            }

            ui.add_space(Space::LG);
            rule(ui, self.theme);
            ui.add_space(Space::MD);
            ui.horizontal(|ui| {
                if running {
                    let cancel = button(ui, self.theme, "Cancel export", false, true);
                    if cancel.clicked() {
                        actions.push(VideoEditorAction::CancelExport);
                    }
                    cancel_export_response = Some(cancel);
                }

                let export_enabled = validation_error.is_none() && !running;
                let export = button(
                    ui,
                    self.theme,
                    if matches!(self.model.transcode, TranscodeView::Finished { .. }) {
                        "Export again"
                    } else {
                        "Export"
                    },
                    true,
                    export_enabled,
                );
                if export.clicked() {
                    actions.push(VideoEditorAction::Export(plan));
                }
                export_response = Some(export);
            });
        });

        if plan_changed {
            actions.push(VideoEditorAction::PlanChanged(plan));
        }
        let validation_error = document
            .validate_plan(&plan)
            .err()
            .map(|error| error.to_string());
        let audio_enabled = metadata.audio_channels > 0 && plan.output.supports_audio() && !running;
        let controls = VideoEditorControls {
            transport_enabled: !running,
            timeline_enabled: !running,
            audio_enabled,
            mono_enabled: audio_enabled && metadata.audio_channels > 1 && !plan.audio.mute,
            export_enabled: validation_error.is_none() && !running,
            cancel_export_enabled: running,
            validation_error,
        };

        VideoEditorResponse {
            plan,
            actions,
            controls,
            response: inner.response,
            transport_response: transport_response.expect("the editor always draws transport"),
            seek_response: seek_response.expect("the editor always draws seek"),
            export_response: export_response.expect("the editor always draws export"),
            cancel_export_response,
            reveal_response,
        }
    }
}

fn quality_label(quality: Quality) -> &'static str {
    match quality {
        Quality::Low => "Small",
        Quality::Balanced => "Balanced",
        Quality::High => "High",
    }
}

fn resolution_label(resolution: ResolutionCap) -> &'static str {
    match resolution {
        ResolutionCap::Native => "Native",
        ResolutionCap::Uhd2160 => "2160p",
        ResolutionCap::Qhd1440 => "1440p",
        ResolutionCap::Fhd1080 => "1080p",
        ResolutionCap::Hd720 => "720p",
        ResolutionCap::Half => "Half size",
    }
}

#[allow(clippy::cast_precision_loss)]
fn draw_trim_track(
    ui: &mut Ui,
    theme: &Theme,
    duration: Duration,
    position: Duration,
    plan: EditPlan,
) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 58.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, corner(Radius::THUMB), theme.palette.chip_fill);
    painter.rect_stroke(
        rect,
        corner(Radius::THUMB),
        Stroke::new(1.0, theme.palette.thumb_border),
        StrokeKind::Inside,
    );
    let total = duration.as_secs_f64().max(f64::EPSILON);
    let x_for = |time: Duration| {
        rect.left() + rect.width() * (time.as_secs_f64() / total).clamp(0.0, 1.0) as f32
    };
    let trim_left = x_for(plan.trim.start);
    let trim_right = x_for(plan.trim.end);
    let selected = egui::Rect::from_min_max(
        egui::pos2(trim_left, rect.top()),
        egui::pos2(trim_right.max(trim_left), rect.bottom()),
    );
    painter.rect_filled(
        selected,
        corner(Radius::CHIP),
        theme.palette.accent.linear_multiply(0.22),
    );
    painter.line_segment(
        [
            egui::pos2(trim_left, rect.top()),
            egui::pos2(trim_left, rect.bottom()),
        ],
        Stroke::new(2.0, theme.palette.accent_hi),
    );
    painter.line_segment(
        [
            egui::pos2(trim_right, rect.top()),
            egui::pos2(trim_right, rect.bottom()),
        ],
        Stroke::new(2.0, theme.palette.accent_hi),
    );
    let playhead = x_for(position);
    painter.line_segment(
        [
            egui::pos2(playhead, rect.top() - Space::XS),
            egui::pos2(playhead, rect.bottom() + Space::XS),
        ],
        Stroke::new(2.0, theme.palette.recording),
    );

    for index in 1..12 {
        let x = rect.left() + rect.width() * index as f32 / 12.0;
        painter.line_segment(
            [
                egui::pos2(x, rect.bottom() - 7.0),
                egui::pos2(x, rect.bottom() - 2.0),
            ],
            Stroke::new(1.0, theme.palette.text_faint),
        );
    }
}

fn draw_transcode_status(
    ui: &mut Ui,
    theme: &Theme,
    status: TranscodeView<'_>,
    actions: &mut Vec<VideoEditorAction>,
    reveal_response: &mut Option<Response>,
) {
    match status {
        TranscodeView::Idle => {}
        TranscodeView::Running { progress } => {
            section_label(ui, theme, "Exporting");
            ui.add(
                ProgressBar::new(progress.clamp(0.0, 1.0))
                    .show_percentage()
                    .animate(false),
            );
        }
        TranscodeView::Finished { output } => {
            ui.colored_label(theme.palette.success, "Export finished");
            if let Some(output) = output {
                body(ui, theme, format!("Saved to {}", output.path.display()));
                let reveal = button(ui, theme, "Show file", false, true);
                if reveal.clicked() {
                    actions.push(VideoEditorAction::RevealOutput);
                }
                *reveal_response = Some(reveal);
            }
        }
        TranscodeView::Failed { failure } => {
            ui.colored_label(theme.palette.recording, "Export failed");
            if let Some(failure) = failure {
                body(ui, theme, failure.error.to_string());
                if let Some(partial) = &failure.partial {
                    body(
                        ui,
                        theme,
                        format!(
                            "A partial export is available at {}.",
                            partial.path.display()
                        ),
                    );
                    if let Some(reason) = partial.partial_reason() {
                        caption(ui, theme, format!("Could not finalise: {reason}"));
                    }
                    let reveal = button(ui, theme, "Show partial", false, true);
                    if reveal.clicked() {
                        actions.push(VideoEditorAction::RevealPartialOutput);
                    }
                    *reveal_response = Some(reveal);
                } else {
                    caption(ui, theme, "No usable export was written.");
                }
            }
        }
        TranscodeView::Cancelled => {
            caption(ui, theme, "Export cancelled.");
        }
    }
}

/// Real video-editor renderer used by the deterministic harness.
#[derive(Debug, Default)]
pub struct VideoEditorScene;

impl Scene for VideoEditorScene {
    fn name(&self) -> &str {
        "recording-video-editor"
    }

    fn setup(&self, ctx: &egui::Context) {
        install_scene_theme(ctx);
    }

    fn ui(&self, ui: &mut Ui, ctx: &SceneCtx<'_>) {
        let Some(RecordingFixture::Editor(fixture)) = ctx.fixture.recording.as_ref() else {
            return;
        };
        let theme = scene_theme(ctx.theme);
        let transcode = match &fixture.export {
            EditorExportFixture::Idle => TranscodeView::Idle,
            EditorExportFixture::Running { progress } => TranscodeView::Running {
                progress: *progress,
            },
            EditorExportFixture::Finished(output) => TranscodeView::Finished {
                output: Some(output),
            },
            EditorExportFixture::Failed(failure) => TranscodeView::Failed {
                failure: Some(failure),
            },
            EditorExportFixture::Cancelled => TranscodeView::Cancelled,
        };
        ui.vertical_centered(|ui| {
            ui.add_space(Space::XXL);
            VideoEditor::new(
                VideoEditorModel {
                    document: &fixture.document,
                    plan: fixture.plan,
                    transcode,
                },
                &theme,
            )
            .show(ui);
        });
    }
}
