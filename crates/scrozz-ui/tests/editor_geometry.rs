//! Canvas geometry and full-resolution rendering.
//!
//! The editor's canvas maps between three coordinate spaces: document points
//! (what an annotation stores), screen points (where egui draws), and the
//! source pixels the renderer finally writes. A mistake anywhere in that chain
//! puts an arrow somewhere other than where the user drew it, and only shows up
//! at export — long after the preview said it was fine.

use scrozz_annotate::{Annotation, Color, Document, RedactStyle, Style};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Provenance, ScaleFactor,
};
use scrozz_ui::editor::{Command, EditorUi, Tool, fit, rect_to_screen, to_document, to_screen};

const W: u32 = 800;
const H: u32 = 600;

/// A 400x300 logical capture at 2x, with a recognisable non-uniform pattern so
/// a render that silently returns the wrong region is detectable.
fn capture() -> Capture {
    let stride = W as usize * 4;
    let mut data = vec![0u8; stride * H as usize];
    for y in 0..H as usize {
        for x in 0..W as usize {
            let p = y * stride + x * 4;
            data[p] = u8::try_from(x % 256).unwrap_or(0);
            data[p + 1] = u8::try_from(y % 256).unwrap_or(0);
            data[p + 2] = 128;
            data[p + 3] = 255;
        }
    }
    Capture {
        frame: Frame {
            data,
            size: PhysicalSize::new(f64::from(W), f64::from(H)),
            stride,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::new(2.0),
        },
        provenance: Provenance::Region,
        target: CaptureTarget::Region(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(400.0, 300.0),
        )),
    }
}

fn content() -> LogicalRect {
    LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(400.0, 300.0))
}

fn area(w: f32, h: f32) -> egui::Rect {
    egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h))
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> LogicalRect {
    LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(w, h))
}

// ---------------------------------------------------------------------------
// fit
// ---------------------------------------------------------------------------

#[test]
fn a_capture_smaller_than_the_window_is_not_blown_up() {
    let canvas = fit(content(), area(2000.0, 1600.0), 1.0, (0.0, 0.0));
    assert!(
        (canvas.width() - 400.0).abs() < 0.01,
        "a 400pt capture was scaled to {}",
        canvas.width()
    );
    assert!((canvas.height() - 300.0).abs() < 0.01);
}

#[test]
fn a_capture_larger_than_the_window_is_shrunk_to_fit() {
    let canvas = fit(content(), area(200.0, 200.0), 1.0, (0.0, 0.0));
    assert!(canvas.width() <= 200.01, "{}", canvas.width());
    assert!(canvas.height() <= 200.01, "{}", canvas.height());
}

#[test]
fn fitting_preserves_the_aspect_ratio() {
    for size in [(200.0, 200.0), (150.0, 900.0), (1200.0, 180.0)] {
        let canvas = fit(content(), area(size.0, size.1), 1.0, (0.0, 0.0));
        let ratio = canvas.width() / canvas.height();
        assert!(
            (ratio - 400.0 / 300.0).abs() < 0.01,
            "{size:?} distorted the image to {ratio}"
        );
    }
}

#[test]
fn the_canvas_is_centred_in_its_area() {
    let area = area(2000.0, 1600.0);
    let canvas = fit(content(), area, 1.0, (0.0, 0.0));
    assert!((canvas.center().x - area.center().x).abs() < 0.01);
    assert!((canvas.center().y - area.center().y).abs() < 0.01);
}

#[test]
fn zoom_scales_the_canvas_about_its_centre() {
    let area = area(2000.0, 1600.0);
    let one = fit(content(), area, 1.0, (0.0, 0.0));
    let two = fit(content(), area, 2.0, (0.0, 0.0));

    assert!((two.width() - one.width() * 2.0).abs() < 0.01);
    assert!(
        (two.center().x - one.center().x).abs() < 0.01,
        "zooming drifted sideways"
    );
}

