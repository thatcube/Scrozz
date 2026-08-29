//! Headless recording-surface behavior and scenario invariants.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{sync::Arc, time::Duration};

use egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect, vec2};
use scrozz_core::{CaptureTarget, Error, LogicalPoint, LogicalRect, LogicalSize};
use scrozz_record::{
    CameraDevice, CameraDeviceId, CameraDeviceState, CameraPermission, CameraRecordingMetadata,
    Recording,
    edit::{EditPlan, SourceMetadata, TrimRange, VideoDocument},
    engine::EngineCapabilities,
    machine::{MachineFailure, RecordingPhase},
    selection::{AspectRatio, LastSelectionMemory, SelectionConstraints, SelectionMode},
};
use scrozz_ui::{
    CameraSettingsAction, CameraSettingsModel, CameraSettingsPanel, Countdown, RecordingHud,
    RecordingHudAction, RecordingHudControls, RecordingHudModel, RecordingOverlay,
    RecordingOverlayAction, RecordingOverlayModel, RecordingSelectionState, Theme, TranscodeView,
    VideoEditor, VideoEditorAction, VideoEditorModel, VideoPreview,
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
    draw: impl FnMut(&mut egui::Ui) -> T,
) -> T {
    run_ui_at_size(context, vec2(1200.0, 900.0), events, draw)
}

fn run_ui_at_size<T>(
    context: &egui::Context,
    size: egui::Vec2,
    events: Vec<Event>,
    mut draw: impl FnMut(&mut egui::Ui) -> T,
) -> T {
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
        focused: true,
        events,
        ..Default::default()
    };
    let mut result = None;
    let mut output = context.run_ui(input, |ui| result = Some(draw(ui)));
    output.textures_delta.clear();
    result.expect("headless draw ran")
}

fn key(key: egui::Key, modifiers: Modifiers) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
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
        camera_status: None,
        camera_preview: None,
        camera_settings: None,
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
fn camera_settings_preview_is_explicit_and_permission_aware() {
    let context = new_context();
    let theme = Theme::dark();
    let device = CameraDevice {
        id: CameraDeviceId::new("stable-camera").unwrap(),
        name: "Studio camera".to_owned(),
        state: CameraDeviceState::Available,
        is_default: true,
    };
    let settings = scrozz_record::settings::CameraSettings {
        enabled: true,
        ..scrozz_record::settings::CameraSettings::default()
    };
    let denied = run_ui(&context, Vec::new(), |ui| {
        CameraSettingsPanel::new(
            CameraSettingsModel {
                settings,
                devices: std::slice::from_ref(&device),
                selected_device: None,
                capture_configuration_locked: false,
                permission: CameraPermission::Denied,
                preview: None,
                preview_status: None,
                error: None,
            },
            &theme,
        )
        .show(ui)
    });
    assert!(!denied.preview_response.enabled());

    let ready = run_ui(&context, Vec::new(), |ui| {
        CameraSettingsPanel::new(
            CameraSettingsModel {
                settings,
                devices: std::slice::from_ref(&device),
                selected_device: Some(&device.id),
                capture_configuration_locked: false,
                permission: CameraPermission::Authorized,
                preview: None,
                preview_status: None,
                error: None,
            },
            &theme,
        )
        .show(ui)
    });
    assert!(ready.preview_response.enabled());
    let started = click(&context, ready.preview_response.rect.center(), |ui| {
        CameraSettingsPanel::new(
            CameraSettingsModel {
                settings,
                devices: std::slice::from_ref(&device),
                selected_device: Some(&device.id),
                capture_configuration_locked: false,
                permission: CameraPermission::Authorized,
                preview: None,
                preview_status: None,
                error: None,
            },
            &theme,
        )
        .show(ui)
    });
    assert!(
        started
            .actions
            .contains(&CameraSettingsAction::StartPreview)
    );
}

#[test]
fn recording_hud_keeps_camera_privacy_state_visible_and_controls_live() {
    let context = new_context();
    let theme = Theme::dark();
    let status = scrozz_record::CameraRuntimeStatus {
        active: true,
        privacy_indicator_visible: true,
        device_state: CameraDeviceState::Available,
        frames_received: 12,
        dropped_frames: 1,
        queued_frames: 3,
        warning: None,
    };
    let settings = scrozz_record::settings::CameraSettings {
        enabled: true,
        ..scrozz_record::settings::CameraSettings::default()
    };
    let mut model = hud_model(RecordingPhase::Recording);
    model.camera_status = Some(&status);
    model.camera_settings = Some(settings);

    let response = run_ui(&context, Vec::new(), |ui| {
        RecordingHud::new(model, &theme).show(ui)
    });
    assert!(response.controls.camera_enabled);
    assert!(status.privacy_indicator_visible);
}

