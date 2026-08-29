//! Non-destructive recording playback, trimming, and export controls.

use std::time::Duration;

use egui::{
    Align2, CentralPanel, ComboBox, Frame, Image, ProgressBar, Response, RichText, ScrollArea,
    Sense, Slider, Stroke, StrokeKind, TextureHandle, Ui, ViewportBuilder, WindowLevel,
    load::SizedTexture, vec2,
};
use scrozz_record::{
    edit::{ChannelBehavior, EditOutput, EditPlan, PlaybackState, VideoDocument},
    playback::{PlaybackAudio, PlaybackPhase, PlaybackSnapshot},
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

/// Stable native title used for activation and AppKit diagnostics.
pub const VIDEO_EDITOR_WINDOW_TITLE: &str = "Scrozz Video Editor";

/// Stable identity of the ordinary recording-editor viewport.
#[must_use]
pub fn viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("scrozz-recording-video-editor")
}

/// Opaque, focus-taking, movable recording-editor window properties.
#[must_use]
pub fn viewport_builder() -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title(VIDEO_EDITOR_WINDOW_TITLE)
        .with_inner_size([900.0, 900.0])
        .with_min_inner_size([680.0, 620.0])
        .with_clamp_size_to_monitor_size(true)
        .with_resizable(true)
        .with_decorations(true)
        .with_transparent(false)
        .with_mouse_passthrough(false)
        .with_has_shadow(true)
        .with_taskbar(true)
        .with_active(false)
        .with_visible(false)
        .with_window_level(WindowLevel::Normal)
}

/// Editor state retained by the application and cloned into its native viewport.
#[derive(Debug, Clone)]
pub struct VideoEditorSnapshot {
    /// Native source plus deterministic playback cursor.
    pub document: VideoDocument,
    /// Current non-destructive edit plan.
    pub plan: EditPlan,
    /// Decoded preview frame and native media-clock state.
    pub playback: PlaybackSnapshot,
    /// Current native export state, or `None` before the first export.
    pub transcode_status: Option<TranscodeStatus>,
    /// Last normalized export progress.
    pub transcode_progress: f32,
    /// Complete exported artifact.
    pub transcode_output: Option<TranscodeOutput>,
    /// Explicit export failure, including any retained partial.
    pub transcode_failure: Option<TranscodeFailure>,
}

#[derive(Clone)]
struct EditorTexture {
    stream_id: u64,
    sequence: u64,
    handle: TextureHandle,
}

/// Draws one editor pass over an opaque system-theme canvas.
///
/// The returned panel response spans the entire viewport, so even blank
/// background areas are real hit-testable window content rather than holes in a
/// transparent overlay.
#[must_use]
pub fn show_window(
    ui: &mut Ui,
    snapshot: &VideoEditorSnapshot,
    theme: &Theme,
) -> VideoEditorResponse {
    install_window_style(ui, theme);
    let preview_texture = editor_texture(ui.ctx(), &snapshot.playback);
    let transcode = snapshot
        .transcode_status
        .map_or(TranscodeView::Idle, |status| {
            TranscodeView::from_status(
                status,
                snapshot.transcode_progress,
                snapshot.transcode_output.as_ref(),
                snapshot.transcode_failure.as_ref(),
            )
        });
    CentralPanel::default()
        .frame(
            Frame::new()
                .fill(theme.palette.canvas())
                .inner_margin(Space::XL),
        )
        .show(ui, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        VideoEditor::new(
                            VideoEditorModel {
                                document: &snapshot.document,
                                plan: snapshot.plan,
                                preview: VideoPreview::from_snapshot(
                                    &snapshot.playback,
                                    preview_texture,
                                ),
                                transcode,
                            },
                            theme,
                        )
                        .show(ui)
                    })
                    .inner
                })
                .inner
        })
        .inner
}

fn install_window_style(ui: &mut Ui, theme: &Theme) {
    ui.set_style(window_style(ui.style(), theme));
}

