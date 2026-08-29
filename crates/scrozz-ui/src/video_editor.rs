//! Non-destructive recording playback, trimming, and export controls.

use std::time::Duration;

use egui::{
    Align, Align2, CentralPanel, CollapsingHeader, ComboBox, CursorIcon, DragValue, Frame, Image,
    Key, Layout, ProgressBar, Response, RichText, ScrollArea, Sense, Slider, Stroke, StrokeKind,
    TextureHandle, Ui, ViewportBuilder, WidgetInfo, WidgetType, WindowLevel, load::SizedTexture,
    pos2, vec2,
};
use scrozz_record::{
    edit::{ChannelBehavior, EditOutput, EditPlan, OutputDimensions, PlaybackState, VideoDocument},
    playback::{PlaybackAudio, PlaybackPhase, PlaybackSnapshot},
    settings::{Quality, ResolutionCap},
    storyboard::{STORYBOARD_SLOTS, StoryboardSnapshot},
    transcode::{TranscodeFailure, TranscodeOutput, TranscodeStatus},
};

use crate::{
    harness::{EditorExportFixture, RecordingFixture, Scene, SceneCtx},
    recording_controls::{
        body, button, caption, format_duration, heading, install_scene_theme, rule, scene_theme,
        section_label,
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
        .with_inner_size([1180.0, 820.0])
        .with_min_inner_size([720.0, 640.0])
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
    /// Incrementally decoded filmstrip and waveform samples.
    pub storyboard: StoryboardSnapshot,
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
    interactions: scrozz_record::InteractionEdits,
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
    let preview_texture = editor_texture(
        ui.ctx(),
        &snapshot.playback,
        snapshot.document.recording().interactions(),
        snapshot.plan.interactions,
    );
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
                                    Some(&snapshot.storyboard),
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

fn editor_texture(
    ctx: &egui::Context,
    playback: &PlaybackSnapshot,
    interactions: Option<&scrozz_record::InteractionRecording>,
    edits: scrozz_record::InteractionEdits,
) -> Option<SizedTexture> {
    let preview = playback.frame.as_ref()?;
    let id = egui::Id::new("scrozz-recording-editor-texture");
    let mut state = ctx.data_mut(|data| data.get_temp::<EditorTexture>(id));
    let replace = state.as_ref().is_none_or(|state| {
        state.stream_id != playback.stream_id
            || state.sequence != preview.sequence
            || state.interactions != edits
    });
    if replace {
        let mut frame = preview.frame.image.clone();
        if let Some(interactions) = interactions {
            scrozz_record::render_interactions(
                &mut frame,
                interactions,
                preview.frame.timestamp,
                edits,
            );
        }
        let width = usize::try_from(frame.width).ok()?;
        let height = usize::try_from(frame.height).ok()?;
        let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &frame.data);
        if let Some(state) = &mut state {
            state.handle.set(image, egui::TextureOptions::LINEAR);
            state.stream_id = playback.stream_id;
            state.sequence = preview.sequence;
            state.interactions = edits;
        } else {
            state = Some(EditorTexture {
                stream_id: playback.stream_id,
                sequence: preview.sequence,
                interactions: edits,
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
    /// Number of decoded frames retained by the bounded queue.
    pub buffered_frames: usize,
    /// Captured-audio behavior.
    pub audio: PlaybackAudio,
    /// Distance from the native clock to the displayed frame interval.
    pub av_drift: Option<Duration>,
    /// Explicit playback failure.
    pub error: Option<&'a str>,
    /// Incremental filmstrip and waveform sampling.
    pub storyboard: Option<&'a StoryboardSnapshot>,
}

impl<'a> VideoPreview<'a> {
    /// Borrows native playback state and pairs it with an uploaded frame.
    #[must_use]
    pub fn from_snapshot(
        snapshot: &'a PlaybackSnapshot,
        texture: Option<SizedTexture>,
        storyboard: Option<&'a StoryboardSnapshot>,
    ) -> Self {
        Self {
            texture,
            phase: snapshot.phase,
            rate: snapshot.rate,
            buffered_until: snapshot.buffered_until,
            buffered_frames: snapshot.buffered_frames,
            audio: snapshot.audio,
            av_drift: snapshot.av_drift,
            error: snapshot.error.as_deref(),
            storyboard,
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
            buffered_frames: 0,
            audio: PlaybackAudio::NoTrack,
            av_drift: None,
            error: None,
            storyboard: None,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEditorLayout {
    /// Preview/timeline beside a restrained inspector.
    Wide,
    /// Preview followed by collapsible inspector sections.
    Stacked,
}

impl VideoEditorLayout {
    const MIN_WIDE_WIDTH: f32 = 980.0;

    /// Chooses the inspector arrangement for the available logical width.
    #[must_use]
    pub fn for_width(width: f32) -> Self {
        if width >= Self::MIN_WIDE_WIDTH {
            Self::Wide
        } else {
            Self::Stacked
        }
    }
}

/// Derived editor control state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoEditorControls {
    /// Responsive structure used for this pass.
    pub layout: VideoEditorLayout,
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
    /// Interactive filmstrip, trim handles, and playhead.
    pub timeline_response: Response,
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
        let layout = VideoEditorLayout::for_width(ui.available_width());
        let gif = !plan.output.supports_audio();
        if gif && !plan.audio.mute {
            plan.audio.mute = true;
            plan_changed = true;
        }

        handle_editor_keyboard(
            ui,
            document,
            running,
            playback_available,
            &mut plan,
            &mut plan_changed,
            &mut actions,
        );

        let mut responses = EditorResponses::default();
        let width = (ui.available_width() - Space::LG * 2.0).clamp(1.0, 1180.0);
        let inner = Frame::new()
            .fill(self.theme.palette.card_fill)
            .stroke(Stroke::new(1.0, self.theme.palette.hairline))
            .corner_radius(corner(Radius::CARD))
            .inner_margin(egui::Margin::same(Space::LG as i8))
            .show(ui, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        heading(ui, self.theme, "Video Editor");
                        caption(
                            ui,
                            self.theme,
                            format!(
                                "{} × {}  •  {:.2} fps  •  {}",
                                metadata.width,
                                metadata.height,
                                metadata.fps,
                                if metadata.audio_channels == 0 {
                                    "No audio"
                                } else {
                                    "Captured audio"
                                },
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
                ui.add_space(Space::SM);
                rule(ui, self.theme);
                ui.add_space(Space::MD);
                let body_height = (ui.available_height() - 64.0).max(360.0);
                ScrollArea::vertical()
                    .id_salt("video-editor-workspace")
                    .auto_shrink([false, false])
                    .max_height(body_height)
                    .show(ui, |ui| match layout {
                        VideoEditorLayout::Wide => {
                            let inspector_width = 280.0;
                            ui.spacing_mut().item_spacing.x = Space::LG;
                            let media_width =
                                (ui.available_width() - inspector_width - Space::LG).max(420.0);
                            ui.horizontal_top(|ui| {
                                ui.allocate_ui_with_layout(
                                    vec2(media_width, ui.available_height()),
                                    Layout::top_down(Align::Min),
                                    |ui| {
                                        draw_media_workspace(
                                            ui,
                                            self.theme,
                                            document,
                                            self.model.preview,
                                            running,
                                            &mut plan,
                                            &mut plan_changed,
                                            &mut actions,
                                            &mut responses,
                                        );
                                    },
                                );
                                ui.allocate_ui_with_layout(
                                    vec2(inspector_width, ui.available_height()),
                                    Layout::top_down(Align::Min),
                                    |ui| {
                                        draw_inspector(
                                            ui,
                                            self.theme,
                                            document,
                                            running,
                                            true,
                                            &mut plan,
                                            &mut plan_changed,
                                            &mut actions,
                                            &mut responses.reveal,
                                            self.model.transcode,
                                        );
                                    },
                                );
                            });
                        }
                        VideoEditorLayout::Stacked => {
                            draw_media_workspace(
                                ui,
                                self.theme,
                                document,
                                self.model.preview,
                                running,
                                &mut plan,
                                &mut plan_changed,
                                &mut actions,
                                &mut responses,
                            );
                            ui.add_space(Space::LG);
                            draw_inspector(
                                ui,
                                self.theme,
                                document,
                                running,
                                false,
                                &mut plan,
                                &mut plan_changed,
                                &mut actions,
                                &mut responses.reveal,
                                self.model.transcode,
                            );
                        }
                    });
                ui.add_space(Space::MD);
                rule(ui, self.theme);
                ui.add_space(Space::MD);

                let validation_error = document
                    .validate_plan(&plan)
                    .err()
                    .map(|error| error.to_string());
                ui.horizontal(|ui| {
                    let close = button(ui, self.theme, "Cancel", false, !running);
                    if close.clicked() {
                        actions.push(VideoEditorAction::Close);
                    }
                    responses.close = Some(close);

                    ui.add_space(Space::SM);
                    if let Some(bytes) = estimated_output_bytes(plan, metadata) {
                        caption(
                            ui,
                            self.theme,
                            format!("Estimated size {}", format_file_size(bytes)),
                        );
                    } else {
                        caption(ui, self.theme, "Size is estimated during GIF export");
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let export_enabled = validation_error.is_none() && !running;
                        let export_label =
                            if matches!(self.model.transcode, TranscodeView::Finished { .. }) {
                                "Export again"
                            } else if plan.trim.start > Duration::ZERO || plan.trim.end < duration {
                                "Trim & Export"
                            } else {
                                "Export"
                            };
                        let export = button(ui, self.theme, export_label, true, export_enabled);
                        if export.clicked() {
                            actions.push(VideoEditorAction::Export(plan));
                        }
                        responses.export = Some(export);

                        if running {
                            let cancel = button(ui, self.theme, "Cancel export", false, true);
                            if cancel.clicked() {
                                actions.push(VideoEditorAction::CancelExport);
                            }
                            responses.cancel_export = Some(cancel);
                        }
                    });
                });
                if let Some(error) = &validation_error {
                    ui.colored_label(self.theme.palette.recording, error);
                }
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
            layout,
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
            close_response: responses.close.expect("the editor always draws close"),
            transport_response: responses
                .transport
                .expect("the editor always draws transport"),
            seek_response: responses.seek.expect("the editor always draws seek"),
            timeline_response: responses
                .timeline
                .expect("the editor always draws its trim timeline"),
            seek_backward_response: responses
                .seek_backward
                .expect("the editor always draws backward seek"),
            seek_forward_response: responses
                .seek_forward
                .expect("the editor always draws forward seek"),
            set_in_response: responses.set_in.expect("the editor always draws set-in"),
            set_out_response: responses.set_out.expect("the editor always draws set-out"),
            export_response: responses.export.expect("the editor always draws export"),
            cancel_export_response: responses.cancel_export,
            reveal_response: responses.reveal,
        }
    }
}

#[derive(Default)]
struct EditorResponses {
    close: Option<Response>,
    transport: Option<Response>,
    seek: Option<Response>,
    timeline: Option<Response>,
    seek_backward: Option<Response>,
    seek_forward: Option<Response>,
    set_in: Option<Response>,
    set_out: Option<Response>,
    export: Option<Response>,
    cancel_export: Option<Response>,
    reveal: Option<Response>,
}

#[allow(clippy::too_many_arguments)]
fn draw_media_workspace(
    ui: &mut Ui,
    theme: &Theme,
    document: &VideoDocument,
    preview: VideoPreview<'_>,
    running: bool,
    plan: &mut EditPlan,
    plan_changed: &mut bool,
    actions: &mut Vec<VideoEditorAction>,
    responses: &mut EditorResponses,
) {
    let metadata = document.metadata();
    draw_video_preview(ui, theme, preview, metadata.width, metadata.height);
    ui.add_space(Space::SM);

    Frame::new()
        .fill(theme.palette.card_fill_raised)
        .stroke(Stroke::new(1.0, theme.palette.hairline))
        .corner_radius(corner(Radius::BAR))
        .inner_margin(egui::Margin::same(Space::SM as i8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let label = if document.playback() == PlaybackState::Playing {
                    "Pause"
                } else {
                    "Play"
                };
                let transport = button(
                    ui,
                    theme,
                    label,
                    false,
                    !running && preview.phase != PlaybackPhase::Failed,
                );
                if transport.clicked() {
                    actions.push(if document.playback() == PlaybackState::Playing {
                        VideoEditorAction::Pause
                    } else {
                        VideoEditorAction::Play
                    });
                }
                responses.transport = Some(transport);
                ui.label(
                    RichText::new(format!(
                        "{} / {}",
                        format_precise_duration(document.position()),
                        format_precise_duration(document.duration())
                    ))
                    .font(theme.font(Text::Shortcut))
                    .color(theme.palette.text),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_enabled_ui(!running && preview.phase != PlaybackPhase::Failed, |ui| {
                        ComboBox::from_id_salt("video-editor-rate")
                            .selected_text(format!("{}×", format_rate(preview.rate)))
                            .popup_style(ui.style().as_ref().clone().into())
                            .show_ui(ui, |ui| {
                                for rate in [0.5_f32, 1.0, 1.5, 2.0] {
                                    if ui
                                        .selectable_label(
                                            (preview.rate - rate).abs() < f32::EPSILON,
                                            format!("{}×", format_rate(rate)),
                                        )
                                        .clicked()
                                    {
                                        actions.push(VideoEditorAction::SetRate(rate));
                                    }
                                }
                            });
                    });
                    caption(ui, theme, "Space play/pause");
                });
            });

            ui.add_space(Space::XS);
            ui.horizontal(|ui| {
                let frame = frame_duration(metadata.fps);
                let frame_back = button(
                    ui,
                    theme,
                    "‹ Frame",
                    false,
                    !running && document.position() > plan.trim.start,
                );
                if frame_back.clicked() {
                    actions.push(VideoEditorAction::Seek(
                        document
                            .position()
                            .saturating_sub(frame)
                            .max(plan.trim.start),
                    ));
                }
                let backward = button(
                    ui,
                    theme,
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
                responses.seek_backward = Some(backward);

                let mut position = document.position().as_secs_f64();
                let seek_width = (ui.available_width() - 170.0).max(120.0);
                let seek = ui
                    .add_enabled_ui(!running, |ui| {
                        ui.add_sized(
                            [seek_width, ui.spacing().interact_size.y],
                            Slider::new(
                                &mut position,
                                plan.trim.start.as_secs_f64()..=plan.trim.end.as_secs_f64(),
                            )
                            .show_value(false)
                            .text("Playback position")
                            .min_decimals(2)
                            .max_decimals(2)
                            .custom_formatter(|value, _| {
                                format_precise_duration(
                                    Duration::try_from_secs_f64(value).unwrap_or(Duration::ZERO),
                                )
                            })
                            .trailing_fill(true)
                            .handle_shape(egui::style::HandleShape::Rect { aspect_ratio: 0.4 })
                            .step_by(1.0 / metadata.fps.max(1.0)),
                        )
                    })
                    .inner;
                if seek.changed()
                    && let Ok(position) = Duration::try_from_secs_f64(position)
                {
                    actions.push(VideoEditorAction::Seek(position));
                }
                responses.seek = Some(seek);

                let forward = button(
                    ui,
                    theme,
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
                responses.seek_forward = Some(forward);
                let frame_forward = button(
                    ui,
                    theme,
                    "Frame ›",
                    false,
                    !running && document.position() < plan.trim.end,
                );
                if frame_forward.clicked() {
                    actions.push(VideoEditorAction::Seek(
                        document.position().saturating_add(frame).min(plan.trim.end),
                    ));
                }
            });
        });

    ui.add_space(Space::MD);
    section_label(ui, theme, "TRIM RANGE");
    ui.add_space(Space::XS);
    responses.timeline = Some(draw_trim_timeline(
        ui,
        theme,
        document,
        preview,
        running,
        plan,
        plan_changed,
        actions,
    ));
    ui.add_space(Space::SM);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("In {}", format_precise_duration(plan.trim.start)))
                .font(theme.font(Text::Shortcut))
                .color(theme.palette.text_muted),
        );
        let set_in = button(
            ui,
            theme,
            "Set in",
            false,
            !running
                && document.position() < plan.trim.end
                && document.position() != plan.trim.start,
        );
        if set_in.clicked() {
            plan.trim.start = document.position();
            *plan_changed = true;
        }
        responses.set_in = Some(set_in);
        ui.add_space(Space::SM);
        ui.label(
            RichText::new(format!("Out {}", format_precise_duration(plan.trim.end)))
                .font(theme.font(Text::Shortcut))
                .color(theme.palette.text_muted),
        );
        let set_out = button(
            ui,
            theme,
            "Set out",
            false,
            !running
                && document.position() > plan.trim.start
                && document.position() != plan.trim.end,
        );
        if set_out.clicked() {
            plan.trim.end = document.position();
            *plan_changed = true;
        }
        responses.set_out = Some(set_out);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            caption(
                ui,
                theme,
                format!(
                    "{} selected  •  I / O bounds  •  Shift+←/→ frame",
                    format_precise_duration(safe_trim_duration(plan.trim))
                ),
            );
        });
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimelineDrag {
    In,
    Out,
    Playhead,
}

#[allow(clippy::too_many_arguments)]
fn draw_trim_timeline(
    ui: &mut Ui,
    theme: &Theme,
    document: &VideoDocument,
    preview: VideoPreview<'_>,
    running: bool,
    plan: &mut EditPlan,
    plan_changed: &mut bool,
    actions: &mut Vec<VideoEditorAction>,
) -> Response {
    let desired = vec2(ui.available_width(), 126.0);
    let (outer, response) = ui.allocate_exact_size(desired, Sense::click_and_drag());
    let response = response.on_hover_text(
        "Filmstrip timeline. Click to seek; drag either trim handle to change the retained range.",
    );
    let strip = egui::Rect::from_min_max(
        pos2(outer.left(), outer.top() + 22.0),
        pos2(outer.right(), outer.bottom() - 30.0),
    );
    let waveform_rect = egui::Rect::from_min_max(
        pos2(strip.left(), strip.bottom() - 24.0),
        strip.right_bottom(),
    );
    let filmstrip_rect =
        egui::Rect::from_min_max(strip.left_top(), pos2(strip.right(), waveform_rect.top()));
    ui.painter()
        .rect_filled(strip, corner(Radius::THUMB), theme.palette.chip_fill);
    draw_filmstrip(ui, theme, filmstrip_rect, preview);
    draw_waveform(ui, theme, waveform_rect, preview);
    let painter = ui.painter();

    let total = document.duration().as_secs_f64().max(f64::EPSILON);
    let x_for = |time: Duration| {
        strip.left() + strip.width() * (time.as_secs_f64() / total).clamp(0.0, 1.0) as f32
    };
    let trim_left = x_for(plan.trim.start);
    let trim_right = x_for(plan.trim.end);
    let before = egui::Rect::from_min_max(strip.left_top(), pos2(trim_left, strip.bottom()));
    let after = egui::Rect::from_min_max(pos2(trim_right, strip.top()), strip.right_bottom());
    let shade = if theme.palette.is_dark() {
        egui::Color32::from_black_alpha(155)
    } else {
        egui::Color32::from_white_alpha(180)
    };
    painter.rect_filled(before, corner(Radius::THUMB), shade);
    painter.rect_filled(after, corner(Radius::THUMB), shade);
    painter.rect_stroke(
        egui::Rect::from_min_max(
            pos2(trim_left, strip.top()),
            pos2(trim_right.max(trim_left), strip.bottom()),
        ),
        corner(Radius::BUTTON),
        Stroke::new(3.0, theme.palette.accent_hi),
        StrokeKind::Inside,
    );
    draw_trim_handle(painter, theme, trim_left, strip, true);
    draw_trim_handle(painter, theme, trim_right, strip, false);

    let playhead = x_for(document.position());
    painter.line_segment(
        [
            pos2(playhead, strip.top() - Space::XS),
            pos2(playhead, strip.bottom() + Space::XS),
        ],
        Stroke::new(2.0, theme.palette.recording),
    );
    painter.circle_filled(
        pos2(playhead, strip.top() - Space::XS),
        4.0,
        theme.palette.recording,
    );
    painter.rect_stroke(
        strip,
        corner(Radius::THUMB),
        Stroke::new(1.0, theme.palette.thumb_border),
        StrokeKind::Inside,
    );

    if let Some(pointer) = response.hover_pos().filter(|point| strip.contains(*point)) {
        let time = duration_for_x(pointer.x, strip, document.duration());
        let label = format_precise_duration(time);
        let galley = painter.layout_no_wrap(label, theme.font(Text::Shortcut), theme.palette.text);
        let bubble = egui::Rect::from_center_size(
            pos2(
                pointer.x.clamp(strip.left() + 30.0, strip.right() - 30.0),
                outer.top() + 8.0,
            ),
            galley.size() + vec2(Space::MD, Space::XS),
        );
        painter.rect_filled(bubble, corner(Radius::CHIP), theme.palette.card_fill_raised);
        painter.rect_stroke(
            bubble,
            corner(Radius::CHIP),
            Stroke::new(1.0, theme.palette.hairline),
            StrokeKind::Inside,
        );
        painter.galley(
            bubble.center() - galley.size() / 2.0,
            galley,
            theme.palette.text,
        );
    }

    let drag_id = response.id.with("active-trim-handle");
    let pointer_pressed = ui.input(|input| input.pointer.primary_pressed());
    if pointer_pressed
        && response.hovered()
        && let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
    {
        let nearest = timeline_drag_for_x(pointer.x, trim_left, trim_right);
        ui.ctx().data_mut(|data| data.insert_temp(drag_id, nearest));
    }
    if !running
        && (response.dragged() || response.clicked())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let time = duration_for_x(pointer.x, strip, document.duration());
        let active = ui
            .ctx()
            .data(|data| data.get_temp::<TimelineDrag>(drag_id))
            .unwrap_or(TimelineDrag::Playhead);
        let minimum = frame_duration(document.metadata().fps);
        match active {
            TimelineDrag::In if time.saturating_add(minimum) <= plan.trim.end => {
                plan.trim.start = time;
                *plan_changed = true;
            }
            TimelineDrag::Out if time >= plan.trim.start.saturating_add(minimum) => {
                plan.trim.end = time;
                *plan_changed = true;
            }
            TimelineDrag::Playhead => {
                actions.push(VideoEditorAction::Seek(clamp_to_trim(time, plan.trim)));
            }
            TimelineDrag::In | TimelineDrag::Out => {}
        }
    }
    if response.drag_stopped() {
        ui.ctx().data_mut(|data| {
            data.remove::<TimelineDrag>(drag_id);
        });
    }
    let selected_fraction = (safe_trim_duration(plan.trim).as_secs_f64()
        / document.duration().as_secs_f64())
    .clamp(0.0, 1.0);
    response.widget_info(|| {
        WidgetInfo::slider(
            !running,
            selected_fraction,
            "Video trim timeline. I and O set trim bounds; click or drag the playhead to seek.",
        )
    });
    response.on_hover_cursor(CursorIcon::PointingHand)
}

fn draw_trim_handle(painter: &egui::Painter, theme: &Theme, x: f32, strip: egui::Rect, left: bool) {
    let center = if left { x + 4.0 } else { x - 4.0 };
    let rect =
        egui::Rect::from_center_size(pos2(center, strip.center().y), vec2(10.0, strip.height()));
    painter.rect_filled(rect, corner(Radius::CHIP), theme.palette.accent_hi);
    for offset in [-1.5_f32, 1.5] {
        painter.line_segment(
            [
                pos2(center + offset, strip.center().y - 8.0),
                pos2(center + offset, strip.center().y + 8.0),
            ],
            Stroke::new(1.0, theme.palette.on_accent),
        );
    }
}

#[derive(Clone)]
struct StoryboardTextureCache {
    stream_id: u64,
    handles: [Option<TextureHandle>; STORYBOARD_SLOTS],
}

fn storyboard_textures(
    ctx: &egui::Context,
    storyboard: Option<&StoryboardSnapshot>,
) -> [Option<SizedTexture>; STORYBOARD_SLOTS] {
    let Some(storyboard) = storyboard else {
        return std::array::from_fn(|_| None);
    };
    let id = egui::Id::new("scrozz-recording-editor-storyboard-textures");
    let mut cache = ctx
        .data_mut(|data| data.get_temp::<StoryboardTextureCache>(id))
        .filter(|cache| cache.stream_id == storyboard.stream_id)
        .unwrap_or_else(|| StoryboardTextureCache {
            stream_id: storyboard.stream_id,
            handles: std::array::from_fn(|_| None),
        });
    for (index, slot) in storyboard.frames.iter().enumerate() {
        if cache.handles[index].is_some() {
            continue;
        }
        let Some(slot) = slot else {
            continue;
        };
        let image = &slot.frame.image;
        let Ok(width) = usize::try_from(image.width) else {
            continue;
        };
        let Ok(height) = usize::try_from(image.height) else {
            continue;
        };
        let color = egui::ColorImage::from_rgba_unmultiplied([width, height], &image.data);
        cache.handles[index] = Some(ctx.load_texture(
            format!("scrozz.recording.storyboard.{index}"),
            color,
            egui::TextureOptions::LINEAR,
        ));
    }
    ctx.data_mut(|data| data.insert_temp(id, cache.clone()));
    std::array::from_fn(|index| cache.handles[index].as_ref().map(SizedTexture::from_handle))
}

fn draw_filmstrip(ui: &mut Ui, theme: &Theme, rect: egui::Rect, preview: VideoPreview<'_>) {
    let textures = storyboard_textures(ui.ctx(), preview.storyboard);
    let slot_width = rect.width() / STORYBOARD_SLOTS as f32;
    for (index, texture) in textures.into_iter().enumerate() {
        let slot = egui::Rect::from_min_max(
            pos2(rect.left() + slot_width * index as f32, rect.top()),
            pos2(rect.left() + slot_width * (index + 1) as f32, rect.bottom()),
        );
        let texture = texture.or(preview.texture);
        if let Some(texture) = texture {
            paint_cover(ui.painter(), texture, slot);
        } else {
            ui.painter().rect_filled(
                slot,
                0.0,
                if index % 2 == 0 {
                    theme.palette.card_fill_raised
                } else {
                    theme.palette.chip_fill
                },
            );
        }
        if index > 0 {
            ui.painter().line_segment(
                [slot.left_top(), slot.left_bottom()],
                Stroke::new(1.0, theme.palette.thumb_border),
            );
        }
    }
}

fn paint_cover(painter: &egui::Painter, texture: SizedTexture, rect: egui::Rect) {
    let source_aspect = texture.size.x / texture.size.y.max(1.0);
    let target_aspect = rect.width() / rect.height().max(1.0);
    let uv = if source_aspect > target_aspect {
        let visible = target_aspect / source_aspect;
        egui::Rect::from_min_max(
            pos2((1.0 - visible) / 2.0, 0.0),
            pos2((1.0 + visible) / 2.0, 1.0),
        )
    } else {
        let visible = source_aspect / target_aspect;
        egui::Rect::from_min_max(
            pos2(0.0, (1.0 - visible) / 2.0),
            pos2(1.0, (1.0 + visible) / 2.0),
        )
    };
    painter.image(texture.id, rect, uv, egui::Color32::WHITE);
}

fn draw_waveform(ui: &mut Ui, theme: &Theme, rect: egui::Rect, preview: VideoPreview<'_>) {
    ui.painter()
        .rect_filled(rect, 0.0, theme.palette.card_fill_raised);
    let Some(waveform) = preview
        .storyboard
        .and_then(|storyboard| storyboard.waveform.as_ref())
    else {
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            if preview.audio == PlaybackAudio::NoTrack {
                "No audio track"
            } else {
                "Building waveform…"
            },
            theme.font(Text::Caption),
            theme.palette.text_faint,
        );
        return;
    };
    let center = rect.center().y;
    const BARS: usize = 72;
    let step = rect.width() / BARS as f32;
    for index in 0..BARS {
        let sample = index as f32 * (waveform.len() - 1) as f32 / (BARS - 1) as f32;
        let left = sample.floor() as usize;
        let right = sample.ceil() as usize;
        let mix = sample.fract();
        let left_peak = waveform[left].unwrap_or(0.08);
        let right_peak = waveform[right].unwrap_or(left_peak);
        let peak = left_peak + (right_peak - left_peak) * mix;
        let height = peak.clamp(0.04, 1.0) * (rect.height() - 4.0);
        let x = rect.left() + (index as f32 + 0.5) * step;
        ui.painter().line_segment(
            [
                pos2(x, center - height / 2.0),
                pos2(x, center + height / 2.0),
            ],
            Stroke::new(step.min(3.0), theme.palette.accent_hi),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_inspector(
    ui: &mut Ui,
    theme: &Theme,
    document: &VideoDocument,
    running: bool,
    wide: bool,
    plan: &mut EditPlan,
    plan_changed: &mut bool,
    actions: &mut Vec<VideoEditorAction>,
    reveal_response: &mut Option<Response>,
    transcode: TranscodeView<'_>,
) {
    if wide {
        inspector_panel(ui, theme, "Video", |ui| {
            draw_video_controls(ui, theme, document, running, plan, plan_changed);
        });
        if document.recording().interactions().is_some() {
            ui.add_space(Space::SM);
            inspector_panel(ui, theme, "Interactions", |ui| {
                draw_interaction_controls(ui, theme, document, running, plan, plan_changed);
            });
        }
        ui.add_space(Space::SM);
        inspector_panel(ui, theme, "Audio", |ui| {
            draw_audio_controls(ui, theme, document, running, plan, plan_changed);
        });
        ui.add_space(Space::SM);
        inspector_panel(ui, theme, "Export", |ui| {
            draw_export_controls(ui, theme, document, running, plan, plan_changed);
            ui.add_space(Space::SM);
            draw_transcode_status(ui, theme, transcode, actions, reveal_response);
        });
    } else {
        inspector_panel(ui, theme, "", |ui| {
            CollapsingHeader::new("Video")
                .id_salt("video-editor-stacked-video")
                .default_open(true)
                .show(ui, |ui| {
                    draw_video_controls(ui, theme, document, running, plan, plan_changed);
                });
            if document.recording().interactions().is_some() {
                rule(ui, theme);
                CollapsingHeader::new("Interactions")
                    .id_salt("video-editor-stacked-interactions")
                    .default_open(true)
                    .show(ui, |ui| {
                        draw_interaction_controls(ui, theme, document, running, plan, plan_changed);
                    });
            }
            rule(ui, theme);
            CollapsingHeader::new("Audio")
                .id_salt("video-editor-stacked-audio")
                .default_open(true)
                .show(ui, |ui| {
                    draw_audio_controls(ui, theme, document, running, plan, plan_changed);
                });
            rule(ui, theme);
            CollapsingHeader::new("Export")
                .id_salt("video-editor-stacked-export")
                .default_open(true)
                .show(ui, |ui| {
                    draw_export_controls(ui, theme, document, running, plan, plan_changed);
                    draw_transcode_status(ui, theme, transcode, actions, reveal_response);
                });
        });
    }
}

fn draw_interaction_controls(
    ui: &mut Ui,
    theme: &Theme,
    document: &VideoDocument,
    running: bool,
    plan: &mut EditPlan,
    plan_changed: &mut bool,
) {
    let Some(interactions) = document.recording().interactions() else {
        caption(
            ui,
            theme,
            "This recording has no retained interaction timeline.",
        );
        return;
    };
    if !interactions.is_editable() {
        caption(
            ui,
            theme,
            "The interaction timeline is available, but its private source is no longer retained.",
        );
        return;
    }

    ui.add_enabled_ui(!running, |ui| {
        let summary = interactions.summary();
        ui.add_enabled_ui(summary.cursor_samples > 0, |ui| {
            if ui
                .checkbox(&mut plan.interactions.cursor, "Pointer")
                .on_hover_text("Show or hide the captured pointer without changing the source.")
                .changed()
            {
                if !plan.interactions.cursor {
                    plan.interactions.smooth_cursor = false;
                }
                *plan_changed = true;
            }
        });
        ui.add_enabled_ui(
            plan.interactions.cursor && summary.cursor_samples > 0,
            |ui| {
                if ui
                .checkbox(
                    &mut plan.interactions.smooth_cursor,
                    "Smooth pointer movement",
                )
                .on_hover_text(
                    "Apply deterministic bounded smoothing. Raw pointer timings remain unchanged.",
                )
                .changed()
            {
                *plan_changed = true;
            }
            },
        );
        ui.add_enabled_ui(interactions.clicks.enabled, |ui| {
            if ui
                .checkbox(&mut plan.interactions.clicks, "Click highlights")
                .changed()
            {
                *plan_changed = true;
            }
        });
        ui.add_enabled_ui(interactions.keystrokes.enabled, |ui| {
            if ui
                .checkbox(&mut plan.interactions.keystrokes, "Keystrokes")
                .changed()
            {
                *plan_changed = true;
            }
        });
    });
    if !interactions.clicks.enabled || !interactions.keystrokes.enabled {
        caption(
            ui,
            theme,
            "Layers not enabled during capture have no retained events to reveal later.",
        );
    }
    ui.add_space(Space::XS);
    caption(
        ui,
        theme,
        "Only filtered display labels are held in memory; no key event sidecar is written.",
    );
}

fn inspector_panel(ui: &mut Ui, theme: &Theme, title: &str, add_contents: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(theme.palette.card_fill_raised)
        .stroke(Stroke::new(1.0, theme.palette.hairline))
        .corner_radius(corner(Radius::BUTTON))
        .inner_margin(egui::Margin::same(Space::MD as i8))
        .show(ui, |ui| {
            if !title.is_empty() {
                section_label(ui, theme, title);
                ui.add_space(Space::SM);
            }
            add_contents(ui);
        });
}

fn draw_video_controls(
    ui: &mut Ui,
    theme: &Theme,
    document: &VideoDocument,
    running: bool,
    plan: &mut EditPlan,
    plan_changed: &mut bool,
) {
    let metadata = document.metadata();
    caption(ui, theme, "Quality");
    ui.add_enabled_ui(!running, |ui| {
        ComboBox::from_id_salt("video-editor-quality")
            .width(ui.available_width())
            .selected_text(quality_label(plan.quality))
            .popup_style(ui.style().as_ref().clone().into())
            .show_ui(ui, |ui| {
                for quality in Quality::ALL {
                    if ui
                        .selectable_value(&mut plan.quality, quality, quality_label(quality))
                        .changed()
                    {
                        *plan_changed = true;
                    }
                }
            });
    });

    ui.add_space(Space::SM);
    caption(ui, theme, "Output dimensions");
    let selected_dimensions = plan.custom_dimensions.map_or_else(
        || resolution_label(plan.resolution).to_owned(),
        |size| format!("Custom  {} × {}", size.width, size.height),
    );
    ui.add_enabled_ui(!running, |ui| {
        ComboBox::from_id_salt("video-editor-resolution")
            .width(ui.available_width())
            .selected_text(selected_dimensions)
            .popup_style(ui.style().as_ref().clone().into())
            .show_ui(ui, |ui| {
                for resolution in ResolutionCap::ALL {
                    let selected =
                        plan.custom_dimensions.is_none() && plan.resolution == resolution;
                    if ui
                        .selectable_label(selected, resolution_label(resolution))
                        .clicked()
                    {
                        plan.resolution = resolution;
                        plan.custom_dimensions = None;
                        *plan_changed = true;
                    }
                }
                if ui
                    .selectable_label(plan.custom_dimensions.is_some(), "Custom")
                    .clicked()
                    && plan.custom_dimensions.is_none()
                {
                    let (width, _) = plan.output_dimensions(metadata);
                    if let Ok(dimensions) = OutputDimensions::from_width(width, metadata) {
                        plan.custom_dimensions = Some(dimensions);
                        *plan_changed = true;
                    }
                }
            });
    });
    if let Some(dimensions) = plan.custom_dimensions {
        let mut width = dimensions.width;
        let mut height = dimensions.height;
        ui.horizontal(|ui| {
            ui.label("W");
            let width_response = ui.add_enabled(
                !running,
                DragValue::new(&mut width)
                    .range(2..=metadata.width.max(2))
                    .speed(2.0)
                    .suffix(" px"),
            );
            ui.label("H");
            let height_response = ui.add_enabled(
                !running,
                DragValue::new(&mut height)
                    .range(2..=metadata.height.max(2))
                    .speed(2.0)
                    .suffix(" px"),
            );
            if width_response.changed()
                && let Ok(size) = OutputDimensions::from_width(width, metadata)
            {
                plan.custom_dimensions = Some(size);
                *plan_changed = true;
            } else if height_response.changed()
                && let Ok(size) = OutputDimensions::from_height(height, metadata)
            {
                plan.custom_dimensions = Some(size);
                *plan_changed = true;
            }
        });
        caption(ui, theme, "Aspect ratio is locked; output never upscales.");
    }

    ui.add_space(Space::SM);
    caption(ui, theme, "Frame rate");
    body(
        ui,
        theme,
        format!("Match source  •  {:.2} fps", metadata.fps),
    );
}

fn draw_audio_controls(
    ui: &mut Ui,
    theme: &Theme,
    document: &VideoDocument,
    running: bool,
    plan: &mut EditPlan,
    plan_changed: &mut bool,
) {
    let channels = document.metadata().audio_channels;
    let available = channels > 0 && plan.output.supports_audio();
    if channels == 0 {
        caption(
            ui,
            theme,
            "This recording has no audio track. Audio controls are unavailable.",
        );
        return;
    }
    if !plan.output.supports_audio() {
        caption(
            ui,
            theme,
            "GIF cannot contain audio. The export will be muted.",
        );
        return;
    }

    ui.add_enabled_ui(!running && available, |ui| {
        let keep = !plan.audio.mute && plan.audio.channels == ChannelBehavior::Preserve;
        if ui.radio(keep, "Keep original").clicked() {
            plan.audio.mute = false;
            plan.audio.channels = ChannelBehavior::Preserve;
            *plan_changed = true;
        }
        if channels > 1 {
            let mono = !plan.audio.mute && plan.audio.channels == ChannelBehavior::StereoToMono;
            if ui.radio(mono, "Convert to mono").clicked() {
                plan.audio.mute = false;
                plan.audio.channels = ChannelBehavior::StereoToMono;
                *plan_changed = true;
            }
        }
        if ui.radio(plan.audio.mute, "Mute").clicked() {
            plan.audio.mute = true;
            *plan_changed = true;
        }
        ui.add_space(Space::SM);
        caption(ui, theme, "Gain");
        let mut percent = plan.audio.volume * 100.0;
        let gain = ui.add_enabled(
            !plan.audio.mute,
            Slider::new(&mut percent, 0.0..=200.0)
                .suffix("%")
                .integer()
                .text("Audio gain"),
        );
        if gain.changed() {
            plan.audio.volume = percent / 100.0;
            *plan_changed = true;
        }
    });
}

fn draw_export_controls(
    ui: &mut Ui,
    theme: &Theme,
    document: &VideoDocument,
    running: bool,
    plan: &mut EditPlan,
    plan_changed: &mut bool,
) {
    caption(ui, theme, "Format");
    ui.add_enabled_ui(!running, |ui| {
        ComboBox::from_id_salt("video-editor-format")
            .width(ui.available_width())
            .selected_text(match plan.output {
                EditOutput::Video => "MP4  •  H.264",
                EditOutput::Animation(_) => "GIF  •  no audio",
            })
            .popup_style(ui.style().as_ref().clone().into())
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(matches!(plan.output, EditOutput::Video), "MP4  •  H.264")
                    .clicked()
                {
                    plan.output = EditOutput::Video;
                    *plan_changed = true;
                }
                if ui
                    .selectable_label(
                        matches!(plan.output, EditOutput::Animation(_)),
                        "GIF  •  no audio",
                    )
                    .clicked()
                    && let Ok(gif) = EditPlan::gif(document)
                {
                    plan.output = gif.output;
                    plan.audio.mute = true;
                    *plan_changed = true;
                }
            });
    });
    ui.add_space(Space::SM);
    let (width, height) = plan.output_dimensions(document.metadata());
    caption(
        ui,
        theme,
        format!(
            "{} × {}  •  {}",
            width,
            height,
            if plan.output.supports_audio() {
                "Source preserved"
            } else {
                "Silent animation"
            }
        ),
    );
    caption(
        ui,
        theme,
        "A new file is created beside the source. The original is never overwritten.",
    );
}

#[allow(clippy::too_many_arguments)]
fn handle_editor_keyboard(
    ui: &Ui,
    document: &VideoDocument,
    running: bool,
    playback_available: bool,
    plan: &mut EditPlan,
    plan_changed: &mut bool,
    actions: &mut Vec<VideoEditorAction>,
) {
    let keyboard_owned = ui.ctx().memory(|memory| memory.focused().is_some());
    let (space, escape, left, right, shift, command, set_in, set_out, export, close) =
        ui.ctx().input(|input| {
            (
                input.key_pressed(Key::Space),
                input.key_pressed(Key::Escape),
                input.key_pressed(Key::ArrowLeft),
                input.key_pressed(Key::ArrowRight),
                input.modifiers.shift,
                input.modifiers.command,
                input.key_pressed(Key::I),
                input.key_pressed(Key::O),
                input.key_pressed(Key::E),
                input.key_pressed(Key::W),
            )
        });
    if (escape || (command && close)) && !running {
        actions.push(VideoEditorAction::Close);
    }
    if command && export && !running && document.validate_plan(plan).is_ok() {
        actions.push(VideoEditorAction::Export(*plan));
    }
    if command && (export || close) {
        ui.ctx().input_mut(|input| {
            let modifiers = input.modifiers;
            let _ = input.consume_key(modifiers, if export { Key::E } else { Key::W });
        });
    }
    if keyboard_owned || running {
        return;
    }
    if space && playback_available {
        actions.push(if document.playback() == PlaybackState::Playing {
            VideoEditorAction::Pause
        } else {
            VideoEditorAction::Play
        });
    }
    let step = if shift {
        frame_duration(document.metadata().fps)
    } else {
        Duration::from_secs(5)
    };
    if left {
        actions.push(VideoEditorAction::Seek(
            document
                .position()
                .saturating_sub(step)
                .max(plan.trim.start),
        ));
    }
    if right {
        actions.push(VideoEditorAction::Seek(
            document.position().saturating_add(step).min(plan.trim.end),
        ));
    }
    if set_in && document.position() < plan.trim.end {
        plan.trim.start = document.position();
        *plan_changed = true;
    }
    if set_out && document.position() > plan.trim.start {
        plan.trim.end = document.position();
        *plan_changed = true;
    }
    if space || escape || left || right || set_in || set_out || export || close {
        ui.ctx().input_mut(|input| {
            let modifiers = input.modifiers;
            for key in [
                Key::Space,
                Key::Escape,
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::I,
                Key::O,
                Key::E,
                Key::W,
            ] {
                let _ = input.consume_key(modifiers, key);
            }
        });
    }
}

fn duration_for_x(x: f32, rect: egui::Rect, duration: Duration) -> Duration {
    let fraction = ((x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
    Duration::try_from_secs_f64(duration.as_secs_f64() * f64::from(fraction)).unwrap_or(duration)
}

fn timeline_drag_for_x(pointer_x: f32, trim_left: f32, trim_right: f32) -> TimelineDrag {
    let left_distance = (pointer_x - trim_left).abs();
    let right_distance = (pointer_x - trim_right).abs();
    let nearest = left_distance.min(right_distance);
    if nearest > 14.0 {
        TimelineDrag::Playhead
    } else if left_distance <= right_distance {
        TimelineDrag::In
    } else {
        TimelineDrag::Out
    }
}

fn safe_trim_duration(trim: scrozz_record::edit::TrimRange) -> Duration {
    trim.end.checked_sub(trim.start).unwrap_or_default()
}

fn clamp_to_trim(time: Duration, trim: scrozz_record::edit::TrimRange) -> Duration {
    if trim.start <= trim.end {
        time.clamp(trim.start, trim.end)
    } else {
        time
    }
}

fn frame_duration(fps: f64) -> Duration {
    Duration::try_from_secs_f64(1.0 / fps.clamp(1.0, 240.0)).unwrap_or(Duration::from_millis(33))
}

fn format_precise_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3_600;
    let minutes = total % 3_600 / 60;
    let seconds = total % 60;
    let centiseconds = duration.subsec_millis() / 10;
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}.{centiseconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}.{centiseconds:02}")
    }
}

fn estimated_output_bytes(
    plan: EditPlan,
    metadata: scrozz_record::edit::SourceMetadata,
) -> Option<u64> {
    if !plan.output.supports_audio() {
        return None;
    }
    let (width, height) = plan.output_dimensions(metadata);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let fps = metadata.fps.round().clamp(1.0, 240.0) as u32;
    let video_bps = plan.quality.target_bitrate(width, height, fps);
    let audio_bps: u64 = if plan.output_audio_channels(metadata) == 0 {
        0
    } else {
        192_000
    };
    let bits =
        (u128::from(video_bps) + u128::from(audio_bps)) * safe_trim_duration(plan.trim).as_millis();
    u64::try_from(bits / 8_000).ok()
}

fn format_file_size(bytes: u64) -> String {
    if bytes < 1_000_000 {
        format!("{:.0} KB", bytes as f64 / 1_000.0)
    } else if bytes < 1_000_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else {
        format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
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
    let preview_height = (available_width / aspect).clamp(220.0, 250.0);
    let size = vec2(
        (preview_height * aspect).min(available_width),
        preview_height,
    );
    ui.vertical_centered(|ui| {
        let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
        response.widget_info(|| {
            WidgetInfo::labeled(WidgetType::Image, true, "Decoded recording preview")
        });
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
        format!("  •  A/V sync {} ms", drift.as_millis())
    });
    caption(
        ui,
        theme,
        format!(
            "{}  •  {audio}  •  buffered to {} ({} frames){drift}",
            playback_phase_label(preview.phase),
            format_duration(preview.buffered_until),
            preview.buffered_frames,
        ),
    );
    if let Some(error) = preview.error {
        ui.colored_label(theme.palette.recording, error);
    }
    if let Some(error) = preview
        .storyboard
        .and_then(|storyboard| storyboard.error.as_deref())
    {
        ui.colored_label(
            theme.palette.warning,
            format!("Timeline preview is incomplete: {error}"),
        );
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
                buffered_frames: 8,
                audio: if fixture.document.metadata().audio_channels == 0 {
                    PlaybackAudio::NoTrack
                } else if fixture.plan.audio.mute || !fixture.plan.output.supports_audio() {
                    PlaybackAudio::Muted
                } else {
                    PlaybackAudio::Active
                },
                av_drift: Some(Duration::ZERO),
                error: None,
                storyboard: None,
            };
            let storyboard = fixture_storyboard(
                fixture.document.duration(),
                fixture.document.metadata().audio_channels > 0,
            );
            let preview = VideoPreview {
                storyboard: Some(&storyboard),
                ..preview
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

#[allow(clippy::cast_precision_loss)]
fn fixture_storyboard(duration: Duration, has_audio: bool) -> StoryboardSnapshot {
    StoryboardSnapshot {
        stream_id: 0,
        timestamps: std::array::from_fn(|index| {
            Duration::try_from_secs_f64(
                duration.as_secs_f64() * index as f64 / (STORYBOARD_SLOTS - 1) as f64,
            )
            .unwrap_or(duration)
        }),
        frames: std::array::from_fn(|_| None),
        waveform: has_audio
            .then(|| std::array::from_fn(|index| Some(0.18 + ((index * 7) % 10) as f32 / 12.0))),
        complete: true,
        error: None,
    }
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
        assert_eq!(light.animation_time, 0.0);
        assert_eq!(dark.animation_time, 0.0);
        assert_eq!(
            base.visuals.panel_fill,
            egui::Style::default().visuals.panel_fill
        );
    }

    #[test]
    fn editor_text_and_focus_survive_high_contrast_requirements() {
        for theme in [Theme::dark(), Theme::light()] {
            let canvas = theme.palette.canvas();
            assert!(
                crate::theme::contrast_ratio(theme.palette.text, canvas)
                    >= crate::theme::Contrast::AA_TEXT
            );
            assert!(
                crate::theme::contrast_ratio(theme.palette.focus_ring, canvas)
                    >= crate::theme::Contrast::AA_LARGE
            );
        }
    }

    #[test]
    fn overlapping_trim_handles_select_the_nearest_endpoint() {
        assert_eq!(timeline_drag_for_x(100.0, 96.0, 100.0), TimelineDrag::Out);
        assert_eq!(timeline_drag_for_x(96.0, 96.0, 100.0), TimelineDrag::In);
        assert_eq!(
            timeline_drag_for_x(60.0, 96.0, 100.0),
            TimelineDrag::Playhead
        );
    }
}
