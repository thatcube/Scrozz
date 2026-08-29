//! Headless recording-surface behavior and scenario invariants.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{sync::Arc, time::Duration};

use egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect, vec2};
use scrozz_core::{CaptureTarget, Error, LogicalPoint, LogicalRect, LogicalSize};
use scrozz_record::{
    Recording,
    edit::{EditPlan, SourceMetadata, TrimRange, VideoDocument},
    engine::EngineCapabilities,
    machine::{MachineFailure, RecordingPhase},
    selection::{AspectRatio, LastSelectionMemory, SelectionConstraints, SelectionMode},
};
use scrozz_ui::{
    Countdown, RecordingHud, RecordingHudAction, RecordingHudControls, RecordingHudModel,
    RecordingOverlay, RecordingOverlayAction, RecordingOverlayModel, RecordingSelectionState,
    Theme, TranscodeView, VideoEditor, VideoEditorAction, VideoEditorModel, VideoPreview,
    harness::{RecordingFixture, Scenario, SceneRegistry, SoftwareRenderer, VirtualClock},
};

fn new_context() -> egui::Context {
    let context = egui::Context::default();
    context.set_pixels_per_point(1.0);
    scrozz_ui::theme::install_fonts(&context);
    scrozz_ui::theme::install_style(&context, &Theme::dark());
    run_ui(&context, Vec::new(), |_| ());
    context
}

fn run_ui<T>(
    context: &egui::Context,
    events: Vec<Event>,
    mut draw: impl FnMut(&mut egui::Ui) -> T,
) -> T {
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 900.0))),
        focused: true,
        events,
        ..Default::default()
    };
    let mut result = None;
    let mut output = context.run_ui(input, |ui| result = Some(draw(ui)));
    output.textures_delta.clear();
    result.expect("headless draw ran")
}

fn click<T>(context: &egui::Context, point: Pos2, mut draw: impl FnMut(&mut egui::Ui) -> T) -> T {
    let event = |pressed| Event::PointerButton {
        pos: point,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::default(),
    };
    let _ = run_ui(
        context,
        vec![Event::PointerMoved(point), event(true)],
        &mut draw,
    );
    run_ui(
        context,
        vec![Event::PointerMoved(point), event(false)],
        draw,
    )
}

fn hud_model(phase: RecordingPhase) -> RecordingHudModel<'static> {
    RecordingHudModel {
        phase,
        elapsed: Duration::from_secs(73),
        capabilities: EngineCapabilities::ALL,
        warning: None,
        drift: None,
        output: None,
        failure: None,
    }
}

#[test]
fn hud_controls_follow_phase_capabilities_and_partial_output() {
    let idle = RecordingHudControls::for_model(&hud_model(RecordingPhase::Idle));
    assert!(!idle.pause_resume_enabled);
    assert!(!idle.stop_enabled);

    let recording = RecordingHudControls::for_model(&hud_model(RecordingPhase::Recording));
    assert!(recording.pause_resume_enabled);
    assert!(recording.stop_enabled);

    let mut without_pause = hud_model(RecordingPhase::Recording);
    without_pause.capabilities.pause_resume = false;
    let controls = RecordingHudControls::for_model(&without_pause);
    assert!(!controls.pause_resume_enabled);
    assert!(controls.stop_enabled);

    let finalising = RecordingHudControls::for_model(&hud_model(RecordingPhase::Finalising));
    assert!(!finalising.pause_resume_enabled);
    assert!(!finalising.stop_enabled);

    let partial =
        Recording::synthetic_partial("partial.mp4", 7.0, "UI test", "trailer failed").unwrap();
    let failure = MachineFailure {
        error: Arc::new(Error::Codec("finalisation failed".to_owned())),
        partial: Some(partial),
        recovery_error: None,
    };
    let mut failed = hud_model(RecordingPhase::Failed);
    failed.failure = Some(&failure);
    assert!(
        RecordingHudControls::for_model(&failed).reveal_partial_enabled,
        "salvageable output must stay actionable"
    );
}

