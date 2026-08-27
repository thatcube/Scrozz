//! Recording-engine contracts, capability negotiation, and deterministic mocks.

use std::{collections::VecDeque, sync::Mutex};

use scrozz_core::{Error, Result};

use crate::{Recording, RecordingRequest, RecordingSession, RecordingSettings, SessionEvent};

/// Features a concrete recording engine actually supports.
///
/// Callers query this value and validate every requested feature. A platform
/// name is never treated as evidence that an API is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EngineCapabilities {
    /// Can encode video frames.
    pub video: bool,
    /// Can capture system output audio.
    pub system_audio: bool,
    /// Can capture a microphone.
    pub microphone: bool,
    /// Can composite a camera.
    pub camera: bool,
    /// Can observe pointer clicks.
    pub click_capture: bool,
    /// Can observe keyboard input.
    pub key_capture: bool,
    /// Can pause and resume an active session.
    pub pause_resume: bool,
}

impl EngineCapabilities {
    /// Every modeled recording feature, useful for a deterministic mock.
    pub const ALL: Self = Self {
        video: true,
        system_audio: true,
        microphone: true,
        camera: true,
        click_capture: true,
        key_capture: true,
        pause_resume: true,
    };
}

/// A platform or synthetic recording engine.
pub trait RecordingEngine: Send + Sync {
    /// Stable diagnostic name.
    fn name(&self) -> &'static str;

    /// Features this engine actually provides.
    fn capabilities(&self) -> EngineCapabilities;

    /// Starts a platform session for an already validated request.
    ///
    /// # Errors
    ///
    /// Returns an actionable platform, permission, request, or capability error.
    fn start(&self, request: &RecordingRequest) -> Result<Box<dyn RecordingSession>>;
}

/// Detects the native recording engine for the current platform.
///
/// Native branches intentionally have not landed yet. Returning `None` rather
/// than a mock prevents synthetic output from being reported as a real capture.
#[must_use]
pub fn detect_native_engine() -> Option<Box<dyn RecordingEngine>> {
    None
}

/// Validates a request and optional rich settings against engine capabilities.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] for malformed values and
/// [`Error::Unsupported`] naming the first unavailable feature otherwise.
pub fn validate_capabilities(
    capabilities: EngineCapabilities,
    request: &RecordingRequest,
    settings: Option<&RecordingSettings>,
) -> Result<()> {
    request.validate()?;
    if let Some(settings) = settings {
        settings.validate()?;
    }
    require(capabilities.video, "video encoding")?;
    let microphone =
        request.microphone || settings.is_some_and(RecordingSettings::needs_microphone);
    let system_audio =
        request.system_audio || settings.is_some_and(|value| value.audio.system_audio);
    let camera = settings.is_some_and(RecordingSettings::needs_camera);
    let clicks = settings.is_some_and(|value| value.clicks.enabled);
    let keys = settings.is_some_and(|value| value.keystrokes.enabled);

    require(!microphone || capabilities.microphone, "microphone capture")?;
    require(
        !system_audio || capabilities.system_audio,
        "system audio capture",
    )?;
    require(!camera || capabilities.camera, "camera capture")?;
    require(!clicks || capabilities.click_capture, "click capture")?;
    require(!keys || capabilities.key_capture, "keystroke capture")?;
    Ok(())
}

fn require(available: bool, feature: &str) -> Result<()> {
    if available {
        Ok(())
    } else {
        Err(Error::Unsupported {
            what: feature.to_owned(),
            why: "the selected recording engine did not advertise this capability".to_owned(),
        })
    }
}

/// One deterministic result returned by a mock session poll.
#[derive(Debug)]
pub struct MockPoll {
    /// Event returned from this poll.
    pub event: Option<SessionEvent>,
    /// Engine clock exposed after this poll.
    pub engine_elapsed_secs: Option<f64>,
}

