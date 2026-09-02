//! Smart Frame analysis: deterministic, conservative, and bounded.

use std::collections::BTreeMap;

use scrozz_annotate::{
    AnalysisCancellation, AutomaticBackground, Background, Beautification, GeneratedStyle,
    InsetDecision, MAX_ANALYSIS_SAMPLES, PresetBackground, SceneAutomatic, SmartFramePreset,
    SmartFramePresetSettings, analyze_scene_with_style, analyze_smart_frame,
    analyze_with_style_after_fixed_inset, contrast_ratio,
};
use scrozz_core::{ColorSpace, Frame, PhysicalSize, PixelFormat, Provenance, ScaleFactor};

fn rgba_frame(width: u32, height: u32, fill: [u8; 4], color_space: ColorSpace) -> Frame {
    Frame {
        data: fill
            .into_iter()
            .cycle()
            .take(width as usize * height as usize * 4)
            .collect(),
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride: width as usize * 4,
        format: PixelFormat::Rgba8,
        color_space,
        scale: ScaleFactor::IDENTITY,
    }
}

fn paint_rect(frame: &mut Frame, left: u32, top: u32, right: u32, bottom: u32, color: [u8; 4]) {
    let width = frame.width() as usize;
    for y in top..bottom {
        for x in left..right {
            let offset = (y as usize * width + x as usize) * 4;
            frame.data[offset..offset + 4].copy_from_slice(&color);
        }
    }
}

#[test]
fn analysis_is_byte_deterministic() {
    let mut frame = rgba_frame(320, 180, [246, 247, 249, 255], ColorSpace::Srgb);
    paint_rect(&mut frame, 72, 42, 286, 148, [29, 35, 48, 255]);
    let cancellation = AnalysisCancellation::default();

    let a = analyze_smart_frame(&frame, Provenance::Region, &cancellation).unwrap();
    let b = analyze_smart_frame(&frame, Provenance::Region, &cancellation).unwrap();

    assert_eq!(a, b);
    assert!(a.beautification.auto_balance);
    assert!(matches!(
        a.beautification.background,
        Background::Automatic(_)
    ));
}

#[test]
fn automatic_scene_preserves_transparent_edges_and_adds_canvas_padding() {
    let mut frame = rgba_frame(100, 80, [0, 0, 0, 0], ColorSpace::DisplayP3);
    paint_rect(&mut frame, 10, 8, 90, 72, [220, 80, 40, 255]);

    let result =
        analyze_smart_frame(&frame, Provenance::Region, &AnalysisCancellation::default()).unwrap();
    let inset = result.beautification.inset;
    assert!(
        inset.is_zero(),
        "automatic Scenes preserve every source pixel"
    );
    assert!(!result.beautification.automatic.inset);
    let output = result
        .beautification
        .output_size(scrozz_core::LogicalSize::new(100.0, 80.0));
    assert!(output.width > 100.0);
    assert!(output.height > 80.0);
    let metadata = result.beautification.smart_frame.as_ref().unwrap();
    assert_eq!(metadata.inset_decision, InsetDecision::NoExcessMargin);
    assert_eq!(metadata.inset_confidence, 100);
    assert_eq!(
        result.inset_explanation,
        InsetDecision::NoExcessMargin.explanation()
    );
}

#[test]
fn analysis_after_a_fixed_inset_does_not_take_a_second_inset() {
    let mut frame = rgba_frame(100, 80, [0, 0, 0, 0], ColorSpace::Srgb);
    paint_rect(&mut frame, 10, 8, 90, 72, [220, 80, 40, 255]);

    let result = analyze_with_style_after_fixed_inset(
        &frame,
        Provenance::Region,
        GeneratedStyle::Balanced,
        &AnalysisCancellation::default(),
    )
    .unwrap();

    assert!(result.beautification.inset.is_zero());
    let metadata = result.beautification.smart_frame.unwrap();
    assert_eq!(metadata.inset_decision, InsetDecision::NoExcessMargin);
    assert_eq!(
        (metadata.source_width, metadata.source_height),
        (100, 80),
        "focus remains normalized to the complete already-fixed subject"
    );
}

#[test]
fn one_meaningful_edge_pixel_prevents_an_unsafe_inset() {
    let mut frame = rgba_frame(120, 80, [250, 250, 250, 255], ColorSpace::Srgb);
    paint_rect(&mut frame, 20, 12, 100, 68, [35, 40, 55, 255]);
    paint_rect(&mut frame, 2, 40, 3, 41, [35, 40, 55, 255]);

    let result =
        analyze_smart_frame(&frame, Provenance::Region, &AnalysisCancellation::default()).unwrap();

    assert_eq!(
        result.beautification.inset.left, 0.0,
        "a non-background edge pixel must stop the scan before the minimum safe margin"
    );
}

