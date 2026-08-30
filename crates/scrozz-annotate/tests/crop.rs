//! Non-destructive crop: what it changes, and what it must not.

mod common;

use common::{document, frame_with, near, pixel, rect};
use scrozz_annotate::{
    Annotation, Beautification, CropExpansion, Document, DocumentData, ImageOrientation, Renderer,
    ResolvedFocus, SkiaRenderer, SmartFrameMetadata, Style, document::Background,
};
use scrozz_core::{
    Capture, CaptureTarget, LogicalPoint, LogicalSize, Provenance, ScaleFactor, WindowId,
};

/// A 100×100 document whose pixels encode their own coordinates, so a crop that
/// lands one pixel off is detectable rather than merely plausible.
fn coded() -> Document {
    let frame = frame_with(100, 100, 1.0, |x, y| [x as u8, y as u8, 128, 255]);
    Document::new(Capture {
        frame,
        provenance: Provenance::Region,
        target: CaptureTarget::Region(rect(0.0, 0.0, 100.0, 100.0)),
    })
}

#[test]
fn a_new_document_is_uncropped() {
    let doc = document(200, 120);
    assert_eq!(doc.crop(), None);
    assert_eq!(doc.content_bounds(), doc.logical_bounds());
}

#[test]
fn crop_changes_the_rendered_size() {
    let mut doc = coded();
    doc.set_crop(Some(rect(10.0, 20.0, 40.0, 30.0))).unwrap();
    let frame = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!((frame.width(), frame.height()), (40, 30));
}

#[test]
fn source_geometry_changes_invalidate_resolved_scene_focus_but_noops_do_not() {
    let mut doc = coded();
    let mut scene = Beautification {
        auto_balance: true,
        smart_frame: Some(SmartFrameMetadata {
            focus: ResolvedFocus {
                x: 8_000,
                y: 2_000,
                confidence: 90,
            },
            ..SmartFrameMetadata::default()
        }),
        ..Beautification::default()
    };
    doc.set_scene(Some(scene.clone())).unwrap();

    let crop = rect(10.0, 20.0, 40.0, 30.0);
    doc.set_crop(Some(crop)).unwrap();
    assert_eq!(
        doc.scene()
            .unwrap()
            .smart_frame
            .as_ref()
            .unwrap()
            .focus
            .confidence,
        0
    );

    scene.smart_frame.as_mut().unwrap().focus.confidence = 90;
    doc.set_scene(Some(scene.clone())).unwrap();
    let revision = doc.revision();
    doc.set_crop(Some(crop)).unwrap();
    assert_eq!(doc.revision(), revision);
    assert_eq!(
        doc.scene()
            .unwrap()
            .smart_frame
            .as_ref()
            .unwrap()
            .focus
            .confidence,
        90,
        "an unchanged crop must not invalidate metadata or spend a revision"
    );

    doc.set_orientation(ImageOrientation::RotateRight);
    assert_eq!(
        doc.scene()
            .unwrap()
            .smart_frame
            .as_ref()
            .unwrap()
            .focus
            .confidence,
        0
    );
}

#[test]
fn crop_selects_the_right_pixels() {
    let mut doc = coded();
    doc.set_crop(Some(rect(10.0, 20.0, 40.0, 30.0))).unwrap();
    let frame = SkiaRenderer::new().render(&doc).unwrap();

    // The crop's top-left must be the source pixel at (10, 20), which encodes
    // its own coordinates in red and green.
    assert!(near(pixel(&frame, 0, 0), [10, 20, 128, 255], 1));
    assert!(near(pixel(&frame, 39, 29), [49, 49, 128, 255], 1));
}

#[test]
fn fractional_crop_edges_are_quantized_together() {
    let mut doc = coded();
    doc.set_crop(Some(rect(20.5, 30.5, 4.5, 4.5))).unwrap();
    let frame = SkiaRenderer::new().render(&doc).unwrap();

    assert_eq!((frame.width(), frame.height()), (4, 4));
    assert!(
        near(pixel(&frame, 3, 3), [24, 34, 128, 255], 1),
        "rounding origin and size separately exposed the excluded right or bottom edge"
    );
}

