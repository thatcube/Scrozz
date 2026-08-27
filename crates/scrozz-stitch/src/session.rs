//! Capture/scroll/pacing orchestration.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, Error, Frame, Provenance, Result,
    ScrollAxis, ScrollDriver, ScrollGesture, ScrollSynthesis,
};

use crate::{PushOutcome, ScrollStitcher, SeamQuality, StitchConfig, StopReason};

/// Supplies successive viewport frames.
pub trait FrameSource {
    /// Captures the viewport in its current position.
    fn capture_frame(&mut self) -> Result<Frame>;

    /// Human-readable source name for diagnostics.
    fn name(&self) -> &str;
}

/// Adapts a borrowed capture backend without introducing a crate dependency.
pub struct BackendFrameSource<'a> {
    backend: &'a dyn CaptureBackend,
    request: CaptureRequest,
}

impl<'a> BackendFrameSource<'a> {
    /// Captures `request` repeatedly through `backend`.
    #[must_use]
    pub const fn new(backend: &'a dyn CaptureBackend, request: CaptureRequest) -> Self {
        Self { backend, request }
    }
}

impl FrameSource for BackendFrameSource<'_> {
    fn capture_frame(&mut self) -> Result<Frame> {
        Ok(self.backend.capture(&self.request)?.frame)
    }

    fn name(&self) -> &str {
        self.backend.name()
    }
}

/// Waits for the target application and compositor between actions.
pub trait Pacer {
    /// Waits for `duration`.
    fn wait(&mut self, duration: Duration);
}

/// Production pacer.
#[derive(Debug, Default, Clone, Copy)]
pub struct ThreadPacer;

impl Pacer for ThreadPacer {
    fn wait(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Deterministic pacer for callers that schedule externally and for tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopPacer;

impl Pacer for NoopPacer {
    fn wait(&mut self, _duration: Duration) {}
}

/// What to do with pixels captured before cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelAction {
    /// Produce the partial long capture.
    Keep,
    /// Discard every frame and report ordinary cancellation.
    Abort,
}

/// A cancellation source polled only at safe frame boundaries.
pub trait CancelSignal {
    /// Returns a requested action, if any.
    fn cancellation(&mut self) -> Option<CancelAction>;
}

/// A signal that never cancels.
#[derive(Debug, Default, Clone, Copy)]
pub struct NeverCancel;

impl CancelSignal for NeverCancel {
    fn cancellation(&mut self) -> Option<CancelAction> {
        None
    }
}

/// Thread-safe cancellation shared by a HUD and a capture worker.
#[derive(Debug, Clone, Default)]
pub struct AtomicCancellation {
    state: Arc<AtomicU8>,
}

impl AtomicCancellation {
    /// Requests cancellation with the chosen partial-capture policy.
    pub fn cancel(&self, action: CancelAction) {
        self.state.store(
            match action {
                CancelAction::Keep => 1,
                CancelAction::Abort => 2,
            },
            Ordering::Release,
        );
    }

    /// Clears an earlier request before reusing the token.
    pub fn reset(&self) {
        self.state.store(0, Ordering::Release);
    }
}

impl CancelSignal for AtomicCancellation {
    fn cancellation(&mut self) -> Option<CancelAction> {
        match self.state.load(Ordering::Acquire) {
            1 => Some(CancelAction::Keep),
            2 => Some(CancelAction::Abort),
            _ => None,
        }
    }
}

/// A meaningful update suitable for a HUD, CLI log or test recorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// The platform input path is ready.
    Prepared {
        /// Driver diagnostic name.
        driver: String,
        /// Whether Scrozz will move the target itself.
        automatic: bool,
        /// Why the user must scroll, when synthesis is manual.
        manual_reason: Option<String>,
    },
    /// One viewport was captured.
    FrameCaptured {
        /// One-based frame number.
        frame: usize,
    },
    /// The manual flow is waiting for the user to move the target.
    WaitingForManualScroll,
    /// New document rows were accepted.
    Advanced {
        /// One-based frame number.
        frame: usize,
        /// Measured physical displacement.
        delta: u32,
        /// Seam quality.
        seam: SeamQuality,
        /// Current output height.
        output_height: u32,
    },
    /// A stationary frame was observed.
    Stalled {
        /// Consecutive stationary observations.
        count: u32,
    },
    /// The session produced its final image.
    Finished {
        /// Why it stopped.
        reason: CompletionReason,
        /// Captured viewport count.
        frames: usize,
        /// Final output height.
        output_height: u32,
    },
}

/// Why a session returned successfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionReason {
    /// The viewport stopped advancing.
    EndOfContent,
    /// The configured safety limit was reached.
    FrameLimit,
    /// The user cancelled but kept the partial image.
    CancelledKeep,
}