#[test]
fn one_sided_content_moves_toward_the_visual_center() {
    let mut frame = rgba_frame(200, 100, [20, 22, 27, 255], ColorSpace::Srgb);
    paint_rect(&mut frame, 150, 20, 194, 80, [245, 245, 245, 255]);

    let result =
        analyze_smart_frame(&frame, Provenance::Region, &AnalysisCancellation::default()).unwrap();
    let focus = result.beautification.smart_frame.unwrap().focus;
    assert!(
        focus.x > 6_000,
        "right-heavy content should resolve right of centre"
    );
}

#[test]
fn already_balanced_content_snaps_to_exact_center() {
    let mut frame = rgba_frame(200, 100, [20, 22, 27, 255], ColorSpace::Srgb);
    paint_rect(&mut frame, 60, 20, 140, 80, [245, 245, 245, 255]);

    let result =
        analyze_smart_frame(&frame, Provenance::Region, &AnalysisCancellation::default()).unwrap();
    let focus = result.beautification.smart_frame.unwrap().focus;
    assert_eq!((focus.x, focus.y), (5_000, 5_000));
}

#[test]
fn automatic_background_has_resolved_contrast_and_space_metadata() {
    let frame = rgba_frame(160, 90, [18, 28, 46, 255], ColorSpace::DisplayP3);
    let result = analyze_smart_frame(
        &frame,
        Provenance::Display,
        &AnalysisCancellation::default(),
    )
    .unwrap();
    let Background::Automatic(background) = result.beautification.background else {
        panic!("automatic background");
    };

    assert_eq!(background.source_color_space, ColorSpace::DisplayP3);
    assert!(contrast_ratio(background.start, background.edge_reference) >= 3.0);
    assert!(contrast_ratio(background.end, background.edge_reference) >= 3.0);
    assert!(background.minimum_contrast_x100 >= 300);
    assert_ne!(background.seed, 0);
    assert_eq!(background.resolved_palette().len(), 4);
}

#[test]
fn generated_styles_are_reproducible_and_visibly_distinct() {
    let mut frame = rgba_frame(180, 120, [24, 31, 52, 255], ColorSpace::Srgb);
    paint_rect(&mut frame, 90, 20, 170, 100, [228, 76, 126, 255]);
    let cancellation = AnalysisCancellation::default();
    let mut resolved = Vec::new();

    for style in [
        GeneratedStyle::Balanced,
        GeneratedStyle::Soft,
        GeneratedStyle::Vibrant,
        GeneratedStyle::Neutral,
    ] {
        let first =
            analyze_scene_with_style(&frame, Provenance::Region, style, &cancellation).unwrap();
        let second =
            analyze_scene_with_style(&frame, Provenance::Region, style, &cancellation).unwrap();
        assert_eq!(first, second);
        let Background::Automatic(background) = first.beautification.background else {
            panic!("generated background");
        };
        assert_eq!(background.style, style);
        resolved.push((background.template, background.resolved_palette()));
    }

    resolved.dedup();
    assert_eq!(
        resolved.len(),
        4,
        "each named direction must visibly change its deterministic treatment"
    );
}

#[test]
fn equivalent_wide_gamut_and_srgb_inputs_resolve_the_same_palette() {
    let srgb = rgba_frame(96, 64, [210, 54, 128, 255], ColorSpace::Srgb);
    let source = scrozz_export::to_straight_rgba8(&srgb).unwrap();
    let s =
        analyze_smart_frame(&srgb, Provenance::Region, &AnalysisCancellation::default()).unwrap();
    let Background::Automatic(s) = s.beautification.background else {
        panic!("sRGB automatic background");
    };
    for space in [ColorSpace::DisplayP3, ColorSpace::Rec2020] {
        let converted =
            scrozz_export::convert_color_space(&source, ColorSpace::Srgb, space).unwrap();
        let wide = Frame {
            data: converted.data,
            size: PhysicalSize::new(96.0, 64.0),
            stride: 96 * 4,
            format: PixelFormat::Rgba8,
            color_space: space,
            scale: ScaleFactor::IDENTITY,
        };
        let result =
            analyze_smart_frame(&wide, Provenance::Region, &AnalysisCancellation::default())
                .unwrap();
        let Background::Automatic(wide) = result.beautification.background else {
            panic!("{space:?} automatic background");
        };
        for (left, right) in [
            (s.start.r, wide.start.r),
            (s.start.g, wide.start.g),
            (s.start.b, wide.start.b),
            (s.end.r, wide.end.r),
            (s.end.g, wide.end.g),
            (s.end.b, wide.end.b),
        ] {
            assert!(
                left.abs_diff(right) <= 3,
                "{space:?} palette diverged: {left} vs {right}"
            );
        }
    }
}

