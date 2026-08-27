//! Rendering: determinism, resolution independence, and the D9 refusal.

mod common;

use common::{
    capture_with, document, flat, near, pixel, pixels, rect, region_capture, window_capture,
};
use scrozz_annotate::{
    Annotation, Background, Beautification, Canvas, CanvasRotation, Color, Document, Renderer,
    SkiaRenderer, Style,
};
use scrozz_core::{Frame, LogicalPoint, PixelFormat, Provenance, ScaleFactor};

#[test]
fn an_empty_document_renders_the_source_unchanged() {
    let doc = Document::new(capture_with(
        flat(32, 24, [17, 99, 200, 255]),
        Provenance::Region,
    ));
    let out = SkiaRenderer::new().render(&doc).unwrap();

    assert_eq!(out.width(), 32);
    assert_eq!(out.height(), 24);
    assert!(out.is_well_formed());
    for p in pixels(&out) {
        assert!(near(p, [17, 99, 200, 255], 1), "got {p:?}");
    }
}

#[test]
fn rendering_is_deterministic() {
    let mut doc = document(160, 120);
    for (annotation, style) in common::every_annotation() {
        doc.add(annotation, style);
    }
    doc.set_beautification(Some(Beautification {
        padding: 20.0,
        corner_radius: 8.0,
        shadow: 12.0,
        background: Background::Gradient {
            start: Color::rgb(250, 250, 250),
            end: Color::rgb(190, 200, 220),
        },
    }))
    .unwrap();

    let renderer = SkiaRenderer::new();
    let a = renderer.render(&doc).unwrap();
    let b = renderer.render(&doc).unwrap();
    assert_eq!(
        a.data, b.data,
        "the same document must always produce the same bytes: golden tests and \
         content-addressed export both depend on it"
    );
    assert_eq!(a.stride, b.stride);
    assert_eq!(a.format, b.format);
}

#[test]
fn rendering_never_mutates_the_source() {
    // Decision D14: the source is untouched forever, so the annotations stay
    // editable forever. A renderer that composited in place would quietly make
    // the first export permanent.
    let mut doc = document(120, 90);
    doc.add(
        Annotation::Redact {
            area: rect(0.0, 0.0, 120.0, 90.0),
            style: scrozz_annotate::RedactStyle::Solid,
        },
        Style::redaction(),
    );
    let before = doc.source.frame.data.clone();

    let _ = SkiaRenderer::new().render(&doc).unwrap();
    let _ = SkiaRenderer::new().render(&doc).unwrap();

    assert_eq!(
        before, doc.source.frame.data,
        "a full-frame redaction must not have touched the document's own pixels"
    );
}

#[test]
fn output_is_premultiplied_and_well_formed() {
    let doc = document(40, 40);
    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!(out.format, PixelFormat::RgbaPremultiplied8);
    assert!(out.is_well_formed());
    assert_eq!(out.stride, out.width() as usize * 4);
    assert_eq!(out.color_space, doc.source.frame.color_space);
}

#[test]
fn rendering_at_2x_doubles_the_canvas() {
    let doc = document(100, 60);
    let renderer = SkiaRenderer::new();
    let one = renderer.render_at(&doc, ScaleFactor::new(1.0)).unwrap();
    let two = renderer.render_at(&doc, ScaleFactor::new(2.0)).unwrap();

    assert_eq!(two.width(), one.width() * 2);
    assert_eq!(two.height(), one.height() * 2);
    assert_eq!(two.scale.get(), 2.0);
}

#[test]
fn crop_rotate_and_flip_transform_pixels_without_touching_the_source() {
    let source = common::frame_with(4, 2, 1.0, |x, _| match x {
        0 => [255, 0, 0, 255],
        1 => [0, 255, 0, 255],
        2 => [0, 0, 255, 255],
        _ => [255, 255, 0, 255],
    });
    let mut doc = Document::new(capture_with(source, Provenance::Region));
    let original = doc.source.frame.data.clone();
    doc.set_canvas(Canvas {
        crop: Some(rect(1.0, 0.0, 2.0, 2.0)),
        rotation: CanvasRotation::Clockwise90,
        flip_horizontal: true,
        flip_vertical: false,
        auto_expand: false,
    })
    .unwrap();

    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!((out.width(), out.height()), (2, 2));
    assert_eq!(doc.source.frame.data, original);
    let colors = pixels(&out);
    assert!(colors.iter().all(|p| p[3] == 255));
    assert!(colors.iter().any(|p| near(*p, [0, 255, 0, 255], 1)));
    assert!(colors.iter().any(|p| near(*p, [0, 0, 255, 255], 1)));
    assert!(
        colors
            .iter()
            .all(|p| !near(*p, [255, 0, 0, 255], 1) && !near(*p, [255, 255, 0, 255], 1))
    );
}

