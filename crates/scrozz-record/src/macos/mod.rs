//! macOS ScreenCaptureKit recording.

mod audio;
mod compositor;
mod content;
pub(crate) mod error;
pub(crate) mod mix;
mod overlay;
pub(crate) mod pcm;
mod permission;
mod plan;
pub(crate) mod settings;
mod stream;
mod timeline;
pub(crate) mod writer;

use scrozz_core::Result;

use crate::{
    EngineCapabilities, OverlaySource, RecordingEngine, RecordingRequest, RecordingSession,
    RecordingSettings,
};

pub(crate) const ENGINE_NAME: &str = "macOS ScreenCaptureKit + VideoToolbox";

pub(crate) struct MacEngine;

impl RecordingEngine for MacEngine {
    fn name(&self) -> &'static str {
        ENGINE_NAME
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            video: true,
            system_audio: stream::system_audio_available(),
            microphone: true,
            click_capture: true,
            key_capture: true,
            pause_resume: true,
            display: true,
            window: true,
            region: true,
            all_displays: true,
            cursor: true,
            mp4: true,
            h264: true,
            hevc: true,
            quality: true,
            resolution: true,
            ..EngineCapabilities::default()
        }
    }

    fn start(&self, request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
        start(request, None, None)
    }

    fn start_with_settings(
        &self,
        request: &RecordingRequest,
        settings: &RecordingSettings,
    ) -> Result<Box<dyn RecordingSession>> {
        start(request, Some(settings), None)
    }

    fn start_with_overlays(
        &self,
        request: &RecordingRequest,
        overlays: Box<dyn OverlaySource>,
    ) -> Result<Box<dyn RecordingSession>> {
        start(request, None, Some(overlays))
    }
}

pub(super) fn start(
    request: &RecordingRequest,
    settings: Option<&RecordingSettings>,
    overlays: Option<Box<dyn OverlaySource>>,
) -> Result<Box<dyn RecordingSession>> {
    stream::start(request, settings, overlays)
}
