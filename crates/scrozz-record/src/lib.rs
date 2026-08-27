//! Screen and audio recording.
//!
//! # Codec licensing is an architectural constraint, not a detail
//!
//! Scrozz ships GPL-3.0. Linking `x264` would be legally fine under the GPL but
//! would forbid the store-distribution exception in `LICENSE` from doing its
//! job, and would make a permissive relicense impossible forever. Recording
//! therefore uses **hardware encoders only** on the default path —
//! VideoToolbox, Media Foundation, VA-API — which carry no such obligation.
//! Software fallback, if it ever exists, must be an optional non-default
//! feature. See `docs/research/capture-stack-landscape.md`.

// Platform APIs are reached through objc2 / windows-rs / x11rb, all of which
// require `unsafe`. It is confined to this crate: every crate above it in the
// dependency graph forbids unsafe outright.
#![deny(unsafe_op_in_unsafe_fn)]

use std::path::{Path, PathBuf};

use scrozz_core::{CaptureTarget, Error, Result};

pub mod edit;
pub mod engine;
pub mod machine;
pub mod overlay;
pub mod selection;
pub mod settings;
pub mod transcode;

pub use engine::{
    EngineCapabilities, RecordingEngine, detect_native_engine, validate_capabilities,
};
pub use machine::{
    ClockDrift, MachineEvent, MachineFailure, RecordingMachine, RecordingPhase, format_status_label,
};
pub use settings::RecordingSettings;

/// What a recording session should include.
///
/// The individual fields are retained as the narrow platform-engine contract.
/// [`RecordingRequest::from_settings`] keeps them aligned with the richer
/// [`RecordingSettings`] value used by the domain state machine.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingRequest {
    /// What to record.
    pub target: CaptureTarget,
    /// Capture microphone input.
    pub microphone: bool,
    /// Capture system audio output.
    ///
    /// The hardest of the three platforms' audio stories: macOS needs
    /// ScreenCaptureKit's audio tap, Windows uses WASAPI loopback, and PipeWire
    /// serves Linux.
    pub system_audio: bool,
    /// Frames per second.
    pub fps: u32,
    /// Draw the pointer into the video.
    pub show_cursor: bool,
}

impl RecordingRequest {
    /// Builds the platform request represented by validated recording settings.
    #[must_use]
    pub const fn from_settings(target: CaptureTarget, settings: &RecordingSettings) -> Self {
        Self {
            target,
            microphone: settings.audio.microphone,
            system_audio: settings.audio.system_audio,
            fps: settings.video.fps,
            show_cursor: settings.shows_cursor(),
        }
    }

    /// Validates the target and frame rate without consulting a platform.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an empty/non-finite region, an
    /// empty platform identifier, or an unsupported frame-rate value.
    pub fn validate(&self) -> Result<()> {
        if !(settings::VideoSettings::MIN_FPS..=settings::VideoSettings::MAX_FPS)
            .contains(&self.fps)
        {
            return Err(Error::InvalidRequest(format!(
                "{} fps is outside {}..={}",
                self.fps,
                settings::VideoSettings::MIN_FPS,
                settings::VideoSettings::MAX_FPS
            )));
        }
        match &self.target {
            CaptureTarget::Region(region) => {
                let values = [
                    region.origin.x,
                    region.origin.y,
                    region.size.width,
                    region.size.height,
                ];
                if values.iter().any(|value| !value.is_finite()) || region.is_empty() {
                    return Err(Error::InvalidRequest(
                        "a recording region must be finite and have non-zero area".to_owned(),
                    ));
                }
            }
            CaptureTarget::Display(id) if id.0.is_empty() => {
                return Err(Error::InvalidRequest(
                    "a recording display identifier cannot be empty".to_owned(),
                ));
            }
            CaptureTarget::Window(id) if id.0.is_empty() => {
                return Err(Error::InvalidRequest(
                    "a recording window identifier cannot be empty".to_owned(),
                ));
            }
            CaptureTarget::Display(_) | CaptureTarget::Window(_) | CaptureTarget::AllDisplays => {}
        }
        Ok(())
    }
}

/// Whether an output came from a real platform encoder or a deterministic test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingProvenance {
    /// A native platform recording.
    Native {
        /// Engine that produced the output.
        engine: String,
    },
    /// Output created by a mock, fixture, or test harness.
    Synthetic {
        /// Synthetic producer, suitable for diagnostics.
        generator: String,
    },
}

impl RecordingProvenance {
    /// Whether this provenance represents a real capture.
    #[must_use]
    pub const fn is_native(&self) -> bool {
        matches!(self, Self::Native { .. })
    }
}

/// Whether finalisation completed or left a usable partial file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingCompletion {
    /// The encoder and container finalised normally.
    Complete,
    /// A file exists but finalisation did not fully succeed.
    Partial {
        /// Actionable explanation of what could not be finalised.
        reason: String,
    },
}

