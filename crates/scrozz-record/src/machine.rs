//! Virtual-clock recording state machine.
//!
//! Time advances only through [`RecordingMachine::tick`]. No wall clock enters
//! this module, so countdown overshoot, pause exclusion, engine drift, and every
//! terminal transition are deterministic.

use std::{collections::VecDeque, fmt, path::PathBuf, sync::Arc, time::Duration};

use scrozz_core::{CaptureTarget, Error, Result};

use crate::{
    OverlaySource, Recording, RecordingEngine, RecordingRequest, RecordingSession,
    RecordingSettings, SessionEvent,
    engine::{EngineCapabilities, detect_native_engine, validate_capabilities},
};

/// Difference large enough to emit a drift event.
pub const DRIFT_EVENT_THRESHOLD_SECS: f64 = 0.050;
/// Maximum recoverable warnings retained for one run.
pub const MAX_WARNING_HISTORY: usize = 64;
/// Maximum undrained machine events retained across runs.
pub const MAX_PENDING_EVENTS: usize = 256;

/// Exact lifecycle phases of a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordingPhase {
    /// No selection or recording is active.
    #[default]
    Idle,
    /// An interactive selector is choosing a target.
    Selecting,
    /// A pre-recording countdown is active.
    Countdown,
    /// Frames are being recorded.
    Recording,
    /// Capture is suspended and elapsed time is frozen.
    Paused,
    /// The session is synchronously flushing and closing.
    Finalising,
    /// Complete output is available.
    Finished,
    /// Recording failed; salvageable partial output may be attached.
    Failed,
}

/// One comparison between the state-machine clock and engine clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockDrift {
    /// Authoritative elapsed recording time.
    pub authoritative_secs: f64,
    /// Engine-reported elapsed time.
    pub engine_secs: f64,
    /// `engine_secs - authoritative_secs`.
    pub delta_secs: f64,
}

/// Actionable terminal failure and any salvageable output.
#[derive(Debug, Clone)]
pub struct MachineFailure {
    /// Primary recording error.
    pub error: Arc<Error>,
    /// Usable partial output, when any bytes survived.
    pub partial: Option<Recording>,
    /// Additional error encountered while attempting to recover/finalise output.
    pub recovery_error: Option<String>,
}

/// One observable state-machine update.
#[derive(Debug, Clone)]
pub enum MachineEvent {
    /// Lifecycle phase changed.
    PhaseChanged(RecordingPhase),
    /// The platform accepted its first video frame.
    FirstFrame,
    /// A recoverable live warning.
    Warning(String),
    /// Engine and authoritative clocks differ materially.
    ClockDrift(ClockDrift),
    /// Complete output became available.
    Finished(Recording),
    /// Recording failed, with partial output attached when available.
    Failed(MachineFailure),
}

/// Recording orchestration driven by an explicit virtual clock.
pub struct RecordingMachine {
    engine: Box<dyn RecordingEngine>,
    capabilities: EngineCapabilities,
    settings: RecordingSettings,
    phase: RecordingPhase,
    request: Option<RecordingRequest>,
    pending_overlays: Option<Box<dyn OverlaySource>>,
    overlays_required: bool,
    session: Option<Box<dyn RecordingSession>>,
    countdown_remaining: Duration,
    elapsed: Duration,
    first_frame: bool,
    warnings: Vec<String>,
    latest_drift: Option<ClockDrift>,
    last_emitted_drift: Option<ClockDrift>,
    output: Option<Recording>,
    failure: Option<MachineFailure>,
    events: VecDeque<MachineEvent>,
}

