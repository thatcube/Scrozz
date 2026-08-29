//! Compact recording status and transport controls.

use std::time::Duration;

use egui::{
    Color32, ComboBox, Image, Response, RichText, Sense, Slider, Stroke, TextureHandle, Ui,
    load::SizedTexture, vec2,
};
use scrozz_record::{
    CameraPreview, CameraRuntimeStatus, Recording,
    camera::render_camera_preview,
    engine::EngineCapabilities,
    machine::{
        ClockDrift, DRIFT_EVENT_THRESHOLD_SECS, MachineFailure, RecordingMachine, RecordingPhase,
    },
    overlay::move_camera,
    settings::{CameraSettings, CameraShape, OverlayAnchor},
};

use crate::{
    camera_settings::preview_size,
    harness::{RecordingFixture, Scene, SceneCtx},
    recording_controls::{
        body, button, caption, format_duration, heading, install_scene_theme, panel, rule,
        scene_theme,
    },
    theme::{Radius, Space, Text, Theme, corner},
};

/// Immutable recording values displayed by [`RecordingHud`].
#[derive(Debug, Clone, Copy)]
pub struct RecordingHudModel<'a> {
    /// Exact lifecycle phase.
    pub phase: RecordingPhase,
    /// Authoritative virtual elapsed time, excluding pauses.
    pub elapsed: Duration,
    /// Capabilities advertised by the active engine.
    pub capabilities: EngineCapabilities,
    /// Most recent recoverable warning.
    pub warning: Option<&'a str>,
    /// Most recent engine-clock comparison.
    pub drift: Option<ClockDrift>,
    /// Complete terminal output.
    pub output: Option<&'a Recording>,
    /// Terminal failure and any salvageable output.
    pub failure: Option<&'a MachineFailure>,
    /// Pixel-free camera health, present only for camera recordings.
    pub camera_status: Option<&'a CameraRuntimeStatus>,
    /// Latest camera frame for the explicit live preview.
    pub camera_preview: Option<&'a CameraPreview>,
    /// Mutable composition values shown by the HUD.
    pub camera_settings: Option<CameraSettings>,
}

/// Owned HUD values suitable for crossing the application/overlay thread seam.
#[derive(Debug, Clone)]
pub struct RecordingHudSnapshot {
    /// Exact lifecycle phase.
    pub phase: RecordingPhase,
    /// Authoritative pause-free elapsed time.
    pub elapsed: Duration,
    /// Native engine capabilities.
    pub capabilities: EngineCapabilities,
    /// Most recent warning.
    pub warning: Option<String>,
    /// Most recent clock comparison.
    pub drift: Option<ClockDrift>,
    /// Complete terminal output.
    pub output: Option<Recording>,
    /// Terminal failure and retained partial output.
    pub failure: Option<MachineFailure>,
    /// Pixel-free live camera health.
    pub camera_status: Option<CameraRuntimeStatus>,
    /// Latest live preview frame.
    pub camera_preview: Option<CameraPreview>,
    /// Current camera composition values.
    pub camera_settings: Option<CameraSettings>,
}

impl RecordingHudSnapshot {
    /// Copies the small observable surface of a recording machine.
    #[must_use]
    pub fn from_machine(machine: &RecordingMachine) -> Self {
        Self {
            phase: machine.phase(),
            elapsed: machine.elapsed(),
            capabilities: machine.capabilities(),
            warning: machine.warnings().last().cloned(),
            drift: machine.latest_drift(),
            output: machine.output().cloned(),
            failure: machine.failure().cloned(),
            camera_status: machine.camera_status(),
            camera_preview: machine.camera_preview(),
            camera_settings: machine
                .request()
                .and_then(|request| request.camera.as_ref().map(|camera| camera.settings)),
        }
    }

    /// Borrows this owned snapshot as the widget model.
    #[must_use]
    pub fn model(&self) -> RecordingHudModel<'_> {
        RecordingHudModel {
            phase: self.phase,
            elapsed: self.elapsed,
            capabilities: self.capabilities,
            warning: self.warning.as_deref(),
            drift: self.drift,
            output: self.output.as_ref(),
            failure: self.failure.as_ref(),
            camera_status: self.camera_status.as_ref(),
            camera_preview: self.camera_preview.as_ref(),
            camera_settings: self.camera_settings,
        }
    }
}

