//! Compact recording status and transport controls.

use std::time::Duration;

use egui::{Color32, Response, RichText, Sense, Stroke, Ui, vec2};
use scrozz_record::{
    Recording,
    engine::EngineCapabilities,
    machine::{
        ClockDrift, DRIFT_EVENT_THRESHOLD_SECS, MachineFailure, RecordingMachine, RecordingPhase,
    },
};

use crate::{
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
        }
    }
}

/// A semantic action requested by the HUD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingHudAction {
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
                },
                &theme,
            )
            .show(ui);
        });
    }
}
