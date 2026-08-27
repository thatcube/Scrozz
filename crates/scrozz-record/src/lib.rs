//! Screen and audio recording.
//!
//! # Codec licensing is an architectural constraint, not a detail
//!
//! Scrozz ships GPL-3.0. Linking `x264` would be legally fine under the GPL but
//! would forbid the store-distribution exception in `LICENSE` from doing its
//! job, and would make a permissive relicense impossible forever. Recording
//! therefore uses hardware encoders on the default path: VideoToolbox, Media
//! Foundation, and VA-API. Software fallback must remain optional.

// Platform APIs are reached through objc2 / windows-rs / x11rb, all of which
// require `unsafe`. It is confined to this crate: every crate above it in the
// dependency graph forbids unsafe outright.
#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use scrozz_core::{CaptureTarget, Error, Frame, PhysicalPoint, PhysicalSize, Result};

pub mod edit;
pub mod engine;
pub mod machine;
#[cfg(target_os = "macos")]
mod macos;
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
pub use settings::{Quality, RecordingSettings, ResolutionCap};

/// Hardware video encoder requested for a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VideoCodec {
    /// Let the native engine choose its preferred supported codec.
    #[default]
    Auto,
    /// Hardware H.264 for broad playback compatibility.
    H264,
    /// Hardware HEVC for large or efficiently compressed recordings.
    Hevc,
    /// AV1, available only where an explicitly supported encoder exists.
    Av1,
}

impl VideoCodec {
    /// Stable command/settings spelling.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }

    /// Resolves the Apple hardware preference used by the macOS engine.
    ///
    /// Other engines remain free to resolve `Auto` according to their advertised
    /// codecs and must reject explicit unsupported choices at start time.
    #[must_use]
    pub const fn resolve(self, width: u32, height: u32) -> Self {
        match self {
            Self::Auto if width > 8_192 || height > 8_192 => Self::Hevc,
            Self::Auto => Self::H264,
            explicit => explicit,
        }
    }
}

/// How captured pixels are sized before encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RecordingResolution {
    /// Keep native backing-pixel dimensions.
    #[default]
    Native,
    /// Encode one pixel per logical display point.
    LogicalPoints,
    /// Scale both dimensions by this percentage without upscaling.
    ScalePercent(u16),
    /// Cap the shorter edge while preserving aspect ratio.
    MaxShortestEdge(u32),
    /// Produce exact dimensions. Engines reject dimensions they cannot encode.
    Exact {
        /// Encoded width.
        width: u32,
        /// Encoded height.
        height: u32,
    },
}

impl RecordingResolution {
    /// Converts the value used by recording settings into the native request.
    #[must_use]
    pub const fn from_cap(cap: ResolutionCap) -> Self {
        match cap {
            ResolutionCap::Native => Self::Native,
            ResolutionCap::Uhd2160 => Self::MaxShortestEdge(2_160),
            ResolutionCap::Qhd1440 => Self::MaxShortestEdge(1_440),
            ResolutionCap::Fhd1080 => Self::MaxShortestEdge(1_080),
            ResolutionCap::Hd720 => Self::MaxShortestEdge(720),
            ResolutionCap::Half => Self::ScalePercent(50),
        }
    }

    /// Applies this policy without upscaling and returns even dimensions.
    #[must_use]
    pub fn apply(self, native_width: u32, native_height: u32, backing_scale: f64) -> (u32, u32) {
        if native_width == 0 || native_height == 0 {
            return (0, 0);
        }

        let (width, height) = match self {
            Self::Native => (native_width, native_height),
            Self::LogicalPoints if backing_scale.is_finite() && backing_scale > 1.0 => {
                scale_dimensions(native_width, native_height, 1.0 / backing_scale)
            }
            Self::LogicalPoints => (native_width, native_height),
            Self::ScalePercent(percent) if percent > 0 => {
                let factor = (f64::from(percent) / 100.0).min(1.0);
                scale_dimensions(native_width, native_height, factor)
            }
            Self::ScalePercent(_) => (0, 0),
            Self::MaxShortestEdge(limit) if limit > 0 => {
                let shortest = native_width.min(native_height);
                if shortest <= limit {
                    (native_width, native_height)
                } else {
                    scale_dimensions(
                        native_width,
                        native_height,
                        f64::from(limit) / f64::from(shortest),
                    )
                }
            }
            Self::MaxShortestEdge(_) => (0, 0),
            Self::Exact { width, height } => (width, height),
        };
        (
            even_at_most(width, native_width),
            even_at_most(height, native_height),
        )
    }