impl fmt::Debug for RecordingMachine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordingMachine")
            .field("engine", &self.engine.name())
            .field("capabilities", &self.capabilities)
            .field("settings", &self.settings)
            .field("phase", &self.phase)
            .field("request", &self.request)
            .field("countdown_remaining", &self.countdown_remaining)
            .field("elapsed", &self.elapsed)
            .field("first_frame", &self.first_frame)
            .field("warnings", &self.warnings)
            .field("latest_drift", &self.latest_drift)
            .field("output", &self.output)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl RecordingMachine {
    /// Creates a machine around an explicit engine.
    ///
    /// Capability validation is repeated immediately before starting because it
    /// depends on the eventual target/request as well as these settings.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for invalid recording settings.
    pub fn with_engine(
        engine: Box<dyn RecordingEngine>,
        settings: RecordingSettings,
    ) -> Result<Self> {
        settings.validate()?;
        let capabilities = engine.capabilities();
        Ok(Self {
            engine,
            capabilities,
            settings,
            phase: RecordingPhase::Idle,
            request: None,
            pending_overlays: None,
            overlays_required: false,
            session: None,
            countdown_remaining: Duration::ZERO,
            elapsed: Duration::ZERO,
            first_frame: false,
            warnings: Vec::new(),
            latest_drift: None,
            last_emitted_drift: None,
            output: None,
            failure: None,
            events: VecDeque::new(),
        })
    }

    /// Creates a machine using the detected real native engine.
    ///
    /// This never substitutes [`crate::engine::MockEngine`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] until a native platform branch is linked,
    /// or [`Error::InvalidRequest`] for invalid settings.
    pub fn native(settings: RecordingSettings) -> Result<Self> {
        let engine = detect_native_engine().ok_or_else(Self::native_engine_unavailable)?;
        Self::with_engine(engine, settings)
    }

    /// Starts interactive selection from idle.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] from any non-idle phase.
    pub fn begin_selection(&mut self) -> Result<()> {
        self.require_phase(RecordingPhase::Idle, "begin target selection")?;
        self.clear_run();
        self.set_phase(RecordingPhase::Selecting);
        Ok(())
    }

    fn native_engine_unavailable() -> Error {
        #[cfg(all(target_os = "linux", not(feature = "linux-native")))]
        {
            Error::Unsupported {
                what: "Linux screen recording".to_owned(),
                why: "this build omits the non-default `scrozz-record/linux-native` feature; enable it after installing PipeWire, FFmpeg, and VA-API development libraries"
                    .to_owned(),
            }
        }

        #[cfg(not(all(target_os = "linux", not(feature = "linux-native"))))]
        {
            Error::Unsupported {
                what: "screen recording".to_owned(),
                why: "no native recording engine is linked for this platform yet".to_owned(),
            }
        }
    }

    /// Cancels interactive selection and returns to idle.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] unless selection is active.
    pub fn cancel_selection(&mut self) -> Result<()> {
        self.require_phase(RecordingPhase::Selecting, "cancel target selection")?;
        self.set_phase(RecordingPhase::Idle);
        Ok(())
    }

    /// Cancels a pending countdown before the platform session starts.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] unless a countdown is active.
    pub fn cancel_countdown(&mut self) -> Result<()> {
        self.require_phase(RecordingPhase::Countdown, "cancel recording countdown")?;
        self.request = None;
        self.pending_overlays = None;
        self.countdown_remaining = Duration::ZERO;
        self.elapsed = Duration::ZERO;
        self.set_phase(RecordingPhase::Idle);
        Ok(())
    }

    /// Completes interactive selection and begins countdown/recording.
    ///
    /// # Errors
    ///
    /// Returns a transition, request, capability, or engine-start error.
    pub fn complete_selection(&mut self, target: CaptureTarget) -> Result<()> {
        self.require_phase(RecordingPhase::Selecting, "complete target selection")?;
        self.prepare_target(target)
    }

    /// Begins countdown/recording for an already known target.
    ///
    /// # Errors
    ///
    /// Returns a transition, request, capability, or engine-start error.
    pub fn begin(&mut self, target: CaptureTarget) -> Result<()> {
        self.require_phase(RecordingPhase::Idle, "begin recording")?;
        self.clear_run();
        self.prepare_target(target)
    }

    /// Begins countdown/recording to a caller-selected durable destination.
    ///
    /// This preserves the machine's interactive settings and countdown while
    /// ensuring native engines never fall back to temporary storage.
    ///
    /// # Errors
    ///
    /// Returns a transition, request, capability, or engine-start error.
    pub fn begin_with_destination(
        &mut self,
        target: CaptureTarget,
        destination: PathBuf,
    ) -> Result<()> {
        self.require_phase(RecordingPhase::Idle, "begin recording")?;
        self.clear_run();
        let mut request = RecordingRequest::from_settings(target, &self.settings);
        request.destination = Some(destination);
        self.prepare_request(request)
    }

    /// Begins countdown/recording with an explicit pull-based overlay source.
    ///
    /// # Errors
    ///
    /// Returns a transition, request, capability, overlay, or engine-start error.
    pub fn begin_with_destination_and_overlays(
        &mut self,
        target: CaptureTarget,
        destination: PathBuf,
        overlays: Box<dyn OverlaySource>,
    ) -> Result<()> {
        self.require_phase(RecordingPhase::Idle, "begin recording")?;
        self.clear_run();
        let mut request = RecordingRequest::from_settings(target, &self.settings);
        request.destination = Some(destination);
        self.pending_overlays = Some(overlays);
        self.prepare_request(request)
    }

    /// Starts an already configured request without applying interactive
    /// settings or a countdown.
    ///
    /// This is the adapter used by a long-lived GUI when a CLI invocation
    /// forwards explicit quality, resolution, codec, audio, and destination
    /// flags into the GUI-owned machine.
    ///
    /// # Errors
    ///
    /// Returns a transition, request, capability, or engine-start error.
    pub fn begin_request(&mut self, request: RecordingRequest) -> Result<()> {
        self.require_phase(RecordingPhase::Idle, "begin configured recording")?;
        request.validate()?;
        validate_capabilities(self.capabilities, &request, None)?;
        self.clear_run();
        self.overlays_required = false;
        self.stage_request(&request);
        self.start_request(request)
    }

    /// Starts an explicit request with a pull-based overlay source.
    ///
    /// # Errors
    ///
    /// Returns a transition, request, capability, overlay, or engine-start error.
    pub fn begin_request_with_overlays(
        &mut self,
        request: RecordingRequest,
        overlays: Box<dyn OverlaySource>,
    ) -> Result<()> {
        self.require_phase(RecordingPhase::Idle, "begin configured recording")?;
        request.validate()?;
        validate_capabilities(self.capabilities, &request, None)?;
        self.clear_run();
        self.pending_overlays = Some(overlays);
        self.overlays_required = true;
        self.stage_request(&request);
        self.start_request(request)
    }

    fn prepare_target(&mut self, target: CaptureTarget) -> Result<()> {
        let request = RecordingRequest::from_settings(target, &self.settings);
        self.prepare_request(request)
    }

    fn prepare_request(&mut self, request: RecordingRequest) -> Result<()> {
        request.validate()?;
        validate_capabilities(self.capabilities, &request, Some(&self.settings))?;
        self.overlays_required = self.settings.clicks.enabled
            || self.settings.keystrokes.enabled
            || self.settings.camera.enabled;
        self.stage_request(&request);

        let countdown = if self.settings.countdown.enabled {
            Duration::from_secs(u64::from(self.settings.countdown.seconds))
        } else {
            Duration::ZERO
        };
        if countdown.is_zero() {
            self.start_request(request)
        } else {
            self.countdown_remaining = countdown;
            self.set_phase(RecordingPhase::Countdown);
            Ok(())
        }
    }

    fn stage_request(&mut self, request: &RecordingRequest) {
        self.request = Some(request.clone());
        self.elapsed = Duration::ZERO;
        self.first_frame = false;
        self.latest_drift = None;
        self.last_emitted_drift = None;
        self.output = None;
        self.failure = None;
        self.warnings.clear();
    }

    fn start_request(&mut self, request: RecordingRequest) -> Result<()> {
        if self.overlays_required && self.pending_overlays.is_none() {
            let error = Error::InvalidRequest(
                "enabled recording overlays require an explicit native OverlaySource".into(),
            );
            let returned = error.clone();
            self.enter_failed(error, None, None);
            return Err(returned);
        }
        let started = if let Some(overlays) = self.pending_overlays.take() {
            self.engine.start_with_overlays(&request, overlays)
        } else {
            self.engine.start(&request)
        };
        match started {
            Ok(session) => {
                self.session = Some(session);
                self.countdown_remaining = Duration::ZERO;
                self.set_phase(RecordingPhase::Recording);
                Ok(())
            }
            Err(error) => {
                let returned = error.clone();
                self.enter_failed(error, None, None);
                Err(returned)
            }
        }
    }

    /// Advances countdown or active recording by virtual time and polls one
    /// session event.
    ///
    /// Countdown overshoot is carried into recording elapsed time. Paused time
    /// is never added. Zero deltas still poll the session but advance no clock.
    ///
    /// # Errors
    ///
    /// Returns an engine-start error if the countdown boundary is crossed and
    /// session creation fails.
    pub fn tick(&mut self, delta: Duration) -> Result<()> {
        match self.phase {
            RecordingPhase::Countdown => {
                if delta < self.countdown_remaining {
                    self.countdown_remaining -= delta;
                    return Ok(());
                }
                let overshoot = delta.saturating_sub(self.countdown_remaining);
                self.countdown_remaining = Duration::ZERO;
                let request = self.request.clone().ok_or_else(|| {
                    Error::Platform("countdown has no pending recording request".to_owned())
                })?;
                self.start_request(request)?;
                self.elapsed = self.elapsed.saturating_add(overshoot);
                self.poll_session();
            }
            RecordingPhase::Recording => {
                self.elapsed = self.elapsed.saturating_add(delta);
                self.poll_session();
            }
            RecordingPhase::Paused => {
                self.poll_session();
            }
            RecordingPhase::Idle
            | RecordingPhase::Selecting
            | RecordingPhase::Finalising
            | RecordingPhase::Finished
            | RecordingPhase::Failed => {}
        }
        Ok(())
    }

    /// Convenience adapter for floating-point time sources.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for negative, non-finite, or
    /// unrepresentable values. The machine is unchanged on error.
    pub fn tick_secs(&mut self, delta_secs: f64) -> Result<()> {
        let delta = Duration::try_from_secs_f64(delta_secs).map_err(|_| {
            Error::InvalidRequest(format!(
                "recording tick {delta_secs} s must be finite and non-negative"
            ))
        })?;
        self.tick(delta)
    }

    fn poll_session(&mut self) {
        let event = self.session.as_mut().and_then(|session| session.poll());
        match event {
            Some(SessionEvent::FirstFrame) => {
                if !self.first_frame {
                    self.first_frame = true;
                    self.push_event(MachineEvent::FirstFrame);
                }
            }
            Some(SessionEvent::Warning(message)) => {
                self.push_warning(message);
            }
            Some(SessionEvent::Finished(output)) => {
                self.session = None;
                self.set_phase(RecordingPhase::Finalising);
                self.finish_output(output);
                return;
            }
            Some(SessionEvent::Failed(error)) => {
                self.session = None;
                self.set_phase(RecordingPhase::Finalising);
                self.enter_failed_arc(error, None, None);
                return;
            }
            None => {}
        }

        let engine_elapsed = self
            .session
            .as_ref()
            .and_then(|session| session.engine_elapsed_secs());
        if let Some(engine_secs) = engine_elapsed {
            if engine_secs.is_finite() && engine_secs >= 0.0 {
                let drift = ClockDrift {
                    authoritative_secs: self.elapsed.as_secs_f64(),
                    engine_secs,
                    delta_secs: engine_secs - self.elapsed.as_secs_f64(),
                };
                let changed = self.last_emitted_drift.is_none_or(|previous| {
                    (previous.delta_secs - drift.delta_secs).abs() >= DRIFT_EVENT_THRESHOLD_SECS
                });
                self.latest_drift = Some(drift);
                if changed && drift.delta_secs.abs() >= DRIFT_EVENT_THRESHOLD_SECS {
                    self.push_event(MachineEvent::ClockDrift(drift));
                    self.last_emitted_drift = Some(drift);
                } else if drift.delta_secs.abs() < DRIFT_EVENT_THRESHOLD_SECS {
                    self.last_emitted_drift = None;
                }
            } else {
                self.push_warning(format!(
                    "recording engine reported invalid elapsed time {engine_secs}"
                ));
            }
        }
    }

    fn push_warning(&mut self, message: String) {
        if self.warnings.last() == Some(&message) {
            return;
        }
        if self.warnings.len() == MAX_WARNING_HISTORY {
            self.warnings.remove(0);
        }
        self.warnings.push(message.clone());
        self.push_event(MachineEvent::Warning(message));
    }

    fn push_event(&mut self, event: MachineEvent) {
        if self.events.len() == MAX_PENDING_EVENTS {
            let incoming_is_terminal =
                matches!(event, MachineEvent::Finished(_) | MachineEvent::Failed(_));
            let removable = self.events.iter().position(|queued| {
                !matches!(queued, MachineEvent::Finished(_) | MachineEvent::Failed(_))
            });
            match (incoming_is_terminal, removable) {
                (_, Some(index)) => {
                    self.events.remove(index);
                }
                (true, None) => {
                    self.events.pop_front();
                }
                (false, None) => return,
            }
        }
        self.events.push_back(event);
    }

    /// Pauses an active recording.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] outside recording,
    /// [`Error::Unsupported`] when the engine did not advertise pause/resume, or
    /// the session's own pause error. The phase changes only after success.
    pub fn pause(&mut self) -> Result<()> {
        self.require_phase(RecordingPhase::Recording, "pause recording")?;
        if !self.capabilities.pause_resume {
            return Err(Error::Unsupported {
                what: "pause and resume".to_owned(),
                why: "the selected recording engine did not advertise this capability".to_owned(),
            });
        }
        self.session_mut()?.pause()?;
        self.set_phase(RecordingPhase::Paused);
        Ok(())
    }

    /// Resumes a paused recording.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] outside pause, an unsupported error, or
    /// the session's own resume error. The phase changes only after success.
    pub fn resume(&mut self) -> Result<()> {
        self.require_phase(RecordingPhase::Paused, "resume recording")?;
        if !self.capabilities.pause_resume {
            return Err(Error::Unsupported {
                what: "pause and resume".to_owned(),
                why: "the selected recording engine did not advertise this capability".to_owned(),
            });
        }
        self.session_mut()?.resume()?;
        self.set_phase(RecordingPhase::Recording);
        Ok(())
    }

    /// Synchronously finalises an active or paused recording.
    ///
    /// The event stream always observes `Finalising` before `Finished` or
    /// `Failed`. A partial [`Recording`] enters `Failed` with that recording
    /// attached. An `Err` from the session means no usable output and also enters
    /// `Failed`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] unless recording or paused.
    pub fn stop(&mut self) -> Result<()> {
        let session = self.begin_finalising()?;
        let result = session.stop();
        self.complete_finalising(result)
    }

    /// Moves the live native session into an external finalisation worker.
    ///
    /// GUI hosts use this to enter `Finalising` immediately without blocking the
    /// UI thread while a native container flushes.
    pub fn begin_finalising(&mut self) -> Result<Box<dyn RecordingSession>> {
        if !matches!(
            self.phase,
            RecordingPhase::Recording | RecordingPhase::Paused
        ) {
            return Err(self.transition_error("stop recording"));
        }
        self.set_phase(RecordingPhase::Finalising);
        let Some(session) = self.session.take() else {
            let message = "active recording session disappeared before stop".to_owned();
            self.enter_failed(Error::Platform(message.clone()), None, None);
            return Err(Error::Platform(message));
        };
        Ok(session)
    }

    /// Applies a native finalisation result returned by an external worker.
    pub fn complete_finalising(&mut self, result: Result<Recording>) -> Result<()> {
        self.require_phase(
            RecordingPhase::Finalising,
            "complete recording finalisation",
        )?;
        match result {
            Ok(output) => self.finish_output(output),
            Err(error) => self.enter_failed(error, None, None),
        }
        Ok(())
    }

    fn finish_output(&mut self, output: Recording) {
        if let Err(error) = output.validate() {
            self.enter_failed(error, None, None);
        } else if output.is_partial() {
            let reason = output
                .partial_reason()
                .unwrap_or("recording finalisation failed")
                .to_owned();
            self.enter_failed(
                Error::Codec(format!(
                    "recording finalisation left partial output: {reason}"
                )),
                Some(output),
                None,
            );
        } else {
            self.output = Some(output.clone());
            self.set_phase(RecordingPhase::Finished);
            self.push_event(MachineEvent::Finished(output));
        }
    }

    fn enter_failed(
        &mut self,
        error: Error,
        partial: Option<Recording>,
        recovery_error: Option<String>,
    ) {
        self.enter_failed_arc(Arc::new(error), partial, recovery_error);
    }

    fn enter_failed_arc(
        &mut self,
        error: Arc<Error>,
        partial: Option<Recording>,
        recovery_error: Option<String>,
    ) {
        self.session = None;
        let failure = MachineFailure {
            error,
            partial,
            recovery_error,
        };
        self.failure = Some(failure.clone());
        self.set_phase(RecordingPhase::Failed);
        self.push_event(MachineEvent::Failed(failure));
    }

    /// Resets a terminal machine for another recording.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] before `Finished` or `Failed`.
    pub fn reset(&mut self) -> Result<()> {
        if !matches!(
            self.phase,
            RecordingPhase::Finished | RecordingPhase::Failed
        ) {
            return Err(self.transition_error("reset recording"));
        }
        self.clear_run();
        self.set_phase(RecordingPhase::Idle);
        Ok(())
    }

    fn clear_run(&mut self) {
        self.request = None;
        self.pending_overlays = None;
        self.overlays_required = false;
        self.session = None;
        self.countdown_remaining = Duration::ZERO;
        self.elapsed = Duration::ZERO;
        self.first_frame = false;
        self.warnings.clear();
        self.latest_drift = None;
        self.last_emitted_drift = None;
        self.output = None;
        self.failure = None;
    }

    fn require_phase(&self, expected: RecordingPhase, action: &str) -> Result<()> {
        if self.phase == expected {
            Ok(())
        } else {
            Err(self.transition_error(action))
        }
    }

    fn transition_error(&self, action: &str) -> Error {
        Error::InvalidRequest(format!(
            "cannot {action} while recording state is {:?}",
            self.phase
        ))
    }

    fn session_mut(&mut self) -> Result<&mut (dyn RecordingSession + '_)> {
        match self.session.as_mut() {
            Some(session) => Ok(session.as_mut()),
            None => Err(Error::Platform(
                "recording state has no active platform session".to_owned(),
            )),
        }
    }

    fn set_phase(&mut self, phase: RecordingPhase) {
        if self.phase != phase {
            self.phase = phase;
            self.push_event(MachineEvent::PhaseChanged(phase));
        }
    }

    /// Current exact phase.
    #[must_use]
    pub const fn phase(&self) -> RecordingPhase {
        self.phase
    }

    /// Whether a start/stop surface should currently offer "Stop Recording".
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self.phase,
            RecordingPhase::Selecting
                | RecordingPhase::Countdown
                | RecordingPhase::Recording
                | RecordingPhase::Paused
                | RecordingPhase::Finalising
        )
    }

    /// Whether a native session stopped without yielding its terminal event.
    ///
    /// Older adapters may use the default no-op poll implementation. Hosts use
    /// this signal to move their session into the same off-thread finaliser used
    /// for an explicit stop rather than leaving a failed capture active forever.
    #[must_use]
    pub fn requires_finalisation(&self) -> bool {
        matches!(
            self.phase,
            RecordingPhase::Recording | RecordingPhase::Paused
        ) && self
            .session
            .as_ref()
            .is_some_and(|session| session.state() == crate::RecordingState::Stopped)
    }

    /// Engine capabilities captured at machine creation.
    #[must_use]
    pub const fn capabilities(&self) -> EngineCapabilities {
        self.capabilities
    }

    /// Recording settings in force.
    #[must_use]
    pub const fn settings(&self) -> &RecordingSettings {
        &self.settings
    }

    /// Current or pending platform request.
    #[must_use]
    pub const fn request(&self) -> Option<&RecordingRequest> {
        self.request.as_ref()
    }

    /// Remaining countdown time.
    #[must_use]
    pub const fn countdown_remaining(&self) -> Duration {
        self.countdown_remaining
    }

    /// Authoritative elapsed recording time, excluding countdown and pauses.
    #[must_use]
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// Whether a first-frame event has arrived.
    #[must_use]
    pub const fn has_first_frame(&self) -> bool {
        self.first_frame
    }

    /// Recoverable warnings observed so far.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Most recent engine-clock comparison, including sub-threshold drift.
    #[must_use]
    pub const fn latest_drift(&self) -> Option<ClockDrift> {
        self.latest_drift
    }

    /// Complete finished output.
    #[must_use]
    pub const fn output(&self) -> Option<&Recording> {
        self.output.as_ref()
    }

    /// Terminal failure and any partial output.
    #[must_use]
    pub const fn failure(&self) -> Option<&MachineFailure> {
        self.failure.as_ref()
    }

    /// Pops one observable event.
    pub fn poll_event(&mut self) -> Option<MachineEvent> {
        self.events.pop_front()
    }

    /// Drains every currently queued event.
    pub fn drain_events(&mut self) -> impl Iterator<Item = MachineEvent> + '_ {
        self.events.drain(..)
    }

    /// Tray/status text for the current state.
    #[must_use]
    pub fn status_label(&self) -> String {
        if self.phase == RecordingPhase::Countdown {
            let seconds = self.countdown_remaining.as_secs_f64().ceil() as u64;
            return format!("Starting in {seconds}s • {}", format_elapsed(self.elapsed));
        }
        format_status_label(self.phase, self.elapsed)
    }
}

