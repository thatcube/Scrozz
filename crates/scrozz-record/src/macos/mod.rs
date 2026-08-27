//! macOS ScreenCaptureKit recording.

mod audio;
mod content;
mod error;
mod mix;
mod overlay;
mod pcm;
mod permission;
mod plan;
mod settings;
mod stream;
mod timeline;
mod writer;

use scrozz_core::Result;

use crate::{OverlaySource, RecordingRequest, RecordingSession};

pub(super) fn start(
    request: &RecordingRequest,
    overlays: Option<Box<dyn OverlaySource>>,
) -> Result<Box<dyn RecordingSession>> {
    stream::start(request, overlays)
}