    /// Stable command/settings spelling.
    #[must_use]
    pub fn slug(self) -> String {
        match self {
            Self::Native => "native".to_owned(),
            Self::LogicalPoints => "points".to_owned(),
            Self::ScalePercent(percent) => format!("{percent}%"),
            Self::MaxShortestEdge(edge) => format!("{edge}p"),
            Self::Exact { width, height } => format!("{width}x{height}"),
        }
    }

    fn validate(self) -> Result<()> {
        let valid = match self {
            Self::Native | Self::LogicalPoints => true,
            Self::ScalePercent(percent) => (1..=100).contains(&percent),
            Self::MaxShortestEdge(edge) => edge > 0,
            Self::Exact { width, height } => width > 0 && height > 0,
        };
        if valid {
            Ok(())
        } else {
            Err(Error::InvalidRequest(format!(
                "recording resolution {:?} has invalid dimensions",
                self
            )))
        }
    }
}

fn scale_dimensions(width: u32, height: u32, factor: f64) -> (u32, u32) {
    (
        (f64::from(width) * factor).round().max(1.0) as u32,
        (f64::from(height) * factor).round().max(1.0) as u32,
    )
}

fn even_at_most(value: u32, native: u32) -> u32 {
    (value.min(native) & !1).max(2.min(native & !1))
}

/// What a recording session should include.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingRequest {
    /// What to record.
    pub target: CaptureTarget,
    /// Where to write the encoded file, or an engine-selected temporary path.
    pub destination: Option<PathBuf>,
    /// Capture microphone input.
    pub microphone: bool,
    /// Capture system audio output.
    pub system_audio: bool,
    /// Frames per second.
    pub fps: u32,
    /// Draw the pointer into the video.
    pub show_cursor: bool,
    /// Encoder quality rung.
    pub quality: Quality,
    /// Encoded-size policy.
    pub resolution: RecordingResolution,
    /// Requested hardware codec.
    pub video_codec: VideoCodec,
}

impl RecordingRequest {
    /// Creates a request with shipped recording defaults and audio disabled.
    #[must_use]
    pub fn new(target: CaptureTarget) -> Self {
        Self {
            target,
            destination: None,
            microphone: false,
            system_audio: false,
            fps: 30,
            show_cursor: false,
            quality: Quality::default(),
            resolution: RecordingResolution::default(),
            video_codec: VideoCodec::default(),
        }
    }

    /// Builds the platform request represented by recording settings.
    #[must_use]
    pub const fn from_settings(target: CaptureTarget, settings: &RecordingSettings) -> Self {
        Self {
            target,
            destination: None,
            microphone: settings.audio.microphone,
            system_audio: settings.audio.system_audio,
            fps: settings.video.fps,
            show_cursor: settings.shows_cursor(),
            quality: settings.video.quality,
            resolution: RecordingResolution::from_cap(settings.video.resolution),
            video_codec: VideoCodec::Auto,
        }
    }

    /// Overrides the output path.
    #[must_use]
    pub fn with_destination(mut self, destination: impl Into<PathBuf>) -> Self {
        self.destination = Some(destination.into());
        self
    }

    /// Validates target and encoder values without consulting a platform.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for malformed values.
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
        self.resolution.validate()?;
        if self
            .destination
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(Error::InvalidRequest(
                "a recording destination cannot be empty".to_owned(),
            ));
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

/// Whether output came from a real platform encoder or a deterministic test.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordingProvenance {
    /// A native platform recording.
    Native {
        /// Engine stack that produced the output.
        engine: String,
        /// Resolved source, when the engine can report it.
        target: Option<CaptureTarget>,
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

    /// Diagnostic producer name.
    #[must_use]
    pub fn producer(&self) -> &str {
        match self {
            Self::Native { engine, .. } => engine,
            Self::Synthetic { generator } => generator,
        }
    }
}

/// How much media a retained partial file contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Salvageability {
    /// Container metadata exists, but no complete media fragment is playable.
    InitialisationOnly,
    /// At least one complete media fragment is playable.
    Playable,
}