#[test]
fn hud_buttons_emit_semantic_actions_headlessly() {
    let context = new_context();
    let theme = Theme::dark();
    let model = hud_model(RecordingPhase::Recording);
    let initial = run_ui(&context, Vec::new(), |ui| {
        RecordingHud::new(model, &theme).show(ui)
    });
    assert!(initial.pause_resume_response.enabled());
    assert!(initial.stop_response.enabled());

    let paused = click(
        &context,
        initial.pause_resume_response.rect.center(),
        |ui| RecordingHud::new(model, &theme).show(ui),
    );
    assert_eq!(paused.action, Some(RecordingHudAction::Pause));

    let stopped = click(&context, initial.stop_response.rect.center(), |ui| {
        RecordingHud::new(model, &theme).show(ui)
    });
    assert_eq!(stopped.action, Some(RecordingHudAction::Stop));

    let paused_model = hud_model(RecordingPhase::Paused);
    let paused = run_ui(&context, Vec::new(), |ui| {
        RecordingHud::new(paused_model, &theme).show(ui)
    });
    let resumed = click(&context, paused.pause_resume_response.rect.center(), |ui| {
        RecordingHud::new(paused_model, &theme).show(ui)
    });
    assert_eq!(resumed.action, Some(RecordingHudAction::Resume));

    let finalising = hud_model(RecordingPhase::Finalising);
    let disabled = run_ui(&context, Vec::new(), |ui| {
        RecordingHud::new(finalising, &theme).show(ui)
    });
    assert!(!disabled.pause_resume_response.enabled());
    assert!(!disabled.stop_response.enabled());
}

#[test]
fn countdown_rounds_up_and_never_stalls_at_disabled_or_zero() {
    let settings = scrozz_record::settings::CountdownSettings {
        enabled: true,
        seconds: 3,
    };
    assert_eq!(
        Countdown::new(settings, Duration::from_millis(2_001)).displayed_count(),
        Some(3)
    );
    assert_eq!(
        Countdown::new(settings, Duration::from_millis(1)).displayed_count(),
        Some(1)
    );
    assert_eq!(
        Countdown::new(settings, Duration::ZERO).displayed_count(),
        None
    );
    assert_eq!(
        Countdown::new(
            scrozz_record::settings::CountdownSettings {
                enabled: false,
                seconds: 3,
            },
            Duration::from_secs(3),
        )
        .displayed_count(),
        None
    );
    assert_eq!(
        Countdown::new(
            scrozz_record::settings::CountdownSettings {
                enabled: true,
                seconds: 0,
            },
            Duration::from_secs(3),
        )
        .displayed_count(),
        None
    );
}

fn desktop() -> LogicalRect {
    LogicalRect::new(
        LogicalPoint::new(0.0, 0.0),
        LogicalSize::new(1920.0, 1080.0),
    )
}

fn region() -> LogicalRect {
    LogicalRect::new(
        LogicalPoint::new(100.0, 80.0),
        LogicalSize::new(1280.0, 720.0),
    )
}

#[test]
fn selection_confirm_and_reuse_return_real_targets() {
    let context = new_context();
    let theme = Theme::dark();
    let mut memory = LastSelectionMemory::new();
    memory
        .remember(&CaptureTarget::Region(region()))
        .expect("valid remembered region");
    let state = RecordingSelectionState {
        mode: SelectionMode::AllInOne,
        constraints: SelectionConstraints::NONE,
        candidate: Some(CaptureTarget::Region(region())),
    };
    let draw = |ui: &mut egui::Ui| {
        RecordingOverlay::new(
            RecordingOverlayModel {
                state: state.clone(),
                desktop_bounds: desktop(),
                drag_preview: Some(region()),
                target_hint: Some("1280 × 720 region"),
                last_selection: &memory,
            },
            &theme,
        )
        .show(ui)
    };
    let initial = run_ui(&context, Vec::new(), draw);
    assert!(initial.controls.confirm_enabled);
    assert!(initial.controls.reuse_enabled);

    let confirmed = click(&context, initial.confirm_response.rect.center(), draw);
    assert!(
        confirmed
            .actions
            .contains(&RecordingOverlayAction::Confirm(CaptureTarget::Region(
                region()
            )))
    );

    let reused = click(&context, initial.reuse_response.rect.center(), draw);
    assert!(
        reused
            .actions
            .contains(&RecordingOverlayAction::ReuseLastSelection(
                CaptureTarget::Region(region())
            ))
    );
    assert_eq!(
        reused.state.candidate,
        Some(CaptureTarget::Region(region()))
    );
}