fn window_style(base: &egui::Style, theme: &Theme) -> egui::Style {
    let mut style = base.clone();
    style.visuals.dark_mode = theme.palette.is_dark();
    style.visuals.override_text_color = Some(theme.palette.text);
    style.visuals.panel_fill = theme.palette.canvas();
    style.visuals.window_fill = theme.palette.canvas();
    style.visuals.selection.bg_fill = theme.palette.accent.linear_multiply(0.55);
    style.visuals.selection.stroke = Stroke::new(1.0, theme.palette.on_accent);
    style.visuals.text_cursor.blink = false;
    style.animation_time = 0.0;
    style.spacing.item_spacing = vec2(Space::SM, Space::SM);
    style.spacing.button_padding = vec2(Space::MD, Space::SM);
    style.text_styles = crate::theme::text_styles(theme);
    style
}

fn editor_texture(ctx: &egui::Context, playback: &PlaybackSnapshot) -> Option<SizedTexture> {
    let preview = playback.frame.as_ref()?;
    let id = egui::Id::new("scrozz-recording-editor-texture");
    let mut state = ctx.data_mut(|data| data.get_temp::<EditorTexture>(id));
    let replace = state.as_ref().is_none_or(|state| {
        state.stream_id != playback.stream_id || state.sequence != preview.sequence
    });
    if replace {
        let frame = &preview.frame.image;
        let width = usize::try_from(frame.width).ok()?;
        let height = usize::try_from(frame.height).ok()?;
        let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &frame.data);
        if let Some(state) = &mut state {
            state.handle.set(image, egui::TextureOptions::LINEAR);
            state.stream_id = playback.stream_id;
            state.sequence = preview.sequence;
        } else {
            state = Some(EditorTexture {
                stream_id: playback.stream_id,
                sequence: preview.sequence,
                handle: ctx.load_texture(
                    "scrozz.recording.editor.preview",
                    image,
                    egui::TextureOptions::LINEAR,
                ),
            });
        }
        if let Some(state) = &state {
            ctx.data_mut(|data| data.insert_temp(id, state.clone()));
        }
    }
    state
        .as_ref()
        .map(|state| SizedTexture::from_handle(&state.handle))
}

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
    Cancelled {
        /// Usable partial output retained before cancellation.
        output: Option<&'a TranscodeOutput>,
    },
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
            TranscodeStatus::Cancelled => Self::Cancelled { output },
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
    /// Decoded preview and native media-clock state.
    pub preview: VideoPreview<'a>,
    /// Current export state.
    pub transcode: TranscodeView<'a>,
}

/// Decoded preview values supplied by the overlay texture owner.
#[derive(Debug, Clone, Copy)]
pub struct VideoPreview<'a> {
    /// Uploaded decoded frame.
    pub texture: Option<SizedTexture>,
    /// Native playback lifecycle.
    pub phase: PlaybackPhase,
    /// Requested playback rate.
    pub rate: f32,
    /// Source timestamp through which frames are decoded.
    pub buffered_until: Duration,
    /// Captured-audio behavior.
    pub audio: PlaybackAudio,
    /// Distance from the native clock to the displayed frame interval.
    pub av_drift: Option<Duration>,
    /// Explicit playback failure.
    pub error: Option<&'a str>,
}

impl<'a> VideoPreview<'a> {
    /// Borrows native playback state and pairs it with an uploaded frame.
    #[must_use]
    pub fn from_snapshot(snapshot: &'a PlaybackSnapshot, texture: Option<SizedTexture>) -> Self {
        Self {
            texture,
            phase: snapshot.phase,
            rate: snapshot.rate,
            buffered_until: snapshot.buffered_until,
            audio: snapshot.audio,
            av_drift: snapshot.av_drift,
            error: snapshot.error.as_deref(),
        }
    }
}

impl Default for VideoPreview<'_> {
    fn default() -> Self {
        Self {
            texture: None,
            phase: PlaybackPhase::Paused,
            rate: 1.0,
            buffered_until: Duration::ZERO,
            audio: PlaybackAudio::NoTrack,
            av_drift: None,
            error: None,
        }
    }
}

