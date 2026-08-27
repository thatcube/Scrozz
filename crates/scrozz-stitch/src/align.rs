//! Two-stage frame alignment.
//!
//! A row- or column-profile sweep cheaply searches every plausible displacement.
//! Only distinct candidate basins then pay for a full-pixel comparison. Keeping
//! those stages separate is what makes alignment both fast on 5K frames and
//! honest on repeated content such as tables, source code and striped lists.

use std::cmp::Ordering;

use scrozz_core::ScrollAxis;
use thiserror::Error;

use crate::luma::{ColumnProfile, LumaPlane, RowProfile};

const MAX_COARSE_AXIS_SAMPLES: u32 = 96;

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

/// Coordinates along the axis that are allowed to influence alignment.
///
/// Vertical compatibility APIs continue to use [`AnalysisBand`]. This
/// axis-neutral span is used by horizontal alignment and by the shared
/// implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalysisSpan {
    /// First included row or column.
    pub start: u32,
    /// First excluded row or column.
    pub end: u32,
}

impl AnalysisSpan {
    /// The complete axis extent.
    #[must_use]
    pub const fn full(extent: u32) -> Self {
        Self {
            start: 0,
            end: extent,
        }
    }

    /// Number of included rows or columns.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// Whether the span contains no rows or columns.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Tuning for deterministic alignment along either scroll axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlignmentConfig {
    /// Minimum number of content rows or columns two frames must share.
    pub min_overlap: u32,
    /// Largest displacement to consider. `None` means every displacement that
    /// leaves [`Self::min_overlap`] pixels along the movement axis.
    pub max_delta: Option<u32>,
    /// Perpendicular means retained in each row or column profile.
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
    /// Largest zero-displacement error that can identify stationary chrome on
    /// an edge perpendicular to the scroll direction.
    pub stationary_edge_match_threshold: u32,
    /// Required error increase at the measured displacement before an edge can
    /// be ignored as stationary chrome.
    pub stationary_edge_min_contrast: u32,
    /// Ignore thinner stationary edge runs; one matching line is not chrome.
    pub min_stationary_edge: u32,
    /// Maximum portion of each perpendicular edge that stationary-chrome
    /// filtering may exclude.
    pub max_stationary_edge_percent: u32,
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
            stationary_edge_match_threshold: 3,
            stationary_edge_min_contrast: 4,
            min_stationary_edge: 4,
            max_stationary_edge_percent: 25,
        }
    }
}

/// A verified displacement between two frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alignment {
    /// Pixels the viewport advanced. Positive means content moved toward the
    /// leading edge of the screen.
    pub delta: u32,
    /// Mean absolute luma difference across the retained full-pixel overlap.
    pub score: u32,
    /// Score of the next verified, distinct basin.
    pub runner_up_score: Option<u32>,
    /// Raw-pixel margin to the next distinct basin.
    pub confidence: u32,
    /// Whether an expected-displacement prior was available to break a tie.
    pub used_expected_prior: bool,
    /// Number of compared rows or columns along the movement axis.
    pub overlap: u32,
    /// Shared perpendicular viewport span retained after stationary edge chrome
    /// is removed.
    pub perpendicular: AnalysisSpan,
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
    pixel_sad_lower_bound: u64,
    overlap: u32,
    perpendicular: AnalysisSpan,
}

enum CoarseProfiles {
    Rows {
        previous: RowProfile,
        current: RowProfile,
    },
    Columns {
        previous: ColumnProfile,
        current: ColumnProfile,
    },
}

impl CoarseProfiles {
    fn new(
        previous: &LumaPlane,
        current: &LumaPlane,
        axis: ScrollAxis,
        buckets: usize,
        perpendicular: AnalysisSpan,
    ) -> Self {
        match axis {
            ScrollAxis::Vertical => Self::Rows {
                previous: RowProfile::new_in(
                    previous,
                    buckets,
                    perpendicular.start,
                    perpendicular.end,
                ),
                current: RowProfile::new_in(
                    current,
                    buckets,
                    perpendicular.start,
                    perpendicular.end,
                ),
            },
            ScrollAxis::Horizontal => Self::Columns {
                previous: ColumnProfile::new_in(
                    previous,
                    buckets,
                    perpendicular.start,
                    perpendicular.end,
                ),
                current: ColumnProfile::new_in(
                    current,
                    buckets,
                    perpendicular.start,
                    perpendicular.end,
                ),
            },
        }
    }

    fn distance(&self, previous_position: u32, current_position: u32) -> u32 {
        match self {
            Self::Rows { previous, current } => {
                previous.row_distance(previous_position, current, current_position)
            }
            Self::Columns { previous, current } => {
                previous.column_distance(previous_position, current, current_position)
            }
        }
    }

