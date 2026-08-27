//! Coordinates and reading order.
//!
//! # Why this is separate from the backends
//!
//! The two system engines disagree about almost everything a coordinate can
//! disagree about. Vision returns rectangles that are **normalised to 0–1 with
//! the origin at the bottom-left**; Windows returns **pixels with the origin at
//! the top-left**, in the resolution of the bitmap it was handed — which is the
//! *upscaled* one, not the frame. Scrozz's UI wants one thing: a top-left
//! [`LogicalRect`] over the original frame.
//!
//! Getting the vertical flip wrong is the classic bug in this area. It is also
//! invisible in an unstructured smoke test, because flipped boxes are still
//! plausible boxes — the text is right, the highlight is just on the wrong line.
//! Keeping the conversion here, as pure functions over plain numbers, is what
//! makes it testable on a Linux CI runner that has no OCR engine at all.
//!
//! The same argument applies to reading order. Both engines return observations
//! in an order that is *not* reading order, and users overwhelmingly copy OCR
//! output and paste it somewhere. Text that pastes as a bag of words is a
//! failure even when every character is correct.

use scrozz_core::{LogicalRect, PhysicalPoint, PhysicalRect, PhysicalSize, ScaleFactor};

use crate::TextBlock;

/// Fraction of the shorter height two boxes must share vertically to count as
/// the same line.
///
/// Half is forgiving enough for a superscript or a mixed-size row, strict enough
/// that consecutive lines of body text do not merge.
const LINE_OVERLAP_RATIO: f64 = 0.5;

/// A rectangle normalised to the unit square with its origin at the
/// **bottom-left** — Vision's convention, and nobody else's.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NormalizedRect {
    /// Left edge, 0 at the image's left.
    pub x: f64,
    /// Bottom edge, 0 at the image's *bottom*.
    pub y: f64,
    /// Width as a fraction of image width.
    pub width: f64,
    /// Height as a fraction of image height.
    pub height: f64,
}