/// Semantic action requested by the video editor.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoEditorAction {
    /// Close the editor while leaving the source recording untouched.
    Close,
    /// Start virtual playback.
    Play,
    /// Pause virtual playback.
    Pause,
    /// Move the document playback cursor.
    Seek(Duration),
    /// Change native playback rate.
    SetRate(f32),
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
    /// Whether non-destructive trim values can change.
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
    /// Done/close control.
    pub close_response: Response,
    /// Play/pause control.
    pub transport_response: Response,
    /// Seek control.
    pub seek_response: Response,
    /// Seek five seconds backward.
    pub seek_backward_response: Response,
    /// Seek five seconds forward.
    pub seek_forward_response: Response,
    /// Set trim-in to the current playhead.
    pub set_in_response: Response,
    /// Set trim-out to the current playhead.
    pub set_out_response: Response,
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
        let playback_available = self.model.preview.phase != PlaybackPhase::Failed;
        let gif = !plan.output.supports_audio();
        if gif && !plan.audio.mute {
            plan.audio.mute = true;
            plan_changed = true;
        }

        let mut transport_response = None;
        let mut close_response = None;
        let mut seek_response = None;
        let mut seek_backward_response = None;
        let mut seek_forward_response = None;
        let mut set_in_response = None;
        let mut set_out_response = None;
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
                    let close = button(ui, self.theme, "Done", false, !running);
                    if close.clicked() {
                        actions.push(VideoEditorAction::Close);
                    }
                    close_response = Some(close);
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

            ui.add_space(Space::SM);
            draw_video_preview(
                ui,
                self.theme,
                self.model.preview,
                metadata.width,
                metadata.height,
            );
            ui.add_space(Space::SM);
            draw_trim_track(ui, self.theme, duration, document.position(), plan);
            ui.add_space(Space::SM);

            ui.horizontal(|ui| {
                let label = if document.playback() == PlaybackState::Playing {
                    "Pause"
                } else {
                    "Play"
                };
                let transport =
                    button(ui, self.theme, label, false, !running && playback_available);
                if transport.clicked() {
                    actions.push(if document.playback() == PlaybackState::Playing {
                        VideoEditorAction::Pause
                    } else {
                        VideoEditorAction::Play
                    });
                }
                transport_response = Some(transport);

                let backward = button(
                    ui,
                    self.theme,
                    "−5s",
                    false,
                    !running && document.position() > plan.trim.start,
                );
                if backward.clicked() {
                    actions.push(VideoEditorAction::Seek(
                        document
                            .position()
                            .saturating_sub(Duration::from_secs(5))
                            .max(plan.trim.start),
                    ));
                }
                seek_backward_response = Some(backward);

                let forward = button(
                    ui,
                    self.theme,
                    "+5s",
                    false,
                    !running && document.position() < plan.trim.end,
                );
                if forward.clicked() {
                    actions.push(VideoEditorAction::Seek(
                        document
                            .position()
                            .saturating_add(Duration::from_secs(5))
                            .min(plan.trim.end),
                    ));
                }
                seek_forward_response = Some(forward);

                let mut position = document.position().as_secs_f64();
                let seek = ui.add_enabled(
                    !running,
                    Slider::new(
                        &mut position,
                        plan.trim.start.as_secs_f64()..=plan.trim.end.as_secs_f64(),
                    )
                    .show_value(false)
                    .text("Playback position"),
                );
                if seek.changed()
                    && let Ok(position) = Duration::try_from_secs_f64(position)
                {
                    actions.push(VideoEditorAction::Seek(position));
                }
                seek_response = Some(seek);

                ui.add_enabled_ui(!running && playback_available, |ui| {
                    ComboBox::from_id_salt("video-editor-rate")
                        .selected_text(format!("{}×", format_rate(self.model.preview.rate)))
                        .popup_style(ui.style().as_ref().clone().into())
                        .show_ui(ui, |ui| {
                            for rate in [0.5_f32, 1.0, 1.5, 2.0] {
                                if ui
                                    .selectable_label(
                                        (self.model.preview.rate - rate).abs() < f32::EPSILON,
                                        format!("{}×", format_rate(rate)),
                                    )
                                    .clicked()
                                {
                                    actions.push(VideoEditorAction::SetRate(rate));
                                }
                            }
                        });
                });
            });

            ui.add_space(Space::SM);
            rule(ui, self.theme);
            ui.add_space(Space::SM);
            section_label(ui, self.theme, "Trim");

            let mut trim_start = plan.trim.start.as_secs_f64();
            let mut trim_end = plan.trim.end.as_secs_f64();
            let minimum_trim = (1.0 / metadata.fps).max(0.001);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("In {}", format_duration(plan.trim.start)))
                        .font(self.theme.font(Text::Caption))
                        .color(self.theme.palette.text_muted),
                );
                let start = ui.add_enabled(
                    !running,
                    Slider::new(&mut trim_start, 0.0..=(trim_end - minimum_trim).max(0.0))
                        .show_value(false)
                        .text("Trim in"),
                );
                if start.changed()
                    && let Ok(value) = Duration::try_from_secs_f64(trim_start)
                {
                    plan.trim.start = value;
                    plan_changed = true;
                }
                let set_in = button(
                    ui,
                    self.theme,
                    "Set in",
                    false,
                    !running
                        && document.position() < plan.trim.end
                        && document.position() != plan.trim.start,
                );
                if set_in.clicked() {
                    plan.trim.start = document.position();
                    plan_changed = true;
                }
                set_in_response = Some(set_in);
            });
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Out {}", format_duration(plan.trim.end)))
                        .font(self.theme.font(Text::Caption))
                        .color(self.theme.palette.text_muted),
                );
                let end = ui.add_enabled(
                    !running,
                    Slider::new(
                        &mut trim_end,
                        (trim_start + minimum_trim).min(duration.as_secs_f64())
                            ..=duration.as_secs_f64(),
                    )
                    .show_value(false)
                    .text("Trim out"),
                );
                if end.changed()
                    && let Ok(value) = Duration::try_from_secs_f64(trim_end)
                {
                    plan.trim.end = value;
                    plan_changed = true;
                }
                let set_out = button(
                    ui,
                    self.theme,
                    "Set out",
                    false,
                    !running
                        && document.position() > plan.trim.start
                        && document.position() != plan.trim.end,
                );
                if set_out.clicked() {
                    plan.trim.end = document.position();
                    plan_changed = true;
                }
                set_out_response = Some(set_out);
            });

            ui.add_space(Space::SM);
            rule(ui, self.theme);
            ui.add_space(Space::SM);
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
                        .popup_style(ui.style().as_ref().clone().into())
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
                        .popup_style(ui.style().as_ref().clone().into())
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
                ui.horizontal_wrapped(|ui| {
                    let volume_response = ui
                        .add_enabled_ui(!running && !plan.audio.mute, |ui| {
                            ui.add_sized(
                                [220.0, ui.spacing().interact_size.y],
                                Slider::new(
                                    &mut volume,
                                    0.0..=scrozz_record::edit::AudioEdit::MAX_VOLUME,
                                )
                                .text("Volume")
                                .suffix("×"),
                            )
                        })
                        .inner;
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
                });
            }

            ui.add_space(Space::SM);
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

            ui.add_space(Space::SM);
            rule(ui, self.theme);
            ui.add_space(Space::SM);
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
            actions.insert(0, VideoEditorAction::PlanChanged(plan));
        }
        let validation_error = document
            .validate_plan(&plan)
            .err()
            .map(|error| error.to_string());
        let audio_enabled = metadata.audio_channels > 0 && plan.output.supports_audio() && !running;
        let controls = VideoEditorControls {
            transport_enabled: !running && playback_available,
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
            close_response: close_response.expect("the editor always draws close"),
            transport_response: transport_response.expect("the editor always draws transport"),
            seek_response: seek_response.expect("the editor always draws seek"),
            seek_backward_response: seek_backward_response
                .expect("the editor always draws backward seek"),
            seek_forward_response: seek_forward_response
                .expect("the editor always draws forward seek"),
            set_in_response: set_in_response.expect("the editor always draws set-in"),
            set_out_response: set_out_response.expect("the editor always draws set-out"),
            export_response: export_response.expect("the editor always draws export"),
            cancel_export_response,
            reveal_response,
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn draw_video_preview(
    ui: &mut Ui,
    theme: &Theme,
    preview: VideoPreview<'_>,
    width: u32,
    height: u32,
) {
    let aspect = (width as f32 / height.max(1) as f32).max(0.1);
    let available_width = ui.available_width().max(1.0);
    let preview_height = (available_width / aspect).clamp(120.0, 170.0);
    let size = vec2(
        (preview_height * aspect).min(available_width),
        preview_height,
    );
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        ui.painter()
            .rect_filled(rect, corner(Radius::THUMB), egui::Color32::BLACK);
        if let Some(texture) = preview.texture {
            let texture_aspect = texture.size.x / texture.size.y.max(1.0);
            let fitted = if texture_aspect >= rect.width() / rect.height() {
                vec2(rect.width(), rect.width() / texture_aspect)
            } else {
                vec2(rect.height() * texture_aspect, rect.height())
            };
            let image_rect = egui::Rect::from_center_size(rect.center(), fitted);
            ui.put(
                image_rect,
                Image::from_texture(texture)
                    .fit_to_exact_size(fitted)
                    .maintain_aspect_ratio(true),
            );
        } else {
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                match preview.phase {
                    PlaybackPhase::Loading => "Decoding preview…",
                    PlaybackPhase::Buffering => "Buffering preview…",
                    PlaybackPhase::Failed => "Preview unavailable",
                    PlaybackPhase::Paused | PlaybackPhase::Playing | PlaybackPhase::Ended => {
                        "Waiting for a decoded frame…"
                    }
                },
                theme.font(Text::Label),
                theme.palette.text_muted,
            );
        }
        ui.painter().rect_stroke(
            rect,
            corner(Radius::THUMB),
            Stroke::new(1.0, theme.palette.thumb_border),
            StrokeKind::Inside,
        );
    });

    let audio = match preview.audio {
        PlaybackAudio::NoTrack => "no audio track",
        PlaybackAudio::Muted => "audio muted by edit",
        PlaybackAudio::Active => "captured audio synced",
    };
    let drift = preview.av_drift.map_or_else(String::new, |drift| {
        format!("  •  A/V offset {} ms", drift.as_millis())
    });
    caption(
        ui,
        theme,
        format!(
            "{}  •  {audio}  •  decoded through {}{drift}",
            playback_phase_label(preview.phase),
            format_duration(preview.buffered_until),
        ),
    );
    if let Some(error) = preview.error {
        ui.colored_label(theme.palette.recording, error);
    }
}

