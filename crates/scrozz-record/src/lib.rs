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

use scrozz_core::{CaptureTarget, Result};

#[cfg(target_os = "windows")]
mod windows;

/// Encoder quality for a recording.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// Prefer a smaller file over fine detail.
    Low,
    /// Keep desktop text crisp without excessive bitrate.
    #[default]
    Balanced,
    /// Preserve detail for footage that will be edited or re-encoded.
    High,
}

/// What a recording session should include.
#[derive(Debug, Clone)]
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
    /// Explicit destination, or `None` for a unique file in the current
    /// directory.
    pub output: Option<PathBuf>,
    /// Encoder quality.
    pub quality: Quality,
    /// Maximum encoded height in pixels, preserving aspect ratio.
    pub max_height: Option<u32>,
}

impl RecordingRequest {
    /// Builds a request with balanced quality at the target's native size.
    #[must_use]
    pub fn new(
        target: CaptureTarget,
        microphone: bool,
        system_audio: bool,
        fps: u32,
        show_cursor: bool,
    ) -> Self {
        Self {
            target,
            microphone,
            system_audio,
            fps,
            show_cursor,
            output: None,
            quality: Quality::Balanced,
            max_height: None,
        }
    }

    /// Overrides the output path.
    #[must_use]
    pub fn with_output(mut self, output: Option<PathBuf>) -> Self {
        self.output = output;
        self
    }

    /// Overrides encoder quality.
    #[must_use]
    pub const fn with_quality(mut self, quality: Quality) -> Self {
        self.quality = quality;
        self
    }

    /// Caps the encoded height while preserving aspect ratio.
    #[must_use]
    pub const fn with_max_height(mut self, max_height: Option<u32>) -> Self {
        self.max_height = max_height;
        self
    }
}

/// A finished recording.
#[derive(Debug, Clone)]
pub struct Recording {
    /// Where the encoded file landed.
    pub path: PathBuf,
    /// Duration in seconds.
    pub duration_secs: f64,
    /// Why this is a playable partial file rather than a cleanly finalised one.
    pub salvaged: Option<String>,
}

impl Recording {
    /// A recording whose container finalised normally.
    #[must_use]
    pub fn complete(path: PathBuf, duration_secs: f64) -> Self {
        Self {
            path,
            duration_secs,
            salvaged: None,
        }
    }

    /// A playable partial recording retained after finalisation failed.
    #[must_use]
    pub fn salvaged(path: PathBuf, duration_secs: f64, reason: impl Into<String>) -> Self {
        Self {
            path,
            duration_secs,
            salvaged: Some(reason.into()),
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
    /// Returns an error if finalising the container or flushing the encoder
    /// failed. A partially written file must still be reported, never silently
    /// discarded: a long recording that fails at the last second is exactly when
    /// the user most wants whatever was salvageable.
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
    #[cfg(target_os = "windows")]
    {
        windows::start(request)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = request;
        todo!("open the platform encoder and begin the capture loop")
    }
}
