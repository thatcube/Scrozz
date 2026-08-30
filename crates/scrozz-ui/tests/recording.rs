//! Headless recording-surface behavior and scenario invariants.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect, vec2};
use scrozz_record::{
    CameraDevice, CameraDeviceId, CameraDeviceState, CameraPermission, CameraRecordingMetadata,
    Recording,
    edit::{EditOutput, EditPlan, SourceMetadata, TrimRange, VideoDocument},
    transcode::{ExportCapabilities, ExportCapability},
};
use scrozz_ui::{
    CameraLiveModel, CameraSettingsAction, CameraSettingsModel, CameraSettingsPanel,
    RecordingSettingsAction, RecordingSettingsPanel, Theme, TranscodeView, VideoEditor,
    VideoEditorAction, VideoEditorModel, VideoPreview,
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
fn live_recording_pane_keeps_camera_privacy_state_visible_and_composition_live() {
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
    let camera = scrozz_record::settings::CameraSettings {
        enabled: true,
        ..scrozz_record::settings::CameraSettings::default()
    };
    let mut settings = scrozz_record::RecordingSettings::shipped();
    settings.camera = camera;
    let model = CameraLiveModel {
        settings: camera,
        status: &status,
        preview: None,
        enabled: true,
    };

    let live = run_ui(&context, Vec::new(), |ui| {
        RecordingSettingsPanel::new(settings, scrozz_record::EngineCapabilities::ALL, &theme)
            .with_active_recording(true)
            .with_camera(model)
            .show(ui)
    });
    // Nothing moved, so nothing is sent into the running native session.
    assert!(live.actions.is_empty());
    assert_eq!(live.settings.camera, camera);
    assert!(status.privacy_indicator_visible);

    // Presenter is reachable while every other preference is locked.
    let presenter = run_ui(&context, Vec::new(), |ui| {
        RecordingSettingsPanel::new(settings, scrozz_record::EngineCapabilities::ALL, &theme)
            .with_active_recording(true)
            .with_camera(CameraLiveModel {
                settings: scrozz_record::settings::CameraSettings {
                    presenter: true,
                    ..camera
                },
                ..model
            })
            .show(ui)
    });
    assert!(presenter.actions.is_empty());
    assert!(presenter.settings.camera.presenter);
}

#[test]
fn live_camera_controls_are_inert_when_the_camera_is_unavailable() {
    let context = new_context();
    let theme = Theme::dark();
    let status = scrozz_record::CameraRuntimeStatus {
        active: false,
        privacy_indicator_visible: false,
        device_state: CameraDeviceState::Disconnected,
        frames_received: 0,
        dropped_frames: 0,
        queued_frames: 0,
        warning: Some("the camera was disconnected".to_owned()),
    };
    let camera = scrozz_record::settings::CameraSettings {
        enabled: true,
        ..scrozz_record::settings::CameraSettings::default()
    };
    let mut settings = scrozz_record::RecordingSettings::shipped();
    settings.camera = camera;
    let response = run_ui(&context, Vec::new(), |ui| {
        RecordingSettingsPanel::new(settings, scrozz_record::EngineCapabilities::ALL, &theme)
            .with_active_recording(true)
            .with_camera(CameraLiveModel {
                settings: camera,
                status: &status,
                preview: None,
                enabled: false,
            })
            .show(ui)
    });
    assert!(
        !response
            .actions
            .iter()
            .any(|action| matches!(action, RecordingSettingsAction::CameraChanged(_))),
        "a disconnected camera must not emit live composition changes"
    );
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
fn video_editor_gates_hardware_and_exposes_explicit_webm_fallback() {
    let context = new_context();
    let theme = Theme::dark();
    let document = document(2);
    let capabilities = ExportCapabilities {
        mp4_h264: ExportCapability::unavailable(
            "no compatible hardware H.264 encoder is available; choose WebM / AV1",
        ),
        gif: ExportCapability::available(),
        webm_av1: ExportCapability::available(),
    };
    let video = run_ui(&context, Vec::new(), |ui| {
        VideoEditor::new(
            VideoEditorModel {
                document: &document,
                plan: EditPlan::video(&document).unwrap(),
                preview: VideoPreview::default(),
                transcode: TranscodeView::Idle,
            },
            &theme,
        )
        .with_capabilities(capabilities)
        .show(ui)
    });
    assert!(!video.controls.mp4_h264_available);
    assert!(video.controls.webm_av1_available);
    assert!(!video.controls.export_enabled);
    assert!(
        video
            .controls
            .validation_error
            .as_deref()
            .is_some_and(|error| error.contains("choose WebM"))
    );

    let webm = run_ui(&context, Vec::new(), |ui| {
        VideoEditor::new(
            VideoEditorModel {
                document: &document,
                plan: EditPlan::webm(&document).unwrap(),
                preview: VideoPreview::default(),
                transcode: TranscodeView::Idle,
            },
            &theme,
        )
        .with_capabilities(capabilities)
        .show(ui)
    });
    assert_eq!(webm.plan.output, EditOutput::WebM);
    assert!(webm.controls.export_enabled);
    assert!(!webm.controls.audio_enabled);
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

const CAMERA_SCENARIOS: &[Scenario] = &[
    Scenario::RecordingCamera,
    Scenario::RecordingPresenter,
    Scenario::CameraSettings,
];

const VIDEO_SCENARIOS: &[Scenario] = &[
    Scenario::VideoEditing,
    Scenario::VideoEditingNarrow,
    Scenario::VideoExporting,
    Scenario::VideoWebmFallback,
    Scenario::VideoExportFailedPartial,
];

#[test]
fn video_scenarios_are_appended_after_smart_frame() {
    let smart_frame = Scenario::ALL
        .iter()
        .position(|scenario| *scenario == Scenario::SmartFrameExpanded)
        .unwrap();
    assert_eq!(Scenario::ALL[smart_frame + 1], Scenario::VideoEditing);
    assert_eq!(
        &Scenario::ALL[smart_frame + 1..smart_frame + 1 + VIDEO_SCENARIOS.len()],
        VIDEO_SCENARIOS
    );
}

#[test]
fn every_video_scenario_has_real_deterministic_scene_and_fixture() {
    let registry = SceneRegistry::production();
    let placeholders = registry.placeholder_scenarios();
    let renderer = SoftwareRenderer::production();

    for &scenario in VIDEO_SCENARIOS.iter().chain(CAMERA_SCENARIOS.iter()) {
        assert!(
            !placeholders.contains(&scenario),
            "{} still uses a placeholder",
            scenario.slug()
        );
        let first_fixture = scenario.fixture();
        let second_fixture = scenario.fixture();
        assert!(
            matches!(
                first_fixture.recording,
                Some(
                    RecordingFixture::Editor(_)
                        | RecordingFixture::CameraLive(_)
                        | RecordingFixture::CameraSettings(_)
                )
            ),
            "{} must carry a real recording fixture",
            scenario.slug()
        );
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
