//! Smart Frame behavior contracts ported from the reviewed monolithic editor.

use scrozz_annotate::{
    AspectPreset, Background, Beautification, BeautificationPreset, CanvasInsets, Color, Document,
    GeneratedStyle, SceneAutomatic, SmartFrameAnalysis, SmartFramePreset, SmartFramePresetSettings,
    SourceInsets, SubjectAppearance,
};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Provenance, ScaleFactor,
};
use scrozz_ui::editor::{Command, EditorState, EditorUi, Handle, Intent, Tool};

fn document(provenance: Provenance) -> Document {
    let size = LogicalSize::new(480.0, 300.0);
    let w = (size.width * 2.0) as u32;
    let h = (size.height * 2.0) as u32;
    let data = vec![128u8; w as usize * h as usize * 4];
    Document::new(Capture {
        frame: Frame {
            data,
            size: PhysicalSize::new(f64::from(w), f64::from(h)),
            stride: w as usize * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::new(2.0),
        },
        provenance,
        target: CaptureTarget::Region(LogicalRect::new(LogicalPoint::new(0.0, 0.0), size)),
    })
}

fn drag(state: &mut EditorState, from: LogicalPoint, to: LogicalPoint) {
    state.pointer_pressed(from);
    state.pointer_dragged(to, false);
    state.pointer_released();
}

#[test]
fn smart_frame_starts_as_an_immediate_default_on_draft() {
    let mut state = EditorState::new(document(Provenance::Region));
    let before = state.document().data();
    let revision_before = state.revision();

    let intent = state.begin_smart_frame();
    let Intent::AnalyzeSmartFrame {
        revision,
        data,
        cancellation,
        ..
    } = &intent
    else {
        panic!("expected AnalyzeSmartFrame intent");
    };

    assert_eq!(*revision, 1);
    assert!(!cancellation.is_cancelled());
    assert!(
        data.beautification.is_none(),
        "analysis receives no old frame"
    );
    let draft = state.document().beautification().expect("visible draft");
    assert!(draft.auto_balance, "Smart Frame's main value starts on");
    assert!(matches!(draft.background, Background::Automatic(_)));
    // Opening a draft bumps revision for preview but does not dirty the
    // document for persistence purposes until Apply.
    assert!(
        !state.is_dirty(),
        "opening a draft is not a persistent edit"
    );

    state.cancel_smart_frame();
    assert_eq!(state.document().data(), before);
}

#[test]
fn opening_scene_panel_is_neutral_and_session_local() {
    let doc = document(Provenance::Region);
    let before = doc.data();
    let mut editor = EditorUi::new(doc);

    editor.set_scene_visible(true);

    assert!(editor.scene_visible());
    assert!(!editor.state().has_scene_draft());
    assert_eq!(editor.document().data(), before);
}

#[test]
fn opening_an_existing_legacy_scene_preserves_it_until_an_explicit_edit() {
    let mut doc = document(Provenance::Region);
    let legacy = Beautification {
        inset: SourceInsets::uniform(8.0),
        padding: 24.0,
        background: Background::Solid(Color::rgb(24, 32, 48)),
        ..Beautification::default()
    };
    doc.set_beautification(Some(legacy.clone())).unwrap();
    let before = doc.data();
    let mut state = EditorState::new(doc);

    state.edit_existing_scene();

    assert!(state.has_scene_draft());
    assert_eq!(state.document().beautification(), Some(&legacy));
    state.cancel_scene();
    assert_eq!(state.document().data(), before);
}

#[test]
fn editing_a_resolved_value_fixes_only_that_automatic_property() {
    let mut state = EditorState::new(document(Provenance::Region));
    let _ = state.begin_scene();
    let mut scene = state.document().scene().cloned().unwrap();
    assert!(scene.automatic.padding);
    assert!(scene.automatic.background);
    scene.padding = 73.0;

    assert!(state.apply_scene_edit(scene).is_none());
    let edited = state.document().scene().unwrap();
    assert!(!edited.automatic.padding);
    assert!(edited.automatic.background);
    assert!(edited.automatic.corners);
}

#[test]
fn clear_canvas_and_remove_scene_are_distinct_reversible_actions() {
    let mut state = EditorState::new(document(Provenance::Region));
    let source = state.document().source().frame.data.clone();
    state.begin_with(Beautification::preset(BeautificationPreset::Social));

    state.clear_scene_canvas();
    assert!(matches!(
        state.document().scene().map(|scene| &scene.background),
        Some(Background::Transparent)
    ));

    state.remove_scene();
    assert!(state.document().scene().is_none());
    assert_eq!(state.document().source().frame.data, source);
}

