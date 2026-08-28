//! What the editor's preview shows, against what the exporter writes.
//!
//! The preview and the export come from the same renderer, so they ought to
//! agree. Two things can silently break that, and both are invisible in a
//! screenshot of a screenshot tool:
//!
//! * **Alpha.** tiny-skia composites premultiplied. Treating those bytes as
//!   straight alpha scales them a second time, so a translucent highlight
//!   previews darker than it exports.
//! * **Gamut.** The renderer works in the capture's colour space. egui shows
//!   texture bytes as sRGB, so Display P3 pixels handed over unchanged preview
//!   over-saturated while the exported file — which carries a P3 profile — is
//!   right.
//!
//! These tests compare the two paths directly rather than eyeballing either.

use scrozz_annotate::{Annotation, Color, Document, Renderer, SkiaRenderer, Style};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Provenance, ScaleFactor, Transform,
};
use scrozz_ui::editor::to_color_image;

/// A flat capture in `space`, filled with mid grey.
fn capture(space: ColorSpace) -> Capture {
    Capture {
        frame: Frame {
            data: vec![128u8; 80 * 60 * 4],
            size: PhysicalSize::new(80.0, 60.0),
            stride: 80 * 4,
            format: PixelFormat::Rgba8,
            color_space: space,
            scale: ScaleFactor::new(1.0),
        },
        provenance: Provenance::Region,
        target: CaptureTarget::Region(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(80.0, 60.0),
        )),
    }
}

/// A document with one half-transparent red rectangle over the whole frame.
fn translucent(space: ColorSpace) -> Document {
    let mut document = Document::new(capture(space));
    let mut style = Style::stroked();
    style.stroke = Color::rgba(0, 0, 0, 0);
    style.fill = Some(Color::rgba(255, 0, 0, 128));
    document.add(
        Annotation::Rectangle(LogicalRect::new(
            LogicalPoint::new(10.0, 10.0),
            LogicalSize::new(40.0, 30.0),
        )),
        style,
    );
    document
}

/// The pixel at `(x, y)` of a rendered frame, as raw bytes.
fn pixel(frame: &Frame, x: usize, y: usize) -> [u8; 4] {
    let row = &frame.data[y * frame.stride..];
    let p = &row[x * 4..x * 4 + 4];
    [p[0], p[1], p[2], p[3]]
}

#[test]
fn the_renderer_really_does_emit_premultiplied_pixels() {
    // The premise everything below rests on. If tiny-skia ever changes, this
    // fails first and points at the reason.
    let frame = SkiaRenderer
        .render(&translucent(ColorSpace::Srgb))
        .expect("render");
    assert!(
        frame.format.is_premultiplied(),
        "format is {:?}",
        frame.format
    );
}

#[test]
fn a_translucent_annotation_previews_as_it_exports() {
    let document = translucent(ColorSpace::Srgb);
    let exported = SkiaRenderer.render(&document).expect("render");
    let preview = to_color_image(&exported);

    // Inside the rectangle: 50% red composited over mid grey.
    let inside = pixel(&exported, 20, 20);
    let shown = preview.pixels[20 * preview.size[0] + 20];

    assert_eq!(
        [shown.r(), shown.g(), shown.b(), shown.a()],
        inside,
        "the preview re-scaled channels the renderer had already scaled"
    );
}

#[test]
fn treating_premultiplied_pixels_as_straight_would_darken_them() {
    // Guards the test above from passing vacuously: the two interpretations must
    // actually differ for this pixel, or it proves nothing.
    let exported = SkiaRenderer
        .render(&translucent(ColorSpace::Srgb))
        .expect("render");
    let raw = pixel(&exported, 20, 20);
    assert!(raw[3] < 255, "pick a pixel that is actually translucent");
    let wrong = egui::Color32::from_rgba_unmultiplied(raw[0], raw[1], raw[2], raw[3]);
    assert_ne!(
        [wrong.r(), wrong.g(), wrong.b()],
        [raw[0], raw[1], raw[2]],
        "the two interpretations coincide here, so this fixture cannot detect the bug"
    );
}

#[test]
fn a_fully_opaque_pixel_is_identical_either_way() {
    let mut document = Document::new(capture(ColorSpace::Srgb));
    let mut style = Style::stroked();
    style.stroke = Color::rgba(0, 0, 0, 0);
    style.fill = Some(Color::rgba(20, 200, 90, 255));
    document.add(
        Annotation::Rectangle(LogicalRect::new(
            LogicalPoint::new(5.0, 5.0),
            LogicalSize::new(50.0, 40.0),
        )),
        style,
    );
    let exported = SkiaRenderer.render(&document).expect("render");
    let preview = to_color_image(&exported);
    let shown = preview.pixels[20 * preview.size[0] + 20];
    assert_eq!([shown.r(), shown.g(), shown.b()], [20, 200, 90]);
}

#[test]
fn a_display_p3_capture_keeps_its_colour_space_through_export() {
    let exported = SkiaRenderer
        .render(&translucent(ColorSpace::DisplayP3))
        .expect("render");
    assert_eq!(
        exported.color_space,
        ColorSpace::DisplayP3,
        "the export lost the capture's colour space, so its profile would be wrong"
    );
}

