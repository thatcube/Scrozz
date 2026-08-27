//! The still-capture contract.

use crate::{
    frame::Frame,
    target::{CaptureTarget, TargetEnumerator},
};

/// How the pointer is treated in a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorMode {
    /// Omit the pointer.
    #[default]
    Hidden,
    /// Composite the pointer at its position at capture time.
    Visible,
}

/// A request to take one still capture.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureRequest {
    /// What to capture.
    pub target: CaptureTarget,
    /// Whether to include the pointer.
    pub cursor: CursorMode,
    /// Whether to keep the window's own shadow, for window targets.
    ///
    /// Ignored for every other target.
    pub include_window_shadow: bool,
}

impl CaptureRequest {
    /// A capture of `target` with default options.
    #[must_use]
    pub fn new(target: CaptureTarget) -> Self {
        Self {
            target,
            cursor: CursorMode::Hidden,
            include_window_shadow: true,
        }
    }
}

/// How a capture was produced.
///
/// Travels with the image for the rest of its life, because decision D9 makes
/// downstream behaviour depend on it: beautification must refuse to composite
/// corners, shadows or backgrounds onto [`Provenance::Window`] pixels. Recording
/// it here rather than re-deriving it later means a capture cannot lose the fact
/// that it is sacred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// A whole display.
    Display,
    /// A single window — the OS already supplied its true shape and shadow.
    Window,
    /// A user-drawn region.
    Region,
    /// Multiple displays composited.
    AllDisplays,
    /// Assembled from several frames by scrolling capture.
    Stitched,
}

impl Provenance {
    /// Whether synthetic corners, shadows and backgrounds are forbidden.
    #[must_use]
    pub const fn forbids_compositing(self) -> bool {
        matches!(self, Self::Window)
    }
}

/// A completed still capture.
#[derive(Debug, Clone)]
pub struct Capture {
    /// The captured pixels.
    pub frame: Frame,
    /// How it was produced.
    pub provenance: Provenance,
    /// The target that produced it, for re-capture and for display in history.
    pub target: CaptureTarget,
}

/// A platform backend that takes still captures.
///
/// One implementation per platform. Every method may fail with
/// [`crate::Error::PermissionDenied`]; per decision D15 permission is requested
/// at first use rather than up front, so this is an ordinary early-life outcome.
pub trait CaptureBackend: TargetEnumerator + Send + Sync {
    /// Takes a single capture.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::TargetGone`] if the window or display vanished
    /// between enumeration and capture, [`crate::Error::PermissionDenied`] if the
    /// OS withheld screen access, or [`crate::Error::Unsupported`] if the
    /// compositor cannot service this target.
    fn capture(&self, request: &CaptureRequest) -> crate::Result<Capture>;

    /// Human-readable backend name for diagnostics, e.g. "ScreenCaptureKit".
    ///
    /// Surfaced in bug reports. Which of several backends was chosen at runtime
    /// is usually the first thing worth knowing about a capture defect.
    fn name(&self) -> &str;
}