impl<'a> RecordingHudModel<'a> {
    /// Borrows every HUD value from a recording machine without owning it.
    #[must_use]
    pub fn from_machine(machine: &'a RecordingMachine) -> Self {
        Self {
            phase: machine.phase(),
            elapsed: machine.elapsed(),
            capabilities: machine.capabilities(),
            warning: machine.warnings().last().map(String::as_str),
            drift: machine.latest_drift(),
            output: machine.output(),
            failure: machine.failure(),
            camera_status: None,
            camera_preview: None,
            camera_settings: machine
                .request()
                .and_then(|request| request.camera.as_ref().map(|camera| camera.settings)),
        }
    }
}

/// A semantic action requested by the HUD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecordingHudAction {
    /// Dismiss terminal recording state.
    Dismiss,
    /// Pause an active recording.
    Pause,
    /// Resume a paused recording.
    Resume,
    /// Stop and finalise an active or paused recording.
    Stop,
    /// Reveal the complete output in the platform file browser.
    RevealOutput,
    /// Reveal salvageable partial output in the platform file browser.
    RevealPartialOutput,
    /// Apply a live camera composition change.
    CameraChanged(CameraSettings),
}

/// Enabled state for every HUD control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingHudControls {
    /// Whether pause or resume can be requested.
    pub pause_resume_enabled: bool,
    /// Whether stop can be requested.
    pub stop_enabled: bool,
    /// Whether complete output can be revealed.
    pub reveal_output_enabled: bool,
    /// Whether partial output can be revealed.
    pub reveal_partial_enabled: bool,
    /// Whether live camera composition controls are available.
    pub camera_enabled: bool,
}

impl RecordingHudControls {
    /// Derives control state solely from current phase, capabilities, and output.
    #[must_use]
    pub fn for_model(model: &RecordingHudModel<'_>) -> Self {
        let active = matches!(
            model.phase,
            RecordingPhase::Recording | RecordingPhase::Paused
        );
        Self {
            pause_resume_enabled: active && model.capabilities.pause_resume,
            stop_enabled: active,
            reveal_output_enabled: model.output.is_some(),
            reveal_partial_enabled: model
                .failure
                .and_then(|failure| failure.partial.as_ref())
                .is_some(),
            camera_enabled: active && model.camera_status.is_some_and(|camera| camera.active),
        }
    }
}

/// Result of drawing a [`RecordingHud`].
#[derive(Debug)]
pub struct RecordingHudResponse {
    /// Semantic action requested this pass.
    pub action: Option<RecordingHudAction>,
    /// Derived enabled state.
    pub controls: RecordingHudControls,
    /// Response for the whole panel.
    pub response: Response,
    /// Pause/resume control, present on every phase.
    pub pause_resume_response: Response,
    /// Stop control, present on every phase.
    pub stop_response: Response,
    /// Reveal control, when terminal output exists.
    pub reveal_response: Option<Response>,
}

/// Compact always-on-top-style recording control surface.
pub struct RecordingHud<'a> {
    model: RecordingHudModel<'a>,
    theme: &'a Theme,
}

impl<'a> RecordingHud<'a> {
    /// Creates a HUD from caller-owned values.
    #[must_use]
    pub const fn new(model: RecordingHudModel<'a>, theme: &'a Theme) -> Self {
        Self { model, theme }
    }

