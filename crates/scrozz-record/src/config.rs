//! Platform-independent recording validation and encoder selection.

use std::num::NonZeroU32;
use std::path::PathBuf;

use scrozz_core::{CaptureTarget, Error, Result};

use crate::{CameraRequest, Quality, RecordingRequest, RecordingResolution, VideoCodec};

/// Linux encoder quality on the donor implementation's 1–100 policy scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EncoderQuality(u8);

impl EncoderQuality {
    /// Returns the 1–100 value.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Maps the numeric policy onto VA-API's useful constant-QP range.
    #[must_use]
    pub const fn vaapi_qp(self) -> u8 {
        // Integer rounding is deterministic across hosts.
        42 - (((self.0 - 1) as u16 * 24) / 99) as u8
    }

    /// Maps quality onto rav1e's 0–255 quantizer range.
    #[must_use]
    pub const fn rav1e_quantizer(self) -> u8 {
        // Keep both ends away from pathological lossless/unwatchable settings.
        200 - (((self.0 - 1) as u16 * 180) / 99) as u8
    }
}

impl From<Quality> for EncoderQuality {
    fn from(quality: Quality) -> Self {
        // Preserve the donor encoder's 1–100 policy without exposing a second
        // user-facing quality type: Low=40, Balanced=70, High=90.
        Self(match quality {
            Quality::Low => 40,
            Quality::Balanced => 70,
            Quality::High => 90,
        })
    }
}

/// Resolves the shared size policy against a Linux source.
///
/// PipeWire reports physical buffers without a portable logical scale. For
/// those sources `backing_scale` is `None`, so `LogicalPoints` deliberately
/// assumes 1:1 rather than inventing compositor-specific coordinates.
pub fn resolve_dimensions(
    resolution: RecordingResolution,
    source_width: u32,
    source_height: u32,
    backing_scale: Option<f64>,
) -> Result<Dimensions> {
    if source_width == 0 || source_height == 0 {
        return Err(Error::InvalidRequest(
            "recording source dimensions must be non-zero".into(),
        ));
    }
    let scale = backing_scale
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(1.0);
    let (width, height) = resolution.apply(source_width, source_height, scale);
    if width == 0 || height == 0 {
        return Err(Error::InvalidRequest(
            "recording output dimensions must be at least 2x2".into(),
        ));
    }
    if width > Dimensions::MAX || height > Dimensions::MAX {
        return Err(Error::InvalidRequest(format!(
            "recording output {width}x{height} exceeds the 16384-pixel encoder limit"
        )));
    }

    Ok(Dimensions { width, height })
}

/// Validated encoder dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Dimensions {
    /// Maximum supported dimension.
    pub const MAX: u32 = 16_384;
}

/// Concrete encoder selected after capability probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectedVideoCodec {
    /// FFmpeg's `h264_vaapi` hardware encoder.
    H264Vaapi,
    /// The rav1e software AV1 encoder.
    Av1Rav1e,
}

/// Runtime encoder availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EncoderAvailability {
    /// A usable VA-API device and `h264_vaapi` encoder were found.
    pub h264_vaapi: bool,
    /// This binary contains the rav1e fallback.
    pub rav1e: bool,
}

/// Selects an encoder without ever considering x264.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] when the requested safe encoder is absent.
pub fn select_video_codec(
    requested: VideoCodec,
    available: EncoderAvailability,
) -> Result<SelectedVideoCodec> {
    match requested {
        VideoCodec::Auto if available.h264_vaapi => Ok(SelectedVideoCodec::H264Vaapi),
        VideoCodec::Auto if available.rav1e => Ok(SelectedVideoCodec::Av1Rav1e),
        VideoCodec::H264 if available.h264_vaapi => Ok(SelectedVideoCodec::H264Vaapi),
        VideoCodec::Av1 if available.rav1e => Ok(SelectedVideoCodec::Av1Rav1e),
        VideoCodec::H264 => Err(Error::Unsupported {
            what: "H.264 recording".into(),
            why: "no usable VA-API H.264 encoder was found; Scrozz deliberately never uses x264"
                .into(),
        }),
        VideoCodec::Av1 => Err(Error::Unsupported {
            what: "AV1 recording".into(),
            why: "this binary was built without the `rav1e-fallback` feature".into(),
        }),
        VideoCodec::Hevc => Err(Error::Unsupported {
            what: "HEVC recording".into(),
            why: "the Linux engine supports H.264 through h264_vaapi and optional AV1 through rav1e, but not HEVC"
                .into(),
        }),
        VideoCodec::Auto => Err(Error::Unsupported {
            what: "video recording".into(),
            why: "no usable VA-API H.264 encoder was found and this binary has no rav1e fallback"
                .into(),
        }),
    }
}

/// Validated, owned recording configuration.
#[derive(Debug, Clone)]
pub struct RecordingConfig {
    /// Capture source.
    pub target: CaptureTarget,
    /// Destination path.
    pub destination: Option<PathBuf>,
    /// Encoder quality.
    pub encoder_quality: EncoderQuality,
    /// Shared quality requested by the product.
    pub quality: Quality,
    /// Output-size policy.
    pub resolution: RecordingResolution,
    /// Codec policy.
    pub codec: VideoCodec,
    /// Capture microphone input.
    pub microphone: bool,
    /// Capture desktop output.
    pub system_audio: bool,
    /// Optional camera capture and composition.
    pub camera: Option<CameraRequest>,
    /// Frames per second.
    pub fps: NonZeroU32,
    /// Composite the pointer.
    pub show_cursor: bool,
}

