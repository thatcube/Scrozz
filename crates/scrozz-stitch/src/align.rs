//! Two-stage vertical frame alignment.
//!
//! A row-profile sweep cheaply searches every plausible displacement. Only
//! distinct candidate basins then pay for a full-pixel comparison. Keeping those
//! stages separate is what makes alignment both fast on 5K frames and honest on
//! repeated content such as tables, source code and striped lists.

use std::cmp::Ordering;

use thiserror::Error;

use crate::luma::{LumaPlane, RowProfile};

/// Rows that are allowed to influence alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalysisBand {
    /// First included row.
    pub top: u32,
    /// First excluded row.
    pub bottom: u32,
}

impl AnalysisBand {
    /// The complete frame.
    #[must_use]
    pub const fn full(height: u32) -> Self {
        Self {
            top: 0,
            bottom: height,
        }
    }

    /// Number of included rows.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.bottom.saturating_sub(self.top)
    }
}

/// Tuning for deterministic vertical alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlignmentConfig {
    /// Minimum number of content rows two frames must share.
    pub min_overlap: u32,
    /// Largest displacement to consider. `None` means every displacement that
    /// leaves [`Self::min_overlap`] rows.
    pub max_delta: Option<u32>,
    /// Column means retained in each row profile.
    pub row_buckets: usize,
    /// Number of distinct coarse basins verified against real pixels.
    pub top_k: usize,
    /// Nearby offsets count as one basin when measuring confidence.
    pub basin_radius: u32,
    /// Largest acceptable mean absolute luma error.
    pub max_mean_error: u32,
    /// Required raw-pixel margin over the next distinct basin when no prior is
    /// available.
    pub min_confidence: u32,
    /// Maximum score points an expected-displacement prior may contribute.
    pub prior_weight: u32,
}

impl Default for AlignmentConfig {
    fn default() -> Self {
        Self {
            min_overlap: 32,
            max_delta: None,
            row_buckets: 32,
            top_k: 6,
            basin_radius: 3,
            max_mean_error: 32,
            min_confidence: 2,
            prior_weight: 12,
        }
    }
}

/// A verified displacement between two frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alignment {
    /// Rows the viewport advanced. Positive means content moved up the screen.
    pub delta: u32,
    /// Mean absolute luma difference across the full overlap.
    pub score: u32,
    /// Score of the next verified, distinct basin.
    pub runner_up_score: Option<u32>,
    /// Raw-pixel margin to the next distinct basin.
    pub confidence: u32,
    /// Whether an expected-displacement prior was available to break a tie.
    pub used_expected_prior: bool,
    /// Number of compared rows.
    pub overlap: u32,
}