fn playback_phase_label(phase: PlaybackPhase) -> &'static str {
    match phase {
        PlaybackPhase::Loading => "Loading",
        PlaybackPhase::Paused => "Paused",
        PlaybackPhase::Playing => "Playing",
        PlaybackPhase::Buffering => "Buffering",
        PlaybackPhase::Ended => "Ended",
        PlaybackPhase::Failed => "Playback failed",
    }
}

fn format_rate(rate: f32) -> String {
    if rate.fract().abs() < f32::EPSILON {
        format!("{rate:.0}")
    } else {
        format!("{rate:.1}")
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
            if let Some(failure) = failure {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(theme.palette.recording, "Export failed");
                    ui.label(
                        RichText::new(failure.error.to_string())
                            .font(theme.font(Text::Body))
                            .color(theme.palette.text),
                    );
                    if failure.partial.is_some() {
                        let reveal = button(ui, theme, "Show partial", false, true);
                        if reveal.clicked() {
                            actions.push(VideoEditorAction::RevealPartialOutput);
                        }
                        *reveal_response = Some(reveal);
                    }
                });
                if let Some(partial) = &failure.partial {
                    let reason = partial
                        .partial_reason()
                        .map_or_else(String::new, |reason| format!(" • {reason}"));
                    caption(
                        ui,
                        theme,
                        format!("Partial export: {}{reason}", partial.path.display(),),
                    );
                } else {
                    caption(ui, theme, "No usable export was written.");
                }
            } else {
                ui.colored_label(theme.palette.recording, "Export failed");
            }
        }
        TranscodeView::Cancelled { output } => {
            caption(ui, theme, "Export cancelled.");
            if let Some(output) = output {
                body(
                    ui,
                    theme,
                    format!(
                        "A partial export is available at {}.",
                        output.path.display()
                    ),
                );
                let reveal = button(ui, theme, "Show partial", false, true);
                if reveal.clicked() {
                    actions.push(VideoEditorAction::RevealPartialOutput);
                }
                *reveal_response = Some(reveal);
            }
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
            EditorExportFixture::Cancelled => TranscodeView::Cancelled { output: None },
        };
        ui.vertical_centered(|ui| {
            ui.add_space(Space::XXL);
            let preview_image = fixture_preview_image();
            let preview_texture = ui.ctx().load_texture(
                "recording-video-editor-fixture",
                preview_image,
                egui::TextureOptions::LINEAR,
            );
            ui.ctx().data_mut(|data| {
                data.insert_temp(
                    egui::Id::new("recording-video-editor-fixture-handle"),
                    preview_texture.clone(),
                );
            });
            let preview = VideoPreview {
                texture: Some(SizedTexture::from_handle(&preview_texture)),
                phase: PlaybackPhase::Paused,
                rate: 1.0,
                buffered_until: fixture.document.position() + Duration::from_millis(250),
                audio: if fixture.document.metadata().audio_channels == 0 {
                    PlaybackAudio::NoTrack
                } else if fixture.plan.audio.mute || !fixture.plan.output.supports_audio() {
                    PlaybackAudio::Muted
                } else {
                    PlaybackAudio::Active
                },
                av_drift: Some(Duration::ZERO),
                error: None,
            };
            VideoEditor::new(
                VideoEditorModel {
                    document: &fixture.document,
                    plan: fixture.plan,
                    preview,
                    transcode,
                },
                &theme,
            )
            .show(ui);
        });
    }
}