#[test]
fn crop_moves_annotations_with_the_image() {
    let mut doc = coded();
    doc.add(
        Annotation::Rectangle(rect(30.0, 30.0, 20.0, 20.0)),
        Style::stroked()
            .with_fill(Some(scrozz_annotate::Color::rgb(255, 0, 0)))
            .with_stroke_width(1.0),
    );
    doc.set_crop(Some(rect(20.0, 20.0, 50.0, 50.0))).unwrap();
    let frame = SkiaRenderer::new().render(&doc).unwrap();

    // The rectangle's interior sat at source (40, 40); after cropping to an
    // origin of (20, 20) it must sit at (20, 20) in the output.
    let inside = pixel(&frame, 20, 20);
    assert!(
        near(inside, [255, 0, 0, 255], 4),
        "annotation did not track the crop: {inside:?}"
    );
}

#[test]
fn crop_is_non_destructive() {
    let mut doc = coded();
    let original = doc.source().frame.data.clone();
    doc.set_crop(Some(rect(10.0, 10.0, 20.0, 20.0))).unwrap();
    assert_eq!(
        doc.source().frame.data,
        original,
        "the source keeps every pixel: a crop is a view, not an edit"
    );

    doc.set_crop(None).unwrap();
    let frame = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!(
        (frame.width(), frame.height()),
        (100, 100),
        "clearing the crop must restore the whole capture"
    );
    assert!(near(pixel(&frame, 99, 99), [99, 99, 128, 255], 1));
}

#[test]
fn crop_keeps_annotations_that_fall_outside_it() {
    let mut doc = coded();
    doc.add_default(Annotation::Rectangle(rect(80.0, 80.0, 10.0, 10.0)));
    doc.set_crop(Some(rect(0.0, 0.0, 40.0, 40.0))).unwrap();
    assert_eq!(
        doc.len(),
        1,
        "cropping must not silently delete work: widening the crop brings it back"
    );
}

#[test]
fn crop_clamps_to_the_capture_rather_than_inventing_margin() {
    let mut doc = coded();
    doc.set_crop(Some(rect(-50.0, -50.0, 120.0, 120.0)))
        .unwrap();
    assert_eq!(doc.content_bounds(), rect(0.0, 0.0, 70.0, 70.0));

    let frame = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!((frame.width(), frame.height()), (70, 70));
}

#[test]
fn a_crop_covering_everything_is_no_crop() {
    let mut doc = coded();
    let revision = doc.revision();
    doc.set_crop(Some(rect(-10.0, -10.0, 500.0, 500.0)))
        .unwrap();
    assert_eq!(
        doc.crop(),
        None,
        "a crop the user cannot see must not be a crop they cannot clear"
    );
    assert_eq!(
        doc.revision(),
        revision,
        "normalizing to the existing full-frame view is not a content change"
    );
}

#[test]
fn a_crop_outside_the_capture_is_refused() {
    let mut doc = coded();
    assert!(doc.set_crop(Some(rect(500.0, 500.0, 10.0, 10.0))).is_err());
    assert_eq!(doc.crop(), None, "a refused crop must not half-apply");
}

#[test]
fn an_empty_crop_is_refused() {
    let mut doc = coded();
    assert!(doc.set_crop(Some(rect(10.0, 10.0, 0.0, 20.0))).is_err());
}

#[test]
fn a_non_finite_crop_is_refused() {
    let mut doc = coded();
    assert!(doc.set_crop(Some(rect(f64::NAN, 0.0, 10.0, 10.0))).is_err());
    assert!(
        doc.set_crop(Some(rect(0.0, 0.0, f64::INFINITY, 10.0)))
            .is_err()
    );
}

#[test]
fn crop_survives_a_round_trip_through_persistence() {
    let mut doc = coded();
    doc.set_crop(Some(rect(12.0, 34.0, 40.0, 25.0))).unwrap();
    let json = serde_json::to_string(&doc.data()).unwrap();
    let data: DocumentData = serde_json::from_str(&json).unwrap();
    let restored = Document::from_data(doc.source().clone(), data).unwrap();
    assert_eq!(restored.crop(), Some(rect(12.0, 34.0, 40.0, 25.0)));
}

