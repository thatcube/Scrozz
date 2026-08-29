//! Incremental scrolling-capture assembly.

use scrozz_core::{
    ColorSpace, Error, Frame, PhysicalSize, PixelFormat, Result, ScaleFactor, ScrollAxis,
};

use crate::{
    Stitcher,
    align::{
        AlignError, Alignment, AlignmentConfig, AnalysisSpan, align_axis_in,
        align_axis_in_perpendicular, stationary_perpendicular_span,
    },
    chrome::{
        AxisChromeBands, ChromeBands, ChromeConfig, SideChromeBands, conservative_axis_chrome,
        detect_sticky_axis_chrome_in,
    },
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
    /// A later viewport no longer overlapped, so the valid partial image was kept.
    OverlapLost,
    /// A later capture or assembly step failed after a valid partial image existed.
    Interrupted,
}

/// Result of adding one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// The first frame is retained but no canvas is allocated yet.
    Started,
    /// New document content was appended.
    Advanced {
        /// Measured physical-pixel displacement.
        delta: u32,
        /// Quality of this seam.
        seam: SeamQuality,
        /// Current stitched length along the selected scroll axis.
        output_extent: u32,
        /// Current pixel height, retained for callers that size image buffers.
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
    /// Non-duplicate frames accepted.
    pub frames: usize,
    /// Successful seams.
    pub seams: usize,
    /// Current output height, zero before a frame arrives.
    pub output_height: u32,
    /// Current output length along the selected scroll axis.
    pub output_extent: u32,
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
    /// Deltas at or below this many pixels count as no movement.
    pub movement_epsilon: u32,
    /// Prior supplied by requested scroll distance, in physical pixels.
    pub expected_delta: Option<u32>,
    /// Largest finished pixel buffer the stitcher may produce.
    pub max_output_bytes: u64,
}

impl Default for StitchConfig {
    fn default() -> Self {
        Self {
            alignment: AlignmentConfig::default(),
            chrome: ChromeConfig::default(),
            stall_limit: 2,
            movement_epsilon: 1,
            expected_delta: None,
            max_output_bytes: 96 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CachedSeam {
    expected_delta: Option<u32>,
    alignment: Alignment,
}

#[derive(Debug)]
enum CanvasPixels {
    Vertical(Vec<u8>),
    Horizontal(Vec<HorizontalStrip>),
}

#[derive(Debug)]
struct HorizontalStrip {
    width: u32,
    row_bytes: usize,
    data: Vec<u8>,
}

#[derive(Debug)]
struct RollingCanvas {
    width: u32,
    height: u32,
    format: PixelFormat,
    color_space: ColorSpace,
    scale: ScaleFactor,
    pixels: CanvasPixels,
}

/// A deterministic scrolling stitcher.
pub struct ScrollStitcher {
    axis: ScrollAxis,
    config: StitchConfig,
    latest_frame: Option<Frame>,
    latest_luma: Option<LumaPlane>,
    frozen_chrome: Option<AxisChromeBands>,
    frozen_perpendicular: Option<AnalysisSpan>,
    seams: Vec<CachedSeam>,
    accepted_frames: usize,
    canvas: Option<RollingCanvas>,
    stalls: u32,
}

impl std::fmt::Debug for ScrollStitcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScrollStitcher")
            .field("axis", &self.axis)
            .field("config", &self.config)
            .field("accepted_frames", &self.accepted_frames)
            .field(
                "retained_source_frames",
                &usize::from(self.latest_frame.is_some()),
            )
            .field("frozen_chrome", &self.frozen_chrome)
            .field("frozen_perpendicular", &self.frozen_perpendicular)
            .field("seams", &self.seams)
            .field(
                "canvas_size",
                &self
                    .canvas
                    .as_ref()
                    .map(|canvas| (canvas.width(), canvas.height())),
            )
            .field("stalls", &self.stalls)
            .finish()
    }
}

impl ScrollStitcher {
    /// Creates an empty stitcher.
    #[must_use]
    pub fn new(config: StitchConfig) -> Self {
        Self::for_axis(ScrollAxis::Vertical, config)
    }

    /// Creates an empty stitcher for `axis`.
    #[must_use]
    pub fn for_axis(axis: ScrollAxis, config: StitchConfig) -> Self {
        Self {
            axis,
            config,
            latest_frame: None,
            latest_luma: None,
            frozen_chrome: None,
            frozen_perpendicular: None,
            seams: Vec::new(),
            accepted_frames: 0,
            canvas: None,
            stalls: 0,
        }
    }

    /// Axis this stitcher gathers.
    #[must_use]
    pub const fn axis(&self) -> ScrollAxis {
        self.axis
    }

    /// Updates the displacement prior once the first frame reveals its scale.
    pub fn set_expected_delta(&mut self, expected_delta: Option<u32>) {
        self.config.expected_delta = expected_delta;
    }

