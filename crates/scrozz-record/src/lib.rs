//! Screen and audio recording.
//!
//! # Codec licensing is an architectural constraint, not a detail
//!
//! Scrozz ships GPL-3.0. Linking `x264` would be legally fine under the GPL but
//! would forbid the store-distribution exception in `LICENSE` from doing its
//! job, and would make a permissive relicense impossible forever. Recording
//! therefore uses **hardware encoders only** on the default path —
//! VideoToolbox, Media Foundation, VA-API — which carry no such obligation.
//! The sole software fallback is rav1e, behind the non-default
//! `rav1e-fallback` feature. Scrozz never discovers encoders by a broad
//! "anything H.264" search: the Linux path asks for `h264_vaapi` by name, so a
//! machine with x264 installed cannot silently change the licence properties of
//! a recording. See `docs/research/capture-stack-landscape.md`.

// Platform APIs are reached through objc2 / windows-rs / x11rb, all of which
// require `unsafe`. It is confined to this crate: every crate above it in the
// dependency graph forbids unsafe outright.
#![deny(unsafe_op_in_unsafe_fn)]

use std::path::PathBuf;

#[cfg(not(target_os = "linux"))]
use scrozz_core::Error;
use scrozz_core::{CaptureTarget, Result};

pub mod audio;
pub mod config;
pub mod format;
pub mod h264;
pub mod muxer;
pub mod pacing;
pub mod state;
pub mod timeline;

mod encoder;
#[cfg(target_os = "linux")]
mod linux;

pub use config::{RecordingQuality, RecordingResolution, VideoCodec};

/// What a recording session should include.
#[derive(Debug, Clone)]
pub struct RecordingRequest {
    /// What to record.
    pub target: CaptureTarget,
    /// The file to create.
    ///
    /// The recorder uses `create_new`; an existing file is never truncated.
    pub destination: PathBuf,
    /// Encoder quality on Scrozz's stable 1–100 scale.
    pub quality: RecordingQuality,
    /// Output dimensions, relative to the captured source.
    pub resolution: RecordingResolution,
    /// Requested video codec and fallback policy.
    pub codec: VideoCodec,
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
    /// Creates a recording request with explicit output decisions.
    ///
    /// Audio starts disabled, the cursor is hidden, and the default frame rate is
    /// 30 fps. Those are ordinary mutable options; destination, quality,
    /// resolution and codec are constructor arguments because opening an encoder
    /// without them would create a file whose format was decided accidentally.
    #[must_use]
    pub fn new(
        target: CaptureTarget,
        destination: impl Into<PathBuf>,
        quality: RecordingQuality,
        resolution: RecordingResolution,
        codec: VideoCodec,
    ) -> Self {
        Self {
            target,
            destination: destination.into(),
            quality,
            resolution,
            codec,
            microphone: false,
            system_audio: false,
            fps: 30,
            show_cursor: false,
        }
    }

    /// Validates all platform-independent request invariants.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an empty destination, a frame rate
    /// outside 1–240 fps, a zero-sized resolution, or a destination whose
    /// extension contradicts the selected codec.
    pub fn validate(&self) -> Result<()> {
        config::RecordingConfig::try_from(self).map(|_| ())
    }
}

/// How much of a partial recording can be recovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Salvageability {
    /// No encoded media reached disk.
    None,
    /// The ISO-BMFF initialisation segment is intact, but no complete media
    /// fragment was flushed.
    InitialisationOnly,
    /// At least one complete `moof`/`mdat` pair is present and independently
    /// playable.
    Playable,
}

impl Salvageability {
    /// Whether the file contains media a player or repair tool can recover.
    #[must_use]
    pub const fn has_playable_media(self) -> bool {
        matches!(self, Self::Playable)
    }
}

/// Whether recording reached an ordinary final stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingCompletion {
    /// Stop flushed every encoder and container fragment.
    Complete,
    /// Capture or encoding ended early, but the file was deliberately retained.
    Partial {
        /// What remains recoverable.
        salvageability: Salvageability,
        /// The failure that ended the session.
        reason: String,
    },
}

/// A finished recording.
#[derive(Debug, Clone)]
pub struct Recording {
    /// Where the encoded file landed.
    pub path: PathBuf,
    /// Duration in seconds.
    pub duration_secs: f64,
    /// Whether the file is complete or retained partial output.
    pub completion: RecordingCompletion,
}

impl Recording {
    /// Whether this is retained partial output.
    #[must_use]
    pub const fn is_partial(&self) -> bool {
        matches!(self.completion, RecordingCompletion::Partial { .. })
    }

    /// What can be recovered from this recording.
    #[must_use]
    pub const fn salvageability(&self) -> Salvageability {
        match self.completion {
            RecordingCompletion::Complete => Salvageability::Playable,
            RecordingCompletion::Partial { salvageability, .. } => salvageability,
        }
    }
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

    /// Ends the session and finalises the file.
    ///
    /// # Errors
    ///
    /// Returns an error only when no meaningful recording result can be reported.
    /// Once a destination exists, finalisation failures return a
    /// [`RecordingCompletion::Partial`] recording with explicit salvageability;
    /// a long recording that fails at the last second is exactly when the user
    /// most wants whatever was recoverable.
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

    #[cfg(target_os = "linux")]
    {
        linux::start(request)
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(Error::Unsupported {
            what: "screen recording".into(),
            why: "this build contains the Linux recording engine; the macOS and Windows native \
                  recording engines are implemented separately"
                .into(),
        })
    }
}