/// A recording output.
///
/// Partial output is deliberately a successful value: once bytes usable by the
/// user exist, an engine must return them rather than hide the path behind an
/// error. [`RecordingCompletion`] says whether the caller should report success
/// or recovery.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    /// Where the encoded file landed.
    pub path: PathBuf,
    /// Duration in seconds.
    pub duration_secs: f64,
    /// Whether this is real platform output or a synthetic fixture.
    pub provenance: RecordingProvenance,
    /// Complete or salvageable partial output.
    pub completion: RecordingCompletion,
}

impl Recording {
    /// Creates a complete native recording.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if any metadata is unusable.
    pub fn native(
        path: impl Into<PathBuf>,
        duration_secs: f64,
        engine: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            path.into(),
            duration_secs,
            RecordingProvenance::Native {
                engine: engine.into(),
            },
            RecordingCompletion::Complete,
        )
    }

    /// Creates a salvageable partial native recording.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if any metadata or the reason is
    /// unusable.
    pub fn native_partial(
        path: impl Into<PathBuf>,
        duration_secs: f64,
        engine: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            path.into(),
            duration_secs,
            RecordingProvenance::Native {
                engine: engine.into(),
            },
            RecordingCompletion::Partial {
                reason: reason.into(),
            },
        )
    }

    /// Creates complete synthetic output for a fixture or mock.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if any metadata is unusable.
    pub fn synthetic(
        path: impl Into<PathBuf>,
        duration_secs: f64,
        generator: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            path.into(),
            duration_secs,
            RecordingProvenance::Synthetic {
                generator: generator.into(),
            },
            RecordingCompletion::Complete,
        )
    }

    /// Creates salvageable partial synthetic output for a fixture or mock.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if any metadata or the reason is
    /// unusable.
    pub fn synthetic_partial(
        path: impl Into<PathBuf>,
        duration_secs: f64,
        generator: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            path.into(),
            duration_secs,
            RecordingProvenance::Synthetic {
                generator: generator.into(),
            },
            RecordingCompletion::Partial {
                reason: reason.into(),
            },
        )
    }

    fn new(
        path: PathBuf,
        duration_secs: f64,
        provenance: RecordingProvenance,
        completion: RecordingCompletion,
    ) -> Result<Self> {
        let recording = Self {
            path,
            duration_secs,
            provenance,
            completion,
        };
        recording.validate()?;
        Ok(recording)
    }

    /// Validates modeled output metadata without touching the filesystem.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an empty path, invalid duration,
    /// empty producer name, or empty partial-output reason.
    pub fn validate(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Err(Error::InvalidRequest(
                "a recording output path cannot be empty".to_owned(),
            ));
        }
        if !self.duration_secs.is_finite() || self.duration_secs < 0.0 {
            return Err(Error::InvalidRequest(format!(
                "recording duration {} must be a finite non-negative number",
                self.duration_secs
            )));
        }
        let producer = match &self.provenance {
            RecordingProvenance::Native { engine } => engine,
            RecordingProvenance::Synthetic { generator } => generator,
        };
        if producer.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "recording provenance must name its producer".to_owned(),
            ));
        }
        if matches!(
            &self.completion,
            RecordingCompletion::Partial { reason } if reason.trim().is_empty()
        ) {
            return Err(Error::InvalidRequest(
                "partial recording output must explain why finalisation failed".to_owned(),
            ));
        }
        Ok(())
    }

    /// Rejects synthetic output at a user-real boundary.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when a mock or fixture is presented as
    /// a real recording.
    pub fn require_native(&self) -> Result<&Self> {
        self.validate()?;
        if let RecordingProvenance::Synthetic { generator } = &self.provenance {
            return Err(Error::InvalidRequest(format!(
                "synthetic recording from {generator:?} cannot be used as a real capture"
            )));
        }
        Ok(self)
    }

    /// Whether finalisation left only partial output.
    #[must_use]
    pub const fn is_partial(&self) -> bool {
        matches!(self.completion, RecordingCompletion::Partial { .. })
    }

    /// The finalisation failure reason, when this is partial output.
    #[must_use]
    pub fn partial_reason(&self) -> Option<&str> {
        match &self.completion {
            RecordingCompletion::Complete => None,
            RecordingCompletion::Partial { reason } => Some(reason),
        }
    }

    /// Turns existing output into partial output while preserving its path and
    /// provenance.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an empty reason.
    pub fn into_partial(mut self, reason: impl Into<String>) -> Result<Self> {
        self.completion = RecordingCompletion::Partial {
            reason: reason.into(),
        };
        self.validate()?;
        Ok(self)
    }

    pub(crate) fn into_synthetic(mut self, generator: impl Into<String>) -> Result<Self> {
        self.provenance = RecordingProvenance::Synthetic {
            generator: generator.into(),
        };
        self.validate()?;
        Ok(self)
    }

    /// Output path as a borrowed [`Path`].
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A live signal emitted by a platform recording session.
#[derive(Debug)]
pub enum SessionEvent {
    /// The encoder accepted its first video frame.
    FirstFrame,
    /// A recoverable condition worth showing without stopping.
    Warning(String),
    /// Capture failed and cannot continue.
    Failed(Error),
}

