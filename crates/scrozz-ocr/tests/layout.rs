//! Coordinate conversion and reading order.
//!
//! These run on every platform, which is the point: the vertical flip and the
//! line grouping are where OCR bugs actually live, and neither needs an engine
//! to test. A Linux CI runner with no recogniser at all still catches a
//! regression here.

use scrozz_core::{
    LogicalPoint, LogicalRect, LogicalSize, PhysicalPoint, PhysicalSize, ScaleFactor,
};
use scrozz_ocr::TextBlock;
use scrozz_ocr::layout::{
    NormalizedRect, bottom_left_normalized_to_physical, group_lines, pixels_to_physical,
    plain_text, sort_reading_order, union,
};

fn image(width: f64, height: f64) -> PhysicalSize {
    PhysicalSize::new(width, height)
}

/// Rectangles are computed in floating point, so `(1.0 - 0.9) * 500.0` is
/// 49.999999999999986. A sub-thousandth-of-a-pixel tolerance keeps the tests
/// about the geometry rather than about IEEE 754.
#[track_caller]
fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-6,
        "expected {expected}, got {actual}"
    );
}

fn block(text: &str, x: f64, y: f64, w: f64, h: f64) -> TextBlock {
    TextBlock {
        text: text.to_string(),
        bounds: LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(w, h)),
        confidence: 1.0,
    }
}

/// The whole point of the module: Vision's `y` is the box's *bottom*, measured
/// up from the image's bottom. A box sitting at the top of the image must come
/// back with a small `origin.y`, not a large one.
#[test]
fn normalized_bottom_left_origin_flips_to_top_left() {
    // Occupies the top tenth: bottom edge at 0.9, height 0.1.
    let top = NormalizedRect::new(0.0, 0.9, 1.0, 0.1);
    let rect = bottom_left_normalized_to_physical(top, image(1000.0, 500.0));
    close(rect.origin.y, 0.0);
    close(rect.size.height, 50.0);

    // Occupies the bottom tenth: bottom edge at 0.0.
    let bottom = NormalizedRect::new(0.0, 0.0, 1.0, 0.1);
    let rect = bottom_left_normalized_to_physical(bottom, image(1000.0, 500.0));
    close(rect.origin.y, 450.0);
    close(rect.size.height, 50.0);
}

/// The classic wrong version uses `1 - y` instead of `1 - (y + height)`, which
/// puts every box exactly one box-height too low. Pin the difference.
#[test]
fn flip_uses_the_far_edge_not_the_near_one() {
    let rect = NormalizedRect::new(0.25, 0.60, 0.5, 0.20);
    let converted = bottom_left_normalized_to_physical(rect, image(400.0, 200.0));

    // top = (1 - (0.60 + 0.20)) * 200 = 40, not (1 - 0.60) * 200 = 80.
    close(converted.origin.y, 40.0);
    close(converted.origin.x, 100.0);
    close(converted.size.width, 200.0);
    close(converted.size.height, 40.0);
}

#[test]
fn flip_is_its_own_inverse_about_the_image_centre() {
    let size = image(800.0, 600.0);
    let low = bottom_left_normalized_to_physical(NormalizedRect::new(0.1, 0.1, 0.2, 0.1), size);
    let high = bottom_left_normalized_to_physical(NormalizedRect::new(0.1, 0.8, 0.2, 0.1), size);

    // Mirrored about the horizontal centre line.
    let low_centre = low.origin.y + low.size.height / 2.0;
    let high_centre = high.origin.y + high.size.height / 2.0;
    close(low_centre + high_centre, 600.0);
}

#[test]
fn out_of_range_boxes_are_clamped_not_propagated() {
    let rect = NormalizedRect::new(-0.2, -0.1, 1.6, 1.4);
    let converted = bottom_left_normalized_to_physical(rect, image(100.0, 100.0));
    close(converted.origin.x, 0.0);
    close(converted.origin.y, 0.0);
    close(converted.size.width, 100.0);
    close(converted.size.height, 100.0);
}

#[test]
fn degenerate_image_size_yields_an_empty_rect() {
    let rect = bottom_left_normalized_to_physical(
        NormalizedRect::new(0.0, 0.0, 1.0, 1.0),
        image(0.0, 0.0),
    );
    assert!(rect.is_empty());
}

/// The Windows path: pixels in the *upscaled* image must be divided back down.
#[test]
fn pixel_rects_are_divided_by_the_upscale_factor() {
    let rect = pixels_to_physical(200.0, 100.0, 80.0, 40.0, 2.0, image(400.0, 300.0));
    assert_eq!(rect.origin.x, 100.0);
    assert_eq!(rect.origin.y, 50.0);
    assert_eq!(rect.size.width, 40.0);
    assert_eq!(rect.size.height, 20.0);
}

#[test]
fn pixel_rects_survive_a_nonsense_upscale_factor() {
    for factor in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let rect = pixels_to_physical(10.0, 10.0, 5.0, 5.0, factor, image(100.0, 100.0));
        assert_eq!(
            rect.origin.x, 10.0,
            "factor {factor} should fall back to 1.0"
        );
        assert_eq!(rect.size.width, 5.0);
    }
}