impl From<CompletionReason> for StopReason {
    fn from(reason: CompletionReason) -> Self {
        match reason {
            CompletionReason::EndOfContent => Self::EndOfContent,
            CompletionReason::FrameLimit => Self::FrameLimit,
            CompletionReason::CancelledKeep => Self::Cancelled,
        }
    }
}

/// A completed scrolling session.
#[derive(Debug)]
pub struct SessionOutput {
    /// Assembled pixels.
    pub frame: Frame,
    /// Why capture stopped.
    pub reason: CompletionReason,
    /// Number of viewport frames captured, including stationary probes.
    pub captured_frames: usize,
    /// Number of accepted seams.
    pub seams: usize,
}

impl SessionOutput {
    /// Wraps the output with the provenance every downstream surface relies on.
    #[must_use]
    pub fn into_capture(self, target: CaptureTarget) -> Capture {
        Capture {
            frame: self.frame,
            provenance: Provenance::Stitched,
            target,
        }
    }
}

/// Timing and safety limits for an orchestrated capture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollSessionConfig {
    /// Scroll delivered after each accepted frame.
    pub gesture: ScrollGesture,
    /// Time given to smooth scrolling and repaint before capture.
    pub settle_delay: Duration,
    /// Poll cadence while the user scrolls manually.
    pub manual_poll_interval: Duration,
    /// Stationary manual polls required after movement to declare completion.
    pub manual_stall_limit: u32,
    /// Hard cap preventing an unbounded capture.
    pub max_frames: usize,
    /// Stitching thresholds.
    pub stitch: StitchConfig,
}

impl ScrollSessionConfig {
    /// A vertical session centred at `gesture.at`.
    #[must_use]
    pub fn new(gesture: ScrollGesture) -> Self {
        Self {
            gesture,
            settle_delay: Duration::from_millis(180),
            manual_poll_interval: Duration::from_millis(250),
            manual_stall_limit: 20,
            max_frames: 100,
            stitch: StitchConfig::default(),
        }
    }
}

/// Drives one scrolling capture to a deterministic stopping point.
pub struct ScrollSession<S, P> {
    source: S,
    driver: Box<dyn ScrollDriver>,
    pacer: P,
    config: ScrollSessionConfig,
}