#[test]
fn window_analysis_only_adds_an_outer_presentation_canvas() {
    let frame = rgba_frame(240, 160, [80, 100, 140, 255], ColorSpace::DisplayP3);
    let result =
        analyze_smart_frame(&frame, Provenance::Window, &AnalysisCancellation::default()).unwrap();
    let beauty = result.beautification;

    assert!(beauty.padding > 0.0);
    assert!(beauty.preserves_subject_pixels());
    assert_eq!(
        beauty.smart_frame.unwrap().inset_decision,
        InsetDecision::WindowPreserved
    );
}

#[test]
fn stitched_scene_uses_modest_vertical_and_useful_horizontal_space() {
    let frame = rgba_frame(300, 900, [80, 100, 140, 255], ColorSpace::Srgb);
    let result = analyze_smart_frame(
        &frame,
        Provenance::Stitched,
        &AnalysisCancellation::default(),
    )
    .unwrap();
    let padding = result.beautification.resolved_padding();
    assert_eq!(padding.left, padding.right);
    assert_eq!(padding.top, padding.bottom);
    assert!(padding.left > padding.top);
}

#[test]
fn cancellation_is_an_outcome_not_a_partial_result() {
    let frame = rgba_frame(640, 480, [20, 30, 40, 255], ColorSpace::Srgb);
    let cancellation = AnalysisCancellation::default();
    cancellation.cancel();
    let error = analyze_smart_frame(&frame, Provenance::Region, &cancellation)
        .expect_err("cancelled analysis");
    assert!(error.is_cancellation());
}

#[test]
fn scene_keeps_the_inner_inset_inside_its_bounds() {
    let mut document = scrozz_annotate::Document::new(scrozz_core::Capture {
        frame: rgba_frame(100, 80, [20, 30, 40, 255], ColorSpace::Srgb),
        provenance: Provenance::Region,
        target: scrozz_core::CaptureTarget::Region(scrozz_core::LogicalRect::new(
            scrozz_core::LogicalPoint::new(0.0, 0.0),
            scrozz_core::LogicalSize::new(100.0, 80.0),
        )),
    });
    let scene = Beautification {
        inset: scrozz_annotate::SourceInsets::uniform(4.0),
        ..Beautification::default()
    };

    document.set_scene(Some(scene)).unwrap();
    assert_eq!(
        document.scene().unwrap().inset,
        scrozz_annotate::SourceInsets::uniform(4.0),
        "a modest inner inset is stored verbatim"
    );

    // An inset authored against a much larger capture must degrade to the
    // nearest safe framing rather than refuse to open, and never past a
    // quarter of either axis.
    document
        .set_scene(Some(Beautification {
            inset: scrozz_annotate::SourceInsets::uniform(900.0),
            ..Beautification::default()
        }))
        .unwrap();
    assert_eq!(
        document.scene().unwrap().inset,
        scrozz_annotate::SourceInsets {
            left: 25.0,
            top: 20.0,
            right: 25.0,
            bottom: 20.0,
        }
    );

    // Clearing it restores the complete source: nothing was destroyed.
    document.set_scene(Some(Beautification::default())).unwrap();
    assert!(document.scene().unwrap().inset.is_zero());
}

#[test]
fn a_window_capture_never_takes_an_inner_inset() {
    let mut document = scrozz_annotate::Document::new(scrozz_core::Capture {
        frame: rgba_frame(100, 80, [20, 30, 40, 255], ColorSpace::Srgb),
        provenance: Provenance::Window,
        target: scrozz_core::CaptureTarget::Window(scrozz_core::WindowId("window-1".into())),
    });

    assert!(
        document
            .set_scene(Some(Beautification {
                inset: scrozz_annotate::SourceInsets::uniform(4.0),
                ..Beautification::default()
            }))
            .is_err(),
        "D9: the OS supplied this silhouette, corners and shadow"
    );
    assert!(!SceneAutomatic::native_window().inset);
}

