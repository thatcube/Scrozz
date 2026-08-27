//! Windows.Graphics.Capture, WASAPI, and Media Foundation recording.

mod audio;
mod com;
mod device;
mod encoder;
mod mix;
mod plan;
mod salvage;
mod session;
mod target;
mod timing;
mod video;

use scrozz_core::Result;

use crate::{RecordingRequest, RecordingSession};

/// Starts a Windows recording session.
pub fn start(request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
    session::start(request)
}
