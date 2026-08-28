//! Redaction: proving the original pixels are actually gone.
//!
//! A blur drawn as a renderable object over intact pixels ships the original
//! underneath the redaction. That has publicly burned other tools — recovering
//! the "hidden" text is a matter of opening the file in something that ignores
//! the overlay. These tests exist to make that failure impossible here.

mod common;

use common::{capture_with, checkerboard, flat, near, pixel, rect};
use scrozz_annotate::{Annotation, Color, Document, RedactStyle, Renderer, SkiaRenderer, Style};
use scrozz_core::{ColorSpace, Frame, Provenance, ScaleFactor};

/// A document whose source is a black/white checkerboard.
fn checkered(width: u32, height: u32) -> Document {
    Document::new(capture_with(
        checkerboard(width, height, 2),
        Provenance::Region,
    ))
}

/// Pixels strictly inside a region, avoiding the antialiased boundary.
fn interior(frame: &Frame, x0: u32, y0: u32, x1: u32, y1: u32) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    for y in (y0 + 2)..(y1 - 2) {
        for x in (x0 + 2)..(x1 - 2) {
            out.push(pixel(frame, x, y));
        }
    }
    out
}

#[test]
fn blur_destroys_the_original_pixels() {
    let mut doc = checkered(120, 120);
    doc.add(
        Annotation::Redact {
            area: rect(20.0, 20.0, 60.0, 60.0),
            style: RedactStyle::Blur,
        },
        Style::redaction(),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    let inside = interior(&out, 20, 20, 80, 80);
    assert!(!inside.is_empty());
    for p in &inside {
        assert!(
            p[0] > 40 && p[0] < 215,
            "a pure checkerboard value survived the blur at {p:?} — the original \
             pixels are still recoverable"
        );
    }

    // And outside the region nothing was touched: a redaction is local.
    let outside = pixel(&out, 100, 100);
    assert!(outside[0] == 0 || outside[0] == 255, "got {outside:?}");
}

#[test]
fn blur_is_strong_enough_to_be_irreversible_in_practice() {
    // A one-pixel blur is not a redaction. The variance inside the region has to
    // collapse, not merely soften.
    let mut doc = checkered(200, 200);
    doc.add(
        Annotation::Redact {
            area: rect(40.0, 40.0, 120.0, 120.0),
            style: RedactStyle::Blur,
        },
        Style::redaction(),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    let inside = interior(&out, 40, 40, 160, 160);
    let mean = inside.iter().map(|p| f64::from(p[0])).sum::<f64>() / inside.len() as f64;
    let spread = inside
        .iter()
        .map(|p| (f64::from(p[0]) - mean).abs())
        .fold(0.0_f64, f64::max);
    assert!(
        spread < 40.0,
        "the blurred region still varies by {spread:.1} levels; the checkerboard \
         is still legible"
    );
}

#[test]
fn blur_is_correct_at_the_edges_of_the_image() {
    // A blur that pads with zeros darkens the border of the region, which both
    // looks wrong and advertises exactly where the redaction is. Blurring a flat
    // image must return that same flat image, corners included.
    let mut doc = Document::new(capture_with(
        flat(60, 60, [128, 64, 192, 255]),
        Provenance::Region,
    ));
    doc.add(
        Annotation::Redact {
            area: rect(0.0, 0.0, 60.0, 60.0),
            style: RedactStyle::Blur,
        },
        Style::redaction(),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    for (x, y) in [
        (0, 0),
        (59, 0),
        (0, 59),
        (59, 59),
        (30, 0),
        (0, 30),
        (30, 30),
    ] {
        let p = pixel(&out, x, y);
        assert!(
            near(p, [128, 64, 192, 255], 2),
            "blurring a flat image changed ({x},{y}) to {p:?} — the kernel is \
             sampling outside the image"
        );
    }
}

#[test]
fn blur_at_a_region_touching_the_image_edge_does_not_darken() {
    let mut doc = Document::new(capture_with(
        flat(80, 80, [200, 200, 200, 255]),
        Provenance::Region,
    ));
    doc.add(
        Annotation::Redact {
            area: rect(0.0, 0.0, 30.0, 30.0),
            style: RedactStyle::Blur,
        },
        Style::redaction(),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    for (x, y) in [(0, 0), (1, 1), (15, 0), (0, 15), (29, 29)] {
        let p = pixel(&out, x, y);
        assert!(near(p, [200, 200, 200, 255], 2), "({x},{y}) became {p:?}");
    }
}

#[test]
fn solid_destroys_the_original_pixels() {
    let mut doc = checkered(100, 100);
    doc.add(
        Annotation::Redact {
            area: rect(10.0, 10.0, 50.0, 40.0),
            style: RedactStyle::Solid,
        },
        Style::redaction().with_fill(Some(Color::rgb(20, 20, 20))),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    for p in interior(&out, 10, 10, 60, 50) {
        assert_eq!(
            p,
            [20, 20, 20, 255],
            "a solid redaction must leave exactly one colour behind"
        );
    }
}

#[test]
fn solid_defaults_to_opaque_even_if_the_style_is_transparent() {
    // A "redaction" that is see-through is the worst possible outcome: it looks
    // applied and is not.
    let mut doc = checkered(60, 60);
    doc.add(
        Annotation::Redact {
            area: rect(10.0, 10.0, 40.0, 40.0),
            style: RedactStyle::Solid,
        },
        Style::redaction()
            .with_fill(Some(Color::TRANSPARENT))
            .with_stroke(Color::TRANSPARENT),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    for p in interior(&out, 10, 10, 50, 50) {
        assert_eq!(p[3], 255, "the redaction left transparent pixels: {p:?}");
        assert!(
            p[0] < 40 && p[1] < 40 && p[2] < 40,
            "expected opaque black, got {p:?}"
        );
    }
}

#[test]
fn pixelate_destroys_the_original_pixels() {
    let mut doc = checkered(120, 120);
    doc.add(
        Annotation::Redact {
            area: rect(20.0, 20.0, 80.0, 80.0),
            style: RedactStyle::Pixelate,
        },
        Style::redaction(),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    // Every block must be flat: within a block all pixels are identical.
    let inside = interior(&out, 20, 20, 100, 100);
    let distinct: std::collections::BTreeSet<[u8; 4]> = inside.iter().copied().collect();
    assert!(
        distinct.len() < inside.len() / 8,
        "pixelation produced {} distinct values over {} pixels — the blocks are \
         not flat and the original is still there",
        distinct.len(),
        inside.len()
    );
}

#[test]
fn pixelate_blocks_are_genuinely_uniform() {
    let mut doc = checkered(160, 160);
    doc.add(
        Annotation::Redact {
            area: rect(0.0, 0.0, 160.0, 160.0),
            style: RedactStyle::Pixelate,
        },
        Style::redaction(),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    // Walk outwards from an arbitrary interior pixel; it must be identical to at
    // least its immediate neighbours, which a per-pixel image never is on a
    // 2px checkerboard.
    let centre = pixel(&out, 80, 80);
    assert_eq!(pixel(&out, 81, 80), centre);
    assert_eq!(pixel(&out, 80, 81), centre);
    assert_eq!(pixel(&out, 81, 81), centre);
}

#[test]
fn a_redaction_destroys_annotations_drawn_beneath_it() {
    // Redaction is applied in z-order, so it erases whatever is under it —
    // including an annotation that itself leaked something.
    let mut doc = Document::new(capture_with(
        flat(80, 80, [255, 255, 255, 255]),
        Provenance::Region,
    ));
    doc.add(
        Annotation::Rectangle(rect(20.0, 20.0, 40.0, 40.0)),
        Style::stroked()
            .with_stroke(Color::TRANSPARENT)
            .with_stroke_width(0.0)
            .with_fill(Some(Color::rgb(255, 0, 0))),
    );
    doc.add(
        Annotation::Redact {
            area: rect(10.0, 10.0, 60.0, 60.0),
            style: RedactStyle::Solid,
        },
        Style::redaction().with_fill(Some(Color::rgb(0, 0, 0))),
    );

    let out = SkiaRenderer::new().render(&doc).unwrap();
    for p in interior(&out, 10, 10, 70, 70) {
        assert_eq!(p, [0, 0, 0, 255], "a red pixel survived the redaction");
    }
}

#[test]
fn an_annotation_above_a_redaction_still_draws() {
    // The converse: z-order is honoured, not special-cased. A label placed on
    // top of a redaction is a legitimate and common thing to want.
    let mut doc = Document::new(capture_with(
        flat(80, 80, [255, 255, 255, 255]),
        Provenance::Region,
    ));
    doc.add(
        Annotation::Redact {
            area: rect(10.0, 10.0, 60.0, 60.0),
            style: RedactStyle::Solid,
        },
        Style::redaction().with_fill(Some(Color::rgb(0, 0, 0))),
    );
    doc.add(
        Annotation::Rectangle(rect(20.0, 20.0, 20.0, 20.0)),
        Style::stroked()
            .with_stroke(Color::TRANSPARENT)
            .with_stroke_width(0.0)
            .with_fill(Some(Color::rgb(255, 0, 0))),
    );

    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert!(near(pixel(&out, 30, 30), [255, 0, 0, 255], 2));
}

#[test]
fn redaction_scales_with_the_export() {
    let mut doc = checkered(100, 100);
    doc.add(
        Annotation::Redact {
            area: rect(25.0, 25.0, 50.0, 50.0),
            style: RedactStyle::Solid,
        },
        Style::redaction().with_fill(Some(Color::rgb(7, 7, 7))),
    );

    let out = SkiaRenderer::new()
        .render_at(&doc, ScaleFactor::new(2.0))
        .unwrap();
    // The region is 25..75 logical, so 50..150 physical at 2x.
    for p in interior(&out, 50, 50, 150, 150) {
        assert_eq!(p, [7, 7, 7, 255]);
    }
    // Outside the redaction the checkerboard is still there. Upscaling
    // interpolates it, so the claim is that it still *varies*, not that the
    // original extremes survive verbatim.
    let outside: Vec<u8> = (4..40).map(|x| pixel(&out, x, 20)[0]).collect();
    let lo = outside.iter().copied().min().unwrap();
    let hi = outside.iter().copied().max().unwrap();
    assert!(
        hi - lo > 100,
        "the unredacted area should still show the checkerboard, spread was {}",
        hi - lo
    );
}

#[test]
fn redaction_survives_a_downscaled_export() {
    // Exporting small must not resample intact pixels back into view: the
    // redaction is applied before any output scaling, so there is nothing left
    // to resample.
    let mut doc = checkered(400, 400);
    doc.add(
        Annotation::Redact {
            area: rect(100.0, 100.0, 200.0, 200.0),
            style: RedactStyle::Solid,
        },
        Style::redaction().with_fill(Some(Color::rgb(0, 0, 0))),
    );
    let out = SkiaRenderer::new().render_to_width(&doc, 100).unwrap();
    // 100..300 of 400 becomes 25..75 of 100.
    for p in interior(&out, 25, 25, 75, 75) {
        assert_eq!(
            p,
            [0, 0, 0, 255],
            "the checkerboard reappeared after downscaling"
        );
    }
}

#[test]
fn the_source_frame_is_untouched_by_every_redaction_style() {
    for style in [RedactStyle::Blur, RedactStyle::Pixelate, RedactStyle::Solid] {
        let mut doc = checkered(80, 80);
        let before = doc.source.frame.data.clone();
        doc.add(
            Annotation::Redact {
                area: rect(0.0, 0.0, 80.0, 80.0),
                style,
            },
            Style::redaction(),
        );
        let _ = SkiaRenderer::new().render(&doc).unwrap();
        assert_eq!(
            before, doc.source.frame.data,
            "{style:?} mutated the document's source; D14 requires it stay editable"
        );
    }
}

#[test]
fn overlapping_redactions_all_apply() {
    let mut doc = checkered(120, 120);
    doc.add(
        Annotation::Redact {
            area: rect(10.0, 10.0, 50.0, 50.0),
            style: RedactStyle::Solid,
        },
        Style::redaction().with_fill(Some(Color::rgb(0, 0, 0))),
    );
    doc.add(
        Annotation::Redact {
            area: rect(40.0, 40.0, 50.0, 50.0),
            style: RedactStyle::Solid,
        },
        Style::redaction().with_fill(Some(Color::rgb(255, 255, 255))),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();

    assert_eq!(pixel(&out, 20, 20), [0, 0, 0, 255]);
    assert_eq!(pixel(&out, 80, 80), [255, 255, 255, 255]);
    // The later one wins in the overlap.
    assert_eq!(pixel(&out, 50, 50), [255, 255, 255, 255]);
}

#[test]
fn a_redaction_of_the_whole_image_leaves_nothing() {
    let mut doc = checkered(64, 64);
    doc.add(
        Annotation::Redact {
            area: rect(0.0, 0.0, 64.0, 64.0),
            style: RedactStyle::Solid,
        },
        Style::redaction().with_fill(Some(Color::rgb(30, 30, 30))),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    for y in 0..64 {
        for x in 0..64 {
            assert_eq!(pixel(&out, x, y), [30, 30, 30, 255], "at ({x},{y})");
        }
    }
}

#[test]
fn a_redaction_extending_past_the_image_is_clipped_and_still_applies() {
    let mut doc = checkered(50, 50);
    doc.add(
        Annotation::Redact {
            area: rect(-20.0, -20.0, 200.0, 200.0),
            style: RedactStyle::Solid,
        },
        Style::redaction().with_fill(Some(Color::rgb(1, 2, 3))),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!(pixel(&out, 0, 0), [1, 2, 3, 255]);
    assert_eq!(pixel(&out, 49, 49), [1, 2, 3, 255]);
}

#[test]
fn blur_and_pixelate_are_deterministic() {
    let mut doc = checkered(100, 100);
    doc.add(
        Annotation::Redact {
            area: rect(10.0, 10.0, 40.0, 40.0),
            style: RedactStyle::Blur,
        },
        Style::redaction(),
    );
    doc.add(
        Annotation::Redact {
            area: rect(55.0, 55.0, 40.0, 40.0),
            style: RedactStyle::Pixelate,
        },
        Style::redaction(),
    );
    let renderer = SkiaRenderer::new();
    assert_eq!(
        renderer.render(&doc).unwrap().data,
        renderer.render(&doc).unwrap().data
    );
}

/// A capture in `space`, so the renderer has a working space to convert into.
fn wide(space: ColorSpace) -> Document {
    let mut frame = flat(40, 40, [200, 200, 200, 255]);
    frame.color_space = space;
    Document::new(capture_with(frame, Provenance::Display))
}

/// The colour a stroke of `color` comes out as, which is by construction the
/// right answer: strokes have gone through the working-space conversion since
/// wide-gamut support landed.
fn stroke_colour(space: ColorSpace, color: Color) -> [u8; 4] {
    let mut doc = wide(space);
    doc.add(
        Annotation::Rectangle(rect(5.0, 5.0, 30.0, 30.0)),
        Style::stroked().with_fill(Some(color)),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    pixel(&out, 20, 20)
}

#[test]
fn a_solid_redaction_uses_the_same_colour_a_stroke_of_that_colour_would() {
    // A solid redaction writes its pixels itself instead of going through a
    // `Paint`, and used to skip the sRGB-to-working conversion every other
    // annotation colour gets. On a wide-gamut capture that made the *one*
    // colour the user picked deliberately the one colour rendered wrong.
    for space in [ColorSpace::DisplayP3, ColorSpace::Rec2020] {
        // Deliberately non-neutral and saturated: a grey would convert to
        // itself and prove nothing.
        let chosen = Color::rgb(220, 40, 30);
        let mut doc = wide(space);
        doc.add(
            Annotation::Redact {
                area: rect(5.0, 5.0, 30.0, 30.0),
                style: RedactStyle::Solid,
            },
            Style::redaction().with_fill(Some(chosen)),
        );
        let out = SkiaRenderer::new().render(&doc).unwrap();
        let got = pixel(&out, 20, 20);
        let want = stroke_colour(space, chosen);

        assert_eq!(
            got, want,
            "{space:?}: redaction painted {got:?}, a stroke of the same colour paints {want:?}"
        );
        assert_ne!(
            [got[0], got[1], got[2]],
            [chosen.r, chosen.g, chosen.b],
            "{space:?}: the sRGB bytes were written straight into a wider space"
        );
    }
}

#[test]
fn a_solid_redaction_in_srgb_is_still_exactly_the_colour_chosen() {
    // The conversion has to stay a no-op where there is nothing to convert.
    let mut doc = wide(ColorSpace::Srgb);
    doc.add(
        Annotation::Redact {
            area: rect(5.0, 5.0, 30.0, 30.0),
            style: RedactStyle::Solid,
        },
        Style::redaction().with_fill(Some(Color::rgb(220, 40, 30))),
    );
    let out = SkiaRenderer::new().render(&doc).unwrap();
    assert_eq!(pixel(&out, 20, 20), [220, 40, 30, 255]);
}
