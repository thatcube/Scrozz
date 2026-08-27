//! Detection of fixed headers and footers.
//!
//! Fixed chrome has a stronger definition than "these rows look alike". It must
//! match at the same screen coordinates and fail to match at the measured scroll
//! displacement. That second condition is what prevents a blank page margin from
//! being mistaken for a sticky toolbar.

use crate::{align::AnalysisBand, luma::LumaPlane};

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

/// Thresholds for sticky-chrome detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeConfig {
    /// Same-coordinate mean error still considered a match.
    pub zero_match_max: u32,
    /// Shifted mean error required to prove the band did not move.
    pub shifted_mismatch_min: u32,
    /// Ignore thinner runs; one matching separator line is not chrome.
    pub min_band: u32,
    /// Maximum combined chrome height as a percentage of the frame.
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
    if measured_delta == 0
        || !previous.matches_width(current)
        || previous.height() != current.height()
    {
        return ChromeBands::default();
    }

    let height = previous.height();
    let cap = (u64::from(height) * u64::from(config.max_height_percent.min(100)) / 100) as u32;
    if cap < config.min_band {
        return ChromeBands::default();
    }

    let top = detect_prefix(previous, current, measured_delta, cap, config);
    let bottom = detect_suffix(previous, current, measured_delta, cap, config);
    cap_combined(ChromeBands { top, bottom }, cap)
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
    let mut pairs = pairs.into_iter();
    let Some(first) = pairs.next() else {
        return ChromeBands::default();
    };
    let mut result = pairs.fold(first, |current, next| ChromeBands {
        top: current.top.min(next.top),
        bottom: current.bottom.min(next.bottom),
    });
    let cap = (u64::from(height) * u64::from(config.max_height_percent.min(100)) / 100) as u32;
    result = cap_combined(result, cap);
    result
}

fn detect_prefix(
    previous: &LumaPlane,
    current: &LumaPlane,
    delta: u32,
    cap: u32,
    config: &ChromeConfig,
) -> u32 {
    let max = cap.min(previous.height().saturating_sub(delta));
    let mut best = 0;
    for rows in config.min_band..=max {
        let zero = rectangular_sad(previous, 0, current, 0, rows);
        let shifted = rectangular_sad(previous, delta, current, 0, rows);
        if zero <= config.zero_match_max && shifted >= config.shifted_mismatch_min {
            best = rows;
        }
    }
    best
}

fn detect_suffix(
    previous: &LumaPlane,
    current: &LumaPlane,
    delta: u32,
    cap: u32,
    config: &ChromeConfig,
) -> u32 {
    let height = previous.height();
    let max = cap.min(height.saturating_sub(delta));
    let mut best = 0;
    for rows in config.min_band..=max {
        let start = height - rows;
        let zero = rectangular_sad(previous, start, current, start, rows);
        let shifted = rectangular_sad(previous, start, current, start - delta.min(start), rows);
        if start >= delta && zero <= config.zero_match_max && shifted >= config.shifted_mismatch_min
        {
            best = rows;
        }
    }
    best
}

fn rectangular_sad(
    first: &LumaPlane,
    first_y: u32,
    second: &LumaPlane,
    second_y: u32,
    rows: u32,
) -> u32 {
    let mut sum = 0u64;
    let mut count = 0u64;
    for row in 0..rows {
        for (&left, &right) in first
            .row(first_y + row)
            .iter()
            .zip(second.row(second_y + row))
        {
            sum += u64::from(left.abs_diff(right));
            count += 1;
        }
    }
    (sum / count.max(1)) as u32
}

fn cap_combined(mut bands: ChromeBands, cap: u32) -> ChromeBands {
    let excess = bands.top.saturating_add(bands.bottom).saturating_sub(cap);
    if excess == 0 {
        return bands;
    }

    if bands.top >= bands.bottom {
        let from_top = excess.min(bands.top);
        bands.top -= from_top;
        bands.bottom = bands.bottom.saturating_sub(excess - from_top);
    } else {
        let from_bottom = excess.min(bands.bottom);
        bands.bottom -= from_bottom;
        bands.top = bands.top.saturating_sub(excess - from_bottom);
    }
    bands
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
    fn a_flat_band_that_matches_both_offsets_is_not_sticky() {
        let first = plane(&[10, 10, 10, 10, 60, 80, 100, 120, 140, 160], 8);
        let second = plane(&[10, 10, 60, 80, 100, 120, 140, 160, 180, 200], 8);
        let chrome = detect_sticky_chrome(&first, &second, 2, &config());
        assert_eq!(chrome.top, 0);
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
}