#[test]
fn panning_translates_the_canvas_without_resizing_it() {
    let area = area(2000.0, 1600.0);
    let still = fit(content(), area, 1.0, (0.0, 0.0));
    let moved = fit(content(), area, 1.0, (60.0, -25.0));

    assert!((moved.center().x - still.center().x - 60.0).abs() < 0.01);
    assert!((moved.center().y - still.center().y + 25.0).abs() < 0.01);
    assert!((moved.width() - still.width()).abs() < 0.01);
}

#[test]
fn a_cropped_document_fits_by_its_crop_not_its_source() {
    let cropped = fit(
        rect(0.0, 0.0, 100.0, 300.0),
        area(2000.0, 1600.0),
        1.0,
        (0.0, 0.0),
    );
    let ratio = cropped.width() / cropped.height();
    assert!((ratio - 1.0 / 3.0).abs() < 0.01, "{ratio}");
}

// ---------------------------------------------------------------------------
// to_document / to_screen
// ---------------------------------------------------------------------------

#[test]
fn screen_and_document_coordinates_round_trip() {
    let canvas = fit(content(), area(900.0, 700.0), 1.0, (0.0, 0.0));
    for point in [
        LogicalPoint::new(0.0, 0.0),
        LogicalPoint::new(400.0, 300.0),
        LogicalPoint::new(137.5, 92.25),
    ] {
        let back = to_document(to_screen(point, canvas, content()), canvas, content());
        assert!(
            (back.x - point.x).abs() < 0.01 && (back.y - point.y).abs() < 0.01,
            "{point:?} came back as {back:?}"
        );
    }
}

#[test]
fn the_document_origin_lands_on_the_canvas_corner() {
    let canvas = fit(content(), area(900.0, 700.0), 1.0, (0.0, 0.0));
    let origin = to_screen(LogicalPoint::new(0.0, 0.0), canvas, content());
    assert!((origin.x - canvas.min.x).abs() < 0.01);
    assert!((origin.y - canvas.min.y).abs() < 0.01);
}

#[test]
fn the_round_trip_survives_zoom_and_pan() {
    let canvas = fit(content(), area(900.0, 700.0), 2.5, (80.0, -40.0));
    let point = LogicalPoint::new(210.0, 155.0);
    let back = to_document(to_screen(point, canvas, content()), canvas, content());
    assert!((back.x - point.x).abs() < 0.01, "{back:?}");
    assert!((back.y - point.y).abs() < 0.01, "{back:?}");
}

#[test]
fn a_crop_shifts_the_mapping_by_its_own_origin() {
    let crop = rect(100.0, 60.0, 200.0, 150.0);
    let canvas = fit(crop, area(900.0, 700.0), 1.0, (0.0, 0.0));
    // The crop's top-left is what sits at the canvas corner, so a click there
    // is that point in *source* coordinates, not the origin.
    let corner = to_document(canvas.min, canvas, crop);
    assert!((corner.x - 100.0).abs() < 0.01, "{corner:?}");
    assert!((corner.y - 60.0).abs() < 0.01, "{corner:?}");
}

#[test]
fn a_click_outside_the_canvas_maps_outside_the_document() {
    let canvas = fit(content(), area(900.0, 700.0), 1.0, (0.0, 0.0));
    let above = to_document(
        egui::pos2(canvas.min.x - 30.0, canvas.min.y - 30.0),
        canvas,
        content(),
    );
    assert!(above.x < 0.0 && above.y < 0.0, "{above:?}");
}

#[test]
fn rects_map_to_screen_as_their_corners_do() {
    let canvas = fit(content(), area(900.0, 700.0), 1.4, (12.0, 7.0));
    let source = rect(50.0, 40.0, 120.0, 90.0);
    let mapped = rect_to_screen(source, canvas, content());

    let min = to_screen(LogicalPoint::new(50.0, 40.0), canvas, content());
    let max = to_screen(LogicalPoint::new(170.0, 130.0), canvas, content());
    assert!((mapped.min.x - min.x).abs() < 0.01);
    assert!((mapped.min.y - min.y).abs() < 0.01);
    assert!((mapped.max.x - max.x).abs() < 0.01);
    assert!((mapped.max.y - max.y).abs() < 0.01);
}