    /// Draws the HUD and returns semantic requests without touching an engine.
    pub fn show(self, ui: &mut Ui) -> RecordingHudResponse {
        let controls = RecordingHudControls::for_model(&self.model);
        let mut action = None;
        let mut pause_resume_response = None;
        let mut stop_response = None;
        let mut reveal_response = None;

        let inner = panel(ui, self.theme, 356.0, |ui| {
            ui.horizontal(|ui| {
                status_mark(ui, self.theme, self.model.phase);
                ui.vertical(|ui| {
                    heading(ui, self.theme, phase_label(self.model.phase));
                    ui.label(
                        RichText::new(format_duration(self.model.elapsed))
                            .font(self.theme.font(Text::Display))
                            .color(self.theme.palette.text),
                    );
                });
            });

            if let Some(warning) = self.model.warning {
                ui.add_space(Space::MD);
                notice(
                    ui,
                    self.theme,
                    self.theme.palette.warning,
                    "Warning",
                    warning,
                );
            }

            if let Some(drift) = self.model.drift {
                let millis = drift.delta_secs * 1_000.0;
                let severity = if drift.delta_secs.abs() >= DRIFT_EVENT_THRESHOLD_SECS {
                    self.theme.palette.warning
                } else {
                    self.theme.palette.text_faint
                };
                ui.add_space(Space::SM);
                notice(
                    ui,
                    self.theme,
                    severity,
                    "Timing",
                    &format!("Encoder clock {millis:+.0} ms from recording time"),
                );
            }

            if let (Some(status), Some(settings)) =
                (self.model.camera_status, self.model.camera_settings)
            {
                ui.add_space(Space::MD);
                let changed = camera_controls(
                    ui,
                    self.theme,
                    self.model.camera_preview,
                    status,
                    settings,
                    controls.camera_enabled,
                );
                if let Some(settings) = changed {
                    action = Some(RecordingHudAction::CameraChanged(settings));
                }
            }

            if let Some(failure) = self.model.failure {
                ui.add_space(Space::MD);
                notice(
                    ui,
                    self.theme,
                    self.theme.palette.recording,
                    "Recording failed",
                    &failure.error.to_string(),
                );
                if let Some(partial) = &failure.partial {
                    ui.add_space(Space::SM);
                    body(
                        ui,
                        self.theme,
                        format!(
                            "A partial recording is available at {}.",
                            partial.path().display()
                        ),
                    );
                    if let Some(reason) = partial.partial_reason() {
                        caption(ui, self.theme, format!("Could not finalise: {reason}"));
                    }
                } else {
                    caption(ui, self.theme, "No usable recording was written.");
                }
                if let Some(recovery) = failure.recovery_error.as_deref() {
                    caption(ui, self.theme, format!("Recovery also failed: {recovery}"));
                }
            } else if let Some(output) = self.model.output {
                ui.add_space(Space::MD);
                body(
                    ui,
                    self.theme,
                    format!("Saved to {}", output.path().display()),
                );
            }

            ui.add_space(Space::LG);
            rule(ui, self.theme);
            ui.add_space(Space::MD);
            ui.horizontal(|ui| {
                let pause_label = if self.model.phase == RecordingPhase::Paused {
                    "Resume"
                } else {
                    "Pause"
                };
                let pause = button(
                    ui,
                    self.theme,
                    pause_label,
                    false,
                    controls.pause_resume_enabled,
                );
                if pause.clicked() {
                    action = Some(if self.model.phase == RecordingPhase::Paused {
                        RecordingHudAction::Resume
                    } else {
                        RecordingHudAction::Pause
                    });
                }
                pause_resume_response = Some(pause);

                let stop = button(
                    ui,
                    self.theme,
                    if self.model.phase == RecordingPhase::Finalising {
                        "Finalising…"
                    } else {
                        "Stop"
                    },
                    true,
                    controls.stop_enabled,
                );
                if stop.clicked() {
                    action = Some(RecordingHudAction::Stop);
                }
                stop_response = Some(stop);

                let reveal = if controls.reveal_partial_enabled {
                    Some(button(ui, self.theme, "Show partial", false, true))
                } else if controls.reveal_output_enabled {
                    Some(button(ui, self.theme, "Show file", false, true))
                } else {
                    None
                };
                if let Some(response) = reveal {
                    if response.clicked() {
                        action = Some(if controls.reveal_partial_enabled {
                            RecordingHudAction::RevealPartialOutput
                        } else {
                            RecordingHudAction::RevealOutput
                        });
                    }
                    reveal_response = Some(response);
                }
                if matches!(
                    self.model.phase,
                    RecordingPhase::Finished | RecordingPhase::Failed
                ) && button(ui, self.theme, "Done", false, true).clicked()
                {
                    action = Some(RecordingHudAction::Dismiss);
                }
            });
        });

        RecordingHudResponse {
            action,
            controls,
            response: inner.response,
            pause_resume_response: pause_resume_response
                .expect("the HUD always draws its pause/resume control"),
            stop_response: stop_response.expect("the HUD always draws its stop control"),
            reveal_response,
        }
    }
}

