//! Windows.Graphics.Capture, WASAPI, and Media Foundation recording.

mod audio;
mod camera;
mod com;
mod device;
mod encoder;
mod geometry;
mod mix;
mod plan;
mod salvage;
mod session;
mod target;
mod terminal;
mod timing;
mod video;

use scrozz_core::Result;

use crate::{EngineCapabilities, RecordingEngine, RecordingRequest, RecordingSession};

pub(crate) const ENGINE_NAME: &str = "Windows Graphics Capture + Media Foundation";

pub(crate) struct WindowsEngine;

impl RecordingEngine for WindowsEngine {
    fn name(&self) -> &'static str {
        ENGINE_NAME
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            video: true,
            system_audio: true,
            microphone: true,
            camera: true,
            pause_resume: true,
            display: true,
            window: true,
            region: true,
            cursor: true,
            mp4: true,
            h264: true,
            quality: true,
            resolution: true,
            ..EngineCapabilities::default()
        }
    }

    fn start(&self, request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
        session::start(request)
    }
}

pub(crate) fn camera_devices() -> Result<Vec<crate::CameraDevice>> {
    camera::devices()
}

pub(crate) fn start_preview(
    request: &crate::CameraRequest,
) -> Result<Box<dyn crate::CameraPreviewSession>> {
    camera::start_preview(request)
}