#[test]
fn documents_saved_before_crop_existed_still_load() {
    // Version 1 payloads have no `crop` key at all. They must load as uncropped
    // rather than failing, or every capture taken before this feature shipped
    // would become unopenable.
    let legacy = r#"{"version":1,"annotations":[],"beautification":null,"next_id":1}"#;
    let data: DocumentData = serde_json::from_str(legacy).unwrap();
    assert_eq!(data.crop, None);
    let doc = Document::from_data(coded().source().clone(), data).unwrap();
    assert_eq!(doc.crop(), None);
}

#[test]
fn crop_is_written_in_the_current_version() {
    let mut doc = coded();
    doc.set_crop(Some(rect(10.0, 10.0, 20.0, 20.0))).unwrap();
    let data = doc.data();

    assert_eq!(data.version, DocumentData::VERSION);
    assert!(data.crop.is_some());
}

#[test]
fn crop_scales_with_the_export_scale() {
    let mut doc = coded();
    doc.set_crop(Some(rect(10.0, 10.0, 30.0, 20.0))).unwrap();
    let frame = SkiaRenderer::new()
        .render_at(&doc, ScaleFactor::new(2.0))
        .unwrap();
    assert_eq!((frame.width(), frame.height()), (60, 40));
}

#[test]
fn render_to_width_measures_the_crop_not_the_capture() {
    let mut doc = coded();
    doc.set_crop(Some(rect(0.0, 0.0, 50.0, 25.0))).unwrap();
    let frame = SkiaRenderer::new().render_to_width(&doc, 200).unwrap();
    assert_eq!(
        (frame.width(), frame.height()),
        (200, 100),
        "the requested width applies to what is visible"
    );
}

#[test]
fn enlarging_a_tiny_crop_refuses_an_unsafe_full_domain_allocation() {
    let mut doc = Document::new(common::capture_with(
        common::checkerboard(1_000, 1_000, 4),
        Provenance::Region,
    ));
    doc.add_default(Annotation::Redact {
        area: rect(400.0, 400.0, 100.0, 100.0),
        style: scrozz_annotate::RedactStyle::Pixelate,
    });
    doc.set_crop(Some(rect(440.0, 440.0, 10.0, 10.0))).unwrap();

    let error = SkiaRenderer::new()
        .render_to_width(&doc, 1_000)
        .expect_err("a huge cropped export must still be refused");
    assert!(error.to_string().contains("not addressable"), "{error}");
}

#[test]
fn an_uncropped_render_is_still_byte_for_byte_at_one_to_one() {
    let doc = coded();
    let frame = SkiaRenderer::new().render(&doc).unwrap();
    for (x, y) in [(0, 0), (1, 1), (50, 73), (99, 99)] {
        assert_eq!(
            pixel(&frame, x, y),
            [x as u8, y as u8, 128, 255],
            "adding a crop transform must not soften the uncropped common case"
        );
    }
}

#[test]
fn crop_applies_before_beautification() {
    let mut doc = coded();
    doc.set_crop(Some(rect(10.0, 10.0, 40.0, 40.0))).unwrap();
    doc.set_beautification(Some(scrozz_annotate::Beautification::padded(
        20.0,
        Background::Solid(scrozz_annotate::Color::rgb(0, 0, 255)),
    )))
    .unwrap();
    let frame = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!(
        (frame.width(), frame.height()),
        (80, 80),
        "padding must frame the cropped image, not the original"
    );
    assert!(
        near(pixel(&frame, 2, 2), [0, 0, 255, 255], 2),
        "the padding is the background colour"
    );
}