impl Salvageability {
    /// Whether playback/editing can safely open this output.
    #[must_use]
    pub const fn is_playable(self) -> bool {
        matches!(self, Self::Playable)
    }
}

/// Whether finalisation completed or left retained partial output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingCompletion {
    /// The encoder and container finalised normally.
    Complete,
    /// A retained file exists but finalisation did not fully succeed.
    Partial {
        /// How much of the retained file is usable.
        salvageability: Salvageability,
        /// Actionable explanation of what could not be finalised.
        reason: String,
    },
}

/// Native summary metadata. Fields are optional for source-compatible engines.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecordingMetadata {
    /// Encoded dimensions in physical pixels.
    pub size: Option<PhysicalSize>,
    /// Video frames successfully submitted to the encoder.
    pub frames: Option<u64>,
    /// Encoded audio channels (`0` means the file is silent).
    pub audio_channels: Option<u16>,
    /// Final on-disk byte count.
    pub file_size_bytes: Option<u64>,
    /// Resolved codec, never `Auto`.
    pub video_codec: Option<VideoCodec>,
    /// Quality used by the encoder.
    pub quality: Option<Quality>,
    /// Applied resolution policy.
    pub resolution: Option<RecordingResolution>,
}

/// A finished recording.
///
/// Retained partial output is deliberately successful: once an engine can
/// report a destination, it returns a value rather than hiding that path behind
/// an error. Only a failure with no reportable output becomes `Err`.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    /// Where the encoded file landed.
    pub path: PathBuf,
    /// Active media duration in seconds, excluding pauses.
    pub duration_secs: f64,
    /// Whether this is real platform output or a synthetic fixture.
    pub provenance: RecordingProvenance,
    /// Complete or retained partial output.
    pub completion: RecordingCompletion,
    /// Native media summary.
    pub metadata: RecordingMetadata,
}