#[test]
fn editor_accepts_camera_composition_metadata_without_mutating_source() {
    let mut recording = Recording::synthetic("editor-camera.mp4", 20.0, "UI test").unwrap();
    recording.metadata.camera = Some(Box::new(CameraRecordingMetadata {
        presenter: false,
        presenter_screen: true,
        shape: scrozz_record::settings::CameraShape::Circle,
        mirrored: true,
        dropped_frames: 2,
    }));
    let document = VideoDocument::open_fixture(
        recording.clone(),
        SourceMetadata {
            width: 1920,
            height: 1080,
            fps: 30.0,
            audio_channels: 2,
        },
    )
    .unwrap();
    let plan = EditPlan::video(&document).unwrap();
    let context = new_context();
    let theme = Theme::dark();
    let _ = run_ui(&context, Vec::new(), |ui| {
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
    });
    assert_eq!(document.recording(), &recording);
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
    assert!(
        initial.actions.is_empty(),
        "idle editor emitted actions without input: {:?}",
        initial.actions
    );

    let closed = click(&context, initial.close_response.rect.center(), draw);
    assert!(closed.actions.contains(&VideoEditorAction::Close));

    let sought = click(&context, initial.seek_forward_response.rect.center(), draw);
    assert!(
        sought
            .actions
            .contains(&VideoEditorAction::Seek(Duration::from_secs(5)))
    );

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
fn video_editor_sets_trim_boundaries_to_the_playhead() {
    let context = new_context();
    let theme = Theme::dark();
    let mut document = document(2);
    document.seek(Duration::from_secs(5)).unwrap();
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
    let set_in = click(&context, initial.set_in_response.rect.center(), draw);
    let mut expected_in = plan;
    expected_in.trim.start = Duration::from_secs(5);
    assert!(
        set_in
            .actions
            .contains(&VideoEditorAction::PlanChanged(expected_in)),
        "set-in emitted {:?} from {:?}",
        set_in.actions,
        initial.set_in_response.rect
    );

    let context = new_context();
    let initial = run_ui(&context, Vec::new(), draw);
    let set_out = click(&context, initial.set_out_response.rect.center(), draw);
    let mut expected_out = plan;
    expected_out.trim.end = Duration::from_secs(5);
    assert!(
        set_out
            .actions
            .contains(&VideoEditorAction::PlanChanged(expected_out)),
        "set-out emitted {:?} from {:?}",
        set_out.actions,
        initial.set_out_response.rect
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
    for (start, end) in [(8, 8), (9, 8)] {
        invalid.trim = TrimRange {
            start: Duration::from_secs(start),
            end: Duration::from_secs(end),
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
}

#[test]
fn video_editor_keyboard_and_responsive_contracts_are_semantic() {
    let context = new_context();
    let theme = Theme::dark();
    let mut document = document(2);
    document.seek(Duration::from_secs(5)).unwrap();
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

    let space = run_ui(&context, vec![key(egui::Key::Space, Modifiers::NONE)], draw);
    assert!(space.actions.contains(&VideoEditorAction::Play));

    let context = new_context();
    let right = run_ui(
        &context,
        vec![key(egui::Key::ArrowRight, Modifiers::NONE)],
        draw,
    );
    assert!(
        right
            .actions
            .contains(&VideoEditorAction::Seek(Duration::from_secs(10)))
    );

    let context = new_context();
    let trim_in = run_ui(&context, vec![key(egui::Key::I, Modifiers::NONE)], draw);
    let mut expected = plan;
    expected.trim.start = Duration::from_secs(5);
    assert!(
        trim_in
            .actions
            .contains(&VideoEditorAction::PlanChanged(expected))
    );

    let command = Modifiers {
        command: true,
        mac_cmd: cfg!(target_os = "macos"),
        ctrl: !cfg!(target_os = "macos"),
        ..Modifiers::NONE
    };
    let context = new_context();
    let export = run_ui(
        &context,
        vec![Event::ModifiersChanged(command), key(egui::Key::E, command)],
        draw,
    );
    assert!(
        export.actions.contains(&VideoEditorAction::Export(plan)),
        "command-E emitted {:?}",
        export.actions
    );

    let context = new_context();
    let narrow = run_ui_at_size(&context, vec2(760.0, 820.0), Vec::new(), draw);
    assert_eq!(
        narrow.controls.layout,
        scrozz_ui::VideoEditorLayout::Stacked
    );
    assert_eq!(
        scrozz_ui::VideoEditorLayout::for_width(1200.0),
        scrozz_ui::VideoEditorLayout::Wide
    );
}

#[test]
fn filmstrip_timeline_drag_updates_trim_without_overwriting_source_state() {
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
    let timeline = initial.timeline_response.rect;
    let start = Pos2::new(timeline.left() + 4.0, timeline.center().y);
    let target = Pos2::new(
        timeline.left() + timeline.width() * 0.25,
        timeline.center().y,
    );
    let pointer = |pos, pressed| Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    let _ = run_ui(
        &context,
        vec![Event::PointerMoved(start), pointer(start, true)],
        draw,
    );
    let dragged = run_ui(&context, vec![Event::PointerMoved(target)], draw);
    let _ = run_ui(
        &context,
        vec![Event::PointerMoved(target), pointer(target, false)],
        draw,
    );

    let changed = dragged.actions.iter().find_map(|action| match action {
        VideoEditorAction::PlanChanged(plan) => Some(*plan),
        _ => None,
    });
    let changed = changed.expect("dragging the in handle changes the plan");
    assert_eq!(changed.trim.start, Duration::from_secs(5));
    assert_eq!(changed.trim.end, document.duration());
    assert_eq!(changed.output, plan.output);
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

    for &scenario in RECORDING_SCENARIOS.iter().chain(
        [
            Scenario::VideoEditingNarrow,
            Scenario::RecordingCamera,
            Scenario::RecordingPresenter,
            Scenario::CameraSettings,
        ]
        .iter(),
    ) {
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
                | Some(RecordingFixture::CameraSettings(_))
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