#[test]
fn invalid_selection_constraints_disable_confirmation() {
    let context = new_context();
    let theme = Theme::dark();
    let memory = LastSelectionMemory::new();
    let response = run_ui(&context, Vec::new(), |ui| {
        RecordingOverlay::new(
            RecordingOverlayModel {
                state: RecordingSelectionState {
                    mode: SelectionMode::Region,
                    constraints: SelectionConstraints {
                        exact_size: Some(LogicalSize::new(100.0, 100.0)),
                        aspect_ratio: AspectRatio::new(16.0, 9.0).ok(),
                    },
                    candidate: Some(CaptureTarget::Region(region())),
                },
                desktop_bounds: desktop(),
                drag_preview: Some(region()),
                target_hint: None,
                last_selection: &memory,
            },
            &theme,
        )
        .show(ui)
    });
    assert!(!response.controls.confirm_enabled);
    assert!(response.controls.validation_error.is_some());
    assert!(!response.confirm_response.enabled());
}

fn document(audio_channels: u16) -> VideoDocument {
    VideoDocument::open_fixture(
        Recording::synthetic("editor.mp4", 20.0, "UI test").unwrap(),
        SourceMetadata {
            width: 1920,
            height: 1080,
            fps: 30.0,
            audio_channels,
        },
    )
    .unwrap()
}

#[test]
fn video_editor_actions_and_enabled_state_are_headless() {
    let context = new_context();
    let theme = Theme::dark();
    let document = document(2);
    let plan = EditPlan::video(&document).unwrap();
    let draw = |ui: &mut egui::Ui| {
        VideoEditor::new(
            VideoEditorModel {
                document: &document,
                plan,
                preview: VideoPreview::default(),
                transcode: TranscodeView::Idle,
            },
            &theme,
        )
        .show(ui)
    };
    let initial = run_ui(&context, Vec::new(), draw);
    assert!(initial.controls.transport_enabled);
    assert!(initial.controls.audio_enabled);
    assert!(initial.controls.mono_enabled);
    assert!(initial.controls.export_enabled);

    let playing = click(&context, initial.transport_response.rect.center(), draw);
    assert!(playing.actions.contains(&VideoEditorAction::Play));

    let exporting = click(&context, initial.export_response.rect.center(), draw);
    assert!(
        exporting.actions.contains(&VideoEditorAction::Export(plan)),
        "export click at {:?} emitted {:?} from {:?}",
        initial.export_response.rect.center(),
        exporting.actions,
        initial.export_response.rect
    );

    let running = run_ui(&context, Vec::new(), |ui| {
        VideoEditor::new(
            VideoEditorModel {
                document: &document,
                plan,
                preview: VideoPreview::default(),
                transcode: TranscodeView::Running { progress: 0.5 },
            },
            &theme,
        )
        .show(ui)
    });
    assert!(!running.controls.transport_enabled);
    assert!(!running.controls.timeline_enabled);
    assert!(!running.controls.export_enabled);
    assert!(running.controls.cancel_export_enabled);
    assert!(!running.transport_response.enabled());
    assert!(!running.export_response.enabled());
    let cancel = running
        .cancel_export_response
        .as_ref()
        .expect("running export has cancel");
    assert!(cancel.enabled());
    let cancel_point = cancel.rect.center();
    let cancelled = click(&context, cancel_point, |ui| {
        VideoEditor::new(
            VideoEditorModel {
                document: &document,
                plan,
                preview: VideoPreview::default(),
                transcode: TranscodeView::Running { progress: 0.5 },
            },
            &theme,
        )
        .show(ui)
    });
    assert!(
        cancelled.actions.contains(&VideoEditorAction::CancelExport),
        "cancel click at {cancel_point:?} emitted {:?} from {:?}",
        cancelled.actions,
        cancel.rect
    );
}

