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
    ScrollControl, ScrollDirection, ScrollDriver, ScrollGesture, ScrollSynthesis,
};

use crate::{
    PushOutcome, ScrollStitcher, SeamQuality, StitchConfig, StopReason, detect_scroll_direction,
};

/// Supplies successive viewport frames from one long-lived capture context.
///
/// A portal backend that creates and tears down its screen-cast session for
/// every ordinary capture is not a suitable implementation. Platform crates
/// should adapt a reusable owner that keeps its native stream alive and yields
/// copied [`Frame`]s without exposing those details here.
pub trait FrameSource {
    /// Captures the viewport in its current position.
    fn capture_frame(&mut self) -> Result<Frame>;

    /// Human-readable source name for diagnostics.
    fn name(&self) -> &str;
}

/// Adapts a borrowed capture backend without introducing a crate dependency.
///
/// Use this only when repeated calls reuse an already-authorized, inexpensive
/// capture path. Wayland callers should adapt their reusable portal/PipeWire
/// frame session instead.
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
    pub fn cancel(&self, action: CancelAction) -> bool {
        let requested = match action {
            CancelAction::Keep => 1,
            CancelAction::Abort => 2,
        };
        self.state.fetch_max(requested, Ordering::AcqRel) != 3
    }

    /// Seals the irreversible output boundary.
    ///
    /// Returns false only when Abort won first. Once sealed, later cancellation
    /// requests cannot make already-published output look discarded.
    pub fn seal_output(&self) -> bool {
        self.state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 2).then_some(3)
            })
            .is_ok()
    }

    /// Clears an earlier request before reusing the token.
    pub fn reset(&self) {
        self.state.store(0, Ordering::Release);
    }

    /// Returns the current request without consuming or mutating it.
    #[must_use]
    pub fn requested(&self) -> Option<CancelAction> {
        match self.state.load(Ordering::Acquire) {
            1 => Some(CancelAction::Keep),
            2 => Some(CancelAction::Abort),
            _ => None,
        }
    }
}

impl CancelSignal for AtomicCancellation {
    fn cancellation(&mut self) -> Option<CancelAction> {
        self.requested()
    }
}

/// A meaningful update suitable for a HUD, CLI log or test recorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// The baseline viewport is captured and the platform input path is ready.
    ///
    /// Interactive callers must not invite the user to scroll before this
    /// event: movement that happens before the baseline capture cannot be
    /// observed by direction detection.
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
    /// Real frame movement established the axis and sign for this session.
    DirectionDetected {
        /// Direction subsequent manual or automatic movement follows.
        direction: ScrollDirection,
    },
    /// New document content was accepted.
    Advanced {
        /// One-based frame number.
        frame: usize,
        /// Measured physical displacement.
        delta: u32,
        /// Seam quality.
        seam: SeamQuality,
        /// Current stitched length along the selected scroll axis.
        output_extent: u32,
        /// Current pixel height.
        output_height: u32,
    },
    /// A stationary frame was observed.
    Stalled {
        /// Consecutive stationary observations.
        count: u32,
    },
    /// Keep the accepted canvas while the user reconnects the viewport.
    WaitingForOverlap {
        /// Why this viewport could not safely extend the capture.
        reason: String,
    },
    /// Acquisition stopped, but an interactive capture still needs an explicit
    /// Finish or Discard before any output can be published.
    AwaitingFinish {
        /// Why acquisition cannot continue.
        reason: CompletionReason,
    },
    /// A later frame could not be captured or assembled; the valid prefix is
    /// retained. Interactive sessions still require Finish before returning it.
    Interrupted {
        /// Diagnostic from the terminal failure.
        reason: String,
    },
    /// The session produced its final image.
    Finished {
        /// Why it stopped.
        reason: CompletionReason,
        /// Captured viewport count.
        frames: usize,
        /// Final stitched length along the selected scroll axis.
        output_extent: u32,
        /// Final pixel height.
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
    /// A later viewport no longer overlapped, so the valid partial image was kept.
    OverlapLost,
    /// Capture or assembly failed after at least one seam, so the valid prefix
    /// was kept.
    Interrupted,
}