impl MockPoll {
    /// Creates a scripted poll.
    #[must_use]
    pub const fn new(event: Option<SessionEvent>, engine_elapsed_secs: Option<f64>) -> Self {
        Self {
            event,
            engine_elapsed_secs,
        }
    }
}

/// How a deterministic mock session ends.
#[derive(Debug)]
pub enum MockStop {
    /// Return modeled output. The mock forcibly marks it synthetic.
    Output(Recording),
    /// No usable bytes exist.
    NoOutput(Error),
}

/// One mock session's deterministic script.
#[derive(Debug)]
pub struct MockSessionPlan {
    /// Poll responses, consumed one per call.
    pub polls: Vec<MockPoll>,
    /// Final stop result.
    pub stop: MockStop,
    /// Optional pause failure text.
    pub pause_failure: Option<String>,
    /// Optional resume failure text.
    pub resume_failure: Option<String>,
}

impl MockSessionPlan {
    /// A successful complete synthetic session.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for unusable output metadata.
    pub fn complete(path: impl Into<std::path::PathBuf>, duration_secs: f64) -> Result<Self> {
        Ok(Self {
            polls: Vec::new(),
            stop: MockStop::Output(Recording::synthetic(
                path,
                duration_secs,
                "deterministic mock engine",
            )?),
            pause_failure: None,
            resume_failure: None,
        })
    }

    /// A failed finalisation with salvageable synthetic output.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for unusable output metadata.
    pub fn partial(
        path: impl Into<std::path::PathBuf>,
        duration_secs: f64,
        reason: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            polls: Vec::new(),
            stop: MockStop::Output(Recording::synthetic_partial(
                path,
                duration_secs,
                "deterministic mock engine",
                reason,
            )?),
            pause_failure: None,
            resume_failure: None,
        })
    }

    /// A failure where no usable output exists.
    #[must_use]
    pub const fn no_output(error: Error) -> Self {
        Self {
            polls: Vec::new(),
            stop: MockStop::NoOutput(error),
            pause_failure: None,
            resume_failure: None,
        }
    }

    /// Replaces the poll script.
    #[must_use]
    pub fn with_polls(mut self, polls: Vec<MockPoll>) -> Self {
        self.polls = polls;
        self
    }
}

/// A deterministic, explicitly synthetic engine for tests and previews.
#[derive(Debug)]
pub struct MockEngine {
    capabilities: EngineCapabilities,
    plan: Mutex<Option<MockSessionPlan>>,
}

impl MockEngine {
    /// Creates a one-session mock with explicit capabilities and behavior.
    #[must_use]
    pub const fn new(capabilities: EngineCapabilities, plan: MockSessionPlan) -> Self {
        Self {
            capabilities,
            plan: Mutex::new(Some(plan)),
        }
    }

    /// Creates a fully capable one-session mock.
    #[must_use]
    pub const fn fully_capable(plan: MockSessionPlan) -> Self {
        Self::new(EngineCapabilities::ALL, plan)
    }
}

impl RecordingEngine for MockEngine {
    fn name(&self) -> &'static str {
        "deterministic-mock"
    }

    fn capabilities(&self) -> EngineCapabilities {
        self.capabilities
    }

    fn start(&self, request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
        validate_capabilities(self.capabilities, request, None)?;
        let plan = self
            .plan
            .lock()
            .map_err(|_| Error::Platform("mock recording plan lock was poisoned".to_owned()))?
            .take()
            .ok_or_else(|| {
                Error::InvalidRequest(
                    "a deterministic mock engine can start its one scripted session only once"
                        .to_owned(),
                )
            })?;
        Ok(Box::new(MockSession {
            polls: plan.polls.into(),
            stop: plan.stop,
            pause_failure: plan.pause_failure,
            resume_failure: plan.resume_failure,
            paused: false,
            engine_elapsed_secs: None,
        }))
    }
}

