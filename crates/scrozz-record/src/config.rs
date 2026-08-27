//! Platform-independent recording validation and encoder selection.

use std::num::NonZeroU32;
use std::path::PathBuf;

use scrozz_core::{CaptureTarget, Error, Result};

use crate::RecordingRequest;

/// Stable user-facing recording quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecordingQuality(u8);

impl RecordingQuality {
    /// Lowest accepted quality.
    pub const MIN: u8 = 1;
    /// Highest accepted quality.
    pub const MAX: u8 = 100;
    /// Balanced default.
    pub const BALANCED: Self = Self(70);

    /// Creates a quality value on Scrozz's 1–100 scale.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when `value` is outside 1–100.
    pub fn new(value: u8) -> Result<Self> {
        if (Self::MIN..=Self::MAX).contains(&value) {
            Ok(Self(value))
        } else {
            Err(Error::InvalidRequest(format!(
                "recording quality must be between {} and {}, got {value}",
                Self::MIN,
                Self::MAX
            )))
        }
    }

    /// Returns the 1–100 value.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Maps quality onto VA-API's useful constant-QP range.
    #[must_use]
    pub const fn vaapi_qp(self) -> u8 {
        // 1 -> 42, 100 -> 18. Integer rounding is deterministic across hosts.
        42 - (((self.0 - 1) as u16 * 24) / 99) as u8
    }

    /// Maps quality onto rav1e's 0–255 quantizer range.
    #[must_use]
    pub const fn rav1e_quantizer(self) -> u8 {
        // Keep both ends away from pathological lossless/unwatchable settings.
        200 - (((self.0 - 1) as u16 * 180) / 99) as u8
    }
}

impl Default for RecordingQuality {
    fn default() -> Self {
        Self::BALANCED
    }
}

/// Requested output dimensions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RecordingResolution {
    /// Preserve the source dimensions (rounded down to an even size when needed).
    #[default]
    Source,
    /// Scale both dimensions by this percentage.
    ScalePercent(u16),
    /// Produce an explicit output size.
    Exact {
        /// Output width.
        width: u32,
        /// Output height.
        height: u32,
    },
}

impl RecordingResolution {
    /// Resolves the request against a captured source.
    ///
    /// Chroma-subsampled encoders require even dimensions, so odd results are
    /// rounded down by one pixel. Values larger than 16K are rejected before a
    /// native encoder allocates hardware frames.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for zero dimensions, a zero scale, or
    /// an output above 16384x16384.
    pub fn resolve(self, source_width: u32, source_height: u32) -> Result<Dimensions> {
        if source_width == 0 || source_height == 0 {
            return Err(Error::InvalidRequest(
                "recording source dimensions must be non-zero".into(),
            ));
        }

        let (width, height) = match self {
            Self::Source => (source_width, source_height),
            Self::ScalePercent(percent) => {
                if percent == 0 {
                    return Err(Error::InvalidRequest(
                        "recording resolution scale must be greater than zero".into(),
                    ));
                }
                (
                    u64::from(source_width)
                        .saturating_mul(u64::from(percent))
                        .div_ceil(100) as u32,
                    u64::from(source_height)
                        .saturating_mul(u64::from(percent))
                        .div_ceil(100) as u32,
                )
            }
            Self::Exact { width, height } => (width, height),
        };

        let width = width & !1;
        let height = height & !1;
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

/// User-visible codec request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCodec {
    /// Prefer VA-API H.264, then use rav1e AV1 when that feature is compiled.
    #[default]
    Auto,
    /// Require the VA-API H.264 encoder. Scrozz never falls back to x264.
    H264,
    /// Require the optional rav1e AV1 encoder.
    Av1,
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
    pub destination: PathBuf,
    /// Encoder quality.
    pub quality: RecordingQuality,
    /// Output-size policy.
    pub resolution: RecordingResolution,
    /// Codec policy.
    pub codec: VideoCodec,
    /// Capture microphone input.
    pub microphone: bool,
    /// Capture desktop output.
    pub system_audio: bool,
    /// Frames per second.
    pub fps: NonZeroU32,
    /// Composite the pointer.
    pub show_cursor: bool,
}

impl TryFrom<&RecordingRequest> for RecordingConfig {
    type Error = Error;

    fn try_from(request: &RecordingRequest) -> Result<Self> {
        if request.destination.as_os_str().is_empty() {
            return Err(Error::InvalidRequest(
                "recording destination must not be empty".into(),
            ));
        }
        let fps = NonZeroU32::new(request.fps).ok_or_else(|| {
            Error::InvalidRequest("recording frame rate must be between 1 and 240".into())
        })?;
        if fps.get() > 240 {
            return Err(Error::InvalidRequest(format!(
                "recording frame rate must be between 1 and 240, got {}",
                fps.get()
            )));
        }
        if let RecordingResolution::ScalePercent(0)
        | RecordingResolution::Exact {
            width: 0,
            height: _,
        }
        | RecordingResolution::Exact {
            width: _,
            height: 0,
        } = request.resolution
        {
            return Err(Error::InvalidRequest(
                "recording resolution must be non-zero".into(),
            ));
        }

        let extension = request
            .destination
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if !extension.eq_ignore_ascii_case("mp4") && !extension.eq_ignore_ascii_case("m4v") {
            return Err(Error::InvalidRequest(format!(
                "recording destination must use the .mp4 or .m4v extension, got `{}`",
                request.destination.display()
            )));
        }

        Ok(Self {
            target: request.target.clone(),
            destination: request.destination.clone(),
            quality: request.quality,
            resolution: request.resolution,
            codec: request.codec,
            microphone: request.microphone,
            system_audio: request.system_audio,
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
        EncoderAvailability, RecordingQuality, RecordingResolution, SelectedVideoCodec, VideoCodec,
        select_video_codec,
    };
    use crate::RecordingRequest;

    #[test]
    fn quality_maps_monotonically_to_encoder_quantizers() {
        let low = RecordingQuality::new(1).unwrap();
        let high = RecordingQuality::new(100).unwrap();
        assert_eq!(low.vaapi_qp(), 42);
        assert_eq!(high.vaapi_qp(), 18);
        assert_eq!(low.rav1e_quantizer(), 200);
        assert_eq!(high.rav1e_quantizer(), 20);
    }

    #[test]
    fn resolution_is_even_and_bounded() {
        assert_eq!(
            RecordingResolution::Source.resolve(1919, 1079).unwrap(),
            super::Dimensions {
                width: 1918,
                height: 1078
            }
        );
        assert!(
            RecordingResolution::Exact {
                width: 20_000,
                height: 1080
            }
            .resolve(1920, 1080)
            .is_err()
        );
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
        let mut request = RecordingRequest::new(
            CaptureTarget::AllDisplays,
            PathBuf::from("capture.webm"),
            RecordingQuality::default(),
            RecordingResolution::Source,
            VideoCodec::Auto,
        );
        assert!(request.validate().is_err());
        request.destination = "capture.mp4".into();
        request.fps = 0;
        assert!(request.validate().is_err());
        request.fps = 241;
        assert!(request.validate().is_err());
    }
}