#[test]
fn generated_direction_survives_analysis_delivery() {
    let mut state = EditorState::new(document(Provenance::Region));
    let Intent::AnalyzeSmartFrame { revision, .. } =
        state.begin_scene_with_style(GeneratedStyle::Vibrant)
    else {
        panic!("expected analysis intent");
    };
    state.finish_smart_frame_analysis(
        revision,
        Ok(SmartFrameAnalysis {
            beautification: Beautification {
                background: Background::Automatic(Default::default()),
                ..Beautification::default()
            },
            inset_explanation: "complete source preserved".to_owned(),
        }),
    );
    let Background::Automatic(background) = &state.document().scene().unwrap().background else {
        panic!("automatic background");
    };
    assert_eq!(background.style, GeneratedStyle::Vibrant);
}

#[test]
fn generated_direction_preserves_scene_edits_and_cancel_baseline() {
    let mut state = EditorState::new(document(Provenance::Region));
    let before = state.document().data();
    state.begin_with(Beautification {
        padding: 61.0,
        canvas_padding: Some(CanvasInsets {
            top: 13.0,
            right: 21.0,
            bottom: 34.0,
            left: 55.0,
        }),
        aspect: AspectPreset::Square,
        background: Background::Solid(Color::rgb(24, 32, 48)),
        ..Beautification::default()
    });

    let intent = state
        .set_generated_scene_style(GeneratedStyle::Soft)
        .expect("generated background needs analysis");

    assert!(matches!(intent, Intent::AnalyzeSmartFrame { .. }));
    let scene = state.document().scene().unwrap();
    assert_eq!(scene.padding, 61.0);
    assert_eq!(
        scene.canvas_padding,
        Some(CanvasInsets {
            top: 13.0,
            right: 21.0,
            bottom: 34.0,
            left: 55.0,
        })
    );
    assert_eq!(scene.aspect, AspectPreset::Square);
    state.cancel_scene();
    assert_eq!(state.document().data(), before);
}

#[test]
fn mixed_preset_resolves_only_automatic_properties() {
    let mut state = EditorState::new(document(Provenance::Region));
    let fixed_background = Background::Solid(Color::rgb(24, 32, 48));
    state.begin_with(Beautification {
        padding: 11.0,
        canvas_padding: Some(CanvasInsets::uniform(11.0)),
        corner_radius: 7.0,
        shadow: 3.0,
        background: fixed_background.clone(),
        aspect: AspectPreset::Square,
        automatic: SceneAutomatic {
            padding: true,
            placement: true,
            ..SceneAutomatic::default()
        },
        ..Beautification::default()
    });
    let Intent::AnalyzeSmartFrame { revision, .. } = state
        .request_scene_automatic_analysis()
        .expect("mixed preset has automatic properties")
    else {
        panic!("expected analysis intent");
    };

    state.finish_smart_frame_analysis(
        revision,
        Ok(SmartFrameAnalysis {
            beautification: Beautification {
                padding: 42.0,
                canvas_padding: Some(CanvasInsets {
                    top: 18.0,
                    right: 42.0,
                    bottom: 18.0,
                    left: 42.0,
                }),
                corner_radius: 99.0,
                shadow: 99.0,
                background: Background::Solid(Color::rgb(200, 20, 20)),
                auto_balance: true,
                ..Beautification::default()
            },
            inset_explanation: "automatic values resolved".to_owned(),
        }),
    );

    let scene = state.document().scene().unwrap();
    assert_eq!(scene.background, fixed_background);
    assert_eq!(scene.padding, 42.0);
    assert_eq!(
        scene.canvas_padding,
        Some(CanvasInsets {
            top: 18.0,
            right: 42.0,
            bottom: 18.0,
            left: 42.0,
        })
    );
    assert!(scene.auto_balance);
    assert_eq!(scene.corner_radius, 7.0);
    assert_eq!(scene.shadow, 3.0);
    assert_eq!(scene.aspect, AspectPreset::Square);
}

#[test]
fn cancel_restores_exact_document() {
    let mut state = EditorState::new(document(Provenance::Region));
    let before = state.document().data();
    let _ = state.begin_smart_frame();
    assert!(state.document().beautification().is_some());

    state.cancel_smart_frame();
    assert_eq!(state.document().data(), before);
    assert!(!state.is_dirty());
}

