//! Drawing behaviour of the individual annotation kinds.

mod common;

use common::{capture_with, flat, near, pixel, rect};
use scrozz_annotate::{Annotation, Color, Document, Renderer, SkiaRenderer, Style, font};
use scrozz_core::{Frame, LogicalPoint, Provenance, ScaleFactor};

/// A white canvas to draw black ink onto.
fn white(width: u32, height: u32) -> Document {
    Document::new(capture_with(
        flat(width, height, [255, 255, 255, 255]),
        Provenance::Region,
    ))
}

fn ink(style: Style) -> Style {
    style.with_stroke(Color::rgb(0, 0, 0))
}

/// How many pixels are inked.
fn ink_count(frame: &Frame) -> u32 {
    let mut n = 0;
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            let p = pixel(frame, x, y);
            if p[0] < 128 {
                n += 1;
            }
        }
    }
    n
}

/// The bounding box of everything inked.
fn ink_bounds(frame: &Frame) -> Option<(u32, u32, u32, u32)> {
    let (mut l, mut t, mut r, mut b) = (u32::MAX, u32::MAX, 0u32, 0u32);
    let mut found = false;
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            if pixel(frame, x, y)[0] < 200 {
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

#[test]
fn an_arrow_has_a_head_that_is_wider_than_its_shaft() {
    let mut doc = white(120, 60);
    doc.add(
        Annotation::Arrow {
            from: LogicalPoint::new(10.0, 30.0),
            to: LogicalPoint::new(100.0, 30.0),
        },
        ink(Style::stroked().with_stroke_width(4.0)),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    let at_tail = column_thickness(&out, 20);
    let near_head = column_thickness(&out, 90);
    assert!(at_tail > 0, "the shaft should be drawn");
    assert!(
        near_head > at_tail,
        "the arrowhead should flare out: {near_head} vs shaft {at_tail}"
    );
}

#[test]
fn the_arrowhead_scales_with_stroke_width() {
    // A fixed-size head on a thick arrow looks like a pin; a fixed-size head on
    // a hairline looks like a blob. Both are the same bug.
    let measure = |width: f64| {
        let mut doc = white(200, 100);
        doc.add(
            Annotation::Arrow {
                from: LogicalPoint::new(20.0, 50.0),
                to: LogicalPoint::new(170.0, 50.0),
            },
            ink(Style::stroked().with_stroke_width(width)),
        );
        let out = SkiaRenderer::new().render(&doc).unwrap();
        column_thickness(&out, 160)
    };

    let thin = measure(2.0);
    let thick = measure(8.0);
    assert!(thin > 0 && thick > 0);
    assert!(
        thick >= thin * 3,
        "a 4x thicker stroke should give a markedly bigger head: {thick} vs {thin}"
    );
}

#[test]
fn an_arrow_points_at_its_destination() {
    let mut doc = white(100, 100);
    doc.add(
        Annotation::Arrow {
            from: LogicalPoint::new(10.0, 10.0),
            to: LogicalPoint::new(80.0, 80.0),
        },
        ink(Style::stroked().with_stroke_width(3.0)),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    let (l, t, r, b) = ink_bounds(&out).expect("something was drawn");

    assert!(l <= 12 && t <= 12, "the tail starts near (10,10): {l},{t}");
    assert!(r >= 78 && b >= 78, "the head reaches (80,80): {r},{b}");
}

#[test]
fn a_zero_length_arrow_draws_nothing_rather_than_a_singularity() {
    let mut doc = white(60, 60);
    let plain = SkiaRenderer::new().render(&doc).unwrap();
    doc.add(
        Annotation::Arrow {
            from: LogicalPoint::new(30.0, 30.0),
            to: LogicalPoint::new(30.0, 30.0),
        },
        ink(Style::stroked()),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!(plain.data, out.data);
}

#[test]
fn a_rectangle_can_be_stroked_filled_or_both() {
    let mut doc = white(80, 80);
    doc.add(
        Annotation::Rectangle(rect(20.0, 20.0, 40.0, 40.0)),
        ink(Style::stroked().with_stroke_width(2.0)),
    );
    let stroked = SkiaRenderer::new().render(&doc).unwrap();
    assert!(
        near(pixel(&stroked, 40, 40), [255, 255, 255, 255], 2),
        "hollow"
    );
    assert!(pixel(&stroked, 40, 20)[0] < 128, "the top edge is drawn");

    let mut doc = white(80, 80);
    doc.add(
        Annotation::Rectangle(rect(20.0, 20.0, 40.0, 40.0)),
        ink(Style::stroked()
            .with_stroke_width(2.0)
            .with_fill(Some(Color::rgb(0, 200, 0)))),
    );
    let filled = SkiaRenderer::new().render(&doc).unwrap();
    assert!(near(pixel(&filled, 40, 40), [0, 200, 0, 255], 2), "filled");
    assert!(
        pixel(&filled, 40, 20)[1] < 200,
        "still stroked on top of the fill"
    );
}

#[test]
fn an_ellipse_is_round_not_rectangular() {
    let mut doc = white(100, 100);
    doc.add(
        Annotation::Ellipse(rect(10.0, 10.0, 80.0, 80.0)),
        ink(Style::stroked()
            .with_stroke_width(1.0)
            .with_fill(Some(Color::rgb(0, 0, 0)))),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    assert!(pixel(&out, 50, 50)[0] < 128, "the centre is filled");
    assert!(
        near(pixel(&out, 12, 12), [255, 255, 255, 255], 8),
        "the bounding-box corner is outside the ellipse: {:?}",
        pixel(&out, 12, 12)
    );
    assert!(pixel(&out, 50, 12)[0] < 200, "the top of the arc is inked");
}

#[test]
fn freehand_is_smoothed_rather_than_drawn_as_raw_segments() {
    // A three-point right angle drawn as raw segments has a hard mitred corner.
    // A smoothed stroke rounds it off, so the corner vertex itself is left blank
    // while the ink cuts across the inside of the turn.
    let corner = LogicalPoint::new(60.0, 20.0);
    let points = vec![
        LogicalPoint::new(20.0, 20.0),
        corner,
        LogicalPoint::new(60.0, 60.0),
    ];

    let mut doc = white(100, 100);
    doc.add(
        Annotation::Freehand(points.clone()),
        ink(Style::stroked().with_stroke_width(2.0)),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    // The corner is rounded away rather than drawn as a spike.
    assert!(
        near(pixel(&out, 60, 20), [255, 255, 255, 255], 8),
        "the smoothed curve should round the corner off, not mitre through it: {:?}",
        pixel(&out, 60, 20)
    );
    // Ink appears across the inside of the turn, where a polyline leaves a gap.
    let inside_turn = (52..60).any(|x| (22..30).any(|y| pixel(&out, x, y)[0] < 200));
    assert!(inside_turn, "the rounded corner should cut across the turn");

    // Smoothing must never send ink outside the region the user actually drew.
    // Every control point of the curve is either a sample or the midpoint of two
    // samples, so the whole stroke is confined to the samples' convex hull; only
    // the stroke's own half-width and antialiasing may spill past it.
    let slack = 3;
    for y in 0..100u32 {
        for x in 0..100u32 {
            if pixel(&out, x, y)[0] >= 200 {
                continue;
            }
            assert!(
                x + slack >= 20 && x <= 60 + slack && y + slack >= 20 && y <= 60 + slack,
                "smoothing overshot the drawn area at ({x},{y})"
            );
        }
    }

    // The endpoints are exactly where the pen went down and lifted.
    for p in [points[0], points[2]] {
        let (x, y) = (p.x as u32, p.y as u32);
        let hit = (x.saturating_sub(2)..=x + 2)
            .any(|px| (y.saturating_sub(2)..=y + 2).any(|py| pixel(&out, px, py)[0] < 200));
        assert!(
            hit,
            "the stroke must start and end on its captured point ({x},{y})"
        );
    }
}

#[test]
fn freehand_with_two_points_is_a_straight_line() {
    let mut doc = white(80, 80);
    doc.add(
        Annotation::Freehand(vec![
            LogicalPoint::new(10.0, 40.0),
            LogicalPoint::new(70.0, 40.0),
        ]),
        ink(Style::stroked().with_stroke_width(2.0)),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    let (l, t, r, b) = ink_bounds(&out).unwrap();
    assert!(l <= 11 && r >= 69);
    assert!(b - t <= 4, "a straight line should not bulge: {t}..{b}");
}

#[test]
fn freehand_ignores_duplicate_samples() {
    // A stationary pointer emits the same coordinate repeatedly; feeding those
    // into a spline produces zero-length tangents and NaN control points.
    let mut doc = white(60, 60);
    doc.add(
        Annotation::Freehand(vec![
            LogicalPoint::new(10.0, 30.0),
            LogicalPoint::new(10.0, 30.0),
            LogicalPoint::new(10.0, 30.0),
            LogicalPoint::new(50.0, 30.0),
            LogicalPoint::new(50.0, 30.0),
        ]),
        ink(Style::stroked().with_stroke_width(2.0)),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert!(ink_count(&out) > 0, "the stroke should still be drawn");
    let (l, _, r, _) = ink_bounds(&out).unwrap();
    assert!(l <= 11 && r >= 49);
}

#[test]
fn text_draws_visible_glyphs() {
    let mut doc = white(200, 60);
    doc.add(
        Annotation::Text {
            at: LogicalPoint::new(10.0, 10.0),
            content: "SCROZZ 123".to_owned(),
        },
        ink(Style::stroked().with_font_size(24.0)),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert!(
        ink_count(&out) > 50,
        "the label should actually be legible ink"
    );

    let (l, t, r, b) = ink_bounds(&out).unwrap();
    assert!(l >= 9, "the text starts at its anchor, not before it");
    assert!(t >= 9);
    assert!(r > l + 60, "ten characters should be reasonably wide");
    assert!(b > t + 8);
}

#[test]
fn text_measurement_matches_what_is_drawn() {
    let content = "MEASURE ME";
    let size = 20.0;
    let measured = font::measure(content, size);

    let mut doc = white(400, 120);
    doc.add(
        Annotation::Text {
            at: LogicalPoint::new(20.0, 20.0),
            content: content.to_owned(),
        },
        ink(Style::stroked().with_font_size(size)),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    let (l, t, r, b) = ink_bounds(&out).unwrap();

    let drawn_w = f64::from(r - l);
    let drawn_h = f64::from(b - t);
    assert!(
        drawn_w <= measured.width + 2.0,
        "drawn {drawn_w} exceeds measured {}",
        measured.width
    );
    assert!(
        drawn_h <= measured.height + 2.0,
        "drawn {drawn_h} exceeds measured {}",
        measured.height
    );
    // Layout that claims far more room than the ink needs is just as wrong.
    assert!(drawn_w > measured.width * 0.5);
}

#[test]
fn text_scales_with_font_size() {
    let measure_at = |size: f64| {
        let mut doc = white(400, 200);
        doc.add(
            Annotation::Text {
                at: LogicalPoint::new(10.0, 10.0),
                content: "AB".to_owned(),
            },
            ink(Style::stroked().with_font_size(size)),
        );
        let out = SkiaRenderer::new().render(&doc).unwrap();
        let (l, t, r, b) = ink_bounds(&out).unwrap();
        (r - l, b - t)
    };
    let (w1, h1) = measure_at(20.0);
    let (w2, h2) = measure_at(40.0);
    assert!(w2 >= w1 * 2 - 3 && w2 <= w1 * 2 + 3, "{w1} -> {w2}");
    assert!(h2 >= h1 * 2 - 3 && h2 <= h1 * 2 + 3, "{h1} -> {h2}");
}

#[test]
fn an_unknown_character_renders_a_box_rather_than_vanishing() {
    // Silently dropping a character makes a label subtly wrong with no signal.
    let mut doc = white(120, 60);
    doc.add(
        Annotation::Text {
            at: LogicalPoint::new(10.0, 10.0),
            content: "\u{4e2d}".to_owned(),
        },
        ink(Style::stroked().with_font_size(24.0)),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert!(
        ink_count(&out) > 10,
        "an unsupported glyph should still occupy space visibly"
    );
}

#[test]
fn a_counter_draws_a_disc_with_a_legible_numeral() {
    let mut doc = white(100, 100);
    doc.add(
        Annotation::Counter {
            at: LogicalPoint::new(50.0, 50.0),
            index: 0,
        },
        Style::stroked()
            .with_fill(Some(Color::rgb(220, 0, 0)))
            .with_font_size(20.0),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    // The disc is red.
    let edge = pixel(&out, 50, 34);
    assert!(
        edge[0] > 150 && edge[1] < 90,
        "expected the disc, got {edge:?}"
    );
    // The numeral is drawn in a contrasting ink. Against a dark red disc that
    // resolves to white, so the centre of the marker is brighter than the disc
    // rather than darker.
    let centre_row: Vec<[u8; 4]> = (42..58).map(|x| pixel(&out, x, 50)).collect();
    assert!(
        centre_row.iter().any(|p| p[1] > 150),
        "the numeral should be visible against the disc: {centre_row:?}"
    );
    // And outside the disc nothing was touched.
    assert!(near(pixel(&out, 5, 5), [255, 255, 255, 255], 2));
}

#[test]
fn counter_discs_scale_with_font_size() {
    let extent = |size: f64| {
        let mut doc = white(200, 200);
        doc.add(
            Annotation::Counter {
                at: LogicalPoint::new(100.0, 100.0),
                index: 0,
            },
            Style::stroked()
                .with_fill(Some(Color::rgb(0, 0, 0)))
                .with_font_size(size),
        );
        let out = SkiaRenderer::new().render(&doc).unwrap();
        let (l, _, r, _) = ink_bounds(&out).unwrap();
        r - l
    };
    let small = extent(12.0);
    let large = extent(24.0);
    assert!(
        large >= small * 2 - 3 && large <= small * 2 + 3,
        "{small} -> {large}"
    );
}

#[test]
fn renumbering_after_a_deletion_is_visible_in_the_render() {
    // The document-level test proves the indices renumber; this proves the
    // renderer draws the new numbers rather than a cached old one.
    let mut doc = white(200, 100);
    for i in 0..3 {
        doc.add(
            Annotation::Counter {
                at: LogicalPoint::new(40.0 + 60.0 * f64::from(i), 50.0),
                index: 0,
            },
            Style::stroked()
                .with_fill(Some(Color::rgb(0, 0, 0)))
                .with_font_size(18.0),
        );
    }
    let ids: Vec<_> = doc.annotations().iter().map(|o| o.id).collect();
    let three = SkiaRenderer::new().render(&doc).unwrap();

    doc.remove(ids[0]);
    let two = SkiaRenderer::new().render(&doc).unwrap();

    // Isolate the third marker's disc, which changed from "3" to "2".
    let slice = |f: &Frame| {
        (140..180)
            .flat_map(|x| (30..70).map(move |y| (x, y)))
            .map(|(x, y)| pixel(f, x, y))
            .collect::<Vec<_>>()
    };
    assert_ne!(
        slice(&three),
        slice(&two),
        "the last marker should have been redrawn with its new number"
    );
}

#[test]
fn counters_render_double_digit_numbers() {
    let mut doc = white(400, 100);
    for i in 0..12 {
        doc.add(
            Annotation::Counter {
                at: LogicalPoint::new(20.0 + 30.0 * f64::from(i), 50.0),
                index: 0,
            },
            Style::stroked()
                .with_fill(Some(Color::rgb(0, 0, 0)))
                .with_font_size(14.0),
        );
    }
    let out = SkiaRenderer::new()
        .render(&doc)
        .expect("twelve markers is not unusual");
    assert!(ink_count(&out) > 100);
}

#[test]
fn opacity_lightens_without_removing() {
    let mut doc = white(60, 60);
    doc.add(
        Annotation::Rectangle(rect(10.0, 10.0, 40.0, 40.0)),
        ink(Style::stroked()
            .with_stroke_width(0.0)
            .with_stroke(Color::TRANSPARENT)
            .with_fill(Some(Color::rgb(0, 0, 0)))
            .with_opacity(0.5)),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    let p = pixel(&out, 30, 30);
    assert!(
        p[0] > 100 && p[0] < 160,
        "half-opacity black on white is mid grey: {p:?}"
    );
}

#[test]
fn shapes_stay_crisp_at_high_export_scales() {
    let mut doc = white(50, 50);
    doc.add(
        Annotation::Rectangle(rect(10.0, 10.0, 30.0, 30.0)),
        ink(Style::stroked().with_stroke_width(1.0)),
    );
    // Vector annotations must be re-rasterised at the export scale, not
    // upscaled from a 1x render — a 4x export of a 1pt stroke should still be
    // a clean 4px line.
    let out = SkiaRenderer::new()
        .render_at(&doc, ScaleFactor::new(4.0))
        .unwrap();
    let thickness = column_thickness(&out, 100);
    assert!(
        (3..=6).contains(&thickness),
        "a 1pt stroke at 4x should be about 4px, was {thickness}"
    );
}

/// How many contiguous rows are inked in one column, at its thickest run.
fn column_thickness(frame: &Frame, x: u32) -> u32 {
    let mut best = 0;
    let mut run = 0;
    for y in 0..frame.height() {
        if pixel(frame, x, y)[0] < 160 {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}