#[test]
fn preset_round_trip_preserves_unknown_fields_but_never_pixels() {
    let mut beauty = Beautification {
        inner_padding: 24.0,
        background: Background::Automatic(Default::default()),
        ..Beautification::default()
    };
    let mut settings = SmartFramePresetSettings::from_beautification(&beauty).unwrap();
    settings.extensions.insert(
        "future_control".to_owned(),
        serde_json::json!({"enabled": true}),
    );
    let mut preset = SmartFramePreset::new("my-frame", "My Frame", settings).unwrap();
    preset
        .extensions
        .insert("future".to_owned(), serde_json::json!(7));

    let json = serde_json::to_string(&preset).unwrap();
    let restored: SmartFramePreset = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.extensions["future"], 7);
    assert_eq!(
        restored.settings.extensions["future_control"]["enabled"],
        true
    );
    let rebuilt =
        SmartFramePresetSettings::from_beautification(&restored.settings.to_beautification())
            .unwrap();
    assert_eq!(rebuilt.extensions["future_control"]["enabled"], true);
    assert_eq!(rebuilt.inner_padding, 24.0);
    assert!(!json.contains("pixels_png"));

    let image =
        scrozz_annotate::BackgroundImage::new(1, 1, vec![0, 0, 0, 255], ColorSpace::Srgb).unwrap();
    beauty.background = Background::Image(image);
    assert!(SmartFramePresetSettings::from_beautification(&beauty).is_err());
}

#[test]
fn preset_ids_cannot_collide_with_scene_assignment_sentinels() {
    for id in ["auto", "AUTO", "none", "default"] {
        let error =
            SmartFramePreset::new(id, "Custom", SmartFramePresetSettings::default()).unwrap_err();
        assert!(error.to_string().contains("reserved"), "{error}");
    }
}

#[test]
fn fixed_generated_background_round_trips_through_a_preset() {
    let mut generated =
        AutomaticBackground::fallback(ColorSpace::DisplayP3).restyled(GeneratedStyle::Vibrant);
    generated.seed = 8_675_309;
    let mut beauty = Beautification {
        background: Background::Automatic(generated.clone()),
        ..Beautification::default()
    };
    beauty.automatic.background = false;

    let settings = SmartFramePresetSettings::from_beautification(&beauty).unwrap();
    assert!(matches!(
        settings.background,
        PresetBackground::ResolvedGenerated(_)
    ));
    assert_eq!(
        settings.to_beautification().background,
        Background::Automatic(generated)
    );
}

#[test]
fn legacy_presets_default_only_explicit_automatic_backgrounds_to_automatic() {
    let fixed = SmartFramePresetSettings::from_beautification(&Beautification {
        padding: 48.0,
        background: Background::Solid(scrozz_annotate::Color::rgb(20, 40, 80)),
        ..Beautification::default()
    })
    .unwrap();
    let mut fixed_json = serde_json::to_value(fixed).unwrap();
    fixed_json.as_object_mut().unwrap().remove("automatic");
    fixed_json.as_object_mut().unwrap().remove("inner_padding");
    let fixed: SmartFramePresetSettings = serde_json::from_value(fixed_json).unwrap();
    assert!(!fixed.automatic.any());
    assert_eq!(fixed.inner_padding, 0.0);

    let mut automatic_json = serde_json::to_value(SmartFramePresetSettings {
        background: PresetBackground::Automatic,
        ..SmartFramePresetSettings::default()
    })
    .unwrap();
    automatic_json.as_object_mut().unwrap().remove("automatic");
    let automatic: SmartFramePresetSettings = serde_json::from_value(automatic_json).unwrap();
    assert!(automatic.automatic.background);
    assert!(!automatic.automatic.padding);
    assert!(!automatic.automatic.placement);
    assert!(!automatic.automatic.corners);
    assert!(!automatic.automatic.shadow);
    assert!(!automatic.automatic.output_size);
}

#[test]
fn near_identical_dimensions_change_adaptive_tokens_smoothly() {
    let a = analyze_smart_frame(
        &rgba_frame(1000, 600, [40, 44, 52, 255], ColorSpace::Srgb),
        Provenance::Display,
        &AnalysisCancellation::default(),
    )
    .unwrap();
    let b = analyze_smart_frame(
        &rgba_frame(1001, 601, [40, 44, 52, 255], ColorSpace::Srgb),
        Provenance::Display,
        &AnalysisCancellation::default(),
    )
    .unwrap();
    assert!((a.beautification.padding - b.beautification.padding).abs() <= 2.0);
    assert!((a.beautification.corner_radius - b.beautification.corner_radius).abs() <= 2.0);
    assert!((a.beautification.shadow - b.beautification.shadow).abs() <= 2.0);
}