    fn pixel_sad_lower_bound(&self, previous_position: u32, current_position: u32) -> u64 {
        match self {
            Self::Rows { previous, current } => {
                previous.row_pixel_sad_lower_bound(previous_position, current, current_position)
            }
            Self::Columns { previous, current } => {
                previous.column_pixel_sad_lower_bound(previous_position, current, current_position)
            }
        }
    }
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
    align_axis_in(
        previous,
        current,
        ScrollAxis::Vertical,
        AnalysisSpan::full(previous.height()),
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
    align_axis_in(
        previous,
        current,
        ScrollAxis::Vertical,
        AnalysisSpan {
            start: band.top,
            end: band.bottom,
        },
        expected_delta,
        config,
    )
}

/// Aligns two complete luma planes horizontally.
///
/// A positive displacement means the viewport moved right through the
/// document, so document pixels moved left on screen.
pub fn align_horizontal(
    previous: &LumaPlane,
    current: &LumaPlane,
    expected_delta: Option<u32>,
    config: &AlignmentConfig,
) -> Result<Alignment, AlignError> {
    align_axis_in(
        previous,
        current,
        ScrollAxis::Horizontal,
        AnalysisSpan::full(previous.width()),
        expected_delta,
        config,
    )
}

/// Aligns two luma planes horizontally within a column span.
pub fn align_horizontal_in(
    previous: &LumaPlane,
    current: &LumaPlane,
    span: AnalysisSpan,
    expected_delta: Option<u32>,
    config: &AlignmentConfig,
) -> Result<Alignment, AlignError> {
    align_axis_in(
        previous,
        current,
        ScrollAxis::Horizontal,
        span,
        expected_delta,
        config,
    )
}

/// Aligns two complete luma planes along `axis`.
pub fn align_axis(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    expected_delta: Option<u32>,
    config: &AlignmentConfig,
) -> Result<Alignment, AlignError> {
    let extent = axis_extent(previous, axis);
    align_axis_in(
        previous,
        current,
        axis,
        AnalysisSpan::full(extent),
        expected_delta,
        config,
    )
}

/// Aligns two luma planes along `axis` within an axis-relative span.
pub fn align_axis_in(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    span: AnalysisSpan,
    expected_delta: Option<u32>,
    config: &AlignmentConfig,
) -> Result<Alignment, AlignError> {
    align_axis_search(previous, current, axis, span, None, expected_delta, config)
}

pub(crate) fn align_axis_in_perpendicular(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    span: AnalysisSpan,
    perpendicular: AnalysisSpan,
    expected_delta: Option<u32>,
    config: &AlignmentConfig,
) -> Result<Alignment, AlignError> {
    align_axis_search(
        previous,
        current,
        axis,
        span,
        Some(perpendicular),
        expected_delta,
        config,
    )
}

fn align_axis_search(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    span: AnalysisSpan,
    fixed_perpendicular: Option<AnalysisSpan>,
    expected_delta: Option<u32>,
    config: &AlignmentConfig,
) -> Result<Alignment, AlignError> {
    validate(previous, current, axis, span, config)?;
    let perpendicular_extent = match axis {
        ScrollAxis::Vertical => previous.width(),
        ScrollAxis::Horizontal => previous.height(),
    };
    if let Some(perpendicular) = fixed_perpendicular
        && (perpendicular.is_empty() || perpendicular.end > perpendicular_extent)
    {
        return Err(AlignError::Incompatible(format!(
            "perpendicular span {}..{} is outside the {perpendicular_extent}-pixel viewport",
            perpendicular.start, perpendicular.end
        )));
    }

    let min_overlap = config.min_overlap.min(span.len()).max(1);
    let largest = span
        .len()
        .saturating_sub(min_overlap)
        .min(config.max_delta.unwrap_or(u32::MAX));
    let full_perpendicular =
        fixed_perpendicular.unwrap_or_else(|| AnalysisSpan::full(perpendicular_extent));
    let guaranteed_perpendicular = fixed_perpendicular.unwrap_or_else(|| {
        central_coarse_span(perpendicular_extent, config.max_stationary_edge_percent)
    });
    let search = CoarseSearch {
        axis,
        span,
        perpendicular: full_perpendicular,
        largest,
        expected_delta,
        config,
    };
    let mut primary_coarse = coarse_candidates(
        previous,
        current,
        search.with_perpendicular(full_perpendicular),
    );
    if primary_coarse.is_empty() {
        return Err(AlignError::InsufficientOverlap);
    }
    primary_coarse.sort_by(visual_candidate_order);

    // The central span is guaranteed to remain after the largest permitted side
    // crop. Exact bucket sums from this span can therefore certify omitted
    // candidates even when one later proves stationary edge chrome.
    let certificate_coarse = if guaranteed_perpendicular == full_perpendicular {
        primary_coarse.clone()
    } else {
        let mut candidates = coarse_candidates(
            previous,
            current,
            search.with_perpendicular(guaranteed_perpendicular),
        );
        candidates.sort_by(visual_candidate_order);
        candidates
    };

    // Divide the top-K budget between the full viewport and its guaranteed
    // moving core. This keeps real edge evidence while preventing a stationary
    // sidebar from monopolising the shortlist.
    let top_k = config.top_k.max(2);
    let primary_ranked = distinct_candidates(&primary_coarse, top_k, config.basin_radius);
    let secondary_ranked = if guaranteed_perpendicular == full_perpendicular {
        Vec::new()
    } else {
        distinct_candidates(&certificate_coarse, top_k, config.basin_radius)
    };
    let primary_quota = if secondary_ranked.is_empty() {
        top_k
    } else {
        top_k.div_ceil(2)
    };
    let mut centers = Vec::with_capacity(top_k.saturating_add(1));
    append_distinct_centers(
        &mut centers,
        primary_ranked.iter().take(primary_quota).copied(),
        top_k,
        config.basin_radius,
    );
    append_distinct_centers(
        &mut centers,
        secondary_ranked.iter().copied(),
        top_k,
        config.basin_radius,
    );
    append_distinct_centers(
        &mut centers,
        primary_ranked.iter().skip(primary_quota).copied(),
        top_k,
        config.basin_radius,
    );
    include_expected_basin(
        &primary_coarse,
        &mut centers,
        expected_delta,
        config.basin_radius,
    );

    let mut shortlist = refinement_candidates(&centers, largest, config.basin_radius);
    verify_candidates(
        &mut shortlist,
        previous,
        current,
        axis,
        span,
        fixed_perpendicular,
        expected_delta,
        config,
    );

    // If the initial top-K winner cannot be proven distinct from an omitted
    // basin, verify a bounded second top-K set. Remaining uncertainty is an
    // ambiguity, never permission to trust the expected-delta prior.
    let mut extra_centers = 0usize;
    let extra_budget = top_k.saturating_mul(2);
    loop {
        let (best, runner_up, confidence) = best_verified(&shortlist)?;
        let unresolved = first_unverified_competitor(
            &certificate_coarse,
            &shortlist,
            best,
            span.len(),
            perpendicular_extent,
            config.min_confidence,
        );
        let Some(unresolved) = unresolved else {
            if best.raw_score > config.max_mean_error {
                return Err(AlignError::PoorMatch {
                    score: best.raw_score,
                    limit: config.max_mean_error,
                });
            }
            if let Some(other) = runner_up
                && confidence < config.min_confidence
            {
                return Err(AlignError::Ambiguous {
                    best_delta: best.delta,
                    other_delta: other.delta,
                    margin: confidence,
                });
            }
            return Ok(Alignment {
                delta: best.delta,
                score: best.raw_score,
                runner_up_score: runner_up.map(|candidate| candidate.raw_score),
                confidence,
                used_expected_prior: expected_delta.is_some(),
                overlap: best.overlap,
                perpendicular: best.perpendicular,
            });
        };
        if extra_centers >= extra_budget {
            return Err(AlignError::Ambiguous {
                best_delta: best.delta,
                other_delta: unresolved.delta,
                margin: 0,
            });
        }

        let mut additional = refinement_candidates(&[unresolved], largest, config.basin_radius);
        additional.retain(|candidate| {
            shortlist
                .iter()
                .all(|verified| verified.delta != candidate.delta)
        });
        verify_candidates(
            &mut additional,
            previous,
            current,
            axis,
            span,
            fixed_perpendicular,
            expected_delta,
            config,
        );
        shortlist.extend(additional);
        extra_centers += 1;
    }
}

#[derive(Clone, Copy)]
struct CoarseSearch<'a> {
    axis: ScrollAxis,
    span: AnalysisSpan,
    perpendicular: AnalysisSpan,
    largest: u32,
    expected_delta: Option<u32>,
    config: &'a AlignmentConfig,
}

