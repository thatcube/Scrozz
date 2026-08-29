//! Shared, platform-neutral layout arithmetic.

/// Hostile-input ceiling for a single vertical overlay layout.
///
/// Normal capacity is derived only from work-area height. This bound exists so
/// malformed sub-pixel dimensions cannot request an effectively unbounded
/// allocation; even an 8K portrait display with realistic card metrics remains
/// orders of magnitude below it.
pub const LAYOUT_ITEM_SAFETY_CEILING: usize = 4096;

/// Number of fixed-height items that fit vertically with a gap between them.
///
/// `n` items occupy `n * item_height + (n - 1) * gap`, hence
/// `floor((usable_height + gap) / (item_height + gap))`. Invalid or constrained
/// geometry still returns one so a newly created capture never disappears.
#[must_use]
pub fn vertical_capacity(usable_height: f64, item_height: f64, gap: f64) -> usize {
    let pitch = item_height + gap;
    if !usable_height.is_finite()
        || !item_height.is_finite()
        || !gap.is_finite()
        || item_height <= 0.0
        || gap < 0.0
        || pitch <= 0.0
    {
        return 1;
    }
    let fits = ((usable_height + gap) / pitch).floor();
    if fits.is_nan() || fits < 1.0 {
        1
    } else {
        (fits as usize).min(LAYOUT_ITEM_SAFETY_CEILING)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_is_the_exact_floor_formula_without_a_product_cap() {
        assert_eq!(vertical_capacity(1_504.0, 180.0, 8.0), 8);
        assert_eq!(vertical_capacity(10_000.0, 180.0, 8.0), 53);
    }

    #[test]
    fn invalid_or_hostile_dimensions_stay_bounded() {
        for invalid in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            assert_eq!(vertical_capacity(1000.0, invalid, 8.0), 1);
        }
        assert_eq!(
            vertical_capacity(f64::MAX, f64::MIN_POSITIVE, 0.0),
            LAYOUT_ITEM_SAFETY_CEILING
        );
    }
}