#[test]
fn video_editor_disables_inapplicable_audio_and_invalid_trim() {
    let context = new_context();
    let theme = Theme::dark();
    let silent = document(0);
    let silent_plan = EditPlan::video(&silent).unwrap();
    let response = run_ui(&context, Vec::new(), |ui| {
        VideoEditor::new(
            VideoEditorModel {
                document: &silent,
                plan: silent_plan,
                preview: VideoPreview::default(),
                transcode: TranscodeView::Idle,
            },
            &theme,
        )
        .show(ui)
    });
    assert!(!response.controls.audio_enabled);
    assert!(!response.controls.mono_enabled);

    let with_audio = document(2);
    let gif = EditPlan::gif(&with_audio).unwrap();
    let response = run_ui(&context, Vec::new(), |ui| {
        VideoEditor::new(
            VideoEditorModel {
                document: &with_audio,
                plan: gif,
                preview: VideoPreview::default(),
                transcode: TranscodeView::Idle,
            },
            &theme,
        )
        .show(ui)
    });
    assert!(!response.controls.audio_enabled);
    assert!(response.plan.audio.mute);

    let mut invalid = EditPlan::video(&with_audio).unwrap();
    invalid.trim = TrimRange {
        start: Duration::from_secs(8),
        end: Duration::from_secs(8),
    };
    let response = run_ui(&context, Vec::new(), |ui| {
        VideoEditor::new(
            VideoEditorModel {
                document: &with_audio,
                plan: invalid,
                preview: VideoPreview::default(),
                transcode: TranscodeView::Idle,
            },
            &theme,
        )
        .show(ui)
    });
    assert!(!response.controls.export_enabled);
    assert!(response.controls.validation_error.is_some());
    assert!(!response.export_response.enabled());
}

const RECORDING_SCENARIOS: &[Scenario] = &[
    Scenario::RecordingIdle,
    Scenario::RecordingSelecting,
    Scenario::RecordingCountdown,
    Scenario::RecordingActive,
    Scenario::RecordingPaused,
    Scenario::RecordingFailedPartial,
    Scenario::VideoEditing,
    Scenario::VideoExporting,
    Scenario::VideoExportFailedPartial,
];

#[test]
fn recording_scenarios_are_appended_after_editor_annotating() {
    let editor = Scenario::ALL
        .iter()
        .position(|scenario| *scenario == Scenario::EditorAnnotating)
        .unwrap();
    assert_eq!(Scenario::ALL[editor + 1], Scenario::RecordingIdle);
    assert_eq!(
        &Scenario::ALL[editor + 1..editor + 1 + RECORDING_SCENARIOS.len()],
        RECORDING_SCENARIOS
    );
    assert_eq!(
        &Scenario::ALL[..=editor],
        &[
            Scenario::StackSingle,
            Scenario::StackEntering,
            Scenario::StackFull,
            Scenario::StackOverflowEvicting,
            Scenario::StackDismissing,
            Scenario::StackDragging,
            Scenario::DockCollapsing,
            Scenario::DockCollapsed,
            Scenario::EditorAnnotating,
        ]
    );
}

#[test]
fn every_recording_scenario_has_real_deterministic_scene_and_fixture() {
    let registry = SceneRegistry::production();
    let placeholders = registry.placeholder_scenarios();
    let renderer = SoftwareRenderer::production();

    for &scenario in RECORDING_SCENARIOS {
        assert!(
            !placeholders.contains(&scenario),
            "{} still uses a placeholder",
            scenario.slug()
        );
        let first_fixture = scenario.fixture();
        let second_fixture = scenario.fixture();
        assert!(matches!(
            first_fixture.recording,
            Some(RecordingFixture::Hud(_))
                | Some(RecordingFixture::Countdown(_))
                | Some(RecordingFixture::Selection(_))
                | Some(RecordingFixture::Editor(_))
        ));
        assert_eq!(
            format!("{:?}", first_fixture.recording),
            format!("{:?}", second_fixture.recording),
            "{} fixture construction changed between calls",
            scenario.slug()
        );

        let spec = scrozz_ui::harness::RenderSpec::golden(scenario, VirtualClock::ZERO);
        let first = renderer.render(&spec).expect("first deterministic render");
        let second = renderer.render(&spec).expect("second deterministic render");
        assert_eq!(
            first.fingerprint(),
            second.fingerprint(),
            "{} rendered nondeterministically",
            scenario.slug()
        );
    }
}