impl Recording {
    /// Creates complete native output.
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
                target: None,
            },
            RecordingCompletion::Complete,
            RecordingMetadata::default(),
        )
    }

    /// Creates playable partial native output.
    pub fn native_partial(
        path: impl Into<PathBuf>,
        duration_secs: f64,
        engine: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<Self> {
        Self::native_partial_with_salvageability(
            path,
            duration_secs,
            engine,
            Salvageability::Playable,
            reason,
        )
    }

    /// Creates retained partial native output with explicit salvageability.
    pub fn native_partial_with_salvageability(
        path: impl Into<PathBuf>,
        duration_secs: f64,
        engine: impl Into<String>,
        salvageability: Salvageability,
        reason: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            path.into(),
            duration_secs,
            RecordingProvenance::Native {
                engine: engine.into(),
                target: None,
            },
            RecordingCompletion::Partial {
                salvageability,
                reason: reason.into(),
            },
            RecordingMetadata::default(),
        )
    }

    /// Creates complete synthetic output for a fixture or mock.
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
            RecordingMetadata::default(),
        )
    }

    /// Creates playable partial synthetic output for a fixture or mock.
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
                salvageability: Salvageability::Playable,
                reason: reason.into(),
            },
            RecordingMetadata::default(),
        )
    }

    fn new(
        path: PathBuf,
        duration_secs: f64,
        provenance: RecordingProvenance,
        completion: RecordingCompletion,
        metadata: RecordingMetadata,
    ) -> Result<Self> {
        let recording = Self {
            path,
            duration_secs,
            provenance,
            completion,
            metadata,
        };
        recording.validate()?;
        Ok(recording)
    }

    /// Attaches resolved target and media metadata reported by a native engine.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the resulting report is incoherent.
    pub fn with_native_details(
        mut self,
        target: CaptureTarget,
        metadata: RecordingMetadata,
    ) -> Result<Self> {
        let RecordingProvenance::Native {
            target: output_target,
            ..
        } = &mut self.provenance
        else {
            return Err(Error::InvalidRequest(
                "synthetic output cannot receive native recording details".to_owned(),
            ));
        };
        *output_target = Some(target);
        self.metadata = metadata;
        self.validate()?;
        Ok(self)
    }

    /// Validates modeled output metadata without touching the filesystem.
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
        if self.provenance.producer().trim().is_empty() {
            return Err(Error::InvalidRequest(
                "recording provenance must name its producer".to_owned(),
            ));
        }
        if let RecordingCompletion::Partial { reason, .. } = &self.completion
            && reason.trim().is_empty()
        {
            return Err(Error::InvalidRequest(
                "partial recording output must explain why finalisation failed".to_owned(),
            ));
        }
        if matches!(self.metadata.video_codec, Some(VideoCodec::Auto)) {
            return Err(Error::InvalidRequest(
                "completed recording metadata must name the resolved codec".to_owned(),
            ));
        }
        if self.metadata.size.is_some_and(|size| {
            !size.width.is_finite()
                || !size.height.is_finite()
                || size.width <= 0.0
                || size.height <= 0.0
        }) {
            return Err(Error::InvalidRequest(
                "recording dimensions must be finite and positive".to_owned(),
            ));
        }
        Ok(())
    }

    /// Rejects synthetic output at a user-real boundary.
    pub fn require_native(&self) -> Result<&Self> {
        self.validate()?;
        if let RecordingProvenance::Synthetic { generator } = &self.provenance {
            return Err(Error::InvalidRequest(format!(
                "synthetic recording from {generator:?} cannot be used as a real capture"
            )));
        }
        Ok(self)
    }

    /// Whether finalisation left retained partial output.
    #[must_use]
    pub const fn is_partial(&self) -> bool {
        matches!(self.completion, RecordingCompletion::Partial { .. })
    }

    /// Whether this output can be opened for playback/editing.
    #[must_use]
    pub const fn is_playable(&self) -> bool {
        match self.completion {
            RecordingCompletion::Complete => true,
            RecordingCompletion::Partial { salvageability, .. } => salvageability.is_playable(),
        }
    }

    /// Finalisation failure reason, when partial.
    #[must_use]
    pub fn partial_reason(&self) -> Option<&str> {
        match &self.completion {
            RecordingCompletion::Complete => None,
            RecordingCompletion::Partial { reason, .. } => Some(reason),
        }
    }

    /// Turns existing output into playable partial output.
    pub fn into_partial(self, reason: impl Into<String>) -> Result<Self> {
        self.into_partial_with_salvageability(Salvageability::Playable, reason)
    }

    /// Turns existing output into retained partial output.
    pub fn into_partial_with_salvageability(
        mut self,
        salvageability: Salvageability,
        reason: impl Into<String>,
    ) -> Result<Self> {
        self.completion = RecordingCompletion::Partial {
            salvageability,
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

    /// Output path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Observable state of a recording session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingState {
    /// Samples are currently being recorded.
    Recording,
    /// Capture remains open but incoming samples are being discarded.
    Paused,
    /// Capture and encoding have ended.
    Stopped,
}

/// A composited layer supplied for one video frame.
#[derive(Debug, Clone)]
pub struct OverlayLayer {
    /// Pixels to draw over the captured frame.
    pub content: Frame,
    /// Top-left destination in encoded physical pixels.
    pub origin: PhysicalPoint,
    /// Layer opacity, clamped to `0.0..=1.0`.
    pub opacity: f32,
}

/// Pull-based overlays sampled on the capture callback queue.
pub trait OverlaySource: Send {
    /// Returns layers for this frame at active elapsed time.
    fn layers(&mut self, elapsed: Duration, canvas: PhysicalSize) -> Vec<OverlayLayer>;
}

/// A live signal emitted by a platform recording session.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// The encoder accepted its first video frame.
    FirstFrame,
    /// A recoverable condition worth showing without stopping.
    Warning(String),
    /// Native finalisation completed with complete or retained partial output.
    Finished(Recording),
    /// The session ended without any reportable retained output.
    Failed(Arc<Error>),
}

/// A recording session in progress.
pub trait RecordingSession: Send {
    /// Current native session state.
    ///
    /// Engines predating live state reporting may use the default while active.
    fn state(&self) -> RecordingState {
        RecordingState::Recording
    }