/// Why two frames could not be aligned safely.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AlignError {
    /// Geometry makes the comparison meaningless.
    #[error("frames cannot be aligned: {0}")]
    Incompatible(String),
    /// Every permitted displacement leaves too little shared content.
    #[error("frames do not share the required overlap")]
    InsufficientOverlap,
    /// The best candidate still does not resemble the preceding frame.
    #[error("best alignment has mean luma error {score}, above the limit {limit}")]
    PoorMatch {
        /// Best observed error.
        score: u32,
        /// Configured maximum.
        limit: u32,
    },
    /// Repeated pixels produced two equally plausible, distant seams.
    #[error(
        "alignment is ambiguous: delta {best_delta} and delta {other_delta} differ by only {margin}"
    )]
    Ambiguous {
        /// Best candidate displacement.
        best_delta: u32,
        /// Next distinct displacement.
        other_delta: u32,
        /// Raw score margin.
        margin: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Candidate {
    delta: u32,
    raw_score: u32,
    adjusted_score: u32,
    overlap: u32,
}

/// Aligns two complete luma planes vertically.
///
/// `expected_delta` is the requested scroll converted to physical pixels. It is
/// a prior, not an answer: real pixels still decide the score and confidence.
pub fn align_vertical(
    previous: &LumaPlane,
    current: &LumaPlane,
    expected_delta: Option<u32>,
    config: &AlignmentConfig,
) -> Result<Alignment, AlignError> {
    align_vertical_in(
        previous,
        current,
        AnalysisBand::full(previous.height()),
        expected_delta,
        config,
    )
}

/// Aligns two luma planes while excluding fixed top and bottom chrome.
pub fn align_vertical_in(
    previous: &LumaPlane,
    current: &LumaPlane,
    band: AnalysisBand,
    expected_delta: Option<u32>,
    config: &AlignmentConfig,
) -> Result<Alignment, AlignError> {
    validate(previous, current, band, config)?;

    let profile_a = RowProfile::new(previous, config.row_buckets.max(1));
    let profile_b = RowProfile::new(current, config.row_buckets.max(1));
    let min_overlap = config.min_overlap.min(band.height()).max(1);
    let largest = band
        .height()
        .saturating_sub(min_overlap)
        .min(config.max_delta.unwrap_or(u32::MAX));

    let mut coarse = Vec::with_capacity(largest as usize + 1);
    for delta in 0..=largest {
        let overlap = band.height() - delta;
        let raw_score = profile_sad(&profile_a, &profile_b, band, delta);
        coarse.push(Candidate {
            delta,
            raw_score,
            adjusted_score: with_prior(
                raw_score,
                delta,
                expected_delta,
                band.height(),
                config.prior_weight,
            ),
            overlap,
        });
    }
    if coarse.is_empty() {
        return Err(AlignError::InsufficientOverlap);
    }

    coarse.sort_by(candidate_order);
    let mut shortlist = distinct_candidates(
        &coarse,
        config.top_k.max(2),
        config.basin_radius,
        expected_delta,
    );
    for candidate in &mut shortlist {
        candidate.raw_score = pixel_sad(previous, current, band, candidate.delta);
        candidate.adjusted_score = with_prior(
            candidate.raw_score,
            candidate.delta,
            expected_delta,
            band.height(),
            config.prior_weight,
        );
    }
    shortlist.sort_by(candidate_order);

    let best = shortlist[0];
    if best.raw_score > config.max_mean_error {
        return Err(AlignError::PoorMatch {
            score: best.raw_score,
            limit: config.max_mean_error,
        });
    }

    let runner_up = shortlist
        .iter()
        .copied()
        .find(|candidate| candidate.delta.abs_diff(best.delta) > config.basin_radius);
    let confidence = runner_up.map_or(255, |other| other.raw_score.saturating_sub(best.raw_score));

    if expected_delta.is_none()
        && let Some(other) = runner_up
        && confidence < config.min_confidence
    {
        return Err(AlignError::Ambiguous {
            best_delta: best.delta,
            other_delta: other.delta,
            margin: confidence,
        });
    }

    Ok(Alignment {
        delta: best.delta,
        score: best.raw_score,
        runner_up_score: runner_up.map(|candidate| candidate.raw_score),
        confidence,
        used_expected_prior: expected_delta.is_some(),
        overlap: best.overlap,
    })
}

fn validate(
    previous: &LumaPlane,
    current: &LumaPlane,
    band: AnalysisBand,
    config: &AlignmentConfig,
) -> Result<(), AlignError> {
    if !previous.matches_width(current) || previous.height() != current.height() {
        return Err(AlignError::Incompatible(format!(
            "{}x{} versus {}x{}",
            previous.width(),
            previous.height(),
            current.width(),
            current.height()
        )));
    }
    if band.top >= band.bottom || band.bottom > previous.height() {
        return Err(AlignError::Incompatible(format!(
            "analysis band {}..{} is outside a {}-row frame",
            band.top,
            band.bottom,
            previous.height()
        )));
    }
    if config.min_overlap == 0 {
        return Err(AlignError::Incompatible(
            "minimum overlap must be at least one row".to_owned(),
        ));
    }
    Ok(())
}

fn profile_sad(previous: &RowProfile, current: &RowProfile, band: AnalysisBand, delta: u32) -> u32 {
    let overlap = band.height() - delta;
    let sum: u64 = (0..overlap)
        .map(|row| {
            u64::from(previous.row_distance(band.top + delta + row, current, band.top + row))
        })
        .sum();
    (sum / u64::from(overlap.max(1))) as u32
}

fn pixel_sad(previous: &LumaPlane, current: &LumaPlane, band: AnalysisBand, delta: u32) -> u32 {
    let overlap = band.height() - delta;
    let mut sum = 0u64;
    let mut count = 0u64;
    for row in 0..overlap {
        let a = previous.row(band.top + delta + row);
        let b = current.row(band.top + row);
        for (&left, &right) in a.iter().zip(b) {
            sum += u64::from(left.abs_diff(right));
            count += 1;
        }
    }
    (sum / count.max(1)) as u32
}

fn with_prior(
    raw_score: u32,
    delta: u32,
    expected_delta: Option<u32>,
    search_height: u32,
    prior_weight: u32,
) -> u32 {
    let Some(expected) = expected_delta else {
        return raw_score;
    };
    let distance = u64::from(delta.abs_diff(expected));
    let denominator = u64::from(search_height.saturating_sub(1).max(1));
    let weighted = distance * u64::from(prior_weight);
    // Round away from zero so a nearby repeated row cannot tie the exact prior
    // merely because integer division erased a subpixel score contribution.
    let penalty = weighted.div_ceil(denominator);
    raw_score.saturating_add(penalty as u32)
}

fn candidate_order(left: &Candidate, right: &Candidate) -> Ordering {
    left.adjusted_score
        .cmp(&right.adjusted_score)
        .then_with(|| left.raw_score.cmp(&right.raw_score))
        .then_with(|| left.delta.cmp(&right.delta))
}

fn distinct_candidates(
    sorted: &[Candidate],
    limit: usize,
    radius: u32,
    expected_delta: Option<u32>,
) -> Vec<Candidate> {
    let mut selected = Vec::with_capacity(limit);
    for candidate in sorted {
        if selected
            .iter()
            .all(|other: &Candidate| other.delta.abs_diff(candidate.delta) > radius)
        {
            selected.push(*candidate);
            if selected.len() == limit {
                break;
            }
        }
    }

    if let Some(expected) = expected_delta
        && !selected
            .iter()
            .any(|candidate| candidate.delta.abs_diff(expected) <= radius)
        && let Some(nearest) = sorted
            .iter()
            .min_by_key(|candidate| candidate.delta.abs_diff(expected))
    {
        if selected.len() == limit {
            selected.pop();
        }
        selected.push(*nearest);
    }

    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(rows: &[u8], width: u32) -> LumaPlane {
        let mut data = Vec::new();
        for &row in rows {
            data.extend(std::iter::repeat_n(row, width as usize));
        }
        LumaPlane::from_raw(width, rows.len() as u32, data)
    }

    fn small() -> AlignmentConfig {
        AlignmentConfig {
            min_overlap: 3,
            row_buckets: 4,
            top_k: 5,
            basin_radius: 1,
            max_mean_error: 4,
            min_confidence: 1,
            prior_weight: 12,
            ..AlignmentConfig::default()
        }
    }

    #[test]
    fn recovers_a_known_vertical_displacement() {
        let document: Vec<u8> = (0..20).map(|v| v * 9).collect();
        let first = plane(&document[0..10], 8);
        let second = plane(&document[4..14], 8);
        let aligned = align_vertical(&first, &second, Some(4), &small()).expect("alignment");
        assert_eq!(aligned.delta, 4);
        assert_eq!(aligned.score, 0);
        assert_eq!(aligned.overlap, 6);
    }

    #[test]
    fn repeated_content_without_a_prior_is_ambiguous() {
        let document: Vec<u8> = (0..30).map(|v| if v % 4 < 2 { 20 } else { 220 }).collect();
        let first = plane(&document[0..12], 8);
        let second = plane(&document[4..16], 8);
        let err = align_vertical(&first, &second, None, &small()).expect_err("two equal basins");
        assert!(matches!(err, AlignError::Ambiguous { .. }));
    }

    #[test]
    fn the_expected_delta_breaks_a_repeated_pattern_tie_but_reports_low_confidence() {
        let document: Vec<u8> = (0..30).map(|v| if v % 4 < 2 { 20 } else { 220 }).collect();
        let first = plane(&document[0..12], 8);
        let second = plane(&document[4..16], 8);
        let aligned = align_vertical(&first, &second, Some(4), &small()).expect("prior");
        assert_eq!(aligned.delta, 4);
        assert_eq!(aligned.confidence, 0);
        assert!(aligned.used_expected_prior);
    }

    #[test]
    fn a_bad_analysis_band_is_refused() {
        let input = plane(&[1, 2, 3, 4], 2);
        let err = align_vertical_in(
            &input,
            &input,
            AnalysisBand { top: 3, bottom: 2 },
            None,
            &small(),
        )
        .expect_err("inverted");
        assert!(matches!(err, AlignError::Incompatible(_)));
    }
}