#[test]
fn apply_is_one_undoable_revision() {
    let mut state = EditorState::new(document(Provenance::Region));
    let before = state.document().data();
    state.begin_with(Beautification::preset(BeautificationPreset::Social));
    state.apply_smart_frame();

    assert!(state.is_dirty());
    assert!(state.can_undo_framing());
    assert!(state.document().beautification().is_some());

    state.undo_framing();
    assert_eq!(
        state.document().data().beautification,
        before.beautification
    );
    state.redo_framing();
    assert_eq!(
        state.document().beautification(),
        Some(&Beautification::preset(BeautificationPreset::Social))
    );
}

#[test]
fn stale_analysis_never_replaces_newer_draft() {
    let mut state = EditorState::new(document(Provenance::Region));
    let Intent::AnalyzeSmartFrame {
        revision: stale, ..
    } = state.begin_smart_frame()
    else {
        panic!("expected analysis intent");
    };
    state.cancel_smart_frame();

    let Intent::AnalyzeSmartFrame {
        revision: current, ..
    } = state.begin_smart_frame()
    else {
        panic!("expected analysis intent");
    };
    assert_ne!(stale, current);
    let expected = state.document().beautification().cloned();

    state.finish_smart_frame_analysis(
        stale,
        Ok(SmartFrameAnalysis {
            beautification: Beautification::preset(BeautificationPreset::Story),
            inset_explanation: "stale".to_owned(),
        }),
    );
    assert_eq!(state.document().beautification(), expected.as_ref());
}

#[test]
fn annotation_history_and_scene_history_remain_independent() {
    let mut state = EditorState::new(document(Provenance::Region));
    let scene = Beautification::preset(BeautificationPreset::Social);
    state.begin_with(scene.clone());
    state.apply_scene();

    state.set_tool(Tool::Rectangle);
    drag(
        &mut state,
        LogicalPoint::new(20.0, 20.0),
        LogicalPoint::new(120.0, 90.0),
    );
    assert_eq!(state.document().len(), 1);

    state.command(Command::Undo).unwrap();
    assert!(state.document().is_empty());
    assert_eq!(state.document().scene(), Some(&scene));

    state.command(Command::Redo).unwrap();
    assert_eq!(state.document().len(), 1);
    assert_eq!(state.document().scene(), Some(&scene));

    state.undo_framing();
    assert_eq!(state.document().len(), 1);
    assert!(state.document().scene().is_none());

    state.redo_framing();
    assert_eq!(state.document().len(), 1);
    assert_eq!(state.document().scene(), Some(&scene));
}

#[test]
fn annotation_mutation_invalidates_in_flight_scene_analysis() {
    let mut state = EditorState::new(document(Provenance::Region));
    let Intent::AnalyzeSmartFrame { revision, .. } = state.begin_scene() else {
        panic!("expected analysis intent");
    };
    state.set_tool(Tool::Rectangle);
    drag(
        &mut state,
        LogicalPoint::new(20.0, 20.0),
        LogicalPoint::new(120.0, 90.0),
    );
    let expected = state.document().scene().cloned();

    state.finish_smart_frame_analysis(
        revision,
        Ok(SmartFrameAnalysis {
            beautification: Beautification::preset(BeautificationPreset::Story),
            inset_explanation: "stale annotation snapshot".to_owned(),
        }),
    );

    assert_eq!(state.document().scene(), expected.as_ref());
    assert!(!state.smart_frame_analysis_pending());
}

#[test]
fn crop_mutation_invalidates_in_flight_scene_analysis() {
    let mut state = EditorState::new(document(Provenance::Region));
    let Intent::AnalyzeSmartFrame { revision, .. } = state.begin_scene() else {
        panic!("expected analysis intent");
    };
    state.set_tool(Tool::Crop);
    let full = state.pending_crop().expect("crop session");
    drag(
        &mut state,
        Handle::TopLeft.position(&full),
        LogicalPoint::new(40.0, 30.0),
    );
    state.command(Command::ApplyCrop).unwrap();
    let expected = state.document().scene().cloned();

    state.finish_smart_frame_analysis(
        revision,
        Ok(SmartFrameAnalysis {
            beautification: Beautification::preset(BeautificationPreset::Story),
            inset_explanation: "stale crop snapshot".to_owned(),
        }),
    );

    assert!(state.document().crop().is_some());
    assert_eq!(state.document().scene(), expected.as_ref());
    assert!(!state.smart_frame_analysis_pending());
}

