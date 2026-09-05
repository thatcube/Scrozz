//! Detection of fixed chrome at the leading and trailing scroll edges.
//!
//! Fixed chrome has a stronger definition than "these rows or columns look
//! alike". First its whole edge run must match at the same screen coordinates;
//! then pixels beyond that run must demonstrate the measured displacement. The
//! boundary-level proof matters because a solid toolbar can also match itself
//! when sampled at the displacement.

use scrozz_core::ScrollAxis;

use crate::{
    align::{AnalysisBand, AnalysisSpan},
    luma::LumaPlane,
};

/// Sticky rows excluded from the scrolling content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChromeBands {
    /// Fixed rows at the top.
    pub top: u32,
    /// Fixed rows at the bottom.
    pub bottom: u32,
}

impl ChromeBands {
    /// The moving content between the two fixed bands.
    #[must_use]
    pub fn content_band(self, height: u32) -> AnalysisBand {
        AnalysisBand {
            top: self.top.min(height),
            bottom: height.saturating_sub(self.bottom).max(self.top.min(height)),
        }
    }
}

/// Sticky columns excluded from horizontally scrolling content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SideChromeBands {
    /// Fixed columns at the left.
    pub left: u32,
    /// Fixed columns at the right.
    pub right: u32,
}

impl SideChromeBands {
    /// The moving content between the two fixed side bands.
    #[must_use]
    pub fn content_span(self, width: u32) -> AnalysisSpan {
        AnalysisSpan {
            start: self.left.min(width),
            end: width.saturating_sub(self.right).max(self.left.min(width)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct AxisChromeBands {
    pub(crate) leading: u32,
    pub(crate) trailing: u32,
}

impl AxisChromeBands {
    pub(crate) fn content_span(self, extent: u32) -> AnalysisSpan {
        AnalysisSpan {
            start: self.leading.min(extent),
            end: extent
                .saturating_sub(self.trailing)
                .max(self.leading.min(extent)),
        }
    }
}

impl From<ChromeBands> for AxisChromeBands {
    fn from(value: ChromeBands) -> Self {
        Self {
            leading: value.top,
            trailing: value.bottom,
        }
    }
}

impl From<SideChromeBands> for AxisChromeBands {
    fn from(value: SideChromeBands) -> Self {
        Self {
            leading: value.left,
            trailing: value.right,
        }
    }
}

/// Thresholds for sticky-chrome detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeConfig {
    /// Same-coordinate mean error still considered a match.
    pub zero_match_max: u32,
    /// Contrast required where the measured displacement crosses a fixed-band
    /// boundary into moving content.
    pub shifted_mismatch_min: u32,
    /// Ignore thinner runs; one matching separator line is not chrome.
    pub min_band: u32,
    /// Maximum combined chrome extent as a percentage of the scroll axis.
    pub max_height_percent: u32,
}

impl Default for ChromeConfig {
    fn default() -> Self {
        Self {
            zero_match_max: 3,
            shifted_mismatch_min: 10,
            min_band: 4,
            max_height_percent: 35,
        }
    }
}

/// Detects fixed top and bottom bands in one aligned frame pair.
#[must_use]
pub fn detect_sticky_chrome(
    previous: &LumaPlane,
    current: &LumaPlane,
    measured_delta: u32,
    config: &ChromeConfig,
) -> ChromeBands {
    let bands = detect_sticky_axis_chrome(
        previous,
        current,
        ScrollAxis::Vertical,
        measured_delta,
        config,
    );
    ChromeBands {
        top: bands.leading,
        bottom: bands.trailing,
    }
}

/// Detects fixed left and right bands in one horizontally aligned frame pair.
#[must_use]
pub fn detect_sticky_side_chrome(
    previous: &LumaPlane,
    current: &LumaPlane,
    measured_delta: u32,
    config: &ChromeConfig,
) -> SideChromeBands {
    let bands = detect_sticky_axis_chrome(
        previous,
        current,
        ScrollAxis::Horizontal,
        measured_delta,
        config,
    );
    SideChromeBands {
        left: bands.leading,
        right: bands.trailing,
    }
}

/// Takes the conservative minimum seen across every adjacent frame pair.
///
/// A fixed band is global only when every pair proves it. One zero therefore
/// removes the band, which is intentionally safer than cropping real content.
#[must_use]
pub fn conservative_chrome<I>(pairs: I, height: u32, config: &ChromeConfig) -> ChromeBands
where
    I: IntoIterator<Item = ChromeBands>,
{
    let bands =
        conservative_axis_chrome(pairs.into_iter().map(AxisChromeBands::from), height, config);
    ChromeBands {
        top: bands.leading,
        bottom: bands.trailing,
    }
}

/// Takes the conservative side-band minimum seen across adjacent frame pairs.
#[must_use]
pub fn conservative_side_chrome<I>(pairs: I, width: u32, config: &ChromeConfig) -> SideChromeBands
where
    I: IntoIterator<Item = SideChromeBands>,
{
    let bands =
        conservative_axis_chrome(pairs.into_iter().map(AxisChromeBands::from), width, config);
    SideChromeBands {
        left: bands.leading,
        right: bands.trailing,
    }
}

pub(crate) fn detect_sticky_axis_chrome(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    delta: u32,
    config: &ChromeConfig,
) -> AxisChromeBands {
    let perpendicular_extent = match axis {
        ScrollAxis::Vertical => previous.width(),
        ScrollAxis::Horizontal => previous.height(),
    };
    detect_sticky_axis_chrome_in(
        previous,
        current,
        axis,
        delta,
        AnalysisSpan::full(perpendicular_extent),
        config,
    )
}

pub(crate) fn detect_sticky_axis_chrome_in(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    delta: u32,
    perpendicular: AnalysisSpan,
    config: &ChromeConfig,
) -> AxisChromeBands {
    if delta == 0 || !previous.matches_dimensions(current) {
        return AxisChromeBands::default();
    }
    let perpendicular_extent = match axis {
        ScrollAxis::Vertical => previous.width(),
        ScrollAxis::Horizontal => previous.height(),
    };
    if perpendicular.is_empty() || perpendicular.end > perpendicular_extent {
        return AxisChromeBands::default();
    }

    let extent = axis_extent(previous, axis);
    let cap = (u64::from(extent) * u64::from(config.max_height_percent.min(100)) / 100) as u32;
    if cap < config.min_band {
        return AxisChromeBands::default();
    }

    cap_combined(
        AxisChromeBands {
            leading: detect_prefix(previous, current, axis, delta, perpendicular, cap, config),
            trailing: detect_suffix(previous, current, axis, delta, perpendicular, cap, config),
        },
        cap,
        config.min_band,
    )
}

pub(crate) fn conservative_axis_chrome<I>(
    pairs: I,
    extent: u32,
    config: &ChromeConfig,
) -> AxisChromeBands
where
    I: IntoIterator<Item = AxisChromeBands>,
{
    let mut pairs = pairs.into_iter();
    let Some(first) = pairs.next() else {
        return AxisChromeBands::default();
    };
    let result = pairs.fold(first, |current, next| AxisChromeBands {
        leading: current.leading.min(next.leading),
        trailing: current.trailing.min(next.trailing),
    });
    let cap = (u64::from(extent) * u64::from(config.max_height_percent.min(100)) / 100) as u32;
    cap_combined(result, cap, config.min_band)
}

fn detect_prefix(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    delta: u32,
    perpendicular: AnalysisSpan,
    cap: u32,
    config: &ChromeConfig,
) -> u32 {
    let extent = axis_extent(previous, axis);
    let length = (0..cap)
        .take_while(|&position| {
            rectangular_sad(
                previous,
                position,
                current,
                position,
                1,
                axis,
                perpendicular,
            ) <= config.zero_match_max
        })
        .count() as u32;
    if length >= config.min_band
        && boundary_proves_fixed_band(
            previous,
            current,
            axis,
            delta,
            perpendicular,
            length,
            false,
            config,
        )
        && has_shifted_content_evidence(
            previous,
            current,
            axis,
            delta,
            perpendicular,
            AnalysisSpan {
                start: length,
                end: extent,
            },
            config,
        )
    {
        length
    } else {
        0
    }
}

fn detect_suffix(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    delta: u32,
    perpendicular: AnalysisSpan,
    cap: u32,
    config: &ChromeConfig,
) -> u32 {
    let extent = axis_extent(previous, axis);
    let length = (0..cap)
        .take_while(|&offset| {
            let position = extent - 1 - offset;
            rectangular_sad(
                previous,
                position,
                current,
                position,
                1,
                axis,
                perpendicular,
            ) <= config.zero_match_max
        })
        .count() as u32;
    if length >= config.min_band
        && boundary_proves_fixed_band(
            previous,
            current,
            axis,
            delta,
            perpendicular,
            length,
            true,
            config,
        )
        && has_shifted_content_evidence(
            previous,
            current,
            axis,
            delta,
            perpendicular,
            AnalysisSpan {
                start: 0,
                end: extent.saturating_sub(length),
            },
            config,
        )
    {
        length
    } else {
        0
    }
}

#[allow(clippy::too_many_arguments)]
fn boundary_proves_fixed_band(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    delta: u32,
    perpendicular: AnalysisSpan,
    length: u32,
    trailing: bool,
    config: &ChromeConfig,
) -> bool {
    let extent = axis_extent(previous, axis);
    let samples = config.min_band.min(delta).min(length);
    if samples == 0 {
        return false;
    }
    let start = if trailing {
        extent - length
    } else {
        length - samples
    };
    // A solid toolbar matches itself away from its boundary. Require contrast
    // against the moving content at that boundary, not inside every fixed row.
    (start..start + samples).all(|position| {
        let pair = if trailing {
            position
                .checked_sub(delta)
                .map(|shifted| (position, shifted))
        } else {
            position
                .checked_add(delta)
                .filter(|shifted| *shifted < extent)
                .map(|shifted| (shifted, position))
        };
        pair.is_some_and(|(prior, next)| {
            rectangular_sad(previous, prior, current, next, 1, axis, perpendicular)
                >= config.shifted_mismatch_min
        })
    })
}

fn has_shifted_content_evidence(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    delta: u32,
    perpendicular: AnalysisSpan,
    content: AnalysisSpan,
    config: &ChromeConfig,
) -> bool {
    let end = content
        .end
        .min(axis_extent(previous, axis).saturating_sub(delta));
    let mut run = 0;
    for position in content.start..end {
        let zero = rectangular_sad(
            previous,
            position,
            current,
            position,
            1,
            axis,
            perpendicular,
        );
        let shifted = rectangular_sad(
            previous,
            position + delta,
            current,
            position,
            1,
            axis,
            perpendicular,
        );
        if shifted <= config.zero_match_max && zero > config.zero_match_max {
            run += 1;
            if run >= config.min_band {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

fn rectangular_sad(
    first: &LumaPlane,
    first_y: u32,
    second: &LumaPlane,
    second_y: u32,
    length: u32,
    axis: ScrollAxis,
    perpendicular: AnalysisSpan,
) -> u32 {
    let mut sum = 0u64;
    let mut count = 0u64;
    match axis {
        ScrollAxis::Vertical => {
            for row in 0..length {
                let first = &first.row(first_y + row)
                    [perpendicular.start as usize..perpendicular.end as usize];
                let second = &second.row(second_y + row)
                    [perpendicular.start as usize..perpendicular.end as usize];
                for (&left, &right) in first.iter().zip(second) {
                    sum += u64::from(left.abs_diff(right));
                    count += 1;
                }
            }
        }
        ScrollAxis::Horizontal => {
            let first_start = first_y as usize;
            let second_start = second_y as usize;
            let length = length as usize;
            for row in perpendicular.start..perpendicular.end {
                let first = &first.row(row)[first_start..first_start + length];
                let second = &second.row(row)[second_start..second_start + length];
                for (&left, &right) in first.iter().zip(second) {
                    sum += u64::from(left.abs_diff(right));
                    count += 1;
                }
            }
        }
    }
    (sum / count.max(1)) as u32
}

fn cap_combined(mut bands: AxisChromeBands, cap: u32, min_band: u32) -> AxisChromeBands {
    let excess = bands
        .leading
        .saturating_add(bands.trailing)
        .saturating_sub(cap);
    if excess == 0 {
        return bands;
    }

    if bands.leading >= bands.trailing {
        let from_leading = excess.min(bands.leading);
        bands.leading -= from_leading;
        bands.trailing = bands.trailing.saturating_sub(excess - from_leading);
    } else {
        let from_trailing = excess.min(bands.trailing);
        bands.trailing -= from_trailing;
        bands.leading = bands.leading.saturating_sub(excess - from_trailing);
    }
    if bands.leading < min_band {
        bands.leading = 0;
    }
    if bands.trailing < min_band {
        bands.trailing = 0;
    }
    bands
}

const fn axis_extent(plane: &LumaPlane, axis: ScrollAxis) -> u32 {
    match axis {
        ScrollAxis::Vertical => plane.height(),
        ScrollAxis::Horizontal => plane.width(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(rows: &[u8], width: u32) -> LumaPlane {
        LumaPlane::from_raw(
            width,
            rows.len() as u32,
            rows.iter()
                .flat_map(|row| std::iter::repeat_n(*row, width as usize))
                .collect(),
        )
    }

    fn config() -> ChromeConfig {
        ChromeConfig {
            min_band: 2,
            max_height_percent: 40,
            ..ChromeConfig::default()
        }
    }

    #[test]
    fn fixed_header_matches_zero_but_not_the_measured_delta() {
        let first = plane(&[10, 10, 30, 50, 70, 90, 110, 130, 150, 170], 8);
        let second = plane(&[10, 10, 70, 90, 110, 130, 150, 170, 190, 210], 8);
        let chrome = detect_sticky_chrome(&first, &second, 2, &config());
        assert_eq!(chrome.top, 2);
    }

    #[test]
    fn solid_fixed_bands_larger_than_the_scroll_step_are_detected() {
        let first = plane(
            &[
                10, 10, 10, 10, 10, 10, 40, 60, 80, 100, 120, 140, 160, 180, 240, 240, 240, 240,
            ],
            8,
        );
        let second = plane(
            &[
                10, 10, 10, 10, 10, 10, 80, 100, 120, 140, 160, 180, 200, 220, 240, 240, 240, 240,
            ],
            8,
        );
        let settings = ChromeConfig {
            max_height_percent: 60,
            ..config()
        };
        assert_eq!(
            detect_sticky_chrome(&first, &second, 2, &settings),
            ChromeBands { top: 6, bottom: 4 }
        );
    }

    #[test]
    fn fixed_edges_are_detected_when_body_motion_is_low_contrast_but_distinct() {
        let first = plane(
            &[
                10, 10, 10, 10, 30, 33, 36, 39, 42, 45, 48, 51, 54, 57, 250, 250,
            ],
            8,
        );
        let second = plane(
            &[
                10, 10, 10, 10, 36, 39, 42, 45, 48, 51, 54, 57, 60, 63, 250, 250,
            ],
            8,
        );
        assert_eq!(
            detect_sticky_chrome(&first, &second, 2, &config()),
            ChromeBands { top: 4, bottom: 2 }
        );
    }

    #[test]
    fn a_flat_band_that_also_matches_at_the_measured_delta_is_not_cropped() {
        let first = plane(&[10, 10, 10, 10, 60, 80, 100, 120, 140, 160], 8);
        let second = plane(&[10, 10, 60, 80, 100, 120, 140, 160, 180, 200], 8);
        let chrome = detect_sticky_chrome(&first, &second, 2, &config());
        assert_eq!(chrome.top, 0);
    }

    #[test]
    fn moving_rows_cannot_be_diluted_into_a_fixed_header() {
        let first = plane(&[10, 20, 30, 40, 50, 55, 60, 65, 70, 75], 8);
        let second = plane(&[10, 20, 30, 40, 60, 65, 70, 75, 80, 85], 8);
        let chrome = detect_sticky_chrome(
            &first,
            &second,
            2,
            &ChromeConfig {
                min_band: 2,
                max_height_percent: 60,
                ..ChromeConfig::default()
            },
        );
        assert_eq!(chrome.top, 4);
    }

    #[test]
    fn the_global_answer_is_the_minimum_across_pairs_and_capped() {
        let config = ChromeConfig {
            max_height_percent: 35,
            ..ChromeConfig::default()
        };
        let bands = conservative_chrome(
            [
                ChromeBands {
                    top: 30,
                    bottom: 20,
                },
                ChromeBands {
                    top: 25,
                    bottom: 10,
                },
            ],
            100,
            &config,
        );
        assert_eq!(
            bands,
            ChromeBands {
                top: 25,
                bottom: 10
            }
        );
    }

    #[test]
    fn the_combined_cap_does_not_invent_a_subminimum_band() {
        let config = ChromeConfig {
            min_band: 4,
            max_height_percent: 50,
            ..ChromeConfig::default()
        };
        let bands = conservative_chrome([ChromeBands { top: 4, bottom: 4 }], 10, &config);
        assert_eq!(bands, ChromeBands { top: 0, bottom: 4 });
    }
}