#[test]
fn a_zero_area_canvas_does_not_produce_a_nan() {
    let empty = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(0.0, 0.0));
    let point = to_document(egui::pos2(10.0, 10.0), empty, content());
    assert!(point.x.is_finite() && point.y.is_finite(), "{point:?}");
}

// ---------------------------------------------------------------------------
// Rendering and export
// ---------------------------------------------------------------------------

fn editor() -> EditorUi {
    EditorUi::new(Document::new(capture()))
}

#[test]
fn an_untouched_capture_renders_at_its_own_size() {
    let rendered = editor().render().expect("render");
    assert_eq!(rendered.width(), W);
    assert_eq!(rendered.height(), H);
}

#[test]
fn the_render_keeps_the_captures_scale_factor() {
    let rendered = editor().render().expect("render");
    assert!((rendered.scale.get() - 2.0).abs() < 0.001);
}

#[test]
fn the_render_is_full_resolution_not_the_preview_size() {
    // The preview texture is capped, so a render that went through it would
    // come back smaller. Squeeze the editor into a tiny canvas first.
    let mut editor = editor();
    editor.state_mut().set_zoom(0.1);
    let rendered = editor.render().expect("render");
    assert_eq!(
        rendered.width(),
        W,
        "the export followed the on-screen zoom instead of the capture"
    );
}

#[test]
fn annotations_change_the_rendered_pixels() {
    let plain = editor().render().expect("render");

    let mut editor = editor();
    editor.state_mut().set_stroke_color(Color::rgb(255, 0, 0));
    editor.state_mut().set_tool(Tool::Rectangle);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(40.0, 40.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(200.0, 180.0), false);
    editor.state_mut().pointer_released();

    let drawn = editor.render().expect("render");
    assert_eq!(drawn.data.len(), plain.data.len());
    assert_ne!(drawn.data, plain.data, "the rectangle did not reach export");
}

#[test]
fn rendering_twice_gives_the_same_bytes() {
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Arrow);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(20.0, 20.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(180.0, 140.0), false);
    editor.state_mut().pointer_released();

    assert_eq!(
        editor.render().expect("render").data,
        editor.render().expect("render").data,
        "export is not deterministic"
    );
}

#[test]
fn rendering_does_not_disturb_the_document() {
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Ellipse);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(30.0, 30.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(150.0, 120.0), false);
    editor.state_mut().pointer_released();

    let before = editor.document().clone();
    let revision = editor.state().revision();
    let _ = editor.render().expect("render");

    assert_eq!(
        editor.document().annotations().len(),
        before.annotations().len()
    );
    assert_eq!(editor.document().crop(), before.crop());
    assert_eq!(editor.state().revision(), revision);
}

#[test]
fn a_crop_shrinks_the_exported_image() {
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Crop);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(50.0, 40.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(250.0, 190.0), false);
    editor.state_mut().pointer_released();
    editor
        .state_mut()
        .command(Command::ApplyCrop)
        .expect("crop");

    let rendered = editor.render().expect("render");
    // 200 x 150 logical at 2x.
    assert_eq!(rendered.width(), 400);
    assert_eq!(rendered.height(), 300);
}

#[test]
fn an_undone_crop_exports_the_whole_image_again() {
    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Crop);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(50.0, 40.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(250.0, 190.0), false);
    editor.state_mut().pointer_released();
    editor
        .state_mut()
        .command(Command::ApplyCrop)
        .expect("crop");
    editor.state_mut().command(Command::Undo).expect("undo");

    let rendered = editor.render().expect("render");
    assert_eq!(rendered.width(), W);
    assert_eq!(rendered.height(), H);
}

