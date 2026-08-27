//! Incremental scrolling-capture assembly.

use scrozz_core::{Error, Frame, PhysicalSize, Result};

use crate::{
    Stitcher,
    align::{AlignError, Alignment, AlignmentConfig, AnalysisBand, align_vertical_in},
    chrome::{ChromeBands, ChromeConfig, conservative_chrome, detect_sticky_chrome},
    luma::LumaPlane,
};

/// How cleanly one frame overlaps the preceding frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeamQuality {
    /// Mean absolute luma difference over the overlap. Zero is exact.
    pub mean_absolute_error: u32,
    /// Raw margin to the next distinct alignment basin.
    pub confidence: u32,
}

/// Why a completed stitch stopped accepting new content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Repeated frames show the viewport reached the end.
    EndOfContent,
    /// The caller chose to stop while retaining the partial image.
    Cancelled,
    /// The configured frame limit was reached.
    FrameLimit,
}

/// Result of adding one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// The first frame is retained but no canvas is allocated yet.
    Started,
    /// New document rows were appended.
    Advanced {
        /// Measured physical-pixel displacement.
        delta: u32,
        /// Quality of this seam.
        seam: SeamQuality,
        /// Current stitched height.
        output_height: u32,
    },
    /// The viewport did not move yet.
    NoMovement {
        /// Consecutive stationary observations.
        stalls: u32,
    },
    /// Enough stationary observations prove the document has ended.
    EndOfContent {
        /// Consecutive stationary observations.
        stalls: u32,
    },
    /// The frames cannot be joined without guessing.
    InsufficientOverlap {
        /// Diagnostic reason from alignment.
        reason: String,
    },
}

/// Aggregate facts about the frames accepted so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StitchSummary {
    /// Non-duplicate frames retained.
    pub frames: usize,
    /// Successful seams.
    pub seams: usize,
    /// Current output height, zero before a frame arrives.
    pub output_height: u32,
    /// Conservatively detected fixed chrome.
    pub chrome: ChromeBands,
    /// Consecutive stationary observations.
    pub stalls: u32,
}

/// Tuning for one stitch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StitchConfig {
    /// Alignment thresholds.
    pub alignment: AlignmentConfig,
    /// Sticky-chrome thresholds.
    pub chrome: ChromeConfig,
    /// Stationary frames required to declare end-of-content.
    pub stall_limit: u32,
    /// Deltas at or below this many rows count as no movement.
    pub movement_epsilon: u32,
    /// Prior supplied by requested scroll distance, in physical rows.
    pub expected_delta: Option<u32>,
}

impl Default for StitchConfig {
    fn default() -> Self {
        Self {
            alignment: AlignmentConfig::default(),
            chrome: ChromeConfig::default(),
            stall_limit: 2,
            movement_epsilon: 1,
            expected_delta: None,
        }
    }
}

/// A deterministic vertical scrolling stitcher.
pub struct ScrollStitcher {
    config: StitchConfig,
    frames: Vec<Frame>,
    luma: Vec<LumaPlane>,
    pair_chrome: Vec<ChromeBands>,
    alignments: Vec<Alignment>,
    canvas: Option<Frame>,
    stalls: u32,
}

impl std::fmt::Debug for ScrollStitcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScrollStitcher")
            .field("config", &self.config)
            .field("frames", &self.frames.len())
            .field("pair_chrome", &self.pair_chrome)
            .field("alignments", &self.alignments)
            .field("canvas_height", &self.canvas.as_ref().map(Frame::height))
            .field("stalls", &self.stalls)
            .finish()
    }
}

impl ScrollStitcher {
    /// Creates an empty stitcher.
    #[must_use]
    pub fn new(config: StitchConfig) -> Self {
        Self {
            config,
            frames: Vec::new(),
            luma: Vec::new(),
            pair_chrome: Vec::new(),
            alignments: Vec::new(),
            canvas: None,
            stalls: 0,
        }
    }