fn camera_controls(
    ui: &mut Ui,
    theme: &Theme,
    preview: Option<&CameraPreview>,
    status: &CameraRuntimeStatus,
    mut settings: CameraSettings,
    enabled: bool,
) -> Option<CameraSettings> {
    let original = settings;
    ui.horizontal(|ui| {
        let (rect, response) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        ui.painter().circle_filled(
            rect.center(),
            4.0,
            if status.active {
                theme.palette.success
            } else {
                theme.palette.warning
            },
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Image,
                true,
                if status.active {
                    "Camera active; privacy indicator visible"
                } else {
                    "Camera unavailable"
                },
            )
        });
        crate::recording_controls::section_label(
            ui,
            theme,
            if status.active {
                "CAMERA ACTIVE"
            } else {
                "CAMERA UNAVAILABLE"
            },
        );
        if status.dropped_frames != 0 {
            caption(ui, theme, format!("{} dropped", status.dropped_frames));
        }
    });
    if let Some(message) = status.warning.as_deref() {
        caption(ui, theme, message);
    }
    if let Some(preview) = preview
        && let Some(texture) = camera_texture(ui.ctx(), preview)
    {
        let size = texture.size;
        let response = ui.add(
            Image::from_texture(texture)
                .fit_to_exact_size(size)
                .corner_radius(corner(Radius::BUTTON))
                .sense(Sense::drag()),
        );
        response
            .clone()
            .on_hover_text("Drag to place camera; preview is mirrored exactly as recorded");
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Image,
                true,
                "Live camera preview; drag to place",
            )
        });
        if enabled
            && response.dragged()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let local = pointer - response.rect.min;
            let output = scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(0.0, 0.0),
                scrozz_core::LogicalSize::new(
                    f64::from(response.rect.width()),
                    f64::from(response.rect.height()),
                ),
            );
            if let Ok(moved) = move_camera(
                output,
                preview.frame.oriented_aspect(),
                4.0,
                8.0,
                scrozz_core::LogicalPoint::new(f64::from(local.x), f64::from(local.y)),
                settings,
            ) {
                settings = moved;
            }
        }
    }
    ui.add_enabled_ui(enabled, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut settings.presenter, false, "Picture in picture");
            ui.selectable_value(&mut settings.presenter, true, "Presenter");
        });
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Position")
                    .font(theme.font(Text::Label))
                    .color(theme.palette.text_muted),
            );
            let previous = settings.position;
            ComboBox::from_id_salt("recording-camera-position")
                .selected_text(camera_anchor_label(settings.position))
                .show_ui(ui, |ui| {
                    for anchor in OverlayAnchor::ALL {
                        ui.selectable_value(
                            &mut settings.position,
                            anchor,
                            camera_anchor_label(anchor),
                        );
                    }
                });
            if settings.position != previous {
                settings.placement = None;
            }
        });
        if settings.presenter {
            caption(
                ui,
                theme,
                "Presenter fills the canvas; PiP shape is preserved.",
            );
        } else {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Shape")
                        .font(theme.font(Text::Label))
                        .color(theme.palette.text_muted),
                );
                ComboBox::from_id_salt("recording-camera-shape")
                    .selected_text(camera_shape_label(settings.shape))
                    .show_ui(ui, |ui| {
                        for shape in CameraShape::ALL {
                            ui.selectable_value(
                                &mut settings.shape,
                                shape,
                                camera_shape_label(shape),
                            );
                        }
                    });
            });
        }
        ui.add(
            Slider::new(
                &mut settings.size,
                CameraSettings::MIN_SIZE..=CameraSettings::MAX_SIZE,
            )
            .text("Camera size")
            .show_value(false),
        );
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut settings.mirror, "Mirror");
            ui.checkbox(&mut settings.border, "Border");
            ui.checkbox(&mut settings.shadow, "Shadow");
            if settings.presenter {
                ui.checkbox(&mut settings.presenter_screen, "Screen inset");
            }
        });
    });
    (settings != original).then_some(settings)
}

fn camera_anchor_label(anchor: OverlayAnchor) -> &'static str {
    match anchor {
        OverlayAnchor::TopLeft => "Top left",
        OverlayAnchor::TopCenter => "Top center",
        OverlayAnchor::TopRight => "Top right",
        OverlayAnchor::BottomLeft => "Bottom left",
        OverlayAnchor::BottomCenter => "Bottom center",
        OverlayAnchor::BottomRight => "Bottom right",
    }
}

fn camera_shape_label(shape: CameraShape) -> &'static str {
    match shape {
        CameraShape::Circle => "Circle",
        CameraShape::Rounded => "Rounded rectangle",
        CameraShape::Square => "Square",
        CameraShape::Rectangle => "Rectangle",
    }
}