struct MockSession {
    polls: VecDeque<MockPoll>,
    stop: MockStop,
    pause_failure: Option<String>,
    resume_failure: Option<String>,
    paused: bool,
    engine_elapsed_secs: Option<f64>,
}

impl RecordingSession for MockSession {
    fn pause(&mut self) -> Result<()> {
        if let Some(message) = &self.pause_failure {
            return Err(Error::Platform(message.clone()));
        }
        if self.paused {
            return Err(Error::InvalidRequest(
                "mock recording session is already paused".to_owned(),
            ));
        }
        self.paused = true;
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if let Some(message) = &self.resume_failure {
            return Err(Error::Platform(message.clone()));
        }
        if !self.paused {
            return Err(Error::InvalidRequest(
                "mock recording session is not paused".to_owned(),
            ));
        }
        self.paused = false;
        Ok(())
    }

    fn poll(&mut self) -> Option<SessionEvent> {
        let poll = self.polls.pop_front()?;
        self.engine_elapsed_secs = poll.engine_elapsed_secs;
        poll.event
    }

    fn engine_elapsed_secs(&self) -> Option<f64> {
        self.engine_elapsed_secs
    }

    fn stop(self: Box<Self>) -> Result<Recording> {
        match self.stop {
            MockStop::Output(output) => output.into_synthetic("deterministic mock engine"),
            MockStop::NoOutput(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use scrozz_core::{CaptureTarget, DisplayId};

    use super::*;

    fn request() -> RecordingRequest {
        RecordingRequest {
            target: CaptureTarget::Display(DisplayId("d1".to_owned())),
            microphone: false,
            system_audio: false,
            fps: 30,
            show_cursor: true,
        }
    }

    #[test]
    fn settings_are_checked_against_advertised_capabilities() {
        let mut settings = RecordingSettings::shipped();
        settings.camera.enabled = true;
        let error = validate_capabilities(
            EngineCapabilities {
                video: true,
                ..EngineCapabilities::default()
            },
            &request(),
            Some(&settings),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("camera"), "{error}");
    }

    #[test]
    fn mock_output_is_forcibly_synthetic_and_partial_is_preserved() {
        let plan =
            MockSessionPlan::partial("partial.mp4", 7.0, "container trailer failed").unwrap();
        let engine = MockEngine::fully_capable(plan);
        let output = engine.start(&request()).unwrap().stop().unwrap();
        assert!(!output.provenance.is_native());
        assert!(output.is_partial());
    }

    #[test]
    fn mock_poll_sequence_and_clock_are_deterministic() {
        let plan = MockSessionPlan::complete("mock.mp4", 1.0)
            .unwrap()
            .with_polls(vec![
                MockPoll::new(Some(SessionEvent::FirstFrame), Some(0.25)),
                MockPoll::new(
                    Some(SessionEvent::Warning("late frame".to_owned())),
                    Some(0.5),
                ),
            ]);
        let engine = MockEngine::fully_capable(plan);
        let mut session = engine.start(&request()).unwrap();
        assert!(matches!(session.poll(), Some(SessionEvent::FirstFrame)));
        assert_eq!(session.engine_elapsed_secs(), Some(0.25));
        assert!(matches!(
            session.poll(),
            Some(SessionEvent::Warning(message)) if message == "late frame"
        ));
        assert_eq!(session.engine_elapsed_secs(), Some(0.5));
        assert!(session.poll().is_none());
    }

    #[test]
    fn mock_never_hides_partial_output_behind_an_error() {
        let output =
            Recording::synthetic_partial("partial.mp4", 3.0, "test", "flush failed").unwrap();
        let engine = MockEngine::fully_capable(MockSessionPlan {
            polls: vec![],
            stop: MockStop::Output(output),
            pause_failure: None,
            resume_failure: None,
        });
        let output = engine.start(&request()).unwrap().stop().unwrap();
        assert_eq!(output.partial_reason(), Some("flush failed"));
    }
}
