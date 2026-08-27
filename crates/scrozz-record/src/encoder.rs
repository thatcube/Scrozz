//! Video encoder boundary.

#[cfg(all(
    target_os = "linux",
    feature = "linux-native",
    not(feature = "rav1e-fallback")
))]
use scrozz_core::Error;
use scrozz_core::Result;

#[cfg(all(target_os = "linux", feature = "linux-native"))]
use crate::config::VideoCodec;
use crate::config::{Dimensions, RecordingQuality};
use crate::format::Nv12Frame;
use crate::muxer::VideoCodecConfiguration;

#[cfg(all(target_os = "linux", feature = "linux-native"))]
pub(crate) mod aac;
#[cfg(feature = "rav1e-fallback")]
mod rav1e;
#[cfg(all(target_os = "linux", feature = "linux-native"))]
mod vaapi;

/// Immutable settings shared by native and software encoders.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VideoEncoderSettings {
    pub dimensions: Dimensions,
    pub fps: u32,
    pub quality: RecordingQuality,
}

/// One encoded access unit.
#[derive(Debug, Clone)]
pub(crate) struct EncodedVideoPacket {
    pub frame_index: u64,
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub(crate) trait VideoEncoder: Send {
    fn decoder_configuration(&self) -> VideoCodecConfiguration;
    fn encode(&mut self, frame: &Nv12Frame) -> Result<Vec<EncodedVideoPacket>>;
    fn finish(&mut self) -> Result<Vec<EncodedVideoPacket>>;
}

#[cfg(all(target_os = "linux", feature = "linux-native"))]
pub(crate) fn open(
    requested: VideoCodec,
    settings: VideoEncoderSettings,
) -> Result<Box<dyn VideoEncoder>> {
    match requested {
        VideoCodec::H264 => Ok(Box::new(vaapi::VaapiEncoder::new(settings)?)),
        VideoCodec::Av1 => open_rav1e(settings),
        VideoCodec::Auto => match vaapi::VaapiEncoder::new(settings) {
            Ok(encoder) => Ok(Box::new(encoder)),
            Err(hardware_error) => {
                #[cfg(feature = "rav1e-fallback")]
                {
                    tracing::warn!(
                        %hardware_error,
                        "VA-API H.264 unavailable; selecting the explicitly enabled rav1e fallback"
                    );
                    open_rav1e(settings)
                }
                #[cfg(not(feature = "rav1e-fallback"))]
                {
                    Err(Error::Unsupported {
                        what: "video recording".into(),
                        why: format!(
                            "VA-API H.264 could not be opened ({hardware_error}); this binary was \
                             built without the optional rav1e fallback. Scrozz deliberately never \
                             falls back to x264"
                        ),
                    })
                }
            }
        },
    }
}

#[cfg(all(
    target_os = "linux",
    feature = "linux-native",
    feature = "rav1e-fallback"
))]
fn open_rav1e(settings: VideoEncoderSettings) -> Result<Box<dyn VideoEncoder>> {
    Ok(Box::new(rav1e::Rav1eEncoder::new(settings)?))
}

#[cfg(all(
    target_os = "linux",
    feature = "linux-native",
    not(feature = "rav1e-fallback")
))]
fn open_rav1e(_settings: VideoEncoderSettings) -> Result<Box<dyn VideoEncoder>> {
    Err(Error::Unsupported {
        what: "AV1 recording".into(),
        why: "this binary was built without the `rav1e-fallback` feature".into(),
    })
}