/// A recording session in progress.
///
/// Implementations must make `stop` safe to call from a hotkey handler at any
/// moment, including before the first frame arrives.
pub trait RecordingSession: Send {
    /// Suspends capture, keeping the session open.
    ///
    /// # Errors
    ///
    /// Returns an error if the session already ended.
    fn pause(&mut self) -> Result<()>;

    /// Resumes after [`Self::pause`].
    ///
    /// # Errors
    ///
    /// Returns an error if the session already ended.
    fn resume(&mut self) -> Result<()>;

    /// Polls one live event, if the implementation exposes them.
    ///
    /// Defaulting to `None` keeps in-flight native implementations source
    /// compatible while richer engines can report first-frame, warnings and
    /// failures.
    fn poll(&mut self) -> Option<SessionEvent> {
        None
    }

    /// The encoder's own elapsed clock, for drift diagnostics.
    ///
    /// This is observational only; the virtual-clock state machine remains
    /// authoritative. Older engines need not expose a clock.
    fn engine_elapsed_secs(&self) -> Option<f64> {
        None
    }

    /// Ends the session and finalises the file.
    ///
    /// # Errors
    ///
    /// Returns an error only when no usable output exists. If the engine wrote a
    /// usable file before finalisation failed, it must return an `Ok`
    /// [`Recording`] with [`RecordingCompletion::Partial`].
    fn stop(self: Box<Self>) -> Result<Recording>;
}

/// Starts a real native recording.
///
/// This function never substitutes a mock. Until a native engine branch is
/// linked for the current platform it returns a clean unsupported error.
///
/// # Errors
///
/// Returns [`Error::PermissionDenied`] if screen or audio access was withheld,
/// [`Error::InvalidRequest`] for malformed values, or [`Error::Unsupported`]
/// when no native engine exists or lacks a requested capability.
pub fn start(request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
    request.validate()?;
    let Some(engine) = detect_native_engine() else {
        return Err(no_native_engine_error());
    };
    validate_capabilities(engine.capabilities(), request, None)?;
    engine.start(request)
}

/// Starts a real native recording with rich settings capability validation.
///
/// # Errors
///
/// Returns the same errors as [`start`], plus unsupported errors for camera,
/// click, or keystroke settings the detected engine did not advertise.
pub fn start_with_settings(
    request: &RecordingRequest,
    settings: &RecordingSettings,
) -> Result<Box<dyn RecordingSession>> {
    request.validate()?;
    settings.validate()?;
    let Some(engine) = detect_native_engine() else {
        return Err(no_native_engine_error());
    };
    validate_capabilities(engine.capabilities(), request, Some(settings))?;
    engine.start(request)
}

fn no_native_engine_error() -> Error {
    Error::Unsupported {
        what: "screen recording".to_owned(),
        why: "no native recording engine is linked for this platform yet".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use scrozz_core::{DisplayId, LogicalPoint, LogicalRect, LogicalSize};

    use super::*;

    fn request() -> RecordingRequest {
        RecordingRequest {
            target: CaptureTarget::Display(DisplayId("display-1".to_owned())),
            microphone: false,
            system_audio: false,
            fps: 30,
            show_cursor: true,
        }
    }

    #[test]
    fn real_start_is_cleanly_unsupported_without_a_native_engine() {
        let error = match start(&request()) {
            Ok(_) => panic!("a native engine must not be invented"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("no native recording engine"), "{error}");
    }

    #[test]
    fn malformed_regions_are_rejected() {
        let mut request = request();
        request.target = CaptureTarget::Region(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(0.0, 10.0),
        ));
        assert!(request.validate().is_err());
    }

    #[test]
    fn synthetic_recordings_cannot_cross_a_real_boundary() {
        let fixture = Recording::synthetic("fixture.mp4", 2.0, "unit-test").unwrap();
        assert!(fixture.require_native().is_err());
        let real = Recording::native("capture.mp4", 2.0, "native-test").unwrap();
        assert!(real.require_native().is_ok());
    }

    #[test]
    fn partial_output_is_a_value_with_an_actionable_reason() {
        let output =
            Recording::native_partial("capture.mp4", 9.0, "native-test", "trailer write failed")
                .unwrap();
        assert!(output.is_partial());
        assert_eq!(output.partial_reason(), Some("trailer write failed"));
    }
}