#[test]
fn unchanged_crop_keeps_in_flight_scene_analysis_valid() {
    let mut state = EditorState::new(document(Provenance::Region));
    let Intent::AnalyzeSmartFrame { revision, .. } = state.begin_scene() else {
        panic!("expected analysis intent");
    };
    state.set_tool(Tool::Crop);
    state.command(Command::ApplyCrop).unwrap();
    assert!(
        state.smart_frame_analysis_pending(),
        "applying the initial full-frame crop must not invalidate analysis"
    );
    let analyzed = Beautification::preset(BeautificationPreset::Story);

    state.finish_smart_frame_analysis(
        revision,
        Ok(SmartFrameAnalysis {
            beautification: analyzed.clone(),
            inset_explanation: "current full-frame snapshot".to_owned(),
        }),
    );

    assert_eq!(state.document().scene(), Some(&analyzed));
    assert!(!state.smart_frame_analysis_pending());
}

#[test]
fn manual_preset_choice_cancels_in_flight_analysis() {
    let mut state = EditorState::new(document(Provenance::Region));
    let Intent::AnalyzeSmartFrame {
        revision,
        cancellation,
        ..
    } = state.begin_smart_frame()
    else {
        panic!("expected analysis intent");
    };

    state.begin_with(Beautification::preset(BeautificationPreset::Editorial));
    assert!(cancellation.is_cancelled());

    state.finish_smart_frame_analysis(
        revision,
        Ok(SmartFrameAnalysis {
            beautification: Beautification::preset(BeautificationPreset::Story),
            inset_explanation: "stale".to_owned(),
        }),
    );
    assert_eq!(
        state.document().beautification(),
        Some(&Beautification::preset(BeautificationPreset::Editorial))
    );
}

#[test]
fn d9_window_preserves_subject_controls() {
    let mut state = EditorState::new(document(Provenance::Window));
    let _ = state.begin_smart_frame();
    let beauty = state.document().beautification().expect("outer frame");
    assert!(beauty.padding > 0.0);
    assert!(beauty.preserves_subject_pixels());
    assert!(state.document().may_beautify());
    assert!(!state.document().may_style_subject());
    assert_eq!(
        state.document().subject_appearance(),
        SubjectAppearance::Native
    );
}

#[test]
fn preset_build_and_upsert() {
    let mut state = EditorState::new(document(Provenance::Region));
    state.begin_with(Beautification::preset(BeautificationPreset::Clean));
    state.set_preset_name("My preset".to_owned());
    let preset = state.build_preset(false).expect("build preset");
    assert_eq!(preset.name, "My preset");

    state.upsert_local_preset(preset.clone());
    assert_eq!(state.custom_presets().len(), 1);
    assert_eq!(state.selected_preset(), Some(preset.id.as_str()));
}

#[test]
fn renaming_selected_preset_creates_new_instead_of_overwriting() {
    let mut state = EditorState::new(document(Provenance::Region));
    state.begin_with(Beautification::preset(BeautificationPreset::Clean));

    let original = SmartFramePreset::new(
        "quiet",
        "Quiet",
        SmartFramePresetSettings::from_beautification(state.document().beautification().unwrap())
            .unwrap(),
    )
    .unwrap();
    state.set_custom_presets(vec![original]);
    state.set_selected_preset(Some("quiet".to_owned()));
    state.set_preset_name("Quiet for docs".to_owned());

    let renamed = state.build_preset(false).unwrap();
    assert_ne!(renamed.id, "quiet");
    assert_eq!(renamed.name, "Quiet for docs");
}

#[test]
fn sensitive_review_is_delivered_without_auto_redacting() {
    let mut state = EditorState::new(document(Provenance::Region));
    let review = scrozz_annotate::SensitiveRegionReview {
        revision: 1,
        suggestions: vec![scrozz_annotate::SensitiveRegionSuggestion {
            id: "s1".to_owned(),
            bounds: LogicalRect::new(LogicalPoint::new(10.0, 10.0), LogicalSize::new(50.0, 20.0)),
            category: "email address".to_owned(),
            confidence: 90,
            ..Default::default()
        }],
        ..Default::default()
    };
    state.set_sensitive_review(review.clone());
    let got = state.sensitive_review().expect("review delivered");
    assert_eq!(got.suggestions.len(), 1);
    // Smart Frame NEVER auto-redacts; annotations are unchanged.
    assert!(state.document().annotations().is_empty());
}

#[test]
fn revert_framing_is_undoable() {
    let mut state = EditorState::new(document(Provenance::Region));
    state.begin_with(Beautification::preset(BeautificationPreset::Social));
    state.apply_smart_frame();
    assert!(state.document().beautification().is_some());

    state.revert_framing();
    assert!(state.document().beautification().is_none());
    assert!(state.can_undo_framing());

    state.undo_framing();
    assert!(state.document().beautification().is_some());
}