/// Formats a phase and elapsed time for a tray/menu/status surface.
#[must_use]
pub fn format_status_label(phase: RecordingPhase, elapsed: Duration) -> String {
    let phase = match phase {
        RecordingPhase::Idle => "Ready",
        RecordingPhase::Selecting => "Selecting",
        RecordingPhase::Countdown => "Countdown",
        RecordingPhase::Recording => "Recording",
        RecordingPhase::Paused => "Paused",
        RecordingPhase::Finalising => "Finalising",
        RecordingPhase::Finished => "Finished",
        RecordingPhase::Failed => "Recording failed",
    };
    format!("{phase} • {}", format_elapsed(elapsed))
}

fn format_elapsed(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    let hours = total / 3_600;
    let minutes = total % 3_600 / 60;
    let seconds = total % 60;
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use scrozz_core::{DisplayId, Error};

    use crate::engine::{MockEngine, MockPoll, MockSessionPlan, MockStop};

    use super::*;

    fn target() -> CaptureTarget {
        CaptureTarget::Display(DisplayId("display-1".to_owned()))
    }

    fn settings_without_countdown() -> RecordingSettings {
        let mut settings = RecordingSettings::shipped();
        settings.countdown.enabled = false;
        settings
    }

    fn make_machine(plan: MockSessionPlan) -> RecordingMachine {
        RecordingMachine::with_engine(
            Box::new(MockEngine::fully_capable(plan)),
            settings_without_countdown(),
        )
        .unwrap()
    }

    struct PermissionFailingEngine;

    struct EmptyOverlays;

    impl OverlaySource for EmptyOverlays {
        fn layers(
            &mut self,
            _elapsed: Duration,
            _canvas: scrozz_core::PhysicalSize,
        ) -> Vec<crate::OverlayLayer> {
            Vec::new()
        }
    }

    impl RecordingEngine for PermissionFailingEngine {
        fn name(&self) -> &'static str {
            "permission-failure"
        }

        fn capabilities(&self) -> EngineCapabilities {
            EngineCapabilities::ALL
        }

        fn start(&self, _request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
            Err(Error::PermissionDenied {
                capability: "Screen Recording".into(),
                remedy: "open Privacy settings".into(),
            })
        }
    }

    fn phases(machine: &mut RecordingMachine) -> Vec<RecordingPhase> {
        machine
            .drain_events()
            .filter_map(|event| match event {
                MachineEvent::PhaseChanged(phase) => Some(phase),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn interactive_selection_can_cancel_or_complete() {
        let plan = MockSessionPlan::complete("out.mp4", 1.0).unwrap();
        let mut machine = make_machine(plan);
        assert_eq!(machine.phase(), RecordingPhase::Idle);
        machine.begin_selection().unwrap();
        assert_eq!(machine.phase(), RecordingPhase::Selecting);
        machine.cancel_selection().unwrap();
        assert_eq!(machine.phase(), RecordingPhase::Idle);
        machine.begin_selection().unwrap();
        machine.complete_selection(target()).unwrap();
        assert_eq!(machine.phase(), RecordingPhase::Recording);
        assert_eq!(
            phases(&mut machine),
            [
                RecordingPhase::Selecting,
                RecordingPhase::Idle,
                RecordingPhase::Selecting,
                RecordingPhase::Recording
            ]
        );
    }

    #[test]
    fn configured_request_starts_immediately_and_keeps_forwarded_values() {
        let plan = MockSessionPlan::complete("out.mp4", 1.0).unwrap();
        let mut settings = RecordingSettings::shipped();
        settings.countdown.enabled = true;
        settings.countdown.seconds = 5;
        let mut machine =
            RecordingMachine::with_engine(Box::new(MockEngine::fully_capable(plan)), settings)
                .unwrap();
        let mut request = RecordingRequest::new(target());
        request.destination = Some(PathBuf::from("forwarded.mp4"));
        request.fps = 48;
        request.microphone = true;
        request.system_audio = true;
        request.show_cursor = true;
        request.quality = crate::settings::Quality::High;
        request.resolution = crate::RecordingResolution::ScalePercent(75);
        request.video_codec = crate::VideoCodec::H264;

        machine.begin_request(request.clone()).unwrap();

        assert_eq!(machine.phase(), RecordingPhase::Recording);
        assert_eq!(machine.countdown_remaining(), Duration::ZERO);
        assert_eq!(machine.request(), Some(&request));
    }

    #[test]
    fn configured_destination_keeps_interactive_countdown_and_settings() {
        let plan = MockSessionPlan::complete("out.mp4", 1.0).unwrap();
        let mut settings = RecordingSettings::shipped();
        settings.countdown.enabled = true;
        settings.countdown.seconds = 3;
        settings.video.fps = 48;
        let mut machine =
            RecordingMachine::with_engine(Box::new(MockEngine::fully_capable(plan)), settings)
                .unwrap();

        machine
            .begin_with_destination(target(), PathBuf::from("durable.mp4"))
            .unwrap();

        assert_eq!(machine.phase(), RecordingPhase::Countdown);
        assert_eq!(machine.countdown_remaining(), Duration::from_secs(3));
        let request = machine.request().expect("staged request");
        assert_eq!(
            request.destination.as_deref(),
            Some(std::path::Path::new("durable.mp4"))
        );
        assert_eq!(request.fps, 48);
    }

    #[test]
    fn start_failures_keep_their_actionable_error_category() {
        let mut machine = RecordingMachine::with_engine(
            Box::new(PermissionFailingEngine),
            settings_without_countdown(),
        )
        .unwrap();

        let error = machine.begin(target()).expect_err("permission is denied");

        assert!(matches!(
            error,
            Error::PermissionDenied {
                ref capability,
                ref remedy
            } if capability == "Screen Recording" && remedy == "open Privacy settings"
        ));
        assert_eq!(machine.phase(), RecordingPhase::Failed);
        assert!(matches!(
            machine.failure().map(|failure| failure.error.as_ref()),
            Some(Error::PermissionDenied { .. })
        ));
    }

    #[test]
    fn enabled_overlays_require_and_use_an_explicit_source() {
        let mut settings = settings_without_countdown();
        settings.clicks.enabled = true;
        let plan = MockSessionPlan::complete("out.mp4", 1.0).unwrap();
        let mut missing =
            RecordingMachine::with_engine(Box::new(MockEngine::fully_capable(plan)), settings)
                .unwrap();
        assert!(
            missing
                .begin_with_destination(target(), PathBuf::from("missing.mp4"))
                .expect_err("enabled click overlays cannot disappear")
                .to_string()
                .contains("OverlaySource")
        );

        let plan = MockSessionPlan::complete("out.mp4", 1.0).unwrap();
        let mut supplied =
            RecordingMachine::with_engine(Box::new(MockEngine::fully_capable(plan)), settings)
                .unwrap();
        supplied
            .begin_with_destination_and_overlays(
                target(),
                PathBuf::from("overlay.mp4"),
                Box::new(EmptyOverlays),
            )
            .unwrap();
        assert_eq!(supplied.phase(), RecordingPhase::Recording);
    }

    #[test]
    fn diagnostics_are_bounded_and_keep_the_terminal_event() {
        let mut polls = (0..MAX_PENDING_EVENTS + 32)
            .map(|index| {
                MockPoll::new(
                    Some(SessionEvent::Warning(format!("warning {index}"))),
                    None,
                )
            })
            .collect::<Vec<_>>();
        polls.push(MockPoll::new(
            Some(SessionEvent::Finished(
                Recording::synthetic("bounded.mp4", 1.0, "test").unwrap(),
            )),
            Some(1.0),
        ));
        let plan = MockSessionPlan::complete("unused.mp4", 1.0)
            .unwrap()
            .with_polls(polls);
        let mut machine = make_machine(plan);
        machine.begin(target()).unwrap();

        for _ in 0..MAX_PENDING_EVENTS + 33 {
            machine.tick(Duration::ZERO).unwrap();
        }

        assert_eq!(machine.warnings().len(), MAX_WARNING_HISTORY);
        assert_eq!(machine.events.len(), MAX_PENDING_EVENTS);
        assert!(
            machine
                .drain_events()
                .any(|event| matches!(event, MachineEvent::Finished(_)))
        );
    }

    #[test]
    fn countdown_can_be_cancelled_without_starting_the_engine() {
        let plan = MockSessionPlan::complete("out.mp4", 1.0).unwrap();
        let mut settings = RecordingSettings::shipped();
        settings.countdown.seconds = 3;
        let mut machine =
            RecordingMachine::with_engine(Box::new(MockEngine::fully_capable(plan)), settings)
                .unwrap();

        machine.begin(target()).unwrap();
        assert!(machine.is_active());
        assert_eq!(machine.phase(), RecordingPhase::Countdown);
        machine.cancel_countdown().unwrap();

        assert_eq!(machine.phase(), RecordingPhase::Idle);
        assert!(!machine.is_active());
        assert!(machine.request().is_none());
        assert_eq!(machine.countdown_remaining(), Duration::ZERO);
    }

    #[test]
    fn countdown_overshoot_carries_into_recording_not_countdown_time() {
        let mut settings = settings_without_countdown();
        settings.countdown.enabled = true;
        settings.countdown.seconds = 3;
        let plan = MockSessionPlan::complete("out.mp4", 1.0)
            .unwrap()
            .with_polls(vec![MockPoll::new(
                Some(SessionEvent::FirstFrame),
                Some(0.5),
            )]);
        let mut machine =
            RecordingMachine::with_engine(Box::new(MockEngine::fully_capable(plan)), settings)
                .unwrap();
        machine.begin(target()).unwrap();
        assert_eq!(machine.phase(), RecordingPhase::Countdown);
        machine.tick(Duration::from_secs(2)).unwrap();
        assert_eq!(machine.countdown_remaining(), Duration::from_secs(1));
        assert_eq!(machine.elapsed(), Duration::ZERO);
        machine.tick(Duration::from_millis(1_500)).unwrap();
        assert_eq!(machine.phase(), RecordingPhase::Recording);
        assert_eq!(machine.elapsed(), Duration::from_millis(500));
        assert!(machine.has_first_frame());
    }

    #[test]
    fn pause_excludes_every_paused_span_and_all_success_phases_are_observable() {
        let plan = MockSessionPlan::complete("out.mp4", 3.0).unwrap();
        let mut machine = make_machine(plan);
        machine.begin(target()).unwrap();
        machine.tick(Duration::from_secs(2)).unwrap();
        machine.pause().unwrap();
        machine.tick(Duration::from_secs(20)).unwrap();
        assert_eq!(machine.elapsed(), Duration::from_secs(2));
        machine.resume().unwrap();
        machine.tick(Duration::from_secs(1)).unwrap();
        machine.stop().unwrap();
        assert_eq!(machine.phase(), RecordingPhase::Finished);
        assert_eq!(machine.elapsed(), Duration::from_secs(3));
        assert!(machine.output().is_some());
        assert_eq!(
            phases(&mut machine),
            [
                RecordingPhase::Recording,
                RecordingPhase::Paused,
                RecordingPhase::Recording,
                RecordingPhase::Finalising,
                RecordingPhase::Finished
            ]
        );
    }

    #[test]
    fn first_frame_is_latched_and_warnings_do_not_fail() {
        let plan = MockSessionPlan::complete("out.mp4", 1.0)
            .unwrap()
            .with_polls(vec![
                MockPoll::new(Some(SessionEvent::FirstFrame), None),
                MockPoll::new(Some(SessionEvent::FirstFrame), None),
                MockPoll::new(
                    Some(SessionEvent::Warning("frame dropped".to_owned())),
                    None,
                ),
            ]);
        let mut machine = make_machine(plan);
        machine.begin(target()).unwrap();
        machine.tick(Duration::ZERO).unwrap();
        machine.tick(Duration::ZERO).unwrap();
        machine.tick(Duration::ZERO).unwrap();
        assert!(machine.has_first_frame());
        assert_eq!(machine.warnings(), ["frame dropped"]);
        assert_eq!(machine.phase(), RecordingPhase::Recording);
        let events: Vec<_> = machine.drain_events().collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, MachineEvent::FirstFrame))
                .count(),
            1
        );
        assert!(events.iter().any(
            |event| matches!(event, MachineEvent::Warning(message) if message == "frame dropped")
        ));
    }

    #[test]
    fn terminal_partial_transitions_immediately_and_attaches_output() {
        let output =
            Recording::synthetic_partial("salvage.mp4", 2.0, "test", "capture device vanished")
                .unwrap();
        let plan = MockSessionPlan {
            polls: vec![MockPoll::new(
                Some(SessionEvent::Finished(output.clone())),
                Some(2.0),
            )],
            stop: MockStop::Output(output),
            pause_failure: None,
            resume_failure: None,
        };
        let mut machine = make_machine(plan);
        machine.begin(target()).unwrap();
        machine.tick(Duration::from_secs(2)).unwrap();
        assert_eq!(machine.phase(), RecordingPhase::Failed);
        let failure = machine.failure().unwrap();
        assert!(failure.error.to_string().contains("partial output"));
        assert!(failure.partial.as_ref().unwrap().is_partial());
        assert!(machine.drain_events().any(
            |event| matches!(event, MachineEvent::Failed(failure) if failure.partial.is_some())
        ));
    }

    #[test]
    fn terminal_failure_without_output_has_no_partial() {
        let plan = MockSessionPlan::complete("salvage.mp4", 2.0)
            .unwrap()
            .with_polls(vec![MockPoll::new(
                Some(SessionEvent::Failed(Arc::new(Error::Platform(
                    "stream disconnected".to_owned(),
                )))),
                None,
            )]);
        let mut machine = make_machine(plan);
        machine.begin(target()).unwrap();
        machine.tick(Duration::ZERO).unwrap();
        let failure = machine.failure().unwrap();
        assert!(failure.partial.is_none());
        assert!(failure.error.to_string().contains("stream disconnected"));
    }

    #[test]
    fn engine_drift_is_data_and_an_event_but_never_changes_elapsed() {
        let plan = MockSessionPlan::complete("out.mp4", 1.0)
            .unwrap()
            .with_polls(vec![MockPoll::new(None, Some(3.0))]);
        let mut machine = make_machine(plan);
        machine.begin(target()).unwrap();
        machine.tick(Duration::from_secs(1)).unwrap();
        assert_eq!(machine.elapsed(), Duration::from_secs(1));
        let drift = machine.latest_drift().unwrap();
        assert_eq!(drift.delta_secs, 2.0);
        assert!(
            machine
                .drain_events()
                .any(|event| matches!(event, MachineEvent::ClockDrift(value) if value == drift))
        );
    }

    #[test]
    fn explicit_stop_with_partial_output_is_failed_with_partial_attached() {
        let plan = MockSessionPlan::partial("partial.mp4", 8.0, "container index failed").unwrap();
        let mut machine = make_machine(plan);
        machine.begin(target()).unwrap();
        machine.stop().unwrap();
        assert_eq!(machine.phase(), RecordingPhase::Failed);
        assert!(
            machine
                .failure()
                .unwrap()
                .partial
                .as_ref()
                .unwrap()
                .is_partial()
        );
        assert_eq!(
            phases(&mut machine),
            [
                RecordingPhase::Recording,
                RecordingPhase::Finalising,
                RecordingPhase::Failed
            ]
        );
    }

    #[test]
    fn stop_error_means_no_usable_output() {
        let mut machine = make_machine(MockSessionPlan::no_output(Error::Codec(
            "encoder never opened".to_owned(),
        )));
        machine.begin(target()).unwrap();
        machine.stop().unwrap();
        assert_eq!(machine.phase(), RecordingPhase::Failed);
        assert!(machine.failure().unwrap().partial.is_none());
    }

    #[test]
    fn capability_validation_happens_before_session_start() {
        let mut settings = settings_without_countdown();
        settings.audio.microphone = true;
        let engine = MockEngine::new(
            EngineCapabilities::VIDEO,
            MockSessionPlan::complete("unused.mp4", 1.0).unwrap(),
        );
        let mut machine = RecordingMachine::with_engine(Box::new(engine), settings).unwrap();
        let error = machine.begin(target()).unwrap_err().to_string();
        assert!(error.contains("microphone"), "{error}");
        assert_eq!(machine.phase(), RecordingPhase::Idle);
    }

    #[test]
    fn countdown_engine_start_failure_enters_failed_with_actionable_metadata() {
        let mut settings = settings_without_countdown();
        settings.countdown.enabled = true;
        settings.countdown.seconds = 1;
        let engine =
            MockEngine::fully_capable(MockSessionPlan::complete("consumed.mp4", 1.0).unwrap());
        let request = RecordingRequest::from_settings(target(), &settings);
        engine.start(&request).unwrap().stop().unwrap();

        let mut machine = RecordingMachine::with_engine(Box::new(engine), settings).unwrap();
        machine.begin(target()).unwrap();
        let error = machine
            .tick(Duration::from_secs(1))
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed to start"), "{error}");
        assert_eq!(machine.phase(), RecordingPhase::Failed);
        assert!(
            machine
                .failure()
                .unwrap()
                .error
                .to_string()
                .contains("one scripted session")
        );
    }

    #[test]
    fn unsupported_and_failed_pause_leave_recording_active() {
        let engine = MockEngine::new(
            EngineCapabilities::VIDEO,
            MockSessionPlan::complete("out.mp4", 1.0).unwrap(),
        );
        let mut machine =
            RecordingMachine::with_engine(Box::new(engine), settings_without_countdown()).unwrap();
        machine.begin(target()).unwrap();
        assert!(machine.pause().is_err());
        assert_eq!(machine.phase(), RecordingPhase::Recording);

        let mut plan = MockSessionPlan::complete("out-2.mp4", 1.0).unwrap();
        plan.pause_failure = Some("pause API failed".to_owned());
        let mut machine = make_machine(plan);
        machine.begin(target()).unwrap();
        assert!(machine.pause().is_err());
        assert_eq!(machine.phase(), RecordingPhase::Recording);
    }

    #[test]
    fn resume_error_leaves_the_machine_paused_and_time_frozen() {
        let mut plan = MockSessionPlan::complete("out.mp4", 1.0).unwrap();
        plan.resume_failure = Some("resume API failed".to_owned());
        let mut machine = make_machine(plan);
        machine.begin(target()).unwrap();
        machine.tick(Duration::from_secs(2)).unwrap();
        machine.pause().unwrap();
        assert!(machine.resume().is_err());
        machine.tick(Duration::from_secs(5)).unwrap();
        assert_eq!(machine.phase(), RecordingPhase::Paused);
        assert_eq!(machine.elapsed(), Duration::from_secs(2));
    }

    #[test]
    fn invalid_transitions_and_invalid_float_ticks_are_safe() {
        let plan = MockSessionPlan::complete("out.mp4", 1.0).unwrap();
        let mut machine = make_machine(plan);
        assert!(machine.pause().is_err());
        assert!(machine.stop().is_err());
        assert!(machine.complete_selection(target()).is_err());
        assert!(machine.tick_secs(f64::NAN).is_err());
        assert!(machine.tick_secs(-1.0).is_err());
        assert_eq!(machine.phase(), RecordingPhase::Idle);
        assert_eq!(machine.elapsed(), Duration::ZERO);
    }

    #[test]
    fn terminal_state_can_reset_to_idle() {
        let plan = MockSessionPlan::complete("out.mp4", 1.0).unwrap();
        let mut machine = make_machine(plan);
        machine.begin(target()).unwrap();
        machine.stop().unwrap();
        machine.reset().unwrap();
        assert_eq!(machine.phase(), RecordingPhase::Idle);
        assert!(machine.output().is_none());
        assert_eq!(machine.elapsed(), Duration::ZERO);
    }

    #[test]
    fn labels_include_elapsed_and_countdown() {
        assert_eq!(
            format_status_label(RecordingPhase::Recording, Duration::from_secs(65)),
            "Recording • 01:05"
        );
        assert_eq!(
            format_status_label(RecordingPhase::Paused, Duration::from_secs(3_661)),
            "Paused • 01:01:01"
        );
        let mut settings = settings_without_countdown();
        settings.countdown.enabled = true;
        settings.countdown.seconds = 3;
        let plan = MockSessionPlan::complete(PathBuf::from("out.mp4"), 1.0).unwrap();
        let mut machine =
            RecordingMachine::with_engine(Box::new(MockEngine::fully_capable(plan)), settings)
                .unwrap();
        machine.begin(target()).unwrap();
        assert_eq!(machine.status_label(), "Starting in 3s • 00:00");
    }
}