    /// Adds a frame and reports whether the document advanced.
    pub fn push_frame(&mut self, frame: Frame) -> Result<PushOutcome> {
        if let Some(previous) = self.latest_frame.as_ref() {
            validate_compatible(previous, &frame)?;
        } else {
            ensure_output_limit(
                u64::try_from(frame.data.len()).unwrap_or(u64::MAX),
                self.config.max_output_bytes,
            )?;
        }
        let plane = LumaPlane::from_frame(&frame)?;

        if self.latest_frame.is_none() {
            self.latest_frame = Some(frame);
            self.latest_luma = Some(plane);
            self.accepted_frames = 1;
            return Ok(PushOutcome::Started);
        }

        let previous_frame = self
            .latest_frame
            .as_ref()
            .expect("an accepted frame is retained");
        let previous_luma = self
            .latest_luma
            .as_ref()
            .expect("an accepted frame has retained luma");
        if previous_luma == &plane {
            return Ok(self.stationary());
        }

        let expected_delta = self.config.expected_delta;
        let (alignment, chrome) = if let Some(chrome) = self.frozen_chrome {
            let span = chrome.content_span(axis_extent(previous_frame, self.axis));
            let perpendicular = self
                .frozen_perpendicular
                .expect("a frozen canvas has a perpendicular viewport");
            let alignment = match align_axis_in_perpendicular(
                previous_luma,
                &plane,
                self.axis,
                span,
                perpendicular,
                expected_delta,
                &self.config.alignment,
            ) {
                Ok(alignment) => alignment,
                Err(error) => return Ok(insufficient_overlap(error)),
            };
            if alignment.delta <= self.config.movement_epsilon {
                return Ok(self.stationary());
            }

            let proof = detect_sticky_axis_chrome_in(
                previous_luma,
                &plane,
                self.axis,
                alignment.delta,
                perpendicular,
                &self.config.chrome,
            );
            if !proves_chrome(chrome, proof) {
                return Ok(PushOutcome::InsufficientOverlap {
                    reason: format!(
                        "new frame does not prove the locked sticky chrome: \
                         required {}+{} pixels, observed {}+{}",
                        chrome.leading, chrome.trailing, proof.leading, proof.trailing
                    ),
                });
            }
            let observed_perpendicular = stationary_perpendicular_span(
                previous_luma,
                &plane,
                self.axis,
                span,
                alignment.delta,
                &self.config.alignment,
            );
            if !proves_perpendicular(perpendicular, observed_perpendicular) {
                return Ok(PushOutcome::InsufficientOverlap {
                    reason: format!(
                        "new frame does not prove the locked perpendicular viewport: \
                         required {}..{}, observed {}..{}",
                        perpendicular.start,
                        perpendicular.end,
                        observed_perpendicular.start,
                        observed_perpendicular.end
                    ),
                });
            }
            (alignment, chrome)
        } else {
            let initial = match provisional_alignment(
                previous_luma,
                &plane,
                self.axis,
                expected_delta,
                &self.config.alignment,
                &self.config.chrome,
            ) {
                Ok(alignment) => alignment,
                Err(error) => return Ok(insufficient_overlap(error)),
            };
            if initial.delta <= self.config.movement_epsilon {
                return Ok(self.stationary());
            }

            let (alignment, chrome) = match establish_bootstrap_chrome(
                previous_luma,
                &plane,
                self.axis,
                initial,
                expected_delta,
                &self.config.alignment,
                &self.config.chrome,
            ) {
                Ok(result) => result,
                Err(error) => return Ok(insufficient_overlap(error)),
            };
            if alignment.delta <= self.config.movement_epsilon {
                return Ok(self.stationary());
            }
            (alignment, chrome)
        };

        let span = chrome.content_span(axis_extent(previous_frame, self.axis));
        let perpendicular = alignment.perpendicular;
        let next_frame_count = self
            .accepted_frames
            .checked_add(1)
            .ok_or_else(|| Error::InvalidRequest("too many scrolling frames".to_owned()))?;

        if let Some(canvas) = self.canvas.as_mut() {
            canvas.append(
                &frame,
                span,
                perpendicular,
                alignment.delta,
                self.config.max_output_bytes,
            )?;
        } else {
            self.canvas = Some(RollingCanvas::from_first_pair(
                previous_frame,
                &frame,
                self.axis,
                span,
                perpendicular,
                alignment.delta,
                self.config.max_output_bytes,
            )?);
            self.frozen_chrome = Some(chrome);
            self.frozen_perpendicular = Some(perpendicular);
        }

        self.seams.push(CachedSeam {
            expected_delta,
            alignment,
        });
        self.latest_frame = Some(frame);
        self.latest_luma = Some(plane);
        self.accepted_frames = next_frame_count;
        self.stalls = 0;

        Ok(PushOutcome::Advanced {
            delta: alignment.delta,
            seam: SeamQuality {
                mean_absolute_error: alignment.score,
                confidence: alignment.confidence,
            },
            output_extent: self
                .canvas
                .as_ref()
                .map_or(0, |canvas| canvas.axis_extent(self.axis)),
            output_height: self.canvas.as_ref().map_or(0, RollingCanvas::height),
        })
    }