impl<S, P> ScrollSession<S, P>
where
    S: FrameSource,
    P: Pacer,
{
    /// Creates a session from injectable edges.
    #[must_use]
    pub fn new(
        source: S,
        driver: Box<dyn ScrollDriver>,
        pacer: P,
        config: ScrollSessionConfig,
    ) -> Self {
        Self {
            source,
            driver,
            pacer,
            config,
        }
    }

    /// Runs until end-of-content, cancellation, overlap failure or the frame cap.
    pub fn run<C, F>(mut self, cancel: &mut C, mut progress: F) -> Result<SessionOutput>
    where
        C: CancelSignal,
        F: FnMut(Progress),
    {
        if self.config.gesture.axis != ScrollAxis::Vertical {
            return Err(Error::Unsupported {
                what: "horizontal scrolling capture".to_owned(),
                why: "the current stitcher aligns vertical frame sequences only".to_owned(),
            });
        }
        if self.config.gesture.amount <= 0.0 || !self.config.gesture.amount.is_finite() {
            return Err(Error::InvalidRequest(
                "scrolling capture needs a finite, positive scroll amount".to_owned(),
            ));
        }
        if self.config.max_frames == 0 {
            return Err(Error::InvalidRequest(
                "scrolling capture needs at least one frame".to_owned(),
            ));
        }

        let mut capabilities = self.driver.capabilities();
        if let Err(error) = self.driver.prepare() {
            match error {
                error @ (Error::PermissionDenied { .. } | Error::Unsupported { .. }) => {
                    self.driver = Box::new(scrozz_core::ManualScrollDriver::new(error.to_string()));
                    capabilities = self.driver.capabilities();
                    self.driver.prepare()?;
                }
                error => return Err(error),
            }
        }
        progress(Progress::Prepared {
            driver: self.driver.name().to_owned(),
            automatic: capabilities.is_automatic(),
            manual_reason: match &capabilities.synthesis {
                ScrollSynthesis::Automatic => None,
                ScrollSynthesis::Manual { why } => Some(why.clone()),
            },
        });

        let first = self.source.capture_frame()?;
        let mut stitcher = ScrollStitcher::new(self.config.stitch);
        if self.config.stitch.expected_delta.is_none() {
            stitcher.set_expected_delta(
                self.driver
                    .expected_physical_delta(&self.config.gesture, first.scale),
            );
        }
        stitcher.push_frame(first)?;
        let mut captured_frames = 1usize;
        let mut has_advanced = false;
        progress(Progress::FrameCaptured { frame: 1 });

        loop {
            if let Some(action) = cancel.cancellation() {
                return match action {
                    CancelAction::Abort => Err(Error::Cancelled),
                    CancelAction::Keep => finish(
                        stitcher,
                        CompletionReason::CancelledKeep,
                        captured_frames,
                        &mut progress,
                    ),
                };
            }
            if captured_frames >= self.config.max_frames {
                return finish(
                    stitcher,
                    CompletionReason::FrameLimit,
                    captured_frames,
                    &mut progress,
                );
            }

            if capabilities.is_automatic() {
                self.driver.scroll(&self.config.gesture)?;
                self.pacer.wait(self.config.settle_delay);
            } else {
                progress(Progress::WaitingForManualScroll);
                self.pacer.wait(self.config.manual_poll_interval);
            }

            let frame = self.source.capture_frame()?;
            captured_frames += 1;
            progress(Progress::FrameCaptured {
                frame: captured_frames,
            });
            match stitcher.push_frame(frame)? {
                PushOutcome::Started => {
                    return Err(Error::Platform(
                        "stitcher restarted after already receiving a frame".to_owned(),
                    ));
                }
                PushOutcome::Advanced {
                    delta,
                    seam,
                    output_height,
                } => {
                    has_advanced = true;
                    if capabilities.is_automatic() {
                        stitcher.set_expected_delta(Some(delta));
                    }
                    progress(Progress::Advanced {
                        frame: captured_frames,
                        delta,
                        seam,
                        output_height,
                    });
                }
                PushOutcome::NoMovement { stalls } => {
                    progress(Progress::Stalled { count: stalls });
                }
                PushOutcome::EndOfContent { stalls } => {
                    progress(Progress::Stalled { count: stalls });
                    if !capabilities.is_automatic()
                        && (!has_advanced || stalls < self.config.manual_stall_limit.max(1))
                    {
                        continue;
                    }
                    return finish(
                        stitcher,
                        CompletionReason::EndOfContent,
                        captured_frames,
                        &mut progress,
                    );
                }
                PushOutcome::InsufficientOverlap { reason } => {
                    return Err(Error::InvalidRequest(format!(
                        "scrolling frames do not overlap safely: {reason}. \
                         Scroll more slowly or use a smaller step."
                    )));
                }
            }
        }
    }
}

