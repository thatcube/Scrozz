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

use std::path::PathBuf;
use std::time::Duration;

use scrozz_core::{CaptureTarget, Frame, PhysicalPoint, PhysicalSize, Result};

#[cfg(target_os = "macos")]
mod macos;

/// The bitrate/size trade-off for a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    /// Small files suitable for quick sharing.
    Small,
    /// A practical default for text, motion, and file size.
    #[default]
    Balanced,
    /// Preserve fine detail at a higher bitrate.
    High,
}

impl Quality {
    /// Chooses a target video bitrate for the encoded dimensions and frame rate.
    #[must_use]
    pub fn bits_per_second(self, width: u32, height: u32, fps: u32) -> u64 {
        const MIN_BITRATE: f64 = 800_000.0;
        const MAX_BITRATE: f64 = 120_000_000.0;

        let bits_per_pixel = match self {
            Self::Small => 0.045,
            Self::Balanced => 0.10,
            Self::High => 0.20,
        };
        (f64::from(width) * f64::from(height) * f64::from(fps) * bits_per_pixel)
            .clamp(MIN_BITRATE, MAX_BITRATE)
            .round() as u64
    }
}

/// How captured pixels are sized before encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Resolution {
    /// Keep the source's native backing-pixel dimensions.
    #[default]
    Native,
    /// Encode one pixel per logical display point.
    Points,
    /// Limit the longest encoded side while preserving aspect ratio.
    LongestEdge(u32),
}

impl Resolution {
    /// Applies this policy without upscaling and returns 4:2:0-safe even dimensions.
    #[must_use]
    pub fn apply(self, native_width: u32, native_height: u32, scale: f64) -> (u32, u32) {
        if native_width == 0 || native_height == 0 {
            return (0, 0);
        }

        let ratio = match self {
            Self::Native => 1.0,
            Self::Points if scale.is_finite() && scale > 1.0 => 1.0 / scale,
            Self::Points => 1.0,
            Self::LongestEdge(limit) if limit > 0 => {
                (f64::from(limit) / f64::from(native_width.max(native_height))).min(1.0)
            }
            Self::LongestEdge(_) => 0.0,
        };

        let width = (f64::from(native_width) * ratio).floor() as u32;
        let height = (f64::from(native_height) * ratio).floor() as u32;
        (
            even_at_most(width, native_width),
            even_at_most(height, native_height),
        )
    }
}

fn even_at_most(value: u32, native: u32) -> u32 {
    value.min(native) & !1
}

/// Hardware video encoder to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCodec {
    /// Prefer H.264, switching to HEVC only when H.264 cannot represent a side.
    #[default]
    Auto,
    /// Hardware H.264 for broad playback compatibility.
    H264,
    /// Hardware HEVC for very large or more efficiently compressed recordings.
    Hevc,
}

impl VideoCodec {
    /// Resolves [`Self::Auto`] for the encoded dimensions.
    #[must_use]
    pub const fn resolve(self, width: u32, height: u32) -> Self {
        match self {
            Self::Auto if width > 8_192 || height > 8_192 => Self::Hevc,
            Self::Auto => Self::H264,
            explicit => explicit,
        }
    }
}

/// What a recording session should include.
#[derive(Debug, Clone)]
pub struct RecordingRequest {
    /// What to record.
    pub target: CaptureTarget,
    /// Where to write the MP4, or a temporary path when omitted.
    pub destination: Option<PathBuf>,
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
    /// The bitrate/size trade-off.
    pub quality: Quality,
    /// The encoded dimensions.
    pub resolution: Resolution,
    /// The hardware video codec.
    pub video_codec: VideoCodec,
}