    /// Facts about the partial result.
    #[must_use]
    pub fn summary(&self) -> StitchSummary {
        StitchSummary {
            frames: self.accepted_frames,
            seams: self.seams.len(),
            output_height: self.canvas.as_ref().map_or_else(
                || self.latest_frame.as_ref().map_or(0, Frame::height),
                RollingCanvas::height,
            ),
            output_extent: self.canvas.as_ref().map_or_else(
                || {
                    self.latest_frame
                        .as_ref()
                        .map_or(0, |frame| axis_extent(frame, self.axis))
                },
                |canvas| canvas.axis_extent(self.axis),
            ),
            chrome: self.vertical_chrome(),
            stalls: self.stalls,
        }
    }

    /// Produces the current image.
    pub fn finish_frame(mut self) -> Result<Frame> {
        match self.accepted_frames {
            0 => Err(Error::InvalidRequest(
                "cannot finish a scrolling capture with no frames".to_owned(),
            )),
            1 => Ok(self.latest_frame.take().expect("one retained frame")),
            _ => {
                let canvas = self.canvas.take().ok_or_else(|| {
                    Error::Platform(
                        "scrolling canvas was not built after the second frame".to_owned(),
                    )
                })?;
                drop(self.latest_frame.take());
                drop(self.latest_luma.take());
                canvas.finish()
            }
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

    /// Conservatively detected fixed side chrome.
    ///
    /// Vertical stitchers return zero side bands.
    #[must_use]
    pub fn side_chrome(&self) -> SideChromeBands {
        if self.axis != ScrollAxis::Horizontal {
            return SideChromeBands::default();
        }
        let chrome = self.chrome();
        SideChromeBands {
            left: chrome.leading,
            right: chrome.trailing,
        }
    }

    fn vertical_chrome(&self) -> ChromeBands {
        if self.axis != ScrollAxis::Vertical {
            return ChromeBands::default();
        }
        let chrome = self.chrome();
        ChromeBands {
            top: chrome.leading,
            bottom: chrome.trailing,
        }
    }

    fn chrome(&self) -> AxisChromeBands {
        self.frozen_chrome.unwrap_or_default()
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
    axis: ScrollAxis,
    expected_delta: Option<u32>,
    alignment: &AlignmentConfig,
    chrome: &ChromeConfig,
) -> Result<Alignment, AlignError> {
    let extent = axis_extent_luma(previous, axis);
    let cap = (u64::from(extent) * u64::from(chrome.max_height_percent.min(100)) / 100) as u32;
    let leading_half = cap / 2;
    let trailing_half = cap - leading_half;
    let spans = [
        AnalysisSpan::full(extent),
        AnalysisSpan {
            start: cap,
            end: extent,
        },
        AnalysisSpan {
            start: 0,
            end: extent.saturating_sub(cap),
        },
        AnalysisSpan {
            start: leading_half,
            end: extent.saturating_sub(trailing_half),
        },
    ];

    let mut best = None;
    let mut first_error = None;
    for span in spans {
        if span.len() < alignment.min_overlap {
            continue;
        }
        match align_axis_in(previous, current, axis, span, expected_delta, alignment) {
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

fn establish_bootstrap_chrome(
    previous: &LumaPlane,
    current: &LumaPlane,
    axis: ScrollAxis,
    initial: Alignment,
    expected_delta: Option<u32>,
    alignment_config: &AlignmentConfig,
    chrome_config: &ChromeConfig,
) -> Result<(Alignment, AxisChromeBands), AlignError> {
    let extent = axis_extent_luma(previous, axis);
    let initial_proof = detect_sticky_axis_chrome_in(
        previous,
        current,
        axis,
        initial.delta,
        initial.perpendicular,
        chrome_config,
    );
    let mut chrome =
        conservative_axis_chrome(std::iter::once(initial_proof), extent, chrome_config);
    let mut alignment = align_axis_in(
        previous,
        current,
        axis,
        chrome.content_span(extent),
        expected_delta,
        alignment_config,
    )?;

    let proof = detect_sticky_axis_chrome_in(
        previous,
        current,
        axis,
        alignment.delta,
        alignment.perpendicular,
        chrome_config,
    );
    if !proves_chrome(chrome, proof) {
        chrome = conservative_axis_chrome([chrome, proof], extent, chrome_config);
        alignment = align_axis_in(
            previous,
            current,
            axis,
            chrome.content_span(extent),
            expected_delta,
            alignment_config,
        )?;

        let final_proof = detect_sticky_axis_chrome_in(
            previous,
            current,
            axis,
            alignment.delta,
            alignment.perpendicular,
            chrome_config,
        );
        if !proves_chrome(chrome, final_proof) {
            chrome = AxisChromeBands::default();
            alignment = align_axis_in(
                previous,
                current,
                axis,
                AnalysisSpan::full(extent),
                expected_delta,
                alignment_config,
            )?;
        }
    }

    Ok((alignment, chrome))
}

const fn proves_chrome(required: AxisChromeBands, observed: AxisChromeBands) -> bool {
    observed.leading == required.leading && observed.trailing == required.trailing
}

const fn proves_perpendicular(required: AnalysisSpan, observed: AnalysisSpan) -> bool {
    observed.start == required.start && observed.end == required.end
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

impl RollingCanvas {
    fn from_first_pair(
        first: &Frame,
        second: &Frame,
        axis: ScrollAxis,
        span: AnalysisSpan,
        perpendicular: AnalysisSpan,
        delta: u32,
        max_output_bytes: u64,
    ) -> Result<Self> {
        if span.is_empty() {
            return Err(Error::InvalidRequest(
                "sticky chrome consumed the whole scrolling frame".to_owned(),
            ));
        }

        let appended = delta.min(span.len());
        match axis {
            ScrollAxis::Vertical => {
                let width = perpendicular.len();
                let height = span.len().checked_add(appended).ok_or_else(|| {
                    Error::InvalidRequest("scrolling capture is too tall".to_owned())
                })?;
                let capacity = checked_output_len(
                    width,
                    height,
                    first.format.bytes_per_pixel(),
                    max_output_bytes,
                )?;
                let mut data = Vec::new();
                try_reserve_exact(&mut data, capacity)?;
                append_rectangle(
                    &mut data,
                    first,
                    span.start,
                    span.len(),
                    perpendicular.start,
                    perpendicular.len(),
                )?;
                append_rectangle(
                    &mut data,
                    second,
                    span.end.saturating_sub(appended),
                    appended,
                    perpendicular.start,
                    perpendicular.len(),
                )?;
                debug_assert_eq!(data.len(), capacity);
                Ok(Self {
                    width,
                    height,
                    format: first.format,
                    color_space: first.color_space,
                    scale: first.scale,
                    pixels: CanvasPixels::Vertical(data),
                })
            }
            ScrollAxis::Horizontal => {
                let width = span.len().checked_add(appended).ok_or_else(|| {
                    Error::InvalidRequest("scrolling capture is too wide".to_owned())
                })?;
                let height = perpendicular.len();
                let capacity = checked_output_len(
                    width,
                    height,
                    first.format.bytes_per_pixel(),
                    max_output_bytes,
                )?;
                let row_bytes = checked_row_bytes(width, first.format.bytes_per_pixel())?;
                let mut data = Vec::new();
                try_reserve_exact(&mut data, capacity)?;
                for row in perpendicular.start..perpendicular.end {
                    let row_start = data.len();
                    append_columns(&mut data, first, row, span.start, span.len())?;
                    append_columns(
                        &mut data,
                        second,
                        row,
                        span.end.saturating_sub(appended),
                        appended,
                    )?;
                    debug_assert_eq!(data.len() - row_start, row_bytes);
                }
                debug_assert_eq!(data.len(), capacity);
                Ok(Self {
                    width,
                    height,
                    format: first.format,
                    color_space: first.color_space,
                    scale: first.scale,
                    pixels: CanvasPixels::Horizontal(vec![HorizontalStrip {
                        width,
                        row_bytes,
                        data,
                    }]),
                })
            }
        }
    }

    const fn width(&self) -> u32 {
        self.width
    }

    const fn height(&self) -> u32 {
        self.height
    }

    const fn axis_extent(&self, axis: ScrollAxis) -> u32 {
        match axis {
            ScrollAxis::Vertical => self.height,
            ScrollAxis::Horizontal => self.width,
        }
    }

    fn append(
        &mut self,
        frame: &Frame,
        span: AnalysisSpan,
        perpendicular: AnalysisSpan,
        delta: u32,
        max_output_bytes: u64,
    ) -> Result<()> {
        let appended = delta.min(span.len());
        match &mut self.pixels {
            CanvasPixels::Vertical(data) => {
                let new_height = self.height.checked_add(appended).ok_or_else(|| {
                    Error::InvalidRequest("scrolling capture is too tall".to_owned())
                })?;
                checked_output_len(
                    self.width,
                    new_height,
                    self.format.bytes_per_pixel(),
                    max_output_bytes,
                )?;

                let additional = checked_output_len(
                    self.width,
                    appended,
                    self.format.bytes_per_pixel(),
                    u64::MAX,
                )?;
                let mut rows = Vec::new();
                try_reserve_exact(&mut rows, additional)?;
                append_rectangle(
                    &mut rows,
                    frame,
                    span.end.saturating_sub(appended),
                    appended,
                    perpendicular.start,
                    perpendicular.len(),
                )?;
                try_reserve(data, rows.len())?;
                data.extend_from_slice(&rows);
                self.height = new_height;
            }
            CanvasPixels::Horizontal(strips) => {
                let new_width = self.width.checked_add(appended).ok_or_else(|| {
                    Error::InvalidRequest("scrolling capture is too wide".to_owned())
                })?;
                checked_output_len(
                    new_width,
                    self.height,
                    self.format.bytes_per_pixel(),
                    max_output_bytes,
                )?;

                let additional = checked_row_bytes(appended, self.format.bytes_per_pixel())?;
                let total_additional =
                    additional
                        .checked_mul(self.height as usize)
                        .ok_or_else(|| {
                            Error::InvalidRequest("scrolling capture is too large".to_owned())
                        })?;
                let mut additions = Vec::new();
                try_reserve_exact(&mut additions, total_additional)?;
                for row in 0..self.height {
                    append_columns(
                        &mut additions,
                        frame,
                        perpendicular.start + row,
                        span.end.saturating_sub(appended),
                        appended,
                    )?;
                }
                if additions.len() != total_additional {
                    return Err(Error::Platform(
                        "horizontal scrolling canvas has inconsistent row storage".to_owned(),
                    ));
                }
                strips.try_reserve(1).map_err(|_| {
                    Error::InvalidRequest("horizontal scrolling strip allocation failed".to_owned())
                })?;
                strips.push(HorizontalStrip {
                    width: appended,
                    row_bytes: additional,
                    data: additions,
                });
                self.width = new_width;
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<Frame> {
        let row_bytes = checked_row_bytes(self.width, self.format.bytes_per_pixel())?;
        let expected_len = checked_output_len(
            self.width,
            self.height,
            self.format.bytes_per_pixel(),
            u64::MAX,
        )?;
        let data = match self.pixels {
            CanvasPixels::Vertical(data) => {
                if data.len() != expected_len {
                    return Err(Error::Platform(format!(
                        "vertical scrolling canvas has {} bytes, expected {expected_len}",
                        data.len()
                    )));
                }
                data
            }
            CanvasPixels::Horizontal(strips) => {
                let strip_width: u32 = strips.iter().map(|strip| strip.width).sum();
                if strip_width != self.width {
                    return Err(Error::Platform(format!(
                        "horizontal scrolling strips span {strip_width} pixels, expected {}",
                        self.width
                    )));
                }
                let mut data = Vec::new();
                try_reserve_exact(&mut data, expected_len)?;
                for row in 0..self.height as usize {
                    for strip in &strips {
                        let expected_strip_len = strip
                            .row_bytes
                            .checked_mul(self.height as usize)
                            .ok_or_else(|| {
                                Error::InvalidRequest(
                                    "horizontal scrolling strip is too large".to_owned(),
                                )
                            })?;
                        if strip.data.len() != expected_strip_len {
                            return Err(Error::Platform(format!(
                                "horizontal scrolling strip has {} bytes, expected \
                                 {expected_strip_len}",
                                strip.data.len()
                            )));
                        }
                        let start = row.checked_mul(strip.row_bytes).ok_or_else(|| {
                            Error::InvalidRequest(
                                "horizontal scrolling strip offset overflowed".to_owned(),
                            )
                        })?;
                        data.extend_from_slice(&strip.data[start..start + strip.row_bytes]);
                    }
                }
                debug_assert_eq!(data.len(), expected_len);
                data
            }
        };

        Ok(Frame {
            data,
            size: PhysicalSize::new(f64::from(self.width), f64::from(self.height)),
            stride: row_bytes,
            format: self.format,
            color_space: self.color_space,
            scale: self.scale,
        })
    }
}

fn insufficient_overlap(error: AlignError) -> PushOutcome {
    PushOutcome::InsufficientOverlap {
        reason: error.to_string(),
    }
}

fn ensure_output_limit(bytes: u64, max_output_bytes: u64) -> Result<()> {
    if bytes > max_output_bytes {
        return Err(Error::InvalidRequest(format!(
            "scrolling capture would require {bytes} output bytes, above the configured \
             {max_output_bytes}-byte limit"
        )));
    }
    Ok(())
}

fn checked_output_len(
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
    max_output_bytes: u64,
) -> Result<usize> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel as u64))
        .ok_or_else(|| Error::InvalidRequest("scrolling capture is too large".to_owned()))?;
    ensure_output_limit(bytes, max_output_bytes)?;
    usize::try_from(bytes)
        .map_err(|_| Error::InvalidRequest("scrolling capture is too large to address".to_owned()))
}

fn checked_row_bytes(width: u32, bytes_per_pixel: usize) -> Result<usize> {
    (width as usize)
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| Error::InvalidRequest("scrolling capture is too wide".to_owned()))
}

fn try_reserve_exact<T>(buffer: &mut Vec<T>, additional: usize) -> Result<()> {
    buffer
        .try_reserve_exact(additional)
        .map_err(|_| Error::InvalidRequest("scrolling capture output allocation failed".to_owned()))
}

fn try_reserve<T>(buffer: &mut Vec<T>, additional: usize) -> Result<()> {
    buffer
        .try_reserve(additional)
        .map_err(|_| Error::InvalidRequest("scrolling capture output allocation failed".to_owned()))
}

fn append_rectangle(
    output: &mut Vec<u8>,
    frame: &Frame,
    first_row: u32,
    rows: u32,
    first_column: u32,
    columns: u32,
) -> Result<()> {
    first_column
        .checked_add(columns)
        .filter(|end| *end <= frame.width())
        .ok_or_else(|| {
            Error::InvalidRequest(format!(
                "frame columns {first_column}..{} are outside its {}-column width",
                first_column.saturating_add(columns),
                frame.width()
            ))
        })?;
    let bytes_per_pixel = frame.format.bytes_per_pixel();
    let row_bytes = checked_row_bytes(columns, bytes_per_pixel)?;
    let end_row = first_row
        .checked_add(rows)
        .filter(|end| *end <= frame.height())
        .ok_or_else(|| {
            Error::InvalidRequest(format!(
                "frame rows {first_row}..{} are outside its {}-row height",
                first_row.saturating_add(rows),
                frame.height()
            ))
        })?;
    for row in first_row..end_row {
        let start = (row as usize)
            .checked_mul(frame.stride)
            .and_then(|offset| {
                (first_column as usize)
                    .checked_mul(bytes_per_pixel)
                    .and_then(|column| offset.checked_add(column))
            })
            .ok_or_else(|| Error::InvalidRequest("frame row offset overflowed".to_owned()))?;
        let end = start
            .checked_add(row_bytes)
            .ok_or_else(|| Error::InvalidRequest("frame row range overflowed".to_owned()))?;
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

fn append_columns(
    output: &mut Vec<u8>,
    frame: &Frame,
    row: u32,
    first_column: u32,
    columns: u32,
) -> Result<()> {
    first_column
        .checked_add(columns)
        .filter(|end| *end <= frame.width())
        .ok_or_else(|| {
            Error::InvalidRequest(format!(
                "frame columns {first_column}..{} are outside its {}-column width",
                first_column.saturating_add(columns),
                frame.width()
            ))
        })?;
    let bytes_per_pixel = frame.format.bytes_per_pixel();
    let start = (row as usize)
        .checked_mul(frame.stride)
        .and_then(|offset| {
            (first_column as usize)
                .checked_mul(bytes_per_pixel)
                .and_then(|column| offset.checked_add(column))
        })
        .ok_or_else(|| Error::InvalidRequest("frame column offset overflowed".to_owned()))?;
    let byte_count = (columns as usize)
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| Error::InvalidRequest("frame column count overflowed".to_owned()))?;
    let end = start
        .checked_add(byte_count)
        .ok_or_else(|| Error::InvalidRequest("frame column range overflowed".to_owned()))?;
    let bytes = frame.data.get(start..end).ok_or_else(|| {
        Error::InvalidRequest(format!(
            "frame columns {first_column}..{} on row {row} are outside a {}-byte buffer",
            first_column.saturating_add(columns),
            frame.data.len()
        ))
    })?;
    output.extend_from_slice(bytes);
    Ok(())
}

fn axis_extent(frame: &Frame, axis: ScrollAxis) -> u32 {
    match axis {
        ScrollAxis::Vertical => frame.height(),
        ScrollAxis::Horizontal => frame.width(),
    }
}

const fn axis_extent_luma(plane: &LumaPlane, axis: ScrollAxis) -> u32 {
    match axis {
        ScrollAxis::Vertical => plane.height(),
        ScrollAxis::Horizontal => plane.width(),
    }
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

    fn column_frame(columns: &[u8], height: u32) -> Frame {
        let width = columns.len() as u32;
        let mut data = Vec::new();
        for _ in 0..height {
            for &value in columns {
                data.extend_from_slice(&[value, value, value, 255]);
            }
        }
        Frame {
            data,
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride: width as usize * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::new(1.25),
        }
    }

    fn matrix_frame(width: u32, height: u32, mut pixel: impl FnMut(u32, u32) -> u8) -> Frame {
        let mut data = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                let value = pixel(x, y);
                data.extend_from_slice(&[value, value, value, 255]);
            }
        }
        Frame {
            data,
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
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
                min_stationary_edge: 2,
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
        assert_eq!(
            stitcher.canvas.as_ref().map(RollingCanvas::height),
            Some(11)
        );
    }

    #[test]
    fn the_horizontal_canvas_is_lazy_until_the_second_frame() {
        let document: Vec<u8> = (0..16).map(|v| v * 12).collect();
        let mut stitcher = ScrollStitcher::for_axis(ScrollAxis::Horizontal, config());
        assert_eq!(
            stitcher
                .push_frame(column_frame(&document[0..8], 6))
                .unwrap(),
            PushOutcome::Started
        );
        assert!(stitcher.canvas.is_none());
        assert!(matches!(
            stitcher
                .push_frame(column_frame(&document[3..11], 6))
                .unwrap(),
            PushOutcome::Advanced { delta: 3, .. }
        ));
        assert_eq!(stitcher.canvas.as_ref().map(RollingCanvas::width), Some(11));
    }

    #[test]
    fn retained_source_state_stays_bounded_over_many_frames() {
        let document: Vec<u8> = (0..80).map(|value| value * 3).collect();
        let mut stitch_config = config();
        stitch_config.expected_delta = Some(2);
        let mut stitcher = ScrollStitcher::new(stitch_config);

        for start in (0..=50).step_by(2) {
            stitcher
                .push_frame(frame(&document[start..start + 8], 6))
                .unwrap();
            assert_eq!(usize::from(stitcher.latest_frame.is_some()), 1);
            assert_eq!(usize::from(stitcher.latest_luma.is_some()), 1);
        }

        assert_eq!(stitcher.summary().frames, 26);
        assert_eq!(stitcher.summary().seams, 25);
    }

    #[test]
    fn hundred_4k_frames_have_a_sub_512_mib_peak_estimate_on_both_axes() {
        const MIB: u64 = 1024 * 1024;
        const VIEWPORT_RGBA: u64 = 3_840 * 2_160 * 4;
        const VIEWPORT_LUMA: u64 = 3_840 * 2_160;
        let output_cap = StitchConfig::default().max_output_bytes;
        let retained_frames = 100 * (VIEWPORT_RGBA + VIEWPORT_LUMA);
        assert!(retained_frames > 3 * 1024 * MIB);

        // A growth reallocation may transiently retain the old and new canvas.
        // Alignment also owns previous/current source frames and luma planes,
        // while assembly stages at most one viewport. Horizontal finish flattens
        // append-only strips once, so the 2× canvas term covers both forms.
        for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
            let stitch_peak =
                2 * output_cap + 2 * VIEWPORT_RGBA + 2 * VIEWPORT_LUMA + VIEWPORT_RGBA;
            assert!(
                stitch_peak < 512 * MIB,
                "{axis:?} stitch peak estimate was {stitch_peak} bytes"
            );
        }

        // The GUI/CLI PNG path can simultaneously hold the finished frame,
        // straight RGBA normalization, opaque RGB body and a conservatively
        // incompressible output. Count both sides of a worst-case output Vec
        // growth so the estimate does not assume in-place allocator behaviour.
        // Keeping the logical canvas at 96 MiB leaves this end-to-end worst case
        // below a 512 MiB process budget.
        let encoded_upper_bound = output_cap + output_cap.div_ceil(100);
        let encode_peak =
            output_cap + output_cap + (output_cap * 3 / 4) + 2 * encoded_upper_bound + MIB;
        assert!(encode_peak < 512 * MIB, "{encode_peak} bytes");
    }

    #[test]
    fn changing_the_prior_does_not_rewrite_cached_seams() {
        let document: Vec<u8> = (0..24).map(|value| value * 10).collect();
        let mut stitch_config = config();
        stitch_config.expected_delta = Some(2);
        let mut stitcher = ScrollStitcher::new(stitch_config);
        stitcher.push_frame(frame(&document[0..8], 6)).unwrap();
        stitcher.push_frame(frame(&document[2..10], 6)).unwrap();
        let first_seam = stitcher.seams[0];

        stitcher.set_expected_delta(Some(3));
        assert_eq!(stitcher.seams[0], first_seam);
        stitcher.push_frame(frame(&document[5..13], 6)).unwrap();

        assert_eq!(stitcher.seams[0], first_seam);
        assert_eq!(stitcher.seams[0].expected_delta, Some(2));
        assert_eq!(stitcher.seams[0].alignment.delta, 2);
        assert_eq!(stitcher.seams[1].expected_delta, Some(3));
        assert_eq!(stitcher.seams[1].alignment.delta, 3);
    }

    #[test]
    fn output_byte_cap_rejects_without_mutating_the_partial_canvas() {
        let document: Vec<u8> = (0..24).map(|value| value * 10).collect();
        let mut stitch_config = config();
        stitch_config.max_output_bytes = 11 * 2 * 4;
        let mut stitcher = ScrollStitcher::new(stitch_config);
        stitcher.push_frame(frame(&document[0..8], 2)).unwrap();
        stitcher.push_frame(frame(&document[3..11], 2)).unwrap();
        let summary = stitcher.summary();
        let seams = stitcher.seams.clone();

        let error = stitcher
            .push_frame(frame(&document[6..14], 2))
            .expect_err("the third frame exceeds the byte cap");
        assert!(error.to_string().contains("configured"), "{error}");
        assert_eq!(stitcher.summary(), summary);
        assert_eq!(stitcher.seams, seams);

        let output = stitcher.finish_frame().unwrap();
        let rows: Vec<u8> = output
            .data
            .chunks_exact(output.stride)
            .map(|row| row[0])
            .collect();
        assert_eq!(rows, document[0..11]);
    }

    #[test]
    fn vertical_rows_are_appended_incrementally_and_exactly() {
        let document: Vec<u8> = (0..26).map(|value| value * 9).collect();
        let mut stitcher = ScrollStitcher::new(config());
        for start in [0, 3, 6, 9] {
            stitcher
                .push_frame(frame(&document[start..start + 8], 6))
                .unwrap();
        }

        let output = stitcher.finish_frame().unwrap();
        let rows: Vec<u8> = output
            .data
            .chunks_exact(output.stride)
            .map(|row| row[0])
            .collect();
        assert_eq!(rows, document[0..17]);
    }

    #[test]
    fn horizontal_columns_are_appended_incrementally_and_exactly() {
        let document: Vec<u8> = (0..26).map(|value| value * 9).collect();
        let mut stitcher = ScrollStitcher::for_axis(ScrollAxis::Horizontal, config());
        for start in [0, 3, 6, 9] {
            stitcher
                .push_frame(column_frame(&document[start..start + 8], 6))
                .unwrap();
        }

        let output = stitcher.finish_frame().unwrap();
        let (pixels, _) = output.data[..output.stride].as_chunks::<4>();
        let columns: Vec<u8> = pixels.iter().map(|pixel| pixel[0]).collect();
        assert_eq!(columns, document[0..17]);
    }

    #[test]
    fn horizontal_appends_add_strips_without_rewriting_prior_pixels() {
        let document: Vec<u8> = (0..26).map(|value| value * 9).collect();
        let mut stitcher = ScrollStitcher::for_axis(ScrollAxis::Horizontal, config());
        for start in [0, 3, 6, 9] {
            stitcher
                .push_frame(column_frame(&document[start..start + 8], 6))
                .unwrap();
        }
        let strip_widths = match &stitcher.canvas.as_ref().expect("canvas").pixels {
            CanvasPixels::Horizontal(strips) => {
                strips.iter().map(|strip| strip.width).collect::<Vec<_>>()
            }
            CanvasPixels::Vertical(_) => panic!("horizontal stitcher built a vertical canvas"),
        };
        assert_eq!(strip_widths, vec![11, 3, 3]);

        let output = stitcher.finish_frame().unwrap();
        assert_eq!(output.width(), 17);
        assert_eq!(output.data.len(), output.stride * output.height() as usize);
    }

    #[test]
    fn stationary_sidebars_are_cropped_from_a_vertical_canvas() {
        let viewport = |start: u32| {
            matrix_frame(10, 8, |x, y| match x {
                0..=1 => 20 + y * 13,
                8..=9 => 210u32.saturating_sub(y * 11),
                _ => 40 + (start + y) * 7 + (x - 2) * 3,
            } as u8)
        };
        let mut stitcher = ScrollStitcher::new(config());
        stitcher.push_frame(viewport(0)).unwrap();
        stitcher.push_frame(viewport(3)).unwrap();

        let output = stitcher.finish_frame().unwrap();
        assert_eq!((output.width(), output.height()), (6, 11));
        for y in 0..output.height() {
            for x in 0..output.width() {
                let offset = y as usize * output.stride + x as usize * 4;
                assert_eq!(output.data[offset], (40 + y * 7 + x * 3) as u8);
            }
        }
    }

    #[test]
    fn stationary_toolbars_are_cropped_from_a_horizontal_canvas() {
        let viewport = |start: u32| {
            matrix_frame(8, 10, |x, y| match y {
                0..=1 => 15 + x * 14,
                8..=9 => 220u32.saturating_sub(x * 12),
                _ => 35 + (start + x) * 7 + (y - 2) * 3,
            } as u8)
        };
        let mut stitcher = ScrollStitcher::for_axis(ScrollAxis::Horizontal, config());
        stitcher.push_frame(viewport(0)).unwrap();
        stitcher.push_frame(viewport(3)).unwrap();

        let output = stitcher.finish_frame().unwrap();
        assert_eq!((output.width(), output.height()), (11, 6));
        for y in 0..output.height() {
            for x in 0..output.width() {
                let offset = y as usize * output.stride + x as usize * 4;
                assert_eq!(output.data[offset], (35 + x * 7 + y * 3) as u8);
            }
        }
    }

    #[test]
    fn a_frame_that_disproves_locked_chrome_is_rejected_transactionally() {
        let document: Vec<u8> = (0..24).map(|value| 40 + value * 8).collect();
        let with_chrome = |start: usize| {
            let mut rows = vec![3, 17];
            rows.extend_from_slice(&document[start..start + 8]);
            rows.extend_from_slice(&[229, 247]);
            frame(&rows, 6)
        };
        let mut stitcher = ScrollStitcher::new(config());
        stitcher.push_frame(with_chrome(0)).unwrap();
        stitcher.push_frame(with_chrome(3)).unwrap();
        assert_eq!(stitcher.summary().chrome, ChromeBands { top: 2, bottom: 2 });
        let summary = stitcher.summary();

        let outcome = stitcher.push_frame(frame(&document[4..16], 6)).unwrap();
        assert!(
            matches!(outcome, PushOutcome::InsufficientOverlap { ref reason }
                if reason.contains("locked sticky chrome")),
            "{outcome:?}"
        );
        assert_eq!(stitcher.summary(), summary);

        let output = stitcher.finish_frame().unwrap();
        let rows: Vec<u8> = output
            .data
            .chunks_exact(output.stride)
            .map(|row| row[0])
            .collect();
        assert_eq!(rows, document[0..11]);
    }

    #[test]
    fn enlarged_chrome_or_a_narrower_viewport_cannot_change_frozen_geometry() {
        assert!(!proves_chrome(
            AxisChromeBands {
                leading: 2,
                trailing: 2,
            },
            AxisChromeBands {
                leading: 3,
                trailing: 2,
            },
        ));
        assert!(!proves_perpendicular(
            AnalysisSpan { start: 2, end: 10 },
            AnalysisSpan { start: 3, end: 10 },
        ));
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
    fn overlap_loss_has_a_distinct_terminal_reason() {
        assert_eq!(
            StopReason::from(crate::session::CompletionReason::OverlapLost),
            StopReason::OverlapLost
        );
        assert_eq!(
            StopReason::from(crate::session::CompletionReason::Interrupted),
            StopReason::Interrupted
        );
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
