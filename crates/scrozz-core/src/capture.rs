//! The still-capture contract.

use crate::{
    frame::Frame,
    selection::{SourceApp, WindowPickingCapability, WindowSelection},
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
    /// A single window — the OS supplied its true shape and any available shadow.
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
    /// Which application the pixels came from, where the OS said.
    ///
    /// Recorded here rather than looked up later because it is not recoverable
    /// later: the window may be closed and the process gone by the time history
    /// draws a badge. Empty for display, region and all-display captures, which
    /// belong to no single application.
    pub source_app: SourceApp,
    /// Whether the window's own shadow is actually in these pixels.
    ///
    /// The *resolved* answer, not the request: a platform that cannot omit the
    /// shadow overrules [`CaptureRequest::include_window_shadow`], and recording
    /// the request instead would label the capture wrongly. `None` either when
    /// the target is not a window or when the capture source does not disclose
    /// whether a shadow is present.
    pub window_shadow: Option<bool>,
}

impl Capture {
    /// A capture with no source-application metadata and no shadow question.
    ///
    /// The constructor for display, region and all-display captures, which is
    /// most of them.
    #[must_use]
    pub fn new(frame: Frame, provenance: Provenance, target: CaptureTarget) -> Self {
        Self {
            frame,
            provenance,
            target,
            source_app: SourceApp::default(),
            window_shadow: None,
        }
    }

    /// Records which application this came from.
    #[must_use]
    pub fn with_source_app(mut self, source_app: SourceApp) -> Self {
        self.source_app = source_app;
        self
    }

    /// Records what the shadow flag resolved to in the pixels.
    #[must_use]
    pub const fn with_window_shadow(mut self, present: bool) -> Self {
        self.window_shadow = Some(present);
        self
    }
}

/// A backend's interactive window-capture capabilities.
///
/// Kept as its own trait because the answer is needed *before* a capture is
/// requested — the picker flow branches on it, and on Wayland the branch it
/// takes is "do not open a picker at all". Every [`CaptureBackend`] implements
/// it, so a caller can ask the backend it will actually capture through.
pub trait WindowPicking {
    /// How the user picks a window here, and what the resulting pixels contain.
    ///
    /// Infallible on purpose: "this platform cannot do it" is
    /// [`WindowSelection::Unavailable`], a value the caller can render, rather
    /// than an error it must decide how to report.
    fn window_picking(&self) -> WindowPickingCapability;

    /// How the user picks a window here.
    fn window_selection(&self) -> WindowSelection {
        self.window_picking().selection
    }
}

/// A platform backend that takes still captures.
///
/// One implementation per platform. Every method may fail with
/// [`crate::Error::PermissionDenied`]; per decision D15 permission is requested
/// at first use rather than up front, so this is an ordinary early-life outcome.
pub trait CaptureBackend: TargetEnumerator + WindowPicking + Send + Sync {
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