fn finish<F>(
    stitcher: ScrollStitcher,
    reason: CompletionReason,
    captured_frames: usize,
    progress: &mut F,
) -> Result<SessionOutput>
where
    F: FnMut(Progress),
{
    let summary = stitcher.summary();
    let frame = stitcher.finish_frame()?;
    progress(Progress::Finished {
        reason,
        frames: captured_frames,
        output_height: frame.height(),
    });
    Ok(SessionOutput {
        frame,
        reason,
        captured_frames,
        seams: summary.seams,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use scrozz_core::{
        ColorSpace, LogicalPoint, ManualScrollDriver, PhysicalSize, PixelFormat, ScaleFactor,
        ScrollCapabilities,
    };

    use super::*;
    use crate::AlignmentConfig;

    struct Frames {
        frames: VecDeque<Frame>,
    }

    impl FrameSource for Frames {
        fn capture_frame(&mut self) -> Result<Frame> {
            self.frames
                .pop_front()
                .ok_or_else(|| Error::Platform("fixture ran out of frames".to_owned()))
        }

        fn name(&self) -> &str {
            "fixture"
        }
    }

    #[derive(Default)]
    struct Driver {
        scrolls: usize,
    }

    impl ScrollDriver for Driver {
        fn capabilities(&self) -> ScrollCapabilities {
            ScrollCapabilities::automatic(false)
        }

        fn prepare(&mut self) -> Result<()> {
            Ok(())
        }

        fn scroll(&mut self, _gesture: &ScrollGesture) -> Result<()> {
            self.scrolls += 1;
            Ok(())
        }

        fn name(&self) -> &str {
            "fixture-driver"
        }
    }

    struct PermissionDriver;

    impl ScrollDriver for PermissionDriver {
        fn capabilities(&self) -> ScrollCapabilities {
            ScrollCapabilities::automatic(true)
        }

        fn prepare(&mut self) -> Result<()> {
            Err(Error::PermissionDenied {
                capability: "fixture input".to_owned(),
                remedy: "grant it".to_owned(),
            })
        }

        fn scroll(&mut self, _gesture: &ScrollGesture) -> Result<()> {
            panic!("a denied driver must be replaced before scrolling")
        }

        fn name(&self) -> &str {
            "denied-fixture"
        }
    }

    struct CancelAfter {
        polls: usize,
        after: usize,
        action: CancelAction,
    }

    impl CancelSignal for CancelAfter {
        fn cancellation(&mut self) -> Option<CancelAction> {
            self.polls += 1;
            (self.polls > self.after).then_some(self.action)
        }
    }

    fn frame(rows: &[u8]) -> Frame {
        let width = 6;
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
            scale: ScaleFactor::IDENTITY,
        }
    }

    fn config(max_frames: usize) -> ScrollSessionConfig {
        let mut config =
            ScrollSessionConfig::new(ScrollGesture::down(LogicalPoint::new(10.0, 10.0), 3.0));
        config.max_frames = max_frames;
        config.settle_delay = Duration::ZERO;
        config.stitch.alignment = AlignmentConfig {
            min_overlap: 3,
            row_buckets: 4,
            top_k: 5,
            basin_radius: 1,
            max_mean_error: 8,
            min_confidence: 1,
            ..AlignmentConfig::default()
        };
        config
    }

    #[test]
    fn a_session_reports_progress_and_stops_at_end_of_content() {
        let document: Vec<u8> = (0..18).map(|v| v * 10).collect();
        let last = frame(&document[6..14]);
        let source = Frames {
            frames: VecDeque::from([
                frame(&document[0..8]),
                frame(&document[3..11]),
                last.clone(),
                last.clone(),
                last,
            ]),
        };
        let session = ScrollSession::new(source, Box::<Driver>::default(), NoopPacer, config(8));
        let mut events = Vec::new();
        let output = session
            .run(&mut NeverCancel, |event| events.push(event))
            .expect("session");
        assert_eq!(output.reason, CompletionReason::EndOfContent);
        assert_eq!(output.frame.height(), 14);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Progress::Advanced { .. }))
        );
        assert!(matches!(events.last(), Some(Progress::Finished { .. })));
    }

    #[test]
    fn keep_returns_a_partial_image_while_abort_discards_it() {
        let document: Vec<u8> = (0..18).map(|v| v * 10).collect();
        let make = || Frames {
            frames: VecDeque::from([frame(&document[0..8]), frame(&document[3..11])]),
        };

        let kept = ScrollSession::new(make(), Box::<Driver>::default(), NoopPacer, config(8))
            .run(
                &mut CancelAfter {
                    polls: 0,
                    after: 1,
                    action: CancelAction::Keep,
                },
                |_| {},
            )
            .expect("partial");
        assert_eq!(kept.reason, CompletionReason::CancelledKeep);
        assert_eq!(kept.frame.height(), 11);

        let aborted = ScrollSession::new(make(), Box::<Driver>::default(), NoopPacer, config(8))
            .run(
                &mut CancelAfter {
                    polls: 0,
                    after: 1,
                    action: CancelAction::Abort,
                },
                |_| {},
            )
            .expect_err("abort");
        assert!(aborted.is_cancellation());
    }

    #[test]
    fn manual_mode_waits_for_the_first_scroll_before_deciding_the_page_ended() {
        let document: Vec<u8> = (0..18).map(|v| v * 10).collect();
        let first = frame(&document[0..8]);
        let second = frame(&document[3..11]);
        let source = Frames {
            frames: VecDeque::from([
                first.clone(),
                first.clone(),
                first,
                second.clone(),
                second.clone(),
                second,
            ]),
        };
        let mut config = config(8);
        config.manual_stall_limit = 2;
        let output = ScrollSession::new(
            source,
            Box::new(ManualScrollDriver::new("fixture")),
            NoopPacer,
            config,
        )
        .run(&mut NeverCancel, |_| {})
        .expect("manual session");
        assert_eq!(output.reason, CompletionReason::EndOfContent);
        assert_eq!(output.frame.height(), 11);
    }

    #[test]
    fn denied_synthesis_falls_back_to_a_manual_session() {
        let source = Frames {
            frames: VecDeque::from([frame(&[10, 20, 30, 40, 50, 60])]),
        };
        let mut events = Vec::new();
        let output = ScrollSession::new(source, Box::new(PermissionDriver), NoopPacer, config(1))
            .run(&mut NeverCancel, |event| events.push(event))
            .expect("manual fallback");
        assert_eq!(output.reason, CompletionReason::FrameLimit);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                Progress::Prepared {
                    automatic: false,
                    manual_reason: Some(reason),
                    ..
                } if reason.contains("fixture input")
            )
        }));
    }
}