#[test]
fn diverse_capture_shapes_stay_bounded_and_deterministic() {
    for (width, height, dark, transparent) in [
        (2048, 240, true, false),
        (240, 2048, false, false),
        (1024, 768, true, true),
        (640, 480, false, false),
    ] {
        let fill = if transparent {
            [0, 0, 0, 0]
        } else if dark {
            [18, 22, 30, 255]
        } else {
            [244, 242, 236, 255]
        };
        let mut frame = rgba_frame(width, height, fill, ColorSpace::Srgb);
        let object = if width > height {
            (width * 2 / 3, height / 4, width - 8, height * 3 / 4)
        } else {
            (width / 4, height * 2 / 3, width * 3 / 4, height - 8)
        };
        paint_rect(
            &mut frame,
            object.0,
            object.1,
            object.2,
            object.3,
            [230, 75, 90, 255],
        );
        let first =
            analyze_smart_frame(&frame, Provenance::Region, &AnalysisCancellation::default())
                .unwrap();
        let second =
            analyze_smart_frame(&frame, Provenance::Region, &AnalysisCancellation::default())
                .unwrap();
        assert_eq!(first, second);
        let metadata = first.beautification.smart_frame.unwrap();
        assert!(u64::from(metadata.analysis_samples) <= MAX_ANALYSIS_SAMPLES);
        assert!((24.0..=96.0).contains(&first.beautification.padding));
        assert!((8.0..=24.0).contains(&first.beautification.corner_radius));
        assert!((10.0..=28.0).contains(&first.beautification.shadow));
    }
}

#[test]
fn disconnected_objects_contribute_to_one_stable_focus() {
    let mut frame = rgba_frame(300, 160, [28, 30, 36, 255], ColorSpace::Srgb);
    paint_rect(&mut frame, 20, 20, 70, 80, [245, 245, 245, 255]);
    paint_rect(&mut frame, 220, 90, 286, 148, [210, 70, 120, 255]);
    let result =
        analyze_smart_frame(&frame, Provenance::Region, &AnalysisCancellation::default()).unwrap();
    let focus = result.beautification.smart_frame.unwrap().focus;
    assert!((3_500..=7_500).contains(&focus.x));
    assert!((3_000..=7_500).contains(&focus.y));
}

#[test]
fn detailed_photo_like_edges_keep_the_full_source() {
    let mut frame = rgba_frame(256, 160, [0, 0, 0, 255], ColorSpace::Srgb);
    for y in 0..frame.height() as usize {
        for x in 0..frame.width() as usize {
            let offset = y * frame.stride + x * 4;
            let seed = (x as u32)
                .wrapping_mul(73)
                .wrapping_add((y as u32).wrapping_mul(151));
            frame.data[offset..offset + 4].copy_from_slice(&[
                seed as u8,
                seed.rotate_left(7) as u8,
                seed.rotate_left(13) as u8,
                255,
            ]);
        }
    }
    let result =
        analyze_smart_frame(&frame, Provenance::Region, &AnalysisCancellation::default()).unwrap();
    assert!(result.beautification.inset.is_zero());
    assert_eq!(
        result.beautification.smart_frame.unwrap().inset_decision,
        InsetDecision::NoExcessMargin,
        "automatic Scene analysis never turns source pixels into padding"
    );
}

#[test]
fn malformed_and_oversized_analysis_inputs_are_refused() {
    let malformed = Frame {
        data: vec![0; 3],
        size: PhysicalSize::new(100.0, 100.0),
        stride: 400,
        format: PixelFormat::Rgba8,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::IDENTITY,
    };
    assert!(
        analyze_smart_frame(
            &malformed,
            Provenance::Region,
            &AnalysisCancellation::default()
        )
        .is_err()
    );

    let oversized = Frame {
        data: Vec::new(),
        size: PhysicalSize::new(40_000_001.0, 1.0),
        stride: 40_000_001 * 4,
        format: PixelFormat::Rgba8,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::IDENTITY,
    };
    let error = analyze_smart_frame(
        &oversized,
        Provenance::Region,
        &AnalysisCancellation::default(),
    )
    .expect_err("area bound is checked before reading the empty buffer");
    assert!(error.to_string().contains("malformed") || error.to_string().contains("limit"));
}

#[test]
fn preset_schema_rejects_future_versions_without_losing_unknown_values() {
    let preset = SmartFramePreset {
        version: u32::MAX,
        id: "future".to_owned(),
        name: "Future".to_owned(),
        extensions: BTreeMap::from([("vendor".to_owned(), serde_json::json!("kept"))]),
        ..SmartFramePreset::default()
    };
    assert!(preset.validate().is_err());
    assert_eq!(preset.extensions["vendor"], "kept");
}