impl RecordingRequest {
    /// Creates a recording request with audio disabled.
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
            resolution: Resolution::default(),
            video_codec: VideoCodec::default(),
        }
    }

    /// Rejects incoherent parameters before any permission prompt can appear.
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::InvalidRequest`] for an invalid frame rate
    /// or an empty region.
    pub fn validate(&self) -> Result<()> {
        if !(1..=240).contains(&self.fps) {
            return Err(scrozz_core::Error::InvalidRequest(
                "recording frame rate must be between 1 and 240 fps".to_owned(),
            ));
        }
        if matches!(self.target, CaptureTarget::Region(rect) if rect.is_empty()) {
            return Err(scrozz_core::Error::InvalidRequest(
                "a recording region must have a non-zero width and height".to_owned(),
            ));
        }
        Ok(())
    }
}

/// A finished recording.
#[derive(Debug, Clone)]
pub struct Recording {
    /// Where the encoded file landed.
    pub path: PathBuf,
    /// Duration in seconds.
    pub duration_secs: f64,
    /// Encoded dimensions in physical pixels.
    pub size: PhysicalSize,
    /// Video frames successfully submitted to the encoder.
    pub frames: u64,
    /// Whether the file contains an audio track.
    pub has_audio: bool,
    /// Why only a playable partial recording could be finalised, if applicable.
    pub partial: Option<String>,
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
    /// Returns the layers for this frame at the recording's active elapsed time.
    fn layers(&mut self, elapsed: Duration, canvas: PhysicalSize) -> Vec<OverlayLayer>;
}

/// A recording session in progress.
///
/// Implementations must make `stop` safe to call from a hotkey handler at any
/// moment, including before the first frame arrives.
pub trait RecordingSession: Send {
    /// Returns the current state without waiting on the capture queue.
    fn state(&self) -> RecordingState;

    /// Returns active recording time, excluding pauses, without blocking.
    fn elapsed(&self) -> Duration;

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

    /// Ends the session and finalises the file.
    ///
    /// # Errors
    ///
    /// Returns an error only when no playable output exists. A finalisation
    /// problem after playable fragments were written is reported through
    /// [`Recording::partial`] so a long recording is never silently discarded.
    fn stop(self: Box<Self>) -> Result<Recording>;
}

/// Starts recording.
///
/// # Errors
///
/// Returns [`scrozz_core::Error::PermissionDenied`] if screen or audio access
/// was withheld, or [`scrozz_core::Error::Unsupported`] if no hardware encoder
/// is available.
pub fn start(request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
    request.validate()?;
    start_platform(request, None)
}

/// Starts recording with pull-based composited overlays.
///
/// # Errors
///
/// Returns the same errors as [`start`].
pub fn start_with_overlays(
    request: &RecordingRequest,
    overlays: Box<dyn OverlaySource>,
) -> Result<Box<dyn RecordingSession>> {
    request.validate()?;
    start_platform(request, Some(overlays))
}

#[cfg(target_os = "macos")]
fn start_platform(
    request: &RecordingRequest,
    overlays: Option<Box<dyn OverlaySource>>,
) -> Result<Box<dyn RecordingSession>> {
    macos::start(request, overlays)
}

#[cfg(not(target_os = "macos"))]
fn start_platform(
    request: &RecordingRequest,
    overlays: Option<Box<dyn OverlaySource>>,
) -> Result<Box<dyn RecordingSession>> {
    let _ = (request, overlays);
    todo!("open the platform encoder and begin the capture loop")
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};

    #[test]
    fn a_new_request_keeps_every_audio_source_off() {
        let request = RecordingRequest::new(CaptureTarget::AllDisplays);
        assert!(!request.microphone);
        assert!(!request.system_audio);
    }

    #[test]
    fn quality_bitrates_are_ordered_and_clamped() {
        let small = Quality::Small.bits_per_second(1920, 1080, 30);
        let balanced = Quality::Balanced.bits_per_second(1920, 1080, 30);
        let high = Quality::High.bits_per_second(1920, 1080, 30);
        assert!(small < balanced && balanced < high);
        assert_eq!(Quality::Small.bits_per_second(2, 2, 1), 800_000);
        assert_eq!(
            Quality::High.bits_per_second(u32::MAX, u32::MAX, 240),
            120_000_000
        );
    }

    #[test]
    fn resolution_never_upscales_and_always_returns_even_dimensions() {
        assert_eq!(Resolution::Native.apply(101, 99, 2.0), (100, 98));
        assert_eq!(Resolution::Points.apply(200, 100, 2.0), (100, 50));
        assert_eq!(
            Resolution::LongestEdge(4_096).apply(1920, 1080, 1.0),
            (1920, 1080)
        );
        let limited = Resolution::LongestEdge(1000).apply(1920, 1080, 1.0);
        assert_eq!(limited.0, 1000);
        assert_eq!(limited.0 % 2, 0);
        assert_eq!(limited.1 % 2, 0);
    }

    #[test]
    fn automatic_codec_only_uses_hevc_past_the_h264_side_limit() {
        assert_eq!(VideoCodec::Auto.resolve(8192, 8192), VideoCodec::H264);
        assert_eq!(VideoCodec::Auto.resolve(8194, 1080), VideoCodec::Hevc);
    }

    #[test]
    fn validation_rejects_bad_rates_and_empty_regions() {
        let mut request = RecordingRequest::new(CaptureTarget::AllDisplays);
        request.fps = 0;
        assert!(matches!(
            request.validate(),
            Err(scrozz_core::Error::InvalidRequest(_))
        ));

        request.target = CaptureTarget::Region(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(0.0, 20.0),
        ));
        request.fps = 30;
        assert!(matches!(
            request.validate(),
            Err(scrozz_core::Error::InvalidRequest(_))
        ));
    }
}