impl NormalizedRect {
    /// Creates a normalised rectangle.
    #[must_use]
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Converts a bottom-left normalised rectangle into top-left physical pixels.
///
/// The flip is the whole point:
///
/// ```text
/// top_in_pixels = (1 - (y + height)) * image_height
/// ```
///
/// Note `y + height`, not `y`. Vision's `y` is the box's *bottom*, so the top
/// edge is found by measuring the far side of the box down from the image's top.
/// Using `1 - y` instead is the mistake that puts every highlight one box-height
/// too low, and it looks almost right, which is why it survives review.
///
/// Because the input is normalised, an upscale applied before recognition
/// cancels out and no division is needed here. Results are clamped to the image,
/// as engines occasionally report boxes a hair outside it.
#[must_use]
pub fn bottom_left_normalized_to_physical(
    rect: NormalizedRect,
    image: PhysicalSize,
) -> PhysicalRect {
    let (w, h) = (image.width, image.height);
    if w <= 0.0 || h <= 0.0 {
        return PhysicalRect::default();
    }

    let left = rect.x * w;
    let right = (rect.x + rect.width) * w;
    // Flip: distance from the top is the complement of the box's far edge.
    let top = (1.0 - (rect.y + rect.height)) * h;
    let bottom = (1.0 - rect.y) * h;

    clamped(left, top, right, bottom, w, h)
}

/// Converts a top-left pixel rectangle in a prepared image back to the frame.
///
/// `upscale` is [`crate::prepare::Prepared::upscale`] — prepared pixels per
/// original pixel. Dividing undoes it. Forgetting to divide is silent when the
/// factor happens to be 1.0, which it always is on the developer's Retina
/// machine and never is on a user's 1× monitor.
#[must_use]
pub fn pixels_to_physical(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    upscale: f64,
    image: PhysicalSize,
) -> PhysicalRect {
    let factor = if upscale.is_finite() && upscale > 0.0 {
        upscale
    } else {
        1.0
    };
    clamped(
        x / factor,
        y / factor,
        (x + width) / factor,
        (y + height) / factor,
        image.width,
        image.height,
    )
}

/// The smallest rectangle containing both inputs.
///
/// Windows reports bounds per *word* and none per line, so a line's box has to
/// be assembled from its words.
#[must_use]
pub fn union(a: PhysicalRect, b: PhysicalRect) -> PhysicalRect {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let left = a.origin.x.min(b.origin.x);
    let top = a.origin.y.min(b.origin.y);
    let right = (a.origin.x + a.size.width).max(b.origin.x + b.size.width);
    let bottom = (a.origin.y + a.size.height).max(b.origin.y + b.size.height);
    PhysicalRect::new(
        PhysicalPoint::new(left, top),
        PhysicalSize::new(right - left, bottom - top),
    )
}

/// Converts to logical space, the one sanctioned bridge between the two.
#[must_use]
pub fn to_logical(rect: PhysicalRect, scale: ScaleFactor) -> LogicalRect {
    rect.to_logical(scale)
}

/// Groups blocks into visual lines, top to bottom, each ordered left to right.
///
/// Blocks share a line when their vertical extents overlap by at least
/// [`LINE_OVERLAP_RATIO`] of the shorter one. Comparing *overlap* rather than
/// centre distance is what makes this survive mixed font sizes in the same row,
/// which is the normal case in UI chrome: a heading beside a badge beside a
/// timestamp.
///
/// # Known limitation
///
/// True side-by-side columns will be joined into shared lines. That is the right
/// trade for screenshots — a toolbar, a table row, and a label-and-value pair are
/// all genuinely one line, and they are vastly more common than a two-column
/// journal page in a screenshot.
#[must_use]
pub fn group_lines(blocks: Vec<TextBlock>) -> Vec<Vec<TextBlock>> {
    let mut blocks = blocks;
    blocks.retain(|b| !b.text.is_empty());
    if blocks.is_empty() {
        return Vec::new();
    }

    // Sort by top edge so the sweep only ever looks forward. Ties broken by x
    // keeps the result deterministic, which tests depend on.
    blocks.sort_by(|a, b| {
        a.bounds
            .origin
            .y
            .partial_cmp(&b.bounds.origin.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.bounds
                    .origin
                    .x
                    .partial_cmp(&b.bounds.origin.x)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    let mut lines: Vec<Vec<TextBlock>> = Vec::new();
    // Running vertical extent of the line being built.
    let mut band: (f64, f64) = (0.0, 0.0);

    for block in blocks {
        let top = block.bounds.origin.y;
        let bottom = top + block.bounds.size.height;

        let joins = match lines.last() {
            None => false,
            Some(_) => {
                let overlap = (bottom.min(band.1) - top.max(band.0)).max(0.0);
                let shorter = (bottom - top).min(band.1 - band.0);
                if shorter <= 0.0 {
                    // A zero-height box has no overlap to measure; fall back to
                    // containment so it still lands on a sensible line.
                    top >= band.0 && top <= band.1
                } else {
                    overlap >= LINE_OVERLAP_RATIO * shorter
                }
            }
        };

        if joins {
            if let Some(line) = lines.last_mut() {
                line.push(block);
            }
            band = (band.0.min(top), band.1.max(bottom));
        } else {
            lines.push(vec![block]);
            band = (top, bottom);
        }
    }

    for line in &mut lines {
        line.sort_by(|a, b| {
            a.bounds
                .origin
                .x
                .partial_cmp(&b.bounds.origin.x)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    lines
}

/// Orders blocks the way a person reads them: down the page, then across.
#[must_use]
pub fn sort_reading_order(blocks: Vec<TextBlock>) -> Vec<TextBlock> {
    group_lines(blocks).into_iter().flatten().collect()
}

/// Renders blocks as text, preserving line structure.
///
/// Blocks on one visual line are joined with a space and lines with `\n`, so
/// pasting the result reproduces what the screenshot looked like rather than a
/// run-on paragraph. Copying is what users do with this feature; it is worth
/// getting right.
#[must_use]
pub fn plain_text(blocks: &[TextBlock]) -> String {
    let lines = group_lines(blocks.to_vec());
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for (j, block) in line.iter().enumerate() {
            // Do not manufacture a double space when a block already ends in one.
            if j > 0 && !out.ends_with(char::is_whitespace) {
                out.push(' ');
            }
            out.push_str(&block.text);
        }
    }
    out
}

/// Builds a physical rectangle from edges, clamped to the image.
fn clamped(left: f64, top: f64, right: f64, bottom: f64, width: f64, height: f64) -> PhysicalRect {
    let left = left.clamp(0.0, width);
    let top = top.clamp(0.0, height);
    let right = right.clamp(left, width);
    let bottom = bottom.clamp(top, height);
    PhysicalRect::new(
        PhysicalPoint::new(left, top),
        PhysicalSize::new(right - left, bottom - top),
    )
}