#[test]
fn temporary_canvas_render_reveals_the_source_without_changing_the_crop() {
    let mut doc = Document::new(capture_with(
        flat(80, 50, [17, 99, 200, 255]),
        Provenance::Region,
    ));
    doc.set_canvas(Canvas {
        crop: Some(rect(10.0, 8.0, 40.0, 24.0)),
        ..Canvas::default()
    })
    .unwrap();
    let persisted = doc.data();

    let renderer = SkiaRenderer::new();
    let cropped = renderer.render(&doc).unwrap();
    let workspace = renderer
        .render_canvas(
            &doc,
            Canvas {
                crop: None,
                ..*doc.canvas()
            },
        )
        .unwrap();

    assert_eq!((cropped.width(), cropped.height()), (40, 24));
    assert_eq!((workspace.width(), workspace.height()), (80, 50));
    assert_eq!(doc.data(), persisted);
}

#[test]
fn auto_expand_keeps_objects_drawn_beyond_the_source() {
    let mut doc = Document::new(capture_with(
        flat(40, 30, [255, 255, 255, 255]),
        Provenance::Region,
    ));
    doc.add(
        Annotation::Rectangle(rect(45.0, 5.0, 20.0, 15.0)),
        Style::stroked()
            .with_stroke(Color::BLACK)
            .with_stroke_width(3.0),
    );
    doc.set_canvas(Canvas {
        auto_expand: true,
        ..Canvas::default()
    })
    .unwrap();

    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert!(out.width() > 60);
    assert!(
        (40..out.width()).any(|x| (0..out.height()).any(|y| pixel(&out, x, y)[0] < 64)),
        "the outside annotation should remain visible"
    );
}

#[test]
fn geometry_at_2x_is_exactly_twice_geometry_at_1x() {
    let mut doc = Document::new(capture_with(
        flat(120, 80, [255, 255, 255, 255]),
        Provenance::Region,
    ));
    doc.add(
        Annotation::Rectangle(rect(20.0, 15.0, 50.0, 30.0)),
        Style::stroked()
            .with_stroke(Color::rgb(0, 0, 0))
            .with_stroke_width(4.0),
    );

    let renderer = SkiaRenderer::new();
    let one = renderer.render_at(&doc, ScaleFactor::new(1.0)).unwrap();
    let two = renderer.render_at(&doc, ScaleFactor::new(2.0)).unwrap();

    let a = ink_bounds(&one).expect("1x drew something");
    let b = ink_bounds(&two).expect("2x drew something");

    // Every edge doubles, within a pixel of antialias slop. That is the whole
    // claim of resolution independence: stroke width scales with the geometry
    // rather than staying a fixed number of device pixels.
    for (lo, hi) in [(a.0, b.0), (a.1, b.1), (a.2, b.2), (a.3, b.3)] {
        let expected = lo * 2;
        assert!(
            hi.abs_diff(expected) <= 2,
            "expected ~{expected} at 2x, got {hi} (1x was {lo})"
        );
    }
}

#[test]
fn stroke_width_scales_with_the_export() {
    let mut doc = Document::new(capture_with(
        flat(100, 100, [255, 255, 255, 255]),
        Provenance::Region,
    ));
    doc.add(
        Annotation::Arrow {
            from: LogicalPoint::new(10.0, 50.0),
            to: LogicalPoint::new(90.0, 50.0),
        },
        Style::stroked()
            .with_stroke(Color::rgb(0, 0, 0))
            .with_stroke_width(6.0),
    );

    let renderer = SkiaRenderer::new();
    let one = renderer.render_at(&doc, ScaleFactor::new(1.0)).unwrap();
    let two = renderer.render_at(&doc, ScaleFactor::new(2.0)).unwrap();

    let thin = column_ink(&one, 30);
    let thick = column_ink(&two, 60);
    assert!(thin > 0 && thick > 0);
    assert!(
        thick >= thin * 2 - 2 && thick <= thin * 2 + 2,
        "a 6pt stroke should cover ~{} rows at 2x, covered {thick}",
        thin * 2
    );
}