    /// Updates the displacement prior once the first frame reveals its scale.
    pub fn set_expected_delta(&mut self, expected_delta: Option<u32>) {
        self.config.expected_delta = expected_delta;
    }

    /// Adds a frame and reports whether the document advanced.
    pub fn push_frame(&mut self, frame: Frame) -> Result<PushOutcome> {
        let plane = LumaPlane::from_frame(&frame)?;
        if let Some(first) = self.frames.first() {
            validate_compatible(first, &frame)?;
        }

        if self.frames.is_empty() {
            self.frames.push(frame);
            self.luma.push(plane);
            return Ok(PushOutcome::Started);
        }

        let previous = self.luma.last().expect("a retained frame has luma");
        if previous == &plane {
            return Ok(self.stationary());
        }

        let initial = match provisional_alignment(
            previous,
            &plane,
            self.config.expected_delta,
            &self.config.alignment,
            &self.config.chrome,
        ) {
            Ok(alignment) => alignment,
            Err(error) => {
                return Ok(PushOutcome::InsufficientOverlap {
                    reason: error.to_string(),
                });
            }
        };
        if initial.delta <= self.config.movement_epsilon {
            return Ok(self.stationary());
        }

        let pair_chrome =
            detect_sticky_chrome(previous, &plane, initial.delta, &self.config.chrome);
        self.frames.push(frame);
        self.luma.push(plane);
        self.pair_chrome.push(pair_chrome);

        let chrome = self.chrome();
        let band = chrome.content_band(self.frames[0].height());
        match self.realign_all(band) {
            Ok(()) => {}
            Err(error) => {
                self.frames.pop();
                self.luma.pop();
                self.pair_chrome.pop();
                return Ok(PushOutcome::InsufficientOverlap {
                    reason: error.to_string(),
                });
            }
        }

        self.stalls = 0;
        self.canvas = Some(build_canvas(&self.frames, &self.alignments, chrome)?);
        let alignment = *self.alignments.last().expect("the new pair was aligned");
        Ok(PushOutcome::Advanced {
            delta: alignment.delta,
            seam: SeamQuality {
                mean_absolute_error: alignment.score,
                confidence: alignment.confidence,
            },
            output_height: self.canvas.as_ref().map_or(0, Frame::height),
        })
    }

    /// Facts about the partial result.
    #[must_use]
    pub fn summary(&self) -> StitchSummary {
        StitchSummary {
            frames: self.frames.len(),
            seams: self.alignments.len(),
            output_height: self.canvas.as_ref().map_or_else(
                || self.frames.first().map_or(0, Frame::height),
                Frame::height,
            ),
            chrome: self.chrome(),
            stalls: self.stalls,
        }
    }

    /// Produces the current image.
    pub fn finish_frame(mut self) -> Result<Frame> {
        match self.frames.len() {
            0 => Err(Error::InvalidRequest(
                "cannot finish a scrolling capture with no frames".to_owned(),
            )),
            1 => Ok(self.frames.pop().expect("one frame")),
            _ => self.canvas.take().ok_or_else(|| {
                Error::Platform("scrolling canvas was not built after the second frame".to_owned())
            }),
        }
    }

    fn stationary(&mut self) -> PushOutcome {
        self.stalls = self.stalls.saturating_add(1);
        if self.stalls >= self.config.stall_limit.max(1) {
            PushOutcome::EndOfContent {
                stalls: self.stalls,
            }
        } else {
            PushOutcome::NoMovement {
                stalls: self.stalls,
            }
        }
    }

    fn chrome(&self) -> ChromeBands {
        let Some(first) = self.frames.first() else {
            return ChromeBands::default();
        };
        conservative_chrome(
            self.pair_chrome.iter().copied(),
            first.height(),
            &self.config.chrome,
        )
    }

    fn realign_all(&mut self, band: crate::AnalysisBand) -> Result<(), AlignError> {
        let mut alignments = Vec::with_capacity(self.luma.len().saturating_sub(1));
        for pair in self.luma.windows(2) {
            alignments.push(align_vertical_in(
                &pair[0],
                &pair[1],
                band,
                self.config.expected_delta,
                &self.config.alignment,
            )?);
        }
        self.alignments = alignments;
        Ok(())
    }
}