#[test]
fn cropping_a_window_capture_does_not_smuggle_in_compositing() {
    // Decision D9. Crop is a legitimate operation on a window capture, and
    // Smart Frame may still add only an outer canvas afterwards. Subject styling
    // remains forbidden.
    let capture = common::window_capture(100, 100);
    let mut doc = Document::new(capture);
    doc.set_crop(Some(rect(10.0, 10.0, 50.0, 50.0))).unwrap();
    assert!(doc.may_beautify());
    doc.set_beautification(Some(scrozz_annotate::Beautification::padded(
        8.0,
        scrozz_annotate::Background::Solid(scrozz_annotate::Color::WHITE),
    )))
    .unwrap();
    assert!(
        doc.set_beautification(Some(scrozz_annotate::Beautification {
            padding: 8.0,
            corner_radius: 6.0,
            background: scrozz_annotate::Background::Solid(scrozz_annotate::Color::WHITE),
            ..scrozz_annotate::Beautification::default()
        }))
        .is_err()
    );
    let frame = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!((frame.width(), frame.height()), (50 + 16, 50 + 16));
}

#[test]
fn a_redaction_inside_a_crop_still_destroys_pixels() {
    let mut doc = Document::new(common::capture_with(
        common::checkerboard(100, 100, 2),
        Provenance::Region,
    ));
    doc.add_default(Annotation::Redact {
        area: rect(30.0, 30.0, 20.0, 20.0),
        style: scrozz_annotate::RedactStyle::Pixelate,
    });
    doc.set_crop(Some(rect(20.0, 20.0, 50.0, 50.0))).unwrap();
    let frame = SkiaRenderer::new().render(&doc).unwrap();

    // The redaction sat at source (30..50); in the crop it is at (10..30).
    let mut extremes = 0;
    for y in 12..28 {
        for x in 12..28 {
            let p = pixel(&frame, x, y);
            if p[0] == 0 || p[0] == 255 {
                extremes += 1;
            }
        }
    }
    assert_eq!(
        extremes, 0,
        "the checkerboard's pure black and white must be gone inside the redaction"
    );
}

#[test]
fn a_tiny_crop_cannot_shrink_a_redaction_into_a_no_op() {
    for style in [
        scrozz_annotate::RedactStyle::Pixelate,
        scrozz_annotate::RedactStyle::Blur,
    ] {
        let mut doc = Document::new(common::capture_with(
            common::checkerboard(100, 100, 2),
            Provenance::Region,
        ));
        let original = pixel(&doc.source().frame, 25, 25);
        doc.add_default(Annotation::Redact {
            area: rect(20.0, 20.0, 20.0, 20.0),
            style,
        });
        doc.set_crop(Some(rect(25.0, 25.0, 1.0, 1.0))).unwrap();

        let frame = SkiaRenderer::new().render(&doc).unwrap();
        assert_eq!((frame.width(), frame.height()), (1, 1));
        assert_ne!(
            pixel(&frame, 0, 0),
            original,
            "{style:?} sampled only the crop and left the source pixel intact"
        );
    }
}

#[test]
fn content_size_reports_the_visible_area() {
    let mut doc = coded();
    assert_eq!(doc.content_size().width, 100.0);
    doc.set_crop(Some(rect(0.0, 0.0, 33.0, 44.0))).unwrap();
    assert_eq!(doc.content_size().width, 33.0);
    assert_eq!(doc.content_size().height, 44.0);
}

#[test]
fn hit_testing_stays_in_source_coordinates() {
    // The editor maps a click through the crop before hit-testing; the document
    // itself must not double-apply that shift.
    let mut doc = coded();
    let id = doc.add_default(Annotation::Rectangle(rect(30.0, 30.0, 20.0, 20.0)));
    doc.set_crop(Some(rect(20.0, 20.0, 50.0, 50.0))).unwrap();
    assert_eq!(doc.hit_test(LogicalPoint::new(40.0, 30.0)), Some(id));
}

#[test]
fn a_window_capture_crop_round_trips() {
    let capture = Capture {
        frame: common::flat(80, 80, [10, 20, 30, 255]),
        provenance: Provenance::Window,
        target: CaptureTarget::Window(WindowId("w".to_owned())),
    };
    let mut doc = Document::new(capture.clone());
    doc.set_crop(Some(rect(5.0, 5.0, 40.0, 40.0))).unwrap();
    let restored = Document::from_data(capture, doc.data()).unwrap();
    assert_eq!(restored.crop(), Some(rect(5.0, 5.0, 40.0, 40.0)));
}