#[test]
fn an_srgb_swatch_is_converted_before_it_is_painted_into_a_p3_capture() {
    // The same annotation colour, composited into two captures that differ only
    // in colour space. If the swatch were painted through unconverted the bytes
    // would be identical, and the P3 export would show a more saturated red than
    // the user picked.
    let srgb = SkiaRenderer
        .render(&translucent(ColorSpace::Srgb))
        .expect("render");
    let p3 = SkiaRenderer
        .render(&translucent(ColorSpace::DisplayP3))
        .expect("render");

    assert_ne!(
        pixel(&srgb, 20, 20),
        pixel(&p3, 20, 20),
        "the sRGB swatch went into the P3 buffer unconverted"
    );
}

#[test]
fn the_converted_swatch_is_the_one_the_colour_maths_predicts() {
    // Not merely different — different by the right amount. A fully opaque fill
    // so nothing but the conversion is in play.
    let mut document = Document::new(capture(ColorSpace::DisplayP3));
    let mut style = Style::stroked();
    style.stroke = Color::rgba(0, 0, 0, 0);
    style.fill = Some(Color::rgba(255, 0, 0, 255));
    document.add(
        Annotation::Rectangle(LogicalRect::new(
            LogicalPoint::new(5.0, 5.0),
            LogicalSize::new(50.0, 40.0),
        )),
        style,
    );
    let rendered = SkiaRenderer.render(&document).expect("render");
    let got = pixel(&rendered, 20, 20);

    let want = Transform::new(ColorSpace::Srgb, ColorSpace::DisplayP3).convert_u8([255, 0, 0]);
    for (channel, (a, b)) in got.iter().zip(want.iter()).enumerate() {
        assert!(
            a.abs_diff(*b) <= 1,
            "channel {channel}: painted {got:?}, expected {want:?}"
        );
    }
}

#[test]
fn a_p3_preview_is_converted_back_for_the_screen() {
    // The preview has to undo the working space, or P3 bytes would be shown as
    // sRGB and everything would look over-saturated.
    let mut document = Document::new(capture(ColorSpace::DisplayP3));
    let mut style = Style::stroked();
    style.stroke = Color::rgba(0, 0, 0, 0);
    style.fill = Some(Color::rgba(255, 0, 0, 255));
    document.add(
        Annotation::Rectangle(LogicalRect::new(
            LogicalPoint::new(5.0, 5.0),
            LogicalSize::new(50.0, 40.0),
        )),
        style,
    );
    let rendered = SkiaRenderer.render(&document).expect("render");
    let preview = to_color_image(&rendered);
    let shown = preview.pixels[20 * preview.size[0] + 20];

    // Round trip: sRGB swatch → P3 buffer → sRGB screen ≈ the original swatch.
    assert!(
        shown.r() > 245 && shown.g() < 12 && shown.b() < 12,
        "a red swatch previewed as {:?} instead of coming back to sRGB red",
        [shown.r(), shown.g(), shown.b()]
    );
}

#[test]
fn an_srgb_preview_is_left_exactly_alone() {
    // The overwhelmingly common case must cost nothing and change nothing.
    let mut document = Document::new(capture(ColorSpace::Srgb));
    let mut style = Style::stroked();
    style.stroke = Color::rgba(0, 0, 0, 0);
    style.fill = Some(Color::rgba(17, 133, 91, 255));
    document.add(
        Annotation::Rectangle(LogicalRect::new(
            LogicalPoint::new(5.0, 5.0),
            LogicalSize::new(50.0, 40.0),
        )),
        style,
    );
    let rendered = SkiaRenderer.render(&document).expect("render");
    let preview = to_color_image(&rendered);
    let shown = preview.pixels[20 * preview.size[0] + 20];
    assert_eq!([shown.r(), shown.g(), shown.b()], [17, 133, 91]);
}

#[test]
fn an_unknown_colour_space_is_shown_verbatim() {
    // Nothing is known about the source, so inventing a conversion would shift
    // colours on a guess. Bytes through unchanged is the only honest option.
    let rendered = SkiaRenderer
        .render(&translucent(ColorSpace::Unknown))
        .expect("render");
    let preview = to_color_image(&rendered);
    let shown = preview.pixels[20 * preview.size[0] + 20];
    assert_eq!(
        [shown.r(), shown.g(), shown.b(), shown.a()],
        pixel(&rendered, 20, 20)
    );
}

#[test]
fn a_translucent_p3_pixel_survives_the_preview_round_trip() {
    // Both corrections at once: premultiplied *and* wide gamut. Undoing the
    // premultiplication for the conversion and reapplying it afterwards has to
    // leave alpha untouched and the colour recognisable.
    let rendered = SkiaRenderer
        .render(&translucent(ColorSpace::DisplayP3))
        .expect("render");
    let raw = pixel(&rendered, 20, 20);
    let preview = to_color_image(&rendered);
    let shown = preview.pixels[20 * preview.size[0] + 20];

    assert_eq!(shown.a(), raw[3], "alpha was altered by the conversion");
    assert!(
        shown.r() >= raw[0],
        "converting back to sRGB should not have reduced the red: {:?} from {raw:?}",
        [shown.r(), shown.g(), shown.b()]
    );
}