impl From<CompletionReason> for StopReason {
    fn from(reason: CompletionReason) -> Self {
        match reason {
            CompletionReason::EndOfContent => Self::EndOfContent,
            CompletionReason::FrameLimit => Self::FrameLimit,
            CompletionReason::CancelledKeep => Self::Cancelled,
            CompletionReason::OverlapLost => Self::OverlapLost,
            CompletionReason::Interrupted => Self::Interrupted,
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
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollSessionConfig {
    /// Scroll delivered after each accepted frame.
    pub gesture: ScrollGesture,
    /// Time given to smooth scrolling and repaint before capture.
    pub settle_delay: Duration,
    /// Poll cadence while the user scrolls manually.
    pub manual_poll_interval: Duration,
    /// Stationary manual polls required after movement to declare completion.
    pub manual_stall_limit: u32,
    /// Hard cap preventing an unbounded capture. Interactive sessions count
    /// accepted viewports, not idle probes, and wait for Finish at this limit.
    pub max_frames: usize,
    /// Stitching thresholds.
    pub stitch: StitchConfig,
    /// Explicit GUI input choice. `None` preserves adaptive CLI behavior.
    pub control: Option<ScrollControl>,
    /// Per-axis automatic step sizes when the user's first movement chooses the
    /// axis and sign.
    pub direction_detection: Option<ScrollDirectionAmounts>,
}

/// Positive automatic step sizes used after real frame movement picks a route.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollDirectionAmounts {
    /// Vertical step magnitude in logical points.
    pub vertical: f64,
    /// Horizontal step magnitude in logical points.
    pub horizontal: f64,
}

impl ScrollDirectionAmounts {
    /// Creates per-axis movement magnitudes.
    #[must_use]
    pub const fn new(vertical: f64, horizontal: f64) -> Self {
        Self {
            vertical,
            horizontal,
        }
    }

    fn for_direction(self, direction: ScrollDirection) -> f64 {
        match direction {
            ScrollDirection::Up | ScrollDirection::Down => self.vertical,
            ScrollDirection::Left | ScrollDirection::Right => self.horizontal,
        }
    }
}

impl ScrollSessionConfig {
    /// A session centred at `gesture.at` and moving along `gesture.axis`.
    #[must_use]
    pub fn new(gesture: ScrollGesture) -> Self {
        Self {
            gesture,
            settle_delay: Duration::from_millis(180),
            manual_poll_interval: Duration::from_millis(250),
            manual_stall_limit: 20,
            max_frames: 100,
            stitch: StitchConfig::default(),
            control: None,
            direction_detection: None,
        }
    }

    /// Uses one explicit input mode instead of adaptive fallback.
    #[must_use]
    pub const fn with_control(mut self, control: ScrollControl) -> Self {
        self.control = Some(control);
        self
    }

    /// Lets the first coherent viewport movement choose up/down/left/right.
    #[must_use]
    pub const fn with_direction_detection(
        mut self,
        vertical_amount: f64,
        horizontal_amount: f64,
    ) -> Self {
        self.direction_detection = Some(ScrollDirectionAmounts::new(
            vertical_amount,
            horizontal_amount,
        ));
        self
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

    /// Interactive sessions return pixels only after Finish; adaptive CLI
    /// sessions may also finish at end-of-content or a safety boundary.
    pub fn run<C, F>(mut self, cancel: &mut C, mut progress: F) -> Result<SessionOutput>
    where
        C: CancelSignal,
        F: FnMut(Progress),
    {
        if self.config.gesture.amount == 0.0 || !self.config.gesture.amount.is_finite() {
            return Err(Error::InvalidRequest(
                "scrolling capture needs a finite, non-zero scroll amount".to_owned(),
            ));
        }
        if self.config.direction_detection.is_some_and(|amounts| {
            !amounts.vertical.is_finite()
                || amounts.vertical <= 0.0
                || !amounts.horizontal.is_finite()
                || amounts.horizontal <= 0.0
        }) {
            return Err(Error::InvalidRequest(
                "automatic direction detection needs finite, positive step sizes on both axes"
                    .to_owned(),
            ));
        }
        if self.config.max_frames == 0 {
            return Err(Error::InvalidRequest(
                "scrolling capture needs at least one frame".to_owned(),
            ));
        }
        // Prove that frame acquisition works before asking for input-control
        // permission. In particular, a broken Wayland ScreenCast/PipeWire path
        // must not open a RemoteDesktop grant dialog it can never use.
        let first = self.source.capture_frame()?;
        let first_scale = first.scale;
        let mut captured_frames = 1usize;
        let mut has_advanced = false;
        progress(Progress::FrameCaptured { frame: 1 });
        let _ = cancellation_reason(cancel, has_advanced)?;

        if self.config.control == Some(ScrollControl::Manual) {
            self.driver = Box::new(scrozz_core::ManualScrollDriver::new(
                "manual mode was selected before capture",
            ));
        }
        let mut capabilities = self.driver.capabilities();
        if self.config.control == Some(ScrollControl::Automatic) && !capabilities.is_automatic() {
            let why = match &capabilities.synthesis {
                ScrollSynthesis::Manual { why } => why.clone(),
                ScrollSynthesis::Automatic => unreachable!(),
            };
            return Err(Error::Unsupported {
                what: "automatic scrolling for this target".to_owned(),
                why,
            });
        }
        if let Err(error) = self.driver.prepare() {
            if self.config.control != Some(ScrollControl::Automatic)
                && is_recoverable_synthesis_error(&error)
            {
                self.driver = Box::new(scrozz_core::ManualScrollDriver::new(error.to_string()));
                capabilities = self.driver.capabilities();
                self.driver.prepare()?;
            } else {
                return Err(error);
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

        let mut stitcher = if let Some(amounts) = self.config.direction_detection {
            let mut probes = 0u32;
            let baseline = first;
            let mut recent = None;
            loop {
                let mut finish_after_probe =
                    matches!(poll_cancel_action(cancel)?, Some(CancelAction::Keep));
                if self.config.control.is_none() && captured_frames >= self.config.max_frames {
                    return Err(no_movement_error(
                        "scrolling capture reached its frame limit without detecting movement",
                    ));
                }
                if !finish_after_probe {
                    progress(Progress::WaitingForManualScroll);
                    self.pacer.wait(self.config.manual_poll_interval);
                }
                finish_after_probe |=
                    matches!(poll_cancel_action(cancel)?, Some(CancelAction::Keep));
                let frame = self.source.capture_frame()?;
                captured_frames = captured_frames.saturating_add(1);
                progress(Progress::FrameCaptured {
                    frame: captured_frames,
                });
                finish_after_probe |=
                    matches!(poll_cancel_action(cancel)?, Some(CancelAction::Keep));

                let detected_from_baseline =
                    detect_scroll_direction(&baseline, &frame, &self.config.stitch)?;
                let detected_from_recent = if detected_from_baseline.is_none() {
                    recent
                        .as_ref()
                        .map(|anchor| detect_scroll_direction(anchor, &frame, &self.config.stitch))
                        .transpose()?
                        .flatten()
                } else {
                    None
                };
                let Some(direction) = detected_from_baseline.or(detected_from_recent) else {
                    if finish_after_probe {
                        return Err(no_movement_error(
                            "cannot keep a scrolling capture before the viewport moves",
                        ));
                    }
                    probes = probes.saturating_add(1);
                    progress(Progress::Stalled { count: probes });
                    recent = Some(frame);
                    continue;
                };
                let anchor = if detected_from_baseline.is_some() {
                    baseline
                } else {
                    recent.take().expect("a recent direction has an anchor")
                };
                self.config.gesture.axis = direction.axis();
                self.config.gesture.amount = direction.amount(amounts.for_direction(direction));
                let mut detected = ScrollStitcher::for_direction(direction, self.config.stitch);
                let started = detected.push_frame(anchor)?;
                if started != PushOutcome::Started {
                    return Err(Error::Platform(
                        "direction detector did not initialize its stitcher".to_owned(),
                    ));
                }
                let outcome = detected.push_frame(frame)?;
                let PushOutcome::Advanced {
                    delta,
                    seam,
                    output_extent,
                    output_height,
                } = outcome
                else {
                    return Err(Error::Platform(format!(
                        "detected {direction:?} movement could not initialize stitching: {outcome:?}"
                    )));
                };
                has_advanced = true;
                progress(Progress::DirectionDetected { direction });
                progress(Progress::Advanced {
                    frame: captured_frames,
                    delta,
                    seam,
                    output_extent,
                    output_height,
                });
                if finish_after_probe {
                    return finish_checked(
                        detected,
                        CompletionReason::CancelledKeep,
                        captured_frames,
                        cancel,
                        &mut progress,
                    );
                }
                break detected;
            }
        } else {
            let direction = self.config.gesture.direction().ok_or_else(|| {
                Error::InvalidRequest("scrolling capture gesture cannot be a no-op".to_owned())
            })?;
            let mut fixed = ScrollStitcher::for_direction(direction, self.config.stitch);
            fixed.push_frame(first)?;
            fixed
        };

        if self.config.stitch.expected_delta.is_none() {
            stitcher.set_expected_delta(
                self.driver
                    .expected_physical_delta(&self.config.gesture, first_scale),
            );
        }

        let mut waiting_for_movement = false;
        loop {
            let mut finish_after_frame =
                matches!(poll_cancel_action(cancel)?, Some(CancelAction::Keep));
            let budget_frames = if self.config.control.is_some() {
                stitcher.summary().frames
            } else {
                captured_frames
            };
            if budget_frames >= self.config.max_frames {
                if !has_advanced {
                    return Err(no_movement_error(
                        "scrolling capture reached its frame limit without detecting movement",
                    ));
                }
                return self.complete_session(
                    stitcher,
                    if finish_after_frame {
                        CompletionReason::CancelledKeep
                    } else {
                        CompletionReason::FrameLimit
                    },
                    captured_frames,
                    cancel,
                    &mut progress,
                );
            }

            if !finish_after_frame {
                if capabilities.is_automatic() && !waiting_for_movement {
                    if let Err(error) = self.driver.scroll(&self.config.gesture) {
                        if let Some(reason) = cancellation_reason(cancel, has_advanced)? {
                            return finish_checked(
                                stitcher,
                                reason,
                                captured_frames,
                                cancel,
                                &mut progress,
                            );
                        }
                        if self.config.control == Some(ScrollControl::Automatic)
                            || !is_recoverable_synthesis_error(&error)
                        {
                            if !has_advanced {
                                return Err(error);
                            }
                            progress(Progress::Interrupted {
                                reason: error.to_string(),
                            });
                            return self.complete_session(
                                stitcher,
                                CompletionReason::Interrupted,
                                captured_frames,
                                cancel,
                                &mut progress,
                            );
                        }
                        self.driver =
                            Box::new(scrozz_core::ManualScrollDriver::new(error.to_string()));
                        capabilities = self.driver.capabilities();
                        if let Err(error) = self.driver.prepare() {
                            if !has_advanced {
                                return Err(error);
                            }
                            progress(Progress::Interrupted {
                                reason: error.to_string(),
                            });
                            return self.complete_session(
                                stitcher,
                                CompletionReason::Interrupted,
                                captured_frames,
                                cancel,
                                &mut progress,
                            );
                        }
                        progress(Progress::Prepared {
                            driver: self.driver.name().to_owned(),
                            automatic: false,
                            manual_reason: match &capabilities.synthesis {
                                ScrollSynthesis::Manual { why } => Some(why.clone()),
                                ScrollSynthesis::Automatic => None,
                            },
                        });
                        continue;
                    }
                    self.pacer.wait(self.config.settle_delay);
                } else {
                    progress(Progress::WaitingForManualScroll);
                    self.pacer.wait(self.config.manual_poll_interval);
                }
            }

            finish_after_frame |= matches!(poll_cancel_action(cancel)?, Some(CancelAction::Keep));
            let frame = match self.source.capture_frame() {
                Ok(frame) => frame,
                Err(_error) if finish_after_frame && has_advanced => {
                    return finish_checked(
                        stitcher,
                        CompletionReason::CancelledKeep,
                        captured_frames,
                        cancel,
                        &mut progress,
                    );
                }
                Err(error) if has_advanced => {
                    if let Some(reason) = cancellation_reason(cancel, has_advanced)? {
                        return finish_checked(
                            stitcher,
                            reason,
                            captured_frames,
                            cancel,
                            &mut progress,
                        );
                    }
                    progress(Progress::Interrupted {
                        reason: error.to_string(),
                    });
                    return self.complete_session(
                        stitcher,
                        CompletionReason::Interrupted,
                        captured_frames,
                        cancel,
                        &mut progress,
                    );
                }
                Err(error) => return Err(error),
            };
            captured_frames = captured_frames.saturating_add(1);
            progress(Progress::FrameCaptured {
                frame: captured_frames,
            });
            finish_after_frame |= matches!(poll_cancel_action(cancel)?, Some(CancelAction::Keep));
            let outcome = match stitcher.push_frame(frame) {
                Ok(outcome) => outcome,
                Err(_error) if finish_after_frame && has_advanced => {
                    return finish_checked(
                        stitcher,
                        CompletionReason::CancelledKeep,
                        captured_frames,
                        cancel,
                        &mut progress,
                    );
                }
                Err(error) if has_advanced => {
                    if let Some(reason) = cancellation_reason(cancel, has_advanced)? {
                        return finish_checked(
                            stitcher,
                            reason,
                            captured_frames,
                            cancel,
                            &mut progress,
                        );
                    }
                    progress(Progress::Interrupted {
                        reason: error.to_string(),
                    });
                    return self.complete_session(
                        stitcher,
                        CompletionReason::Interrupted,
                        captured_frames,
                        cancel,
                        &mut progress,
                    );
                }
                Err(error) => return Err(error),
            };
            match outcome {
                PushOutcome::Started => {
                    return Err(Error::Platform(
                        "stitcher restarted after already receiving a frame".to_owned(),
                    ));
                }
                PushOutcome::Advanced {
                    delta,
                    seam,
                    output_extent,
                    output_height,
                } => {
                    has_advanced = true;
                    waiting_for_movement = false;
                    stitcher.set_expected_delta(Some(delta));
                    progress(Progress::Advanced {
                        frame: captured_frames,
                        delta,
                        seam,
                        output_extent,
                        output_height,
                    });
                    if finish_after_frame {
                        return finish_checked(
                            stitcher,
                            CompletionReason::CancelledKeep,
                            captured_frames,
                            cancel,
                            &mut progress,
                        );
                    }
                }
                PushOutcome::NoMovement { stalls } => {
                    progress(Progress::Stalled { count: stalls });
                    if finish_after_frame {
                        if !has_advanced {
                            return Err(no_movement_error(
                                "cannot keep a scrolling capture before the viewport moves",
                            ));
                        }
                        return finish_checked(
                            stitcher,
                            CompletionReason::CancelledKeep,
                            captured_frames,
                            cancel,
                            &mut progress,
                        );
                    }
                }
                PushOutcome::EndOfContent { stalls } => {
                    progress(Progress::Stalled { count: stalls });
                    if finish_after_frame {
                        if !has_advanced {
                            return Err(no_movement_error(
                                "cannot keep a scrolling capture before the viewport moves",
                            ));
                        }
                        return finish_checked(
                            stitcher,
                            CompletionReason::CancelledKeep,
                            captured_frames,
                            cancel,
                            &mut progress,
                        );
                    }
                    if self.config.control.is_some() {
                        waiting_for_movement = true;
                        continue;
                    }
                    if !capabilities.is_automatic()
                        && (!has_advanced || stalls < self.config.manual_stall_limit.max(1))
                    {
                        continue;
                    }
                    if !has_advanced {
                        return Err(no_movement_error(
                            "scrolling capture ended before the viewport moved",
                        ));
                    }
                    return self.complete_session(
                        stitcher,
                        CompletionReason::EndOfContent,
                        captured_frames,
                        cancel,
                        &mut progress,
                    );
                }
                PushOutcome::InsufficientOverlap { reason } => {
                    if self.config.control.is_some() && !finish_after_frame {
                        waiting_for_movement = true;
                        progress(Progress::WaitingForOverlap { reason });
                        continue;
                    }
                    if has_advanced {
                        return self.complete_session(
                            stitcher,
                            if finish_after_frame {
                                CompletionReason::CancelledKeep
                            } else {
                                CompletionReason::OverlapLost
                            },
                            captured_frames,
                            cancel,
                            &mut progress,
                        );
                    }
                    return Err(Error::InvalidRequest(format!(
                        "scrolling frames do not overlap safely: {reason}. \
                         Scroll more slowly or use a smaller step."
                    )));
                }
            }
        }
    }

    fn complete_session<C, F>(
        &mut self,
        stitcher: ScrollStitcher,
        mut reason: CompletionReason,
        captured_frames: usize,
        cancel: &mut C,
        progress: &mut F,
    ) -> Result<SessionOutput>
    where
        C: CancelSignal,
        F: FnMut(Progress),
    {
        if self.config.control.is_some() && reason != CompletionReason::CancelledKeep {
            progress(Progress::AwaitingFinish { reason });
            loop {
                if matches!(poll_cancel_action(cancel)?, Some(CancelAction::Keep)) {
                    reason = CompletionReason::CancelledKeep;
                    break;
                }
                self.pacer.wait(self.config.manual_poll_interval);
            }
        }
        finish_checked(stitcher, reason, captured_frames, cancel, progress)
    }
}

const fn is_recoverable_synthesis_error(error: &Error) -> bool {
    matches!(
        error,
        Error::PermissionDenied { .. } | Error::Unsupported { .. } | Error::Platform(_)
    )
}

fn no_movement_error(message: &str) -> Error {
    Error::InvalidRequest(format!(
        "{message}; move the target once in any one direction and try again"
    ))
}

fn cancellation_reason<C>(cancel: &mut C, has_advanced: bool) -> Result<Option<CompletionReason>>
where
    C: CancelSignal,
{
    match poll_cancel_action(cancel)? {
        Some(CancelAction::Keep) if has_advanced => Ok(Some(CompletionReason::CancelledKeep)),
        Some(CancelAction::Keep) => Err(no_movement_error(
            "cannot keep a scrolling capture before the viewport moves",
        )),
        None => Ok(None),
        Some(CancelAction::Abort) => unreachable!("abort is returned as an error"),
    }
}

fn poll_cancel_action<C>(cancel: &mut C) -> Result<Option<CancelAction>>
where
    C: CancelSignal,
{
    match cancel.cancellation() {
        Some(CancelAction::Abort) => Err(Error::Cancelled),
        action => Ok(action),
    }
}

fn finish_checked<C, F>(
    stitcher: ScrollStitcher,
    reason: CompletionReason,
    captured_frames: usize,
    cancel: &mut C,
    progress: &mut F,
) -> Result<SessionOutput>
where
    C: CancelSignal,
    F: FnMut(Progress),
{
    let mut reason = cancellation_reason(cancel, true)?.unwrap_or(reason);
    let summary = stitcher.summary();
    let frame = stitcher.finish_frame()?;
    if let Some(after_assembly) = cancellation_reason(cancel, true)? {
        reason = after_assembly;
    }
    progress(Progress::Finished {
        reason,
        frames: captured_frames,
        output_extent: summary.output_extent,
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
    use std::sync::{Arc, Mutex};

    use scrozz_core::{
        ColorSpace, LogicalPoint, ManualScrollDriver, PhysicalSize, PixelFormat, ScaleFactor,
        ScrollAxis, ScrollCapabilities, ScrollControl, ScrollDirection,
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

    struct RecordingDriver {
        gestures: Arc<Mutex<Vec<ScrollGesture>>>,
    }

    impl ScrollDriver for RecordingDriver {
        fn capabilities(&self) -> ScrollCapabilities {
            ScrollCapabilities::automatic(false)
        }

        fn prepare(&mut self) -> Result<()> {
            Ok(())
        }

        fn scroll(&mut self, gesture: &ScrollGesture) -> Result<()> {
            self.gestures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(gesture.clone());
            Ok(())
        }

        fn name(&self) -> &str {
            "recording-driver"
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

    struct PanicPrepareDriver;

    impl ScrollDriver for PanicPrepareDriver {
        fn capabilities(&self) -> ScrollCapabilities {
            ScrollCapabilities::automatic(true)
        }

        fn prepare(&mut self) -> Result<()> {
            panic!("input permission must not be requested before frame acquisition succeeds")
        }

        fn scroll(&mut self, _gesture: &ScrollGesture) -> Result<()> {
            unreachable!("prepare never succeeds")
        }

        fn name(&self) -> &str {
            "panic-prepare"
        }
    }

    #[derive(Default)]
    struct TargetGoneAfterOneScroll {
        scrolls: usize,
    }

    impl ScrollDriver for TargetGoneAfterOneScroll {
        fn capabilities(&self) -> ScrollCapabilities {
            ScrollCapabilities::automatic(true)
        }

        fn prepare(&mut self) -> Result<()> {
            Ok(())
        }

        fn scroll(&mut self, _gesture: &ScrollGesture) -> Result<()> {
            self.scrolls += 1;
            if self.scrolls == 1 {
                Ok(())
            } else {
                Err(Error::TargetGone("fixture window closed".to_owned()))
            }
        }

        fn name(&self) -> &str {
            "target-gone-fixture"
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

    fn finish_after_frame(
        session: ScrollSession<Frames, NoopPacer>,
        finish_at: usize,
    ) -> (SessionOutput, Vec<Progress>) {
        let request = AtomicCancellation::default();
        let mut signal = request.clone();
        let mut events = Vec::new();
        let output = session
            .run(&mut signal, |event| {
                assert!(
                    !matches!(event, Progress::AwaitingFinish { .. }),
                    "paused too early: {event:?}"
                );
                if matches!(event, Progress::FrameCaptured { frame } if frame == finish_at) {
                    request.cancel(CancelAction::Keep);
                }
                events.push(event);
            })
            .expect("explicit Finish");
        assert_eq!(output.reason, CompletionReason::CancelledKeep);
        (output, events)
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
    fn automatic_mode_learns_upward_motion_before_driving_the_same_route() {
        let document: Vec<u8> = (0..18).map(|value| value * 10).collect();
        let first = frame(&document[6..14]);
        let source = Frames {
            frames: VecDeque::from([
                first.clone(),
                first,
                frame(&document[3..11]),
                frame(&document[0..8]),
            ]),
        };
        let gestures = Arc::new(Mutex::new(Vec::new()));
        let driver = RecordingDriver {
            gestures: Arc::clone(&gestures),
        };
        let mut config = config(4)
            .with_control(ScrollControl::Automatic)
            .with_direction_detection(3.0, 3.0);
        config.manual_poll_interval = Duration::ZERO;
        let (output, events) = finish_after_frame(
            ScrollSession::new(source, Box::new(driver), NoopPacer, config),
            4,
        );
        assert_eq!(output.frame.height(), 14);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                Progress::DirectionDetected {
                    direction: ScrollDirection::Up
                }
            )
        }));
        let gestures = gestures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(gestures.len(), 1);
        assert_eq!(gestures[0].direction(), Some(ScrollDirection::Up));
    }

    #[test]
    fn direction_learning_recovers_after_unalignable_pre_scroll_repaints() {
        let document: Vec<u8> = (0..14).map(|value| value * 10).collect();
        let source = Frames {
            frames: VecDeque::from([
                frame(&[0; 8]),
                frame(&[255; 8]),
                frame(&document[0..8]),
                frame(&document[3..11]),
            ]),
        };
        let config = config(4)
            .with_control(ScrollControl::Manual)
            .with_direction_detection(3.0, 3.0);
        let (output, events) = finish_after_frame(
            ScrollSession::new(source, Box::<Driver>::default(), NoopPacer, config),
            4,
        );
        assert_eq!(output.frame.height(), 11);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                Progress::DirectionDetected {
                    direction: ScrollDirection::Down
                }
            )
        }));
    }

    #[test]
    fn direction_learning_keeps_the_pre_scroll_baseline_across_a_transient_frame() {
        let document: Vec<u8> = (0..14).map(|value| value * 10).collect();
        let after = frame(&document[3..11]);
        let source = Frames {
            frames: VecDeque::from([
                frame(&document[0..8]),
                frame(&[255; 8]),
                after.clone(),
                after,
            ]),
        };
        let config = config(4)
            .with_control(ScrollControl::Manual)
            .with_direction_detection(3.0, 3.0);
        let (output, events) = finish_after_frame(
            ScrollSession::new(source, Box::<Driver>::default(), NoopPacer, config),
            4,
        );

        assert_eq!(output.frame.height(), 11);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                Progress::DirectionDetected {
                    direction: ScrollDirection::Down
                }
            )
        }));
    }

    #[test]
    fn direction_learning_prefers_the_original_baseline_when_both_pairs_align() {
        let document: Vec<u8> = (0..14).map(|value| value * 10).collect();
        let final_frame = frame(&document[3..11]);
        let source = Frames {
            frames: VecDeque::from([
                frame(&document[0..8]),
                frame(&document[1..9]),
                final_frame.clone(),
                final_frame,
            ]),
        };
        let config = config(4)
            .with_control(ScrollControl::Manual)
            .with_direction_detection(3.0, 3.0);

        let (output, _) = finish_after_frame(
            ScrollSession::new(source, Box::<Driver>::default(), NoopPacer, config),
            4,
        );

        let rows: Vec<u8> = output
            .frame
            .data
            .chunks_exact(output.frame.stride)
            .map(|row| row[0])
            .collect();
        assert_eq!(rows, document[0..11]);
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
                    after: 4,
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
                    after: 4,
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
        let document: Vec<u8> = (0..12).map(|value| value * 10).collect();
        let source = Frames {
            frames: VecDeque::from([frame(&document[0..8]), frame(&document[3..11])]),
        };
        let mut events = Vec::new();
        let output = ScrollSession::new(source, Box::new(PermissionDriver), NoopPacer, config(2))
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

    #[test]
    fn explicit_automatic_mode_reports_permission_failure_without_switching_modes() {
        let source = Frames {
            frames: VecDeque::from([frame(&[10, 20, 30, 40, 50, 60, 70, 80])]),
        };
        let config = config(2).with_control(ScrollControl::Automatic);
        let error = ScrollSession::new(source, Box::new(PermissionDriver), NoopPacer, config)
            .run(&mut NeverCancel, |_| {})
            .expect_err("automatic mode must remain automatic");
        assert!(matches!(error, Error::PermissionDenied { .. }));
    }

    #[test]
    fn explicit_manual_mode_waits_for_finish_instead_of_auto_ending_when_idle() {
        let document: Vec<u8> = (0..14).map(|value| value * 10).collect();
        let moved = frame(&document[3..11]);
        let source = Frames {
            frames: VecDeque::from([frame(&document[0..8]), moved.clone(), moved.clone(), moved]),
        };
        let mut events = Vec::new();
        let config = config(8).with_control(ScrollControl::Manual);
        let output = ScrollSession::new(source, Box::<Driver>::default(), NoopPacer, config)
            .run(
                &mut CancelAfter {
                    polls: 0,
                    after: 6,
                    action: CancelAction::Keep,
                },
                |event| events.push(event),
            )
            .expect("manual finish");
        assert_eq!(output.reason, CompletionReason::CancelledKeep);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                Progress::Prepared {
                    automatic: false,
                    ..
                }
            )
        }));
    }

    #[test]
    fn finish_analyzes_the_frame_that_triggered_the_request() {
        let document: Vec<u8> = (0..18).map(|value| value * 10).collect();
        for (finish_at, expected_height) in [(2, 11), (3, 14)] {
            let source = Frames {
                frames: VecDeque::from([
                    frame(&document[0..8]),
                    frame(&document[3..11]),
                    frame(&document[6..14]),
                ]),
            };
            let cancellation = AtomicCancellation::default();
            let request = cancellation.clone();
            let mut signal = cancellation.clone();
            let output = ScrollSession::new(
                source,
                Box::<Driver>::default(),
                NoopPacer,
                config(8).with_control(ScrollControl::Manual),
            )
            .run(&mut signal, |event| {
                if matches!(event, Progress::FrameCaptured { frame } if frame == finish_at) {
                    assert!(request.cancel(CancelAction::Keep));
                }
            })
            .expect("Finish should retain the newest captured viewport");

            assert_eq!(output.reason, CompletionReason::CancelledKeep);
            assert_eq!(output.frame.height(), expected_height);
        }
    }

    #[test]
    fn interactive_idle_probes_do_not_spend_the_frame_budget_or_finish_capture() {
        let document: Vec<u8> = (0..20).map(|value| value * 10).collect();
        for control in [ScrollControl::Manual, ScrollControl::Automatic] {
            let moved = frame(&document[3..11]);
            let mut frames = VecDeque::from([frame(&document[0..8]), moved.clone()]);
            frames.extend(std::iter::repeat_n(moved, 8));
            frames.extend([frame(&document[6..14]), frame(&document[9..17])]);
            let finish_at = frames.len();
            let gestures = Arc::new(Mutex::new(Vec::new()));
            let driver = RecordingDriver {
                gestures: Arc::clone(&gestures),
            };
            let (output, events) = finish_after_frame(
                ScrollSession::new(
                    Frames { frames },
                    Box::new(driver),
                    NoopPacer,
                    config(4)
                        .with_control(control)
                        .with_direction_detection(3.0, 3.0),
                ),
                finish_at,
            );
            assert_eq!(output.frame.height(), 17);
            assert_eq!(output.captured_frames, finish_at);
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, Progress::Stalled { count } if *count > 4))
            );
            let scrolls = gestures.lock().expect("gestures").len();
            assert_eq!(
                scrolls,
                if control == ScrollControl::Automatic {
                    3
                } else {
                    0
                },
                "automatic input pauses when stationary and resumes after real movement"
            );
        }
    }

    #[test]
    fn interactive_direction_detection_waits_beyond_the_old_probe_limit() {
        let document: Vec<u8> = (0..14).map(|value| value * 10).collect();
        for control in [ScrollControl::Manual, ScrollControl::Automatic] {
            let mut frames: VecDeque<_> = std::iter::repeat_n(frame(&document[0..8]), 10).collect();
            frames.push_back(frame(&document[3..11]));
            let (output, _) = finish_after_frame(
                ScrollSession::new(
                    Frames { frames },
                    Box::<Driver>::default(),
                    NoopPacer,
                    config(2)
                        .with_control(control)
                        .with_direction_detection(3.0, 3.0),
                ),
                11,
            );
            assert_eq!(output.frame.height(), 11);
        }
    }

    #[test]
    fn interactive_overlap_loss_waits_for_reconnection_without_saving() {
        let document: Vec<u8> = (0..20).map(|value| value * 10).collect();
        for control in [ScrollControl::Manual, ScrollControl::Automatic] {
            let source = Frames {
                frames: VecDeque::from([
                    frame(&document[0..8]),
                    frame(&document[3..11]),
                    frame(&[255; 8]),
                    frame(&document[6..14]),
                    frame(&document[9..17]),
                ]),
            };
            let (output, events) = finish_after_frame(
                ScrollSession::new(
                    source,
                    Box::<Driver>::default(),
                    NoopPacer,
                    config(8)
                        .with_control(control)
                        .with_direction_detection(3.0, 3.0),
                ),
                5,
            );
            assert_eq!(output.frame.height(), 17);
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, Progress::WaitingForOverlap { .. }))
            );
        }
    }

    #[test]
    fn interactive_limits_and_interruptions_wait_for_finish_or_discard() {
        let document: Vec<u8> = (0..18).map(|value| value * 10).collect();
        for action in [CancelAction::Keep, CancelAction::Abort] {
            for failure in ["frame-limit", "capture-error", "byte-limit", "driver-error"] {
                let mut config = config(if failure == "frame-limit" { 2 } else { 8 })
                    .with_control(ScrollControl::Automatic);
                let mut frames = VecDeque::from([frame(&document[0..8]), frame(&document[3..11])]);
                if failure == "byte-limit" {
                    config.stitch.max_output_bytes = 6 * 11 * 4;
                    frames.push_back(frame(&document[6..14]));
                }
                let driver: Box<dyn ScrollDriver> = if failure == "driver-error" {
                    Box::<TargetGoneAfterOneScroll>::default()
                } else {
                    Box::<Driver>::default()
                };
                let request = AtomicCancellation::default();
                let mut signal = request.clone();
                let mut paused = false;
                let result = ScrollSession::new(Frames { frames }, driver, NoopPacer, config).run(
                    &mut signal,
                    |event| {
                        if let Progress::AwaitingFinish { reason } = event {
                            assert_eq!(
                                reason,
                                if failure == "frame-limit" {
                                    CompletionReason::FrameLimit
                                } else {
                                    CompletionReason::Interrupted
                                }
                            );
                            paused = true;
                            request.cancel(action);
                        }
                        if matches!(event, Progress::Finished { .. }) {
                            assert!(paused, "no unsolicited output for {failure}");
                            assert_eq!(action, CancelAction::Keep);
                        }
                    },
                );
                assert!(paused, "{failure} must preserve the pending choice");
                match action {
                    CancelAction::Keep => {
                        let output = result.expect("explicit Finish keeps the valid prefix");
                        assert_eq!(output.reason, CompletionReason::CancelledKeep);
                        assert_eq!(output.frame.height(), 11);
                    }
                    CancelAction::Abort => assert!(result.expect_err("Discard").is_cancellation()),
                }
            }
        }
    }

    #[test]
    fn frame_acquisition_is_proven_before_input_permission_is_requested() {
        let source = Frames {
            frames: VecDeque::new(),
        };
        let error = ScrollSession::new(source, Box::new(PanicPrepareDriver), NoopPacer, config(2))
            .run(&mut NeverCancel, |_| {})
            .expect_err("an unavailable frame source");
        assert!(error.to_string().contains("ran out"), "{error}");
    }

    #[test]
    fn prepared_is_emitted_only_after_the_baseline_frame_is_captured() {
        let document: Vec<u8> = (0..14).map(|value| value * 10).collect();
        let source = Frames {
            frames: VecDeque::from([frame(&document[0..8]), frame(&document[3..11])]),
        };
        let (_, events) = finish_after_frame(
            ScrollSession::new(
                source,
                Box::<Driver>::default(),
                NoopPacer,
                config(2)
                    .with_control(ScrollControl::Manual)
                    .with_direction_detection(3.0, 3.0),
            ),
            2,
        );

        assert!(matches!(
            events.as_slice(),
            [
                Progress::FrameCaptured { frame: 1 },
                Progress::Prepared { .. },
                Progress::WaitingForManualScroll,
                Progress::FrameCaptured { frame: 2 },
                ..
            ]
        ));
    }

    #[test]
    fn a_later_capture_error_keeps_the_valid_partial_canvas() {
        let document: Vec<u8> = (0..12).map(|value| value * 10).collect();
        let source = Frames {
            frames: VecDeque::from([frame(&document[0..8]), frame(&document[3..11])]),
        };
        let mut events = Vec::new();
        let output = ScrollSession::new(source, Box::<Driver>::default(), NoopPacer, config(8))
            .run(&mut NeverCancel, |event| events.push(event))
            .expect("valid prefix is salvaged");

        assert_eq!(output.reason, CompletionReason::Interrupted);
        assert_eq!(output.frame.height(), 11);
        assert!(events.iter().any(
            |event| matches!(event, Progress::Interrupted { reason } if reason.contains("ran out"))
        ));
    }

    #[test]
    fn abort_wins_when_requested_as_an_end_of_content_frame_arrives() {
        let document: Vec<u8> = (0..18).map(|value| value * 10).collect();
        let final_frame = frame(&document[3..11]);
        let source = Frames {
            frames: VecDeque::from([
                frame(&document[0..8]),
                final_frame.clone(),
                final_frame.clone(),
                final_frame,
            ]),
        };
        let cancellation = AtomicCancellation::default();
        let request = cancellation.clone();
        let mut signal = cancellation.clone();
        let result = ScrollSession::new(source, Box::<Driver>::default(), NoopPacer, config(8))
            .run(&mut signal, move |progress| {
                if matches!(progress, Progress::FrameCaptured { frame: 4 }) {
                    request.cancel(CancelAction::Abort);
                }
            });

        assert!(
            result
                .expect_err("abort must override terminal overlap")
                .is_cancellation()
        );
    }

    #[test]
    fn abort_is_monotonic_over_keep_until_an_explicit_reset() {
        let cancellation = AtomicCancellation::default();
        cancellation.cancel(CancelAction::Abort);
        cancellation.cancel(CancelAction::Keep);
        assert_eq!(cancellation.requested(), Some(CancelAction::Abort));

        cancellation.reset();
        assert_eq!(cancellation.requested(), None);
        cancellation.cancel(CancelAction::Keep);
        cancellation.cancel(CancelAction::Abort);
        assert_eq!(cancellation.requested(), Some(CancelAction::Abort));
    }

    #[test]
    fn output_seal_and_abort_have_one_atomic_winner() {
        let cancellation = AtomicCancellation::default();
        assert!(cancellation.seal_output());
        assert!(!cancellation.cancel(CancelAction::Abort));
        assert_eq!(cancellation.requested(), None);

        cancellation.reset();
        assert!(cancellation.cancel(CancelAction::Abort));
        assert!(!cancellation.seal_output());
        assert_eq!(cancellation.requested(), Some(CancelAction::Abort));
    }

    #[test]
    fn concurrent_keep_requests_cannot_overwrite_abort() {
        let cancellation = AtomicCancellation::default();
        let mut threads = Vec::new();
        for index in 0..16 {
            let request = cancellation.clone();
            threads.push(std::thread::spawn(move || {
                request.cancel(if index == 7 {
                    CancelAction::Abort
                } else {
                    CancelAction::Keep
                });
            }));
        }
        for thread in threads {
            thread.join().expect("cancellation requester");
        }
        assert_eq!(cancellation.requested(), Some(CancelAction::Abort));
    }

    #[test]
    fn abort_arriving_during_final_assembly_discards_the_finished_canvas() {
        let document: Vec<u8> = (0..18).map(|value| value * 10).collect();
        let mut stitcher = ScrollStitcher::for_axis(ScrollAxis::Vertical, config(8).stitch);
        assert_eq!(
            stitcher
                .push_frame(frame(&document[0..8]))
                .expect("first frame"),
            PushOutcome::Started
        );
        assert!(matches!(
            stitcher
                .push_frame(frame(&document[3..11]))
                .expect("second frame"),
            PushOutcome::Advanced { .. }
        ));

        let error = finish_checked(
            stitcher,
            CompletionReason::EndOfContent,
            2,
            &mut CancelAfter {
                polls: 0,
                after: 1,
                action: CancelAction::Abort,
            },
            &mut |_| {},
        )
        .expect_err("the second cancellation poll happens after canvas assembly");
        assert!(error.is_cancellation());
    }

    #[test]
    fn a_later_target_loss_keeps_the_valid_partial_canvas() {
        let document: Vec<u8> = (0..18).map(|value| value * 10).collect();
        let source = Frames {
            frames: VecDeque::from([frame(&document[0..8]), frame(&document[3..11])]),
        };
        let mut events = Vec::new();
        let output = ScrollSession::new(
            source,
            Box::<TargetGoneAfterOneScroll>::default(),
            NoopPacer,
            config(8),
        )
        .run(&mut NeverCancel, |event| events.push(event))
        .expect("valid prefix is salvaged");

        assert_eq!(output.reason, CompletionReason::Interrupted);
        assert_eq!(output.frame.height(), 11);
        assert!(events.iter().any(
            |event| matches!(event, Progress::Interrupted { reason } if reason.contains("closed"))
        ));
    }
}