impl TryFrom<&RecordingRequest> for RecordingConfig {
    type Error = Error;

    fn try_from(request: &RecordingRequest) -> Result<Self> {
        request.validate()?;
        let fps = NonZeroU32::new(request.fps).ok_or_else(|| {
            Error::InvalidRequest("recording frame rate must be between 1 and 240".into())
        })?;
        if fps.get() > 240 {
            return Err(Error::InvalidRequest(format!(
                "recording frame rate must be between 1 and 240, got {}",
                fps.get()
            )));
        }
        if let Some(destination) = &request.destination {
            let extension = destination
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if !extension.eq_ignore_ascii_case("mp4") && !extension.eq_ignore_ascii_case("m4v") {
                return Err(Error::InvalidRequest(format!(
                    "recording destination must use the .mp4 or .m4v extension, got `{}`",
                    destination.display()
                )));
            }
        }
        if request.video_codec == VideoCodec::Hevc {
            return Err(Error::Unsupported {
                what: "HEVC recording".into(),
                why: "the Linux engine supports H.264 through h264_vaapi and optional AV1 through rav1e, but not HEVC"
                    .into(),
            });
        }

        Ok(Self {
            target: request.target.clone(),
            destination: request.destination.clone(),
            encoder_quality: request.quality.into(),
            quality: request.quality,
            resolution: request.resolution,
            codec: request.video_codec,
            microphone: request.microphone,
            system_audio: request.system_audio,
            camera: request.camera.clone(),
            fps,
            show_cursor: request.show_cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use scrozz_core::CaptureTarget;

    use super::{
        EncoderAvailability, EncoderQuality, SelectedVideoCodec, resolve_dimensions,
        select_video_codec,
    };
    use crate::{Quality, RecordingRequest, RecordingResolution, VideoCodec};

    #[test]
    fn quality_maps_monotonically_to_encoder_quantizers() {
        let low = EncoderQuality::from(Quality::Low);
        let balanced = EncoderQuality::from(Quality::Balanced);
        let high = EncoderQuality::from(Quality::High);
        assert_eq!((low.get(), balanced.get(), high.get()), (40, 70, 90));
        assert!(low.vaapi_qp() > balanced.vaapi_qp());
        assert!(balanced.vaapi_qp() > high.vaapi_qp());
        assert!(low.rav1e_quantizer() > balanced.rav1e_quantizer());
        assert!(balanced.rav1e_quantizer() > high.rav1e_quantizer());
    }

    #[test]
    fn every_shared_resolution_is_even_bounded_and_never_upscales() {
        assert_eq!(
            resolve_dimensions(RecordingResolution::Native, 1919, 1079, Some(2.0)).unwrap(),
            super::Dimensions {
                width: 1918,
                height: 1078
            }
        );
        assert_eq!(
            resolve_dimensions(RecordingResolution::LogicalPoints, 3840, 2160, Some(2.0)).unwrap(),
            super::Dimensions {
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(
            resolve_dimensions(RecordingResolution::LogicalPoints, 1919, 1079, None).unwrap(),
            super::Dimensions {
                width: 1918,
                height: 1078
            }
        );
        assert_eq!(
            resolve_dimensions(RecordingResolution::ScalePercent(50), 1920, 1080, Some(2.0))
                .unwrap(),
            super::Dimensions {
                width: 960,
                height: 540
            }
        );
        assert_eq!(
            resolve_dimensions(
                RecordingResolution::MaxShortestEdge(720),
                3840,
                2160,
                Some(2.0)
            )
            .unwrap(),
            super::Dimensions {
                width: 1280,
                height: 720
            }
        );
        assert_eq!(
            resolve_dimensions(
                RecordingResolution::Exact {
                    width: 4000,
                    height: 3000
                },
                1920,
                1080,
                Some(2.0)
            )
            .unwrap(),
            super::Dimensions {
                width: 1920,
                height: 1080
            }
        );
        assert!(resolve_dimensions(RecordingResolution::Native, 20_000, 1080, Some(1.0)).is_err());
    }

    #[test]
    fn auto_prefers_hardware_and_only_uses_the_explicit_fallback() {
        assert_eq!(
            select_video_codec(
                VideoCodec::Auto,
                EncoderAvailability {
                    h264_vaapi: true,
                    rav1e: true,
                }
            )
            .unwrap(),
            SelectedVideoCodec::H264Vaapi
        );
        assert_eq!(
            select_video_codec(
                VideoCodec::Auto,
                EncoderAvailability {
                    h264_vaapi: false,
                    rav1e: true,
                }
            )
            .unwrap(),
            SelectedVideoCodec::Av1Rav1e
        );
        assert!(select_video_codec(VideoCodec::Auto, EncoderAvailability::default()).is_err());
    }

    #[test]
    fn request_rejects_invalid_fps_and_container() {
        let mut request = RecordingRequest::new(CaptureTarget::AllDisplays);
        request.destination = Some(PathBuf::from("capture.webm"));
        assert!(super::RecordingConfig::try_from(&request).is_err());
        request.destination = Some("capture.mp4".into());
        request.fps = 0;
        assert!(super::RecordingConfig::try_from(&request).is_err());
        request.fps = 241;
        assert!(super::RecordingConfig::try_from(&request).is_err());
    }

    #[test]
    fn hevc_is_explicitly_unsupported() {
        assert!(
            select_video_codec(
                VideoCodec::Hevc,
                EncoderAvailability {
                    h264_vaapi: true,
                    rav1e: true,
                }
            )
            .is_err()
        );
    }
}