#[derive(Clone)]
struct CameraTexture {
    sequence: u64,
    settings: CameraSettings,
    handle: TextureHandle,
}

fn camera_texture(ctx: &egui::Context, preview: &CameraPreview) -> Option<SizedTexture> {
    let id = egui::Id::new("scrozz-recording-camera-preview");
    let sequence = preview.status.frames_received;
    let mut state = ctx.data_mut(|data| data.get_temp::<CameraTexture>(id));
    if state
        .as_ref()
        .is_none_or(|texture| texture.sequence != sequence || texture.settings != preview.settings)
    {
        let (width, height) = preview_size(preview, 144, 96);
        let rendered =
            render_camera_preview(&preview.frame, width, height, preview.settings).ok()?;
        let mut rgba = rendered.data;
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        let image =
            egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
        if let Some(state) = &mut state {
            state.handle.set(image, egui::TextureOptions::LINEAR);
            state.sequence = sequence;
            state.settings = preview.settings;
        } else {
            state = Some(CameraTexture {
                sequence,
                settings: preview.settings,
                handle: ctx.load_texture(
                    "scrozz.recording.camera.preview",
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

fn status_mark(ui: &mut Ui, theme: &Theme, phase: RecordingPhase) {
    let (rect, response) = ui.allocate_exact_size(vec2(42.0, 42.0), Sense::hover());
    let color = match phase {
        RecordingPhase::Recording => theme.palette.recording,
        RecordingPhase::Paused | RecordingPhase::Countdown | RecordingPhase::Finalising => {
            theme.palette.warning
        }
        RecordingPhase::Finished => theme.palette.success,
        RecordingPhase::Failed => theme.palette.recording,
        RecordingPhase::Idle | RecordingPhase::Selecting => theme.palette.text_faint,
    };
    ui.painter().circle_filled(rect.center(), 10.0, color);
    ui.painter().circle_stroke(
        rect.center(),
        15.0,
        Stroke::new(1.0, color.linear_multiply(0.5)),
    );
    response.on_hover_text(phase_label(phase));
}

fn phase_label(phase: RecordingPhase) -> &'static str {
    match phase {
        RecordingPhase::Idle => "Ready to record",
        RecordingPhase::Selecting => "Selecting target",
        RecordingPhase::Countdown => "Starting soon",
        RecordingPhase::Recording => "Recording",
        RecordingPhase::Paused => "Recording paused",
        RecordingPhase::Finalising => "Finalising recording",
        RecordingPhase::Finished => "Recording finished",
        RecordingPhase::Failed => "Recording failed",
    }
}

fn notice(ui: &mut Ui, theme: &Theme, color: Color32, title: &str, message: &str) {
    let frame = egui::Frame::new()
        .fill(theme.palette.chip_fill)
        .stroke(Stroke::new(1.0, color.linear_multiply(0.65)))
        .corner_radius(corner(Radius::CHIP))
        .inner_margin(egui::Margin::same(Space::SM as i8));
    frame.show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(title)
                    .font(theme.font(Text::Label))
                    .color(color),
            );
            body(ui, theme, message);
        });
    });
}

/// Real HUD renderer used by the deterministic harness.
#[derive(Debug, Default)]
pub struct RecordingHudScene;

impl Scene for RecordingHudScene {
    fn name(&self) -> &str {
        "recording-hud"
    }

    fn setup(&self, ctx: &egui::Context) {
        install_scene_theme(ctx);
    }

    fn ui(&self, ui: &mut Ui, ctx: &SceneCtx<'_>) {
        let Some(RecordingFixture::Hud(fixture)) = ctx.fixture.recording.as_ref() else {
            return;
        };
        let theme = scene_theme(ctx.theme);
        ui.vertical_centered(|ui| {
            ui.add_space(Space::XXL);
            RecordingHud::new(
                RecordingHudModel {
                    phase: fixture.phase,
                    elapsed: fixture.elapsed,
                    capabilities: fixture.capabilities,
                    warning: fixture.warning.as_deref(),
                    drift: fixture.drift,
                    output: fixture.output.as_ref(),
                    failure: fixture.failure.as_ref(),
                    camera_status: fixture.camera_status.as_ref(),
                    camera_preview: fixture.camera_preview.as_ref(),
                    camera_settings: fixture.camera_settings,
                },
                &theme,
            )
            .show(ui);
        });
    }
}