#[test]
fn rotation_and_flip_apply_after_crop_without_resampling() {
    let mut rotated = coded();
    rotated.set_crop(Some(rect(10.0, 20.0, 3.0, 2.0))).unwrap();
    rotated.set_orientation(ImageOrientation::RotateRight);
    let frame = SkiaRenderer::new().render(&rotated).unwrap();
    assert_eq!((frame.width(), frame.height()), (2, 3));
    assert_eq!(pixel(&frame, 0, 0), [10, 21, 128, 255]);
    assert_eq!(pixel(&frame, 1, 0), [10, 20, 128, 255]);
    assert_eq!(pixel(&frame, 0, 2), [12, 21, 128, 255]);

    let mut flipped = coded();
    flipped.set_crop(Some(rect(10.0, 20.0, 3.0, 2.0))).unwrap();
    flipped.set_orientation(ImageOrientation::FlipHorizontal);
    let frame = SkiaRenderer::new().render(&flipped).unwrap();
    assert_eq!((frame.width(), frame.height()), (3, 2));
    assert_eq!(pixel(&frame, 0, 0), [12, 20, 128, 255]);
    assert_eq!(pixel(&frame, 2, 1), [10, 21, 128, 255]);
}

#[test]
fn orientation_round_trips_and_legacy_documents_default_to_identity() {
    let mut doc = coded();
    doc.set_orientation(ImageOrientation::Transverse);
    let json = serde_json::to_string(&doc.data()).unwrap();
    let data: DocumentData = serde_json::from_str(&json).unwrap();
    let restored = Document::from_data(doc.source().clone(), data).unwrap();
    assert_eq!(restored.orientation(), ImageOrientation::Transverse);

    let legacy = r#"{"version":5,"annotations":[],"beautification":null,"crop":null,"next_id":1}"#;
    let data: DocumentData = serde_json::from_str(legacy).unwrap();
    assert_eq!(data.orientation, ImageOrientation::Identity);
}

#[test]
fn outward_crop_resolution_is_an_asymmetric_scene_handoff() {
    let doc = coded();
    let resolution = doc
        .resolve_crop(Some(rect(-12.0, 8.0, 140.0, 110.0)))
        .unwrap();

    assert_eq!(resolution.source_crop, Some(rect(0.0, 8.0, 100.0, 92.0)));
    assert_eq!(
        resolution.expansion,
        CropExpansion {
            left: 12.0,
            top: 0.0,
            right: 28.0,
            bottom: 18.0,
        }
    );
    assert_eq!(
        resolution
            .expansion
            .apply_orientation(ImageOrientation::RotateRight),
        CropExpansion {
            left: 18.0,
            top: 12.0,
            right: 0.0,
            bottom: 28.0,
        }
    );
    assert_eq!(doc.crop(), None, "resolving a handoff must not mutate");
}

#[test]
fn every_orientation_has_exact_inverse_point_and_rectangle_mappings() {
    let source = LogicalSize::new(100.0, 80.0);
    let area = rect(13.0, 17.0, 29.0, 31.0);
    for orientation in [
        ImageOrientation::Identity,
        ImageOrientation::RotateRight,
        ImageOrientation::Rotate180,
        ImageOrientation::RotateLeft,
        ImageOrientation::FlipHorizontal,
        ImageOrientation::FlipVertical,
        ImageOrientation::Transpose,
        ImageOrientation::Transverse,
    ] {
        for point in [
            LogicalPoint::new(0.0, 0.0),
            LogicalPoint::new(37.0, 23.0),
            LogicalPoint::new(100.0, 80.0),
        ] {
            assert_eq!(
                orientation.invert_point(orientation.apply_point(point, source), source),
                point,
                "{orientation:?}"
            );
        }
        assert_eq!(
            orientation.invert_rect(orientation.apply_rect(area, source), source),
            area,
            "{orientation:?}"
        );
    }
}