impl Default for ScrollStitcher {
    fn default() -> Self {
        Self::new(StitchConfig::default())
    }
}

impl Stitcher for ScrollStitcher {
    fn push(&mut self, frame: Frame) -> Result<()> {
        match self.push_frame(frame)? {
            PushOutcome::InsufficientOverlap { reason } => Err(Error::InvalidRequest(format!(
                "scrolling frames do not overlap safely: {reason}"
            ))),
            _ => Ok(()),
        }
    }

    fn finish(self: Box<Self>) -> Result<Frame> {
        self.finish_frame()
    }
}

fn provisional_alignment(
    previous: &LumaPlane,
    current: &LumaPlane,
    expected_delta: Option<u32>,
    alignment: &AlignmentConfig,
    chrome: &ChromeConfig,
) -> Result<Alignment, AlignError> {
    let height = previous.height();
    let cap = (u64::from(height) * u64::from(chrome.max_height_percent.min(100)) / 100) as u32;
    let top_half = cap / 2;
    let bottom_half = cap - top_half;
    let bands = [
        AnalysisBand::full(height),
        AnalysisBand {
            top: cap,
            bottom: height,
        },
        AnalysisBand {
            top: 0,
            bottom: height.saturating_sub(cap),
        },
        AnalysisBand {
            top: top_half,
            bottom: height.saturating_sub(bottom_half),
        },
    ];

    let mut best = None;
    let mut first_error = None;
    for band in bands {
        if band.height() < alignment.min_overlap {
            continue;
        }
        match align_vertical_in(previous, current, band, expected_delta, alignment) {
            Ok(candidate) => {
                let key = (
                    candidate.score,
                    expected_delta.map_or(0, |expected| candidate.delta.abs_diff(expected)),
                    std::cmp::Reverse(candidate.confidence),
                );
                if best.as_ref().is_none_or(|(_, best_key)| key < *best_key) {
                    best = Some((candidate, key));
                }
            }
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }

    best.map(|(alignment, _)| alignment)
        .ok_or_else(|| first_error.unwrap_or(AlignError::InsufficientOverlap))
}

fn validate_compatible(first: &Frame, next: &Frame) -> Result<()> {
    if first.width() != next.width()
        || first.height() != next.height()
        || first.format != next.format
        || first.color_space != next.color_space
        || first.scale != next.scale
    {
        return Err(Error::InvalidRequest(format!(
            "scrolling frames changed geometry or pixel interpretation: \
             {}x{} {:?} {:?} {}x versus {}x{} {:?} {:?} {}x",
            first.width(),
            first.height(),
            first.format,
            first.color_space,
            first.scale.get(),
            next.width(),
            next.height(),
            next.format,
            next.color_space,
            next.scale.get()
        )));
    }
    Ok(())
}

fn build_canvas(frames: &[Frame], alignments: &[Alignment], chrome: ChromeBands) -> Result<Frame> {
    let first = frames.first().ok_or_else(|| {
        Error::InvalidRequest("cannot build a scrolling canvas without frames".to_owned())
    })?;
    let band = chrome.content_band(first.height());
    let content_height = band.height();
    if content_height == 0 {
        return Err(Error::InvalidRequest(
            "sticky chrome consumed the whole scrolling frame".to_owned(),
        ));
    }

    let output_height = alignments
        .iter()
        .try_fold(content_height, |height, alignment| {
            height.checked_add(alignment.delta).ok_or_else(|| {
                Error::InvalidRequest("scrolling capture is too tall to represent".to_owned())
            })
        })?;
    let row_bytes = first.width() as usize * first.format.bytes_per_pixel();
    let capacity = row_bytes
        .checked_mul(output_height as usize)
        .ok_or_else(|| Error::InvalidRequest("scrolling capture is too large".to_owned()))?;
    let mut data = Vec::with_capacity(capacity);
    append_rows(&mut data, first, band.top, content_height)?;

    for (frame, alignment) in frames.iter().skip(1).zip(alignments) {
        let delta = alignment.delta.min(content_height);
        append_rows(&mut data, frame, band.bottom.saturating_sub(delta), delta)?;
    }

    Ok(Frame {
        data,
        size: PhysicalSize::new(f64::from(first.width()), f64::from(output_height)),
        stride: row_bytes,
        format: first.format,
        color_space: first.color_space,
        scale: first.scale,
    })
}

fn append_rows(output: &mut Vec<u8>, frame: &Frame, first_row: u32, rows: u32) -> Result<()> {
    let row_bytes = frame.width() as usize * frame.format.bytes_per_pixel();
    for row in first_row..first_row.saturating_add(rows) {
        let start = row as usize * frame.stride;
        let end = start + row_bytes;
        let bytes = frame.data.get(start..end).ok_or_else(|| {
            Error::InvalidRequest(format!(
                "frame row {row} is outside a {}-byte buffer",
                frame.data.len()
            ))
        })?;
        output.extend_from_slice(bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use scrozz_core::{ColorSpace, PixelFormat, ScaleFactor};

    use super::*;

    fn frame(rows: &[u8], width: u32) -> Frame {
        let mut data = Vec::new();
        for &value in rows {
            for _ in 0..width {
                data.extend_from_slice(&[value, value, value, 255]);
            }
        }
        Frame {
            data,
            size: PhysicalSize::new(f64::from(width), rows.len() as f64),
            stride: width as usize * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::new(1.25),
        }
    }

    fn config() -> StitchConfig {
        StitchConfig {
            alignment: AlignmentConfig {
                min_overlap: 3,
                row_buckets: 4,
                top_k: 5,
                basin_radius: 1,
                max_mean_error: 8,
                min_confidence: 1,
                ..AlignmentConfig::default()
            },
            chrome: ChromeConfig {
                min_band: 2,
                ..ChromeConfig::default()
            },
            expected_delta: Some(3),
            ..StitchConfig::default()
        }
    }

    #[test]
    fn the_canvas_is_lazy_until_the_second_frame() {
        let document: Vec<u8> = (0..16).map(|v| v * 12).collect();
        let mut stitcher = ScrollStitcher::new(config());
        assert_eq!(
            stitcher.push_frame(frame(&document[0..8], 6)).unwrap(),
            PushOutcome::Started
        );
        assert!(stitcher.canvas.is_none());
        assert!(matches!(
            stitcher.push_frame(frame(&document[3..11], 6)).unwrap(),
            PushOutcome::Advanced { delta: 3, .. }
        ));
        assert_eq!(stitcher.canvas.as_ref().map(Frame::height), Some(11));
    }

    #[test]
    fn repeated_stationary_frames_end_the_capture_without_growing_it() {
        let input = frame(&[1, 2, 3, 4, 5, 6], 4);
        let mut stitcher = ScrollStitcher::new(config());
        stitcher.push_frame(input.clone()).unwrap();
        assert_eq!(
            stitcher.push_frame(input.clone()).unwrap(),
            PushOutcome::NoMovement { stalls: 1 }
        );
        assert_eq!(
            stitcher.push_frame(input).unwrap(),
            PushOutcome::EndOfContent { stalls: 2 }
        );
        assert_eq!(stitcher.summary().frames, 1);
    }

    #[test]
    fn fractional_scale_survives_stitching() {
        let document: Vec<u8> = (0..16).map(|v| v * 12).collect();
        let mut stitcher = ScrollStitcher::new(config());
        stitcher.push_frame(frame(&document[0..8], 6)).unwrap();
        stitcher.push_frame(frame(&document[3..11], 6)).unwrap();
        let result = stitcher.finish_frame().unwrap();
        assert_eq!(result.scale, ScaleFactor::new(1.25));
        assert_eq!(result.height(), 11);
        assert!(result.is_well_formed());
    }
}