fn fixture_preview_image() -> egui::ColorImage {
    const WIDTH: usize = 96;
    const HEIGHT: usize = 54;

    let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let sky = egui::Color32::from_rgb(
                42 + u8::try_from(x / 3).unwrap_or(0),
                72 + u8::try_from(y / 2).unwrap_or(0),
                118 + u8::try_from((x + y) / 8).unwrap_or(0),
            );
            let colour = if y > 34 {
                egui::Color32::from_rgb(28, 34 + u8::try_from(x / 4).unwrap_or(0), 42)
            } else if (22..74).contains(&x) && (12..34).contains(&y) {
                egui::Color32::from_rgb(244, 157, 72)
            } else {
                sky
            };
            pixels.push(colour);
        }
    }
    egui::ColorImage::new([WIDTH, HEIGHT], pixels)
}

#[cfg(test)]
mod window_tests {
    use super::*;

    #[test]
    fn editor_viewport_is_an_opaque_interactive_normal_window() {
        let viewport = viewport_builder();
        assert_eq!(viewport.title.as_deref(), Some(VIDEO_EDITOR_WINDOW_TITLE));
        assert_eq!(viewport.decorations, Some(true));
        assert_eq!(viewport.resizable, Some(true));
        assert_eq!(viewport.transparent, Some(false));
        assert_eq!(viewport.mouse_passthrough, Some(false));
        assert_eq!(viewport.has_shadow, Some(true));
        assert_eq!(viewport.taskbar, Some(true));
        assert_eq!(viewport.active, Some(false));
        assert_eq!(viewport.visible, Some(false));
        assert_eq!(viewport.window_level, Some(WindowLevel::Normal));
        assert_eq!(viewport.clamp_size_to_monitor_size, Some(true));
        assert_ne!(viewport_id(), egui::ViewportId::ROOT);
    }

    #[test]
    fn both_editor_appearances_have_opaque_canvases() {
        assert_eq!(Theme::dark().palette.canvas().a(), 255);
        assert_eq!(Theme::light().palette.canvas().a(), 255);
        let base = egui::Style::default();
        let light = window_style(&base, &Theme::light());
        let dark = window_style(&base, &Theme::dark());
        assert!(!light.visuals.dark_mode);
        assert!(dark.visuals.dark_mode);
        assert_eq!(light.visuals.panel_fill.a(), 255);
        assert_eq!(dark.visuals.panel_fill.a(), 255);
        assert_eq!(
            base.visuals.panel_fill,
            egui::Style::default().visuals.panel_fill
        );
    }
}