#[test]
fn render_to_width_scales_to_an_arbitrary_export_size() {
    let doc = document(160, 100);
    let out = SkiaRenderer::new().render_to_width(&doc, 400).unwrap();
    assert_eq!(out.width(), 400);
    assert_eq!(out.height(), 250);

    // Down as well as up, and at a non-integer ratio.
    let small = SkiaRenderer::new().render_to_width(&doc, 60).unwrap();
    assert_eq!(small.width(), 60);
    assert!(small.height() == 37 || small.height() == 38);
}

#[test]
fn render_to_width_rejects_a_zero_width() {
    let doc = document(80, 80);
    assert!(SkiaRenderer::new().render_to_width(&doc, 0).is_err());
}

#[test]
fn a_2x_source_renders_at_its_own_scale_by_default() {
    let mut frame = flat(200, 100, [40, 60, 80, 255]);
    frame.scale = ScaleFactor::new(2.0);
    let doc = Document::new(capture_with(frame, Provenance::Region));

    // 200x100 physical at 2x is 100x50 logical; rendering at the native scale
    // must reproduce the original pixel count exactly, losing nothing.
    assert!((doc.logical_size().width - 100.0).abs() < 1e-9);
    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!(out.width(), 200);
    assert_eq!(out.height(), 100);
}