#[test]
fn a_redaction_destroys_the_pixels_it_covers() {
    let plain = editor().render().expect("render");

    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Pixelate);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(60.0, 60.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(180.0, 160.0), false);
    editor.state_mut().pointer_released();
    let redacted = editor.render().expect("render");

    // Sample well inside the redaction, in physical pixels.
    let stride = plain.stride;
    let sample = |frame: &Frame, x: usize, y: usize| {
        let p = y * stride + x * 4;
        [frame.data[p], frame.data[p + 1], frame.data[p + 2]]
    };
    assert_ne!(
        sample(&plain, 240, 220),
        sample(&redacted, 240, 220),
        "the redaction left the original pixels intact"
    );
}

#[test]
fn a_redaction_leaves_the_rest_of_the_image_alone() {
    let plain = editor().render().expect("render");

    let mut editor = editor();
    editor.state_mut().set_tool(Tool::Blur);
    editor
        .state_mut()
        .pointer_pressed(LogicalPoint::new(20.0, 20.0));
    editor
        .state_mut()
        .pointer_dragged(LogicalPoint::new(80.0, 70.0), false);
    editor.state_mut().pointer_released();
    let redacted = editor.render().expect("render");

    let stride = plain.stride;
    // Bottom-right corner, far from the redaction at the top-left.
    let p = (H as usize - 8) * stride + (W as usize - 8) * 4;
    assert_eq!(
        &plain.data[p..p + 3],
        &redacted.data[p..p + 3],
        "the redaction bled across the whole image"
    );
}

#[test]
fn every_tool_produces_something_exportable() {
    for tool in Tool::ALL {
        let mut editor = editor();
        editor.state_mut().set_tool(tool);
        editor
            .state_mut()
            .pointer_pressed(LogicalPoint::new(40.0, 40.0));
        editor
            .state_mut()
            .pointer_dragged(LogicalPoint::new(200.0, 170.0), false);
        editor.state_mut().pointer_released();
        if tool == Tool::Text {
            editor.state_mut().set_text_buffer("Label");
            editor.state_mut().finish_text();
        }

        let rendered = editor
            .render()
            .unwrap_or_else(|error| panic!("{tool:?} could not be exported: {error}"));
        assert!(rendered.width() > 0 && rendered.height() > 0, "{tool:?}");
    }
}

#[test]
fn a_document_carrying_every_annotation_kind_exports() {
    let mut document = Document::new(capture());
    let style = Style::stroked();
    document.add(
        Annotation::Arrow {
            from: LogicalPoint::new(20.0, 20.0),
            to: LogicalPoint::new(120.0, 90.0),
        },
        style,
    );
    document.add(
        Annotation::Line {
            from: LogicalPoint::new(20.0, 120.0),
            to: LogicalPoint::new(140.0, 120.0),
        },
        style,
    );
    document.add(Annotation::Rectangle(rect(160.0, 20.0, 90.0, 70.0)), style);
    document.add(Annotation::Ellipse(rect(160.0, 110.0, 90.0, 70.0)), style);
    document.add(
        Annotation::Freehand(vec![
            LogicalPoint::new(30.0, 200.0),
            LogicalPoint::new(60.0, 230.0),
            LogicalPoint::new(90.0, 195.0),
        ]),
        style,
    );
    document.add(
        Annotation::Text {
            at: LogicalPoint::new(120.0, 250.0),
            content: "Everything".into(),
        },
        Style::stroked(),
    );
    document.add(
        Annotation::Counter {
            at: LogicalPoint::new(300.0, 40.0),
            index: 1,
        },
        Style::stroked(),
    );
    document.add(
        Annotation::Highlight(rect(270.0, 90.0, 100.0, 30.0)),
        Style::highlighter(),
    );
    document.add(
        Annotation::Redact {
            area: rect(270.0, 140.0, 100.0, 60.0),
            style: RedactStyle::Pixelate,
        },
        Style::redaction(),
    );
    document.add(
        Annotation::Redact {
            area: rect(270.0, 210.0, 100.0, 60.0),
            style: RedactStyle::Blur,
        },
        Style::redaction(),
    );

    let rendered = EditorUi::new(document).render().expect("render");
    assert_eq!(rendered.width(), W);
    assert_eq!(rendered.height(), H);
}