    /// Pause-free native media elapsed time.
    ///
    /// Kept alongside [`Self::engine_elapsed_secs`] for native adapters and
    /// diagnostics that use a duration value directly.
    fn elapsed(&self) -> Duration {
        self.engine_elapsed_secs()
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
            .unwrap_or(Duration::ZERO)
    }

    /// Suspends capture, keeping the session open.
    fn pause(&mut self) -> Result<()>;

    /// Resumes after [`Self::pause`].
    fn resume(&mut self) -> Result<()>;

    /// Polls one event. Terminal events are one-shot and already finalised.
    ///
    /// A later [`Self::stop`] must return the same semantic terminal outcome if
    /// the owner retains the session after polling it.
    fn poll(&mut self) -> Option<SessionEvent> {
        None
    }

    /// The engine's pause-free media-timeline elapsed time.
    fn engine_elapsed_secs(&self) -> Option<f64> {
        None
    }

    /// Ends the session and finalises the file.
    ///
    /// Returns `Err` only when no reportable output exists. Every retained
    /// post-start destination is returned as complete or partial output.
    fn stop(self: Box<Self>) -> Result<Recording>;
}

/// Starts a real native recording.
pub fn start(request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
    request.validate()?;
    let Some(engine) = detect_native_engine() else {
        return Err(no_native_engine_error());
    };
    validate_capabilities(engine.capabilities(), request, None)?;
    engine.start(request)
}

/// Starts a real native recording with rich settings validation.
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

/// Starts native recording with pull-based composited overlays.
pub fn start_with_overlays(
    request: &RecordingRequest,
    overlays: Box<dyn OverlaySource>,
) -> Result<Box<dyn RecordingSession>> {
    request.validate()?;
    let Some(engine) = detect_native_engine() else {
        return Err(no_native_engine_error());
    };
    validate_capabilities(engine.capabilities(), request, None)?;
    engine.start_with_overlays(request, overlays)
}

fn no_native_engine_error() -> Error {
    Error::Unsupported {
        what: "screen recording".to_owned(),
        why: "no native recording engine is linked for this platform".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};

    use super::*;

    #[test]
    fn a_new_request_keeps_every_audio_source_off() {
        let request = RecordingRequest::new(CaptureTarget::AllDisplays);
        assert!(!request.microphone);
        assert!(!request.system_audio);
    }

    #[test]
    fn resolution_never_upscales_and_always_returns_even_dimensions() {
        assert_eq!(RecordingResolution::Native.apply(101, 99, 2.0), (100, 98));
        assert_eq!(
            RecordingResolution::LogicalPoints.apply(200, 100, 2.0),
            (100, 50)
        );
        assert_eq!(
            RecordingResolution::MaxShortestEdge(1080).apply(3840, 2160, 1.0),
            (1920, 1080)
        );
    }

    #[test]
    fn validation_rejects_bad_rates_and_empty_regions() {
        let mut request = RecordingRequest::new(CaptureTarget::AllDisplays);
        request.fps = 0;
        assert!(matches!(request.validate(), Err(Error::InvalidRequest(_))));
        request.target = CaptureTarget::Region(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(0.0, 20.0),
        ));
        request.fps = 30;
        assert!(matches!(request.validate(), Err(Error::InvalidRequest(_))));
    }

    #[test]
    fn synthetic_recordings_cannot_cross_a_real_boundary() {
        let fixture = Recording::synthetic("fixture.mp4", 2.0, "unit-test").unwrap();
        assert!(fixture.require_native().is_err());
        let real = Recording::native("capture.mp4", 2.0, "native-test").unwrap();
        assert!(real.require_native().is_ok());
    }

    #[test]
    fn partial_output_carries_salvageability_and_reason() {
        let output = Recording::native_partial_with_salvageability(
            "capture.mp4",
            9.0,
            "native-test",
            Salvageability::InitialisationOnly,
            "capture ended before the first media fragment",
        )
        .unwrap();
        assert!(output.is_partial());
        assert!(!output.is_playable());
        assert_eq!(
            output.partial_reason(),
            Some("capture ended before the first media fragment")
        );
    }
}
