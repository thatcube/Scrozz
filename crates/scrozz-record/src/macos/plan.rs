//! Pure capture and encoder planning.

use scrozz_core::PhysicalSize;

use crate::{RecordingRequest, VideoCodec};

#[cfg(test)]
use crate::Quality;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapturePixelFormat {
    VideoRange420,
    Bgra,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RecordingPlan {
    pub(crate) size: PhysicalSize,
    pub(crate) codec: VideoCodec,
    pub(crate) bitrate: u64,
    pub(crate) pixel_format: CapturePixelFormat,
    pub(crate) fragment_interval_seconds: i64,
}

impl RecordingPlan {
    pub(crate) fn new(
        request: &RecordingRequest,
        native_width: u32,
        native_height: u32,
        scale: f64,
        has_overlays: bool,
    ) -> Self {
        let (width, height) = request.resolution.apply(native_width, native_height, scale);
        let codec = request.video_codec.resolve(width, height);
        Self {
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            codec,
            bitrate: request.quality.bits_per_second(width, height, request.fps),
            pixel_format: if has_overlays {
                CapturePixelFormat::Bgra
            } else {
                CapturePixelFormat::VideoRange420
            },
            fragment_interval_seconds: 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use scrozz_core::CaptureTarget;

    use super::*;
    use crate::Resolution;

    #[test]
    fn recording_plan_keeps_muxer_fragments_and_hardware_safe_dimensions() {
        let mut request = RecordingRequest::new(CaptureTarget::AllDisplays);
        request.resolution = Resolution::LongestEdge(1_920);
        request.quality = Quality::High;
        let plan = RecordingPlan::new(&request, 3_840, 2_161, 2.0, false);

        assert_eq!(plan.size, PhysicalSize::new(1_920.0, 1_080.0));
        assert_eq!(plan.codec, VideoCodec::H264);
        assert_eq!(plan.fragment_interval_seconds, 5);
        assert_eq!(plan.pixel_format, CapturePixelFormat::VideoRange420);
    }

    #[test]
    fn an_overlay_plan_selects_bgra_capture() {
        let request = RecordingRequest::new(CaptureTarget::AllDisplays);
        let plan = RecordingPlan::new(&request, 1_920, 1_080, 1.0, true);
        assert_eq!(plan.pixel_format, CapturePixelFormat::Bgra);
    }
}
