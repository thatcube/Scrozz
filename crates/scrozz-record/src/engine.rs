//! Recording-engine contracts, capability negotiation, and deterministic mocks.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use scrozz_core::{Error, Result};

use crate::{
    OverlaySource, Recording, RecordingRequest, RecordingSession, RecordingSettings, SessionEvent,
    VideoCodec,
};

/// Declarative features a concrete recording engine can potentially support.
///
/// These facts drive UI enablement and reject impossible requests, but they are
/// not a live hardware/permission probe. Encoders, endpoints, capture targets,
/// and permissions can disappear between query and use, so [`RecordingEngine::start`]
/// remains authoritative.
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
    /// Can record one display.
    pub display: bool,
    /// Can record one window.
    pub window: bool,
    /// Can crop a rectangular area from a supported source.
    pub region: bool,
    /// Can composite every attached display into one output.
    pub all_displays: bool,
    /// Can include the pointer when requested.
    pub cursor: bool,
    /// Can replace the native pointer with the shared smoothed renderer.
    pub cursor_smoothing: bool,
    /// Can write MP4 output.
    pub mp4: bool,
    /// Supports a hardware H.264 path.
    pub h264: bool,
    /// Supports a hardware HEVC path.
    pub hevc: bool,
    /// Supports an explicitly enabled AV1 path.
    pub av1: bool,
    /// Can apply the shared quality ladder.
    pub quality: bool,
    /// Can apply the shared resolution policies.
    pub resolution: bool,
}

impl EngineCapabilities {
    /// Core video capabilities without audio, input observation, or pause.
    pub const VIDEO: Self = Self {
        video: true,
        system_audio: false,
        microphone: false,
        camera: false,
        click_capture: false,
        key_capture: false,
        pause_resume: false,
        display: true,
        window: true,
        region: true,
        all_displays: true,
        cursor: true,
        cursor_smoothing: false,
        mp4: true,
        h264: true,
        hevc: false,
        av1: false,
        quality: true,
        resolution: true,
    };