#[test]
fn annotations_land_where_they_were_authored() {
    let mut doc = Document::new(capture_with(
        flat(100, 100, [255, 255, 255, 255]),
        Provenance::Region,
    ));
    doc.add(
        Annotation::Rectangle(rect(20.0, 20.0, 40.0, 40.0)),
        Style::stroked()
            .with_stroke(Color::TRANSPARENT)
            .with_stroke_width(0.0)
            .with_fill(Some(Color::rgb(255, 0, 0))),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    assert!(
        near(pixel(&out, 40, 40), [255, 0, 0, 255], 2),
        "inside the fill"
    );
    assert!(
        near(pixel(&out, 5, 5), [255, 255, 255, 255], 2),
        "outside it"
    );
    assert!(
        near(pixel(&out, 90, 90), [255, 255, 255, 255], 2),
        "outside it"
    );
}

#[test]
fn a_highlight_darkens_rather_than_washing_out() {
    let mut doc = Document::new(capture_with(
        flat(60, 40, [255, 255, 255, 255]),
        Provenance::Region,
    ));
    doc.add(
        Annotation::Highlight(rect(10.0, 10.0, 30.0, 20.0)),
        Style::highlighter(),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    let inside = pixel(&out, 25, 20);
    let outside = pixel(&out, 2, 2);
    assert_eq!(outside, [255, 255, 255, 255]);
    assert!(
        inside != outside,
        "the highlight must actually mark the pixels it covers"
    );
    assert!(
        inside[2] < inside[0],
        "a yellow highlighter multiplies blue down: got {inside:?}"
    );
}

#[test]
fn a_highlight_leaves_dark_text_readable() {
    // The point of multiply rather than source-over: black stays black under a
    // highlighter instead of drifting towards the highlight colour.
    let mut doc = Document::new(capture_with(
        flat(40, 40, [0, 0, 0, 255]),
        Provenance::Region,
    ));
    doc.add(
        Annotation::Highlight(rect(0.0, 0.0, 40.0, 40.0)),
        Style::highlighter(),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    let p = pixel(&out, 20, 20);
    assert!(
        p[0] < 20 && p[1] < 20 && p[2] < 20,
        "black under a highlighter must stay black, got {p:?}"
    );
}

#[test]
fn beautification_is_refused_by_the_renderer_for_window_captures() {
    // Decision D9, enforced a second time. `set_beautification` already refuses,
    // so reaching this state needs a hand-built sidecar — which is exactly the
    // route a future importer or a corrupted file would take.
    let data = scrozz_annotate::DocumentData {
        beautification: Some(Beautification::padded(
            40.0,
            Background::Solid(Color::WHITE),
        )),
        ..Default::default()
    };

    // Build it against a permitted capture, then swap the source for a window.
    let mut doc = Document::from_data(region_capture(60, 60), data).unwrap();
    doc.source = window_capture(60, 60);

    let err = SkiaRenderer::new()
        .render(&doc)
        .expect_err("a window capture must never be re-framed");
    assert!(format!("{err}").to_lowercase().contains("window"), "{err}");
}

#[test]
fn a_window_capture_renders_fine_without_beautification() {
    let mut doc = Document::new(window_capture(50, 50));
    doc.add(
        Annotation::Rectangle(rect(5.0, 5.0, 20.0, 20.0)),
        Style::stroked(),
    );
    let out = SkiaRenderer::new()
        .render(&doc)
        .expect("annotations are always allowed");
    assert_eq!(out.width(), 50);
}

#[test]
fn beautification_pads_the_canvas() {
    let mut doc = Document::new(capture_with(
        flat(40, 30, [200, 30, 30, 255]),
        Provenance::Region,
    ));
    doc.set_beautification(Some(Beautification {
        padding: 25.0,
        corner_radius: 0.0,
        shadow: 0.0,
        background: Background::Solid(Color::rgb(0, 0, 255)),
    }))
    .unwrap();

    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!(out.width(), 40 + 50);
    assert_eq!(out.height(), 30 + 50);

    assert!(near(pixel(&out, 2, 2), [0, 0, 255, 255], 2), "background");
    assert!(near(pixel(&out, 45, 40), [200, 30, 30, 255], 2), "content");
}

#[test]
fn beautification_padding_scales_with_the_export() {
    let mut doc = Document::new(capture_with(
        flat(40, 40, [10, 10, 10, 255]),
        Provenance::Region,
    ));
    doc.set_beautification(Some(Beautification::padded(
        20.0,
        Background::Solid(Color::WHITE),
    )))
    .unwrap();

    let renderer = SkiaRenderer::new();
    let one = renderer.render_at(&doc, ScaleFactor::new(1.0)).unwrap();
    let two = renderer.render_at(&doc, ScaleFactor::new(2.0)).unwrap();
    assert_eq!(one.width(), 80);
    assert_eq!(
        two.width(),
        160,
        "padding is a logical measurement and must scale like everything else"
    );
}

#[test]
fn a_corner_radius_actually_rounds_the_corner() {
    let mut doc = Document::new(capture_with(
        flat(60, 60, [255, 0, 0, 255]),
        Provenance::Region,
    ));
    doc.set_beautification(Some(Beautification {
        padding: 10.0,
        corner_radius: 16.0,
        shadow: 0.0,
        background: Background::Solid(Color::rgb(0, 0, 255)),
    }))
    .unwrap();

    let out = SkiaRenderer::new().render(&doc).unwrap();
    // The content's top-left corner pixel should be background, not content,
    // because the radius has cut it away.
    let corner = pixel(&out, 11, 11);
    assert!(
        corner[2] > corner[0],
        "the rounded corner should show the background through it, got {corner:?}"
    );
    // The centre is still content.
    assert!(near(pixel(&out, 40, 40), [255, 0, 0, 255], 2));
}

#[test]
fn a_shadow_darkens_the_background_beneath_the_content() {
    let mut doc = Document::new(capture_with(
        flat(40, 40, [255, 255, 255, 255]),
        Provenance::Region,
    ));
    doc.set_beautification(Some(Beautification {
        padding: 24.0,
        corner_radius: 6.0,
        shadow: 18.0,
        background: Background::Solid(Color::WHITE),
    }))
    .unwrap();

    let out = SkiaRenderer::new().render(&doc).unwrap();
    let below = pixel(&out, 44, 68);
    let far_corner = pixel(&out, 1, 1);
    assert!(
        below[0] < far_corner[0],
        "a shadow should darken just under the image: {below:?} vs {far_corner:?}"
    );
}

#[test]
fn a_noop_beautification_changes_nothing() {
    let mut doc = Document::new(capture_with(
        flat(30, 30, [70, 80, 90, 255]),
        Provenance::Region,
    ));
    let plain = SkiaRenderer::new().render(&doc).unwrap();

    doc.set_beautification(Some(Beautification {
        padding: 0.0,
        corner_radius: 0.0,
        shadow: 0.0,
        background: Background::Transparent,
    }))
    .unwrap();
    let framed = SkiaRenderer::new().render(&doc).unwrap();

    assert_eq!(plain.data, framed.data);
}

#[test]
fn a_fully_transparent_annotation_draws_nothing() {
    let mut doc = Document::new(capture_with(
        flat(40, 40, [123, 45, 67, 255]),
        Provenance::Region,
    ));
    let plain = SkiaRenderer::new().render(&doc).unwrap();

    doc.add(
        Annotation::Rectangle(rect(5.0, 5.0, 30.0, 30.0)),
        Style::stroked().with_opacity(0.0),
    );
    let annotated = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!(plain.data, annotated.data);
}

#[test]
fn every_annotation_variant_renders_without_panicking() {
    let mut doc = document(200, 160);
    for (annotation, style) in common::every_annotation() {
        doc.add(annotation, style);
    }
    for scale in [0.5, 1.0, 2.0, 3.0] {
        let out = SkiaRenderer::new()
            .render_at(&doc, ScaleFactor::new(scale))
            .unwrap_or_else(|e| panic!("failed at {scale}x: {e}"));
        assert!(out.is_well_formed());
    }
}

#[test]
fn degenerate_annotations_are_survivable() {
    let mut doc = document(80, 80);
    doc.add(
        Annotation::Arrow {
            from: LogicalPoint::new(20.0, 20.0),
            to: LogicalPoint::new(20.0, 20.0),
        },
        Style::stroked(),
    );
    doc.add(
        Annotation::Rectangle(rect(10.0, 10.0, 0.0, 0.0)),
        Style::stroked(),
    );
    doc.add(
        Annotation::Ellipse(rect(10.0, 10.0, 0.0, 30.0)),
        Style::stroked(),
    );
    doc.add(Annotation::Freehand(vec![]), Style::stroked());
    doc.add(
        Annotation::Freehand(vec![LogicalPoint::new(5.0, 5.0)]),
        Style::stroked(),
    );
    doc.add(
        Annotation::Text {
            at: LogicalPoint::new(5.0, 40.0),
            content: String::new(),
        },
        Style::stroked(),
    );
    doc.add(
        Annotation::Redact {
            area: rect(0.0, 0.0, 0.0, 0.0),
            style: scrozz_annotate::RedactStyle::Blur,
        },
        Style::redaction(),
    );

    let out = SkiaRenderer::new()
        .render(&doc)
        .expect("no panics, no errors");
    assert!(out.is_well_formed());
}

#[test]
fn annotations_partly_outside_the_frame_are_clipped_not_fatal() {
    let mut doc = document(60, 60);
    doc.add(
        Annotation::Rectangle(rect(-100.0, -100.0, 500.0, 500.0)),
        Style::stroked(),
    );
    doc.add(
        Annotation::Redact {
            area: rect(-40.0, 40.0, 200.0, 200.0),
            style: scrozz_annotate::RedactStyle::Pixelate,
        },
        Style::redaction(),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!(out.width(), 60);
    assert!(out.is_well_formed());
}

#[test]
fn bgra_and_premultiplied_sources_are_read_correctly() {
    // BGRA is what Windows DXGI and several X11 paths hand back; misreading it
    // swaps red and blue in every export.
    let mut frame = flat(8, 8, [10, 20, 200, 255]);
    frame.format = PixelFormat::Bgra8;
    let doc = Document::new(capture_with(frame, Provenance::Region));
    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert!(
        near(pixel(&out, 4, 4), [200, 20, 10, 255], 1),
        "BGRA should surface as RGBA, got {:?}",
        pixel(&out, 4, 4)
    );

    let mut frame = flat(8, 8, [50, 50, 50, 128]);
    frame.format = PixelFormat::RgbaPremultiplied8;
    let doc = Document::new(capture_with(frame, Provenance::Region));
    let out = SkiaRenderer::new().render(&doc).unwrap();
    let p = pixel(&out, 4, 4);
    assert_eq!(p[3], 128);
    assert!(
        p[0].abs_diff(100) <= 3,
        "premultiplied 50/128 un-multiplies to ~100, got {p:?}"
    );
}

#[test]
fn a_malformed_source_is_rejected_not_read_out_of_bounds() {
    let mut frame = flat(20, 20, [0, 0, 0, 255]);
    frame.data.truncate(10);
    let doc = Document::new(capture_with(frame, Provenance::Region));
    assert!(SkiaRenderer::new().render(&doc).is_err());
}

/// The bounding box of everything that is not the white background.
fn ink_bounds(frame: &Frame) -> Option<(u32, u32, u32, u32)> {
    let (mut l, mut t, mut r, mut b) = (u32::MAX, u32::MAX, 0u32, 0u32);
    let mut found = false;
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            let p = pixel(frame, x, y);
            if p[0] < 200 || p[1] < 200 || p[2] < 200 {
                found = true;
                l = l.min(x);
                t = t.min(y);
                r = r.max(x);
                b = b.max(y);
            }
        }
    }
    found.then_some((l, t, r, b))
}

/// How many rows in one column are inked.
fn column_ink(frame: &Frame, x: u32) -> u32 {
    (0..frame.height())
        .filter(|&y| {
            let p = pixel(frame, x, y);
            p[0] < 128 && p[1] < 128 && p[2] < 128
        })
        .count() as u32
}