impl CoarseSearch<'_> {
    const fn with_perpendicular(mut self, perpendicular: AnalysisSpan) -> Self {
        self.perpendicular = perpendicular;
        self
    }
}

fn coarse_candidates(
    previous: &LumaPlane,
    current: &LumaPlane,
    search: CoarseSearch<'_>,
) -> Vec<Candidate> {
    let profiles = CoarseProfiles::new(
        previous,
        current,
        search.axis,
        search.config.row_buckets.max(1),
        search.perpendicular,
    );
    (0..=search.largest)
        .map(|delta| {
            let overlap = search.span.len() - delta;
            let (raw_score, pixel_sad_lower_bound) = profile_sad(&profiles, search.span, delta);
            Candidate {
                delta,
                raw_score,
                adjusted_score: with_prior(
                    raw_score,
                    delta,
                    search.expected_delta,
                    search.span.len(),
                    search.config.prior_weight,
                ),
                pixel_sad_lower_bound,
                overlap,
                perpendicular: search.perpendicular,
            }
        })
        .collect()
}

fn append_distinct_centers(
    selected: &mut Vec<Candidate>,
    candidates: impl IntoIterator<Item = Candidate>,
    limit: usize,
    radius: u32,
) {
    for candidate in candidates {
        if selected.len() == limit {
            break;
        }
        if selected
            .iter()
            .all(|other| other.delta.abs_diff(candidate.delta) > radius)
        {
            selected.push(candidate);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn verify_candidates(
    candidates: &mut [Candidate],
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    span: AnalysisSpan,
    fixed_perpendicular: Option<AnalysisSpan>,
    expected_delta: Option<u32>,
    config: &AlignmentConfig,
) {
    for candidate in candidates {
        candidate.perpendicular = fixed_perpendicular.unwrap_or_else(|| {
            stationary_perpendicular_span(previous, current, axis, span, candidate.delta, config)
        });
        candidate.raw_score = pixel_sad(
            previous,
            current,
            axis,
            span,
            candidate.perpendicular,
            candidate.delta,
        );
        candidate.adjusted_score = with_prior(
            candidate.raw_score,
            candidate.delta,
            expected_delta,
            span.len(),
            config.prior_weight,
        );
    }
}

fn best_verified(
    candidates: &[Candidate],
) -> Result<(Candidate, Option<Candidate>, u32), AlignError> {
    let mut ordered = candidates.to_vec();
    ordered.sort_by_key(|candidate| candidate.delta);
    let mut basins = verified_basin_minima(&ordered);
    if basins.is_empty() {
        basins = ordered;
    }
    basins.sort_by(candidate_order);
    let best = *basins.first().ok_or(AlignError::InsufficientOverlap)?;
    let runner_up = basins
        .iter()
        .copied()
        .filter(|candidate| candidate.delta != best.delta)
        .min_by(visual_candidate_order);
    let confidence = runner_up.map_or(255, |other| other.raw_score.saturating_sub(best.raw_score));
    Ok((best, runner_up, confidence))
}

fn first_unverified_competitor(
    certificates: &[Candidate],
    verified: &[Candidate],
    best: Candidate,
    movement_extent: u32,
    perpendicular_extent: u32,
    required_confidence: u32,
) -> Option<Candidate> {
    let threshold = best.raw_score.saturating_add(required_confidence);
    if threshold == 0 {
        return None;
    }

    certificates.iter().copied().find(|candidate| {
        if verified
            .iter()
            .any(|checked| checked.delta == candidate.delta)
        {
            return false;
        }
        let overlap = movement_extent.saturating_sub(candidate.delta);
        let maximum_pixels = u64::from(overlap).saturating_mul(u64::from(perpendicular_extent));
        candidate.pixel_sad_lower_bound < u64::from(threshold).saturating_mul(maximum_pixels)
    })
}

const fn axis_extent(plane: &LumaPlane, axis: ScrollAxis) -> u32 {
    match axis {
        ScrollAxis::Vertical => plane.height(),
        ScrollAxis::Horizontal => plane.width(),
    }
}

fn validate(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    span: AnalysisSpan,
    config: &AlignmentConfig,
) -> Result<(), AlignError> {
    if previous.width() == 0 || previous.height() == 0 {
        return Err(AlignError::Incompatible(
            "frames must have non-zero width and height".to_owned(),
        ));
    }
    if !previous.matches_dimensions(current) {
        return Err(AlignError::Incompatible(format!(
            "{}x{} versus {}x{}",
            previous.width(),
            previous.height(),
            current.width(),
            current.height()
        )));
    }
    let extent = axis_extent(previous, axis);
    if span.is_empty() || span.end > extent {
        return Err(AlignError::Incompatible(format!(
            "analysis span {}..{} is outside the {extent}-pixel {axis:?} extent",
            span.start, span.end
        )));
    }
    if config.min_overlap == 0 {
        return Err(AlignError::Incompatible(
            "minimum overlap must be at least one pixel".to_owned(),
        ));
    }
    Ok(())
}

fn profile_sad(profiles: &CoarseProfiles, span: AnalysisSpan, delta: u32) -> (u32, u64) {
    let overlap = span.len() - delta;
    let samples = overlap.min(MAX_COARSE_AXIS_SAMPLES).max(1);
    let mut distance_sum = 0u64;
    let mut pixel_sad_lower_bound = 0u64;
    for sample in 0..samples {
        let position = sampled_position(sample, samples, overlap);
        let previous_position = span.start + delta + position;
        let current_position = span.start + position;
        distance_sum += u64::from(profiles.distance(previous_position, current_position));
        pixel_sad_lower_bound = pixel_sad_lower_bound
            .saturating_add(profiles.pixel_sad_lower_bound(previous_position, current_position));
    }
    (
        (distance_sum / u64::from(samples)) as u32,
        pixel_sad_lower_bound,
    )
}

const fn sampled_position(sample: u32, samples: u32, overlap: u32) -> u32 {
    if samples <= 1 || overlap <= 1 {
        0
    } else {
        ((sample as u64 * (overlap - 1) as u64) / (samples - 1) as u64) as u32
    }
}

fn pixel_sad(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    span: AnalysisSpan,
    perpendicular: AnalysisSpan,
    delta: u32,
) -> u32 {
    let overlap = span.len() - delta;
    let mut sum = 0u64;
    let mut count = 0u64;
    match axis {
        ScrollAxis::Vertical => {
            for row in 0..overlap {
                let a = &previous.row(span.start + delta + row)
                    [perpendicular.start as usize..perpendicular.end as usize];
                let b = &current.row(span.start + row)
                    [perpendicular.start as usize..perpendicular.end as usize];
                for (&left, &right) in a.iter().zip(b) {
                    sum += u64::from(left.abs_diff(right));
                    count += 1;
                }
            }
        }
        ScrollAxis::Horizontal => {
            let previous_start = (span.start + delta) as usize;
            let current_start = span.start as usize;
            let overlap = overlap as usize;
            for row in perpendicular.start..perpendicular.end {
                let a = &previous.row(row)[previous_start..previous_start + overlap];
                let b = &current.row(row)[current_start..current_start + overlap];
                for (&left, &right) in a.iter().zip(b) {
                    sum += u64::from(left.abs_diff(right));
                    count += 1;
                }
            }
        }
    }
    (sum / count.max(1)) as u32
}

pub(crate) fn stationary_perpendicular_span(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    span: AnalysisSpan,
    measured_delta: u32,
    config: &AlignmentConfig,
) -> AnalysisSpan {
    let extent = match axis {
        ScrollAxis::Vertical => previous.width(),
        ScrollAxis::Horizontal => previous.height(),
    };
    if extent <= 1 {
        return AnalysisSpan::full(extent);
    }
    if measured_delta == 0 || measured_delta >= span.len() {
        return AnalysisSpan::full(extent);
    }

    let percent = config.max_stationary_edge_percent.min(49);
    let max_edge = (u64::from(extent) * u64::from(percent))
        .div_ceil(100)
        .min(u64::from((extent - 1) / 2)) as u32;
    let proves_stationary_chrome = |position| {
        let (zero_error, shifted_error) =
            perpendicular_line_errors(previous, current, axis, span, measured_delta, position);
        zero_error <= config.stationary_edge_match_threshold
            && shifted_error >= zero_error.saturating_add(config.stationary_edge_min_contrast)
    };
    let demonstrates_motion = |position| {
        let (zero_error, shifted_error) =
            perpendicular_line_errors(previous, current, axis, span, measured_delta, position);
        shifted_error <= config.stationary_edge_match_threshold
            && zero_error >= shifted_error.saturating_add(config.stationary_edge_min_contrast)
    };

    let mut start = 0;
    while start < max_edge && proves_stationary_chrome(start) {
        start += 1;
    }
    let mut end = extent;
    while extent - end < max_edge && end > start && proves_stationary_chrome(end - 1) {
        end -= 1;
    }
    let leading_motion = (start..extent)
        .take_while(|&position| demonstrates_motion(position))
        .count() as u32;
    let trailing_motion = (0..end)
        .rev()
        .take_while(|&position| demonstrates_motion(position))
        .count() as u32;
    if start < config.min_stationary_edge || leading_motion < config.min_stationary_edge {
        start = 0;
    }
    if extent - end < config.min_stationary_edge || trailing_motion < config.min_stationary_edge {
        end = extent;
    }
    AnalysisSpan { start, end }
}

fn perpendicular_line_errors(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    span: AnalysisSpan,
    delta: u32,
    position: u32,
) -> (u32, u32) {
    let overlap = span.len() - delta;
    let mut zero_sum = 0u64;
    let mut shifted_sum = 0u64;
    match axis {
        ScrollAxis::Vertical => {
            for row in 0..overlap {
                let current_value = current.row(span.start + row)[position as usize];
                zero_sum += u64::from(
                    previous.row(span.start + row)[position as usize].abs_diff(current_value),
                );
                shifted_sum += u64::from(
                    previous.row(span.start + delta + row)[position as usize]
                        .abs_diff(current_value),
                );
            }
        }
        ScrollAxis::Horizontal => {
            let current_row = current.row(position);
            let previous_row = previous.row(position);
            for column in 0..overlap {
                let current_value = current_row[(span.start + column) as usize];
                zero_sum +=
                    u64::from(previous_row[(span.start + column) as usize].abs_diff(current_value));
                shifted_sum += u64::from(
                    previous_row[(span.start + delta + column) as usize].abs_diff(current_value),
                );
            }
        }
    }
    let count = u64::from(overlap.max(1));
    ((zero_sum / count) as u32, (shifted_sum / count) as u32)
}

fn central_coarse_span(extent: u32, max_edge_percent: u32) -> AnalysisSpan {
    if extent <= 1 {
        return AnalysisSpan::full(extent);
    }
    let edge = (u64::from(extent) * u64::from(max_edge_percent.min(49)))
        .div_ceil(100)
        .min(u64::from((extent - 1) / 2)) as u32;
    AnalysisSpan {
        start: edge,
        end: extent - edge,
    }
}

fn refinement_candidates(centers: &[Candidate], largest: u32, radius: u32) -> Vec<Candidate> {
    let mut refined = Vec::new();
    for center in centers {
        let axis_extent = center.delta.saturating_add(center.overlap);
        let start = center.delta.saturating_sub(radius);
        let end = center.delta.saturating_add(radius).min(largest);
        for delta in start..=end {
            if refined
                .iter()
                .any(|candidate: &Candidate| candidate.delta == delta)
            {
                continue;
            }
            let mut candidate = *center;
            candidate.delta = delta;
            candidate.overlap = axis_extent.saturating_sub(delta);
            refined.push(candidate);
        }
    }
    refined
}

fn verified_basin_minima(candidates: &[Candidate]) -> Vec<Candidate> {
    let mut minima = Vec::new();
    let mut segment_start = 0;
    while segment_start < candidates.len() {
        let mut segment_end = segment_start + 1;
        while segment_end < candidates.len()
            && candidates[segment_end].delta == candidates[segment_end - 1].delta + 1
        {
            segment_end += 1;
        }
        minima.extend(segment_minima(&candidates[segment_start..segment_end]));
        segment_start = segment_end;
    }
    minima
}

fn segment_minima(segment: &[Candidate]) -> Vec<Candidate> {
    let mut minima = Vec::new();
    let mut plateau_start = 0;
    while plateau_start < segment.len() {
        let score = segment[plateau_start].raw_score;
        let mut plateau_end = plateau_start + 1;
        while plateau_end < segment.len() && segment[plateau_end].raw_score == score {
            plateau_end += 1;
        }
        let left = plateau_start
            .checked_sub(1)
            .map_or(u32::MAX, |index| segment[index].raw_score);
        let right = segment
            .get(plateau_end)
            .map_or(u32::MAX, |candidate| candidate.raw_score);
        if score < left
            && score < right
            && let Some(best) = segment[plateau_start..plateau_end]
                .iter()
                .min_by(|left, right| candidate_order(left, right))
        {
            minima.push(*best);
            if plateau_end - plateau_start > 1 {
                let alternative = segment[plateau_start..plateau_end]
                    .iter()
                    .filter(|candidate| candidate.delta != best.delta)
                    .max_by_key(|candidate| candidate.delta.abs_diff(best.delta))
                    .expect("a multi-candidate plateau has an alternative");
                minima.push(*alternative);
            }
        }
        plateau_start = plateau_end;
    }
    minima
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
    left.raw_score
        .cmp(&right.raw_score)
        .then_with(|| left.adjusted_score.cmp(&right.adjusted_score))
        .then_with(|| left.delta.cmp(&right.delta))
}

fn visual_candidate_order(left: &Candidate, right: &Candidate) -> Ordering {
    left.raw_score
        .cmp(&right.raw_score)
        .then_with(|| left.delta.cmp(&right.delta))
}

fn distinct_candidates(sorted: &[Candidate], limit: usize, radius: u32) -> Vec<Candidate> {
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

    selected
}

fn include_expected_basin(
    candidates: &[Candidate],
    selected: &mut Vec<Candidate>,
    expected_delta: Option<u32>,
    radius: u32,
) {
    let Some(expected) = expected_delta else {
        return;
    };
    if selected
        .iter()
        .any(|candidate| candidate.delta.abs_diff(expected) <= radius)
    {
        return;
    }
    let Some(nearest) = candidates
        .iter()
        .min_by_key(|candidate| candidate.delta.abs_diff(expected))
    else {
        return;
    };
    if !selected
        .iter()
        .any(|candidate| candidate.delta == nearest.delta)
    {
        // Keep every visually selected basin. The expected basin is an
        // additional continuity check, never a replacement for raw evidence.
        selected.push(*nearest);
    }
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
    fn the_expected_delta_does_not_waive_repeated_pattern_ambiguity() {
        let document: Vec<u8> = (0..30).map(|v| if v % 4 < 2 { 20 } else { 220 }).collect();
        let first = plane(&document[0..12], 8);
        let second = plane(&document[4..16], 8);
        let error = align_vertical(&first, &second, Some(4), &small())
            .expect_err("a prior cannot make indistinguishable pixels safe");
        assert!(matches!(error, AlignError::Ambiguous { margin: 0, .. }));
    }

    #[test]
    fn the_expected_delta_does_not_collapse_a_flat_alignment_plateau() {
        let first = plane(&[40; 12], 8);
        let second = plane(&[40; 12], 8);
        let error = align_vertical(&first, &second, Some(5), &small())
            .expect_err("flat pixels provide no displacement evidence");
        assert!(matches!(error, AlignError::Ambiguous { margin: 0, .. }));
    }

    #[test]
    fn full_pixel_refinement_recovers_a_nearby_offset_hidden_by_equal_profiles() {
        let rows: Vec<Vec<u8>> = (0..10)
            .map(|index| {
                let first = 5 + index * 3;
                let second = 11 + index * 2;
                vec![first, 100 - first, second, 100 - second]
            })
            .collect();
        let make = |range: std::ops::Range<usize>| {
            LumaPlane::from_raw(
                4,
                range.len() as u32,
                range
                    .flat_map(|index| rows[index].iter().copied())
                    .collect(),
            )
        };
        let first = make(0..7);
        let second = make(1..8);
        let config = AlignmentConfig {
            min_overlap: 3,
            max_delta: Some(3),
            row_buckets: 1,
            top_k: 2,
            basin_radius: 1,
            max_mean_error: 20,
            min_confidence: 1,
            ..AlignmentConfig::default()
        };

        let alignment = align_vertical(&first, &second, Some(1), &config).expect("alignment");
        assert_eq!(alignment.delta, 1);
        assert_eq!(alignment.score, 0);
    }

    #[test]
    fn horizontal_refinement_uses_the_same_verified_basin_topology() {
        let columns: Vec<Vec<u8>> = (0..10)
            .map(|index| {
                let first = 5 + index * 3;
                let second = 11 + index * 2;
                vec![first, 100 - first, second, 100 - second]
            })
            .collect();
        let make = |range: std::ops::Range<usize>| {
            let selected: Vec<_> = range.collect();
            let mut data = Vec::new();
            for (row, _) in columns[0].iter().enumerate() {
                for &column in &selected {
                    data.push(columns[column][row]);
                }
            }
            LumaPlane::from_raw(selected.len() as u32, 4, data)
        };
        let first = make(0..7);
        let second = make(1..8);
        let config = AlignmentConfig {
            min_overlap: 3,
            max_delta: Some(3),
            row_buckets: 1,
            top_k: 2,
            basin_radius: 1,
            max_mean_error: 20,
            min_confidence: 1,
            ..AlignmentConfig::default()
        };

        let alignment = align_horizontal(&first, &second, Some(1), &config).expect("alignment");
        assert_eq!(alignment.delta, 1);
        assert_eq!(alignment.score, 0);
    }

    #[test]
    fn omitted_coarse_basins_are_verified_before_accepting_a_winner() {
        let rows: Vec<Vec<u8>> = (0..14)
            .map(|index| {
                let value = 5 + index * 7;
                vec![value, 200 - value, 200 - value, value]
            })
            .collect();
        let make = |range: std::ops::Range<usize>| {
            LumaPlane::from_raw(
                4,
                range.len() as u32,
                range
                    .flat_map(|index| rows[index].iter().copied())
                    .collect(),
            )
        };
        let first = make(0..8);
        let second = make(4..12);
        let config = AlignmentConfig {
            min_overlap: 3,
            max_delta: Some(4),
            row_buckets: 1,
            top_k: 2,
            basin_radius: 0,
            max_mean_error: 20,
            min_confidence: 1,
            ..AlignmentConfig::default()
        };

        let alignment = align_vertical(&first, &second, None, &config).expect("alignment");
        assert_eq!(alignment.delta, 4);
        assert_eq!(alignment.score, 0);
    }

    #[test]
    fn coarse_work_is_capped_for_a_4k_axis() {
        assert_eq!(MAX_COARSE_AXIS_SAMPLES, 96);
        assert_eq!(
            (0..MAX_COARSE_AXIS_SAMPLES)
                .map(|sample| sampled_position(sample, MAX_COARSE_AXIS_SAMPLES, 3_840))
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            MAX_COARSE_AXIS_SAMPLES as usize
        );
        assert!(
            u64::from(MAX_COARSE_AXIS_SAMPLES) * 3_840
                < u64::from(3_840u32) * u64::from(3_840u32) / 32
        );
    }

    fn edge_evidence_pair(axis: ScrollAxis, delta: u32) -> (LumaPlane, LumaPlane) {
        let (width, height) = match axis {
            ScrollAxis::Vertical => (40, 64),
            ScrollAxis::Horizontal => (64, 40),
        };
        let make = |offset: u32| {
            let mut data = Vec::with_capacity((width * height) as usize);
            for y in 0..height {
                for x in 0..width {
                    let (movement, perpendicular) = match axis {
                        ScrollAxis::Vertical => (y + offset, x),
                        ScrollAxis::Horizontal => (x + offset, y),
                    };
                    let value = if (10..30).contains(&perpendicular) {
                        ((movement % 6) * 37) as u8
                    } else {
                        ((movement * 37 + perpendicular * 11 + movement * perpendicular * 3) % 251)
                            as u8
                    };
                    data.push(value);
                }
            }
            LumaPlane::from_raw(width, height, data)
        };
        (make(0), make(delta))
    }

    fn edge_evidence_config() -> AlignmentConfig {
        AlignmentConfig {
            min_overlap: 16,
            max_delta: Some(32),
            row_buckets: 16,
            top_k: 2,
            basin_radius: 1,
            max_mean_error: 4,
            min_confidence: 2,
            max_stationary_edge_percent: 25,
            ..AlignmentConfig::default()
        }
    }

    #[test]
    fn vertical_coarse_search_keeps_true_motion_visible_at_unproven_side_edges() {
        let (first, second) = edge_evidence_pair(ScrollAxis::Vertical, 24);
        let alignment =
            align_vertical(&first, &second, None, &edge_evidence_config()).expect("alignment");
        assert_eq!(alignment.delta, 24);
        assert_eq!(alignment.score, 0);
    }

    #[test]
    fn horizontal_coarse_search_keeps_true_motion_visible_at_unproven_top_and_bottom_edges() {
        let (first, second) = edge_evidence_pair(ScrollAxis::Horizontal, 24);
        let alignment =
            align_horizontal(&first, &second, None, &edge_evidence_config()).expect("alignment");
        assert_eq!(alignment.delta, 24);
        assert_eq!(alignment.score, 0);
    }

    fn perpendicular_chrome_pair(axis: ScrollAxis) -> (LumaPlane, LumaPlane) {
        let (width, height) = (12, 12);
        let make = |offset: u32| {
            let mut data = Vec::with_capacity((width * height) as usize);
            for y in 0..height {
                for x in 0..width {
                    let (movement, perpendicular) = match axis {
                        ScrollAxis::Vertical => (y + offset, x),
                        ScrollAxis::Horizontal => (x + offset, y),
                    };
                    let value = if perpendicular < 3 {
                        let screen_position = match axis {
                            ScrollAxis::Vertical => y,
                            ScrollAxis::Horizontal => x,
                        };
                        ((screen_position * 31 + perpendicular * 7 + 40) % 251) as u8
                    } else {
                        ((movement * 17 + perpendicular * 23) % 251) as u8
                    };
                    data.push(value);
                }
            }
            LumaPlane::from_raw(width, height, data)
        };
        (make(0), make(2))
    }

    #[test]
    fn patterned_left_sidebar_is_proven_at_zero_but_not_at_the_measured_delta() {
        let (first, second) = perpendicular_chrome_pair(ScrollAxis::Vertical);
        let span = stationary_perpendicular_span(
            &first,
            &second,
            ScrollAxis::Vertical,
            AnalysisSpan::full(first.height()),
            2,
            &AlignmentConfig {
                min_stationary_edge: 2,
                max_stationary_edge_percent: 25,
                ..AlignmentConfig::default()
            },
        );
        assert_eq!(span, AnalysisSpan { start: 3, end: 12 });
    }

    #[test]
    fn patterned_top_band_is_proven_for_horizontal_scrolling() {
        let (first, second) = perpendicular_chrome_pair(ScrollAxis::Horizontal);
        let span = stationary_perpendicular_span(
            &first,
            &second,
            ScrollAxis::Horizontal,
            AnalysisSpan::full(first.width()),
            2,
            &AlignmentConfig {
                min_stationary_edge: 2,
                max_stationary_edge_percent: 25,
                ..AlignmentConfig::default()
            },
        );
        assert_eq!(span, AnalysisSpan { start: 3, end: 12 });
    }

    #[test]
    fn a_flat_edge_that_also_matches_at_the_measured_delta_is_not_cropped() {
        let make = |offset: u32| {
            let mut data = Vec::new();
            for y in 0..12 {
                for x in 0..12 {
                    let value = if x < 3 {
                        40
                    } else {
                        ((y + offset) * 17 + x * 23) as u8
                    };
                    data.push(value);
                }
            }
            LumaPlane::from_raw(12, 12, data)
        };
        let first = make(0);
        let second = make(2);
        let span = stationary_perpendicular_span(
            &first,
            &second,
            ScrollAxis::Vertical,
            AnalysisSpan::full(first.height()),
            2,
            &AlignmentConfig {
                min_stationary_edge: 2,
                max_stationary_edge_percent: 25,
                ..AlignmentConfig::default()
            },
        );
        assert_eq!(span, AnalysisSpan::full(12));
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