    /// Every modeled recording feature, useful for a deterministic mock.
    pub const ALL: Self = Self {
        video: true,
        system_audio: true,
        microphone: true,
        camera: true,
        click_capture: true,
        key_capture: true,
        pause_resume: true,
        display: true,
        window: true,
        region: true,
        all_displays: true,
        cursor: true,
        cursor_smoothing: true,
        mp4: true,
        h264: true,
        hevc: true,
        av1: true,
        quality: true,
        resolution: true,
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

    /// Starts with the validated product settings in force.
    ///
    /// Engines override this when settings require native resources beyond the
    /// basic request, such as a lifetime-scoped input monitor.
    fn start_with_settings(
        &self,
        request: &RecordingRequest,
        settings: &RecordingSettings,
    ) -> Result<Box<dyn RecordingSession>> {
        if settings.needs_input_monitoring() || settings.cursor_smoothing {
            return Err(Error::Unsupported {
                what: "recording interaction overlays".to_owned(),
                why: "the selected recording engine does not provide a native interaction source"
                    .to_owned(),
            });
        }
        self.start(request)
    }

    /// Starts with native pull-based overlays when the engine supports them.
    ///
    /// The default is an explicit unsupported result rather than silently
    /// dropping visible recording features.
    fn start_with_overlays(
        &self,
        request: &RecordingRequest,
        overlays: Box<dyn OverlaySource>,
    ) -> Result<Box<dyn RecordingSession>> {
        let _ = (request, overlays);
        Err(Error::Unsupported {
            what: "recording overlays".to_owned(),
            why: "the selected recording engine does not composite pull-based overlays".to_owned(),
        })
    }
}

/// Detects the native recording engine for the current platform.
///
/// Returning `None` rather than a mock prevents synthetic output from being
/// reported as a real capture.
#[must_use]
pub fn detect_native_engine() -> Option<Box<dyn RecordingEngine>> {
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(crate::macos::MacEngine))
    }

    #[cfg(target_os = "windows")]
    {
        Some(Box::new(crate::windows::WindowsEngine))
    }

    #[cfg(all(target_os = "linux", feature = "linux-native"))]
    {
        Some(Box::new(crate::linux::LinuxEngine))
    }

    #[cfg(any(
        all(target_os = "linux", not(feature = "linux-native")),
        not(any(target_os = "macos", target_os = "windows", target_os = "linux"))
    ))]
    {
        None
    }
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
    match request.target {
        scrozz_core::CaptureTarget::Display(_) => {
            require(capabilities.display, "display recording")?;
        }
        scrozz_core::CaptureTarget::Window(_) => {
            require(capabilities.window, "window recording")?;
        }
        scrozz_core::CaptureTarget::Region(_) => {
            require(capabilities.region, "area recording")?;
        }
        scrozz_core::CaptureTarget::AllDisplays => {
            require(capabilities.all_displays, "all-display recording")?;
        }
    }
    require(
        !request.show_cursor || capabilities.cursor,
        "cursor capture",
    )?;
    require(capabilities.mp4, "MP4 output")?;
    require(capabilities.quality, "recording quality")?;
    require(capabilities.resolution, "recording resolution")?;
    match request.video_codec {
        VideoCodec::Auto => require(
            capabilities.h264 || capabilities.hevc || capabilities.av1,
            "video encoding",
        )?,
        VideoCodec::H264 => require(capabilities.h264, "H.264 encoding")?,
        VideoCodec::Hevc => require(capabilities.hevc, "HEVC encoding")?,
        VideoCodec::Av1 => require(capabilities.av1, "AV1 encoding")?,
    }
    let microphone =
        request.microphone || settings.is_some_and(RecordingSettings::needs_microphone);
    let system_audio =
        request.system_audio || settings.is_some_and(|value| value.audio.system_audio);
    let camera = settings.is_some_and(RecordingSettings::needs_camera);
    let clicks = settings.is_some_and(|value| value.clicks.enabled);
    let keys = settings.is_some_and(|value| value.keystrokes.enabled);
    let cursor_smoothing = settings.is_some_and(|value| value.cursor_smoothing);

    require(!microphone || capabilities.microphone, "microphone capture")?;
    require(
        !system_audio || capabilities.system_audio,
        "system audio capture",
    )?;
    require(!camera || capabilities.camera, "camera capture")?;
    require(!clicks || capabilities.click_capture, "click capture")?;
    require(!keys || capabilities.key_capture, "keystroke capture")?;
    require(
        !cursor_smoothing || capabilities.cursor_smoothing,
        "cursor smoothing",
    )?;
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

    fn start_with_overlays(
        &self,
        request: &RecordingRequest,
        _overlays: Box<dyn OverlaySource>,
    ) -> Result<Box<dyn RecordingSession>> {
        self.start(request)
    }

    fn start_with_settings(
        &self,
        request: &RecordingRequest,
        _settings: &RecordingSettings,
    ) -> Result<Box<dyn RecordingSession>> {
        self.start(request)
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
        poll.event.map(|event| match event {
            SessionEvent::Finished(output) => {
                match output.into_synthetic("deterministic mock engine") {
                    Ok(output) => SessionEvent::Finished(output),
                    Err(error) => SessionEvent::Failed(Arc::new(error)),
                }
            }
            event => event,
        })
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
        let mut request = RecordingRequest::new(CaptureTarget::Display(DisplayId("d1".to_owned())));
        request.show_cursor = true;
        request
    }

    #[test]
    fn settings_are_checked_against_advertised_capabilities() {
        let mut settings = RecordingSettings::shipped();
        settings.camera.enabled = true;
        let error = validate_capabilities(EngineCapabilities::VIDEO, &request(), Some(&settings))
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

    #[test]
    fn mock_terminal_poll_cannot_publish_native_provenance() {
        let native = Recording::native("native.mp4", 1.0, "forged native").unwrap();
        let plan = MockSessionPlan::complete("stop.mp4", 1.0)
            .unwrap()
            .with_polls(vec![MockPoll::new(
                Some(SessionEvent::Finished(native)),
                Some(1.0),
            )]);
        let engine = MockEngine::fully_capable(plan);
        let mut session = engine.start(&request()).unwrap();

        let Some(SessionEvent::Finished(output)) = session.poll() else {
            panic!("expected sanitized terminal output");
        };
        assert!(!output.provenance.is_native());
    }
}