#[test]
fn union_grows_to_cover_both_and_ignores_empties() {
    let a = scrozz_core::PhysicalRect::new(
        PhysicalPoint::new(10.0, 10.0),
        PhysicalSize::new(20.0, 5.0),
    );
    let b = scrozz_core::PhysicalRect::new(
        PhysicalPoint::new(40.0, 8.0),
        PhysicalSize::new(10.0, 10.0),
    );
    let u = union(a, b);
    assert_eq!(u.origin.x, 10.0);
    assert_eq!(u.origin.y, 8.0);
    assert_eq!(u.size.width, 40.0);
    assert_eq!(u.size.height, 10.0);

    assert_eq!(union(scrozz_core::PhysicalRect::default(), b), b);
    assert_eq!(union(a, scrozz_core::PhysicalRect::default()), a);
}

#[test]
fn logical_conversion_divides_by_scale() {
    let rect = scrozz_core::PhysicalRect::new(
        PhysicalPoint::new(200.0, 100.0),
        PhysicalSize::new(60.0, 20.0),
    );
    let logical = scrozz_ocr::layout::to_logical(rect, ScaleFactor::new(2.0));
    assert_eq!(logical.origin.x, 100.0);
    assert_eq!(logical.origin.y, 50.0);
    assert_eq!(logical.size.width, 30.0);
    assert_eq!(logical.size.height, 10.0);
}

/// Engines return observations in their own order. Reading order must be
/// recovered, or pasted text is a bag of words.
#[test]
fn reading_order_is_down_then_across() {
    let blocks = vec![
        block("third", 0.0, 40.0, 30.0, 10.0),
        block("second", 50.0, 20.0, 30.0, 10.0),
        block("first", 0.0, 20.0, 30.0, 10.0),
        block("zeroth", 0.0, 0.0, 30.0, 10.0),
    ];
    let ordered = sort_reading_order(blocks);
    let texts: Vec<&str> = ordered.iter().map(|b| b.text.as_str()).collect();
    assert_eq!(texts, ["zeroth", "first", "second", "third"]);
}

#[test]
fn blocks_on_one_row_group_into_one_line() {
    let blocks = vec![
        block("Name", 0.0, 10.0, 40.0, 12.0),
        block("Value", 60.0, 11.0, 40.0, 10.0),
        block("Next", 0.0, 40.0, 40.0, 12.0),
    ];
    let lines = group_lines(blocks);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].len(), 2);
    assert_eq!(lines[0][0].text, "Name");
    assert_eq!(lines[0][1].text, "Value");
    assert_eq!(lines[1][0].text, "Next");
}

/// Mixed font sizes in one row are the normal case in UI chrome — a heading
/// beside a badge. Overlap-based grouping has to hold them together.
#[test]
fn mixed_heights_on_one_row_still_group() {
    let blocks = vec![
        block("Heading", 0.0, 10.0, 100.0, 24.0),
        block("NEW", 120.0, 18.0, 30.0, 10.0),
    ];
    let lines = group_lines(blocks);
    assert_eq!(
        lines.len(),
        1,
        "a small badge sits on the same line as a heading"
    );
}

#[test]
fn adjacent_rows_do_not_merge() {
    // 14pt lines with 2pt of leading: touching, but barely overlapping.
    let blocks = vec![
        block("line one", 0.0, 0.0, 100.0, 14.0),
        block("line two", 0.0, 16.0, 100.0, 14.0),
        block("line three", 0.0, 32.0, 100.0, 14.0),
    ];
    assert_eq!(group_lines(blocks).len(), 3);
}

#[test]
fn empty_blocks_are_dropped() {
    let blocks = vec![
        block("", 0.0, 0.0, 10.0, 10.0),
        block("kept", 0.0, 0.0, 10.0, 10.0),
    ];
    let lines = group_lines(blocks);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].len(), 1);
}

#[test]
fn grouping_nothing_yields_nothing() {
    assert!(group_lines(Vec::new()).is_empty());
    assert_eq!(plain_text(&[]), "");
}

#[test]
fn plain_text_preserves_lines_and_spacing() {
    let blocks = vec![
        block("Save", 0.0, 0.0, 40.0, 12.0),
        block("Cancel", 50.0, 0.0, 40.0, 12.0),
        block("Ready.", 0.0, 30.0, 60.0, 12.0),
    ];
    assert_eq!(plain_text(&blocks), "Save Cancel\nReady.");
}

#[test]
fn plain_text_does_not_double_space() {
    let blocks = vec![
        block("trailing ", 0.0, 0.0, 40.0, 12.0),
        block("space", 50.0, 0.0, 40.0, 12.0),
    ];
    assert_eq!(plain_text(&blocks), "trailing space");
}

/// Grouping must not depend on the order the engine happened to emit.
#[test]
fn grouping_is_order_independent() {
    let ordered = vec![
        block("a", 0.0, 0.0, 10.0, 10.0),
        block("b", 20.0, 0.0, 10.0, 10.0),
        block("c", 0.0, 20.0, 10.0, 10.0),
    ];
    let mut shuffled = ordered.clone();
    shuffled.reverse();
    assert_eq!(plain_text(&ordered), plain_text(&shuffled));
    assert_eq!(plain_text(&ordered), "a b\nc");
}
