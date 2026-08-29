//! AVAssetWriter output settings.

use objc2::Message;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVVideoAverageBitRateKey, AVVideoCodecKey, AVVideoCodecTypeH264, AVVideoCodecTypeHEVC,
    AVVideoColorPrimaries_ITU_R_709_2, AVVideoColorPrimariesKey, AVVideoColorPropertiesKey,
    AVVideoCompressionPropertiesKey, AVVideoEncoderSpecificationKey,
    AVVideoExpectedSourceFrameRateKey, AVVideoHeightKey, AVVideoMaxKeyFrameIntervalDurationKey,
    AVVideoTransferFunction_ITU_R_709_2, AVVideoTransferFunctionKey, AVVideoWidthKey,
    AVVideoYCbCrMatrix_ITU_R_709_2, AVVideoYCbCrMatrixKey,
};
use objc2_foundation::{NSMutableDictionary, NSNumber, NSString};
use scrozz_core::{Error, Result};

use crate::{Quality, VideoCodec};

use super::plan::RecordingPlan;

pub(crate) type SettingsDictionary = NSMutableDictionary<NSString, AnyObject>;
const MAX_KEYFRAME_INTERVAL_SECONDS: f64 = 2.0;

pub(crate) fn video(
    plan: &RecordingPlan,
    fps: u32,
) -> Result<objc2::rc::Retained<SettingsDictionary>> {
    video_settings(
        plan.codec,
        plan.size.width.round() as u32,
        plan.size.height.round() as u32,
        plan.bitrate,
        fps,
    )
}

pub(crate) fn transcode_video(
    width: u32,
    height: u32,
    bitrate_fps: u32,
    encoder_rate_hint: u32,
    quality: Quality,
) -> Result<objc2::rc::Retained<SettingsDictionary>> {
    let settings = video_settings(
        VideoCodec::H264,
        width,
        height,
        quality.target_bitrate(width, height, bitrate_fps),
        encoder_rate_hint,
    )?;
    let color = rec709_color_properties()?;
    let color_key = unsafe { required(AVVideoColorPropertiesKey, "AVVideoColorPropertiesKey") }?;
    settings.insert(color_key, any(&*color));
    Ok(settings)
}

fn rec709_color_properties() -> Result<objc2::rc::Retained<SettingsDictionary>> {
    let properties = SettingsDictionary::new();
    for (key, value, key_name, value_name) in [
        (
            unsafe { AVVideoColorPrimariesKey },
            unsafe { AVVideoColorPrimaries_ITU_R_709_2 },
            "AVVideoColorPrimariesKey",
            "AVVideoColorPrimaries_ITU_R_709_2",
        ),
        (
            unsafe { AVVideoTransferFunctionKey },
            unsafe { AVVideoTransferFunction_ITU_R_709_2 },
            "AVVideoTransferFunctionKey",
            "AVVideoTransferFunction_ITU_R_709_2",
        ),
        (
            unsafe { AVVideoYCbCrMatrixKey },
            unsafe { AVVideoYCbCrMatrix_ITU_R_709_2 },
            "AVVideoYCbCrMatrixKey",
            "AVVideoYCbCrMatrix_ITU_R_709_2",
        ),
    ] {
        properties.insert(required(key, key_name)?, any(required(value, value_name)?));
    }
    Ok(properties)
}

fn video_settings(
    codec: VideoCodec,
    width: u32,
    height: u32,
    target_bitrate: u64,
    fps: u32,
) -> Result<objc2::rc::Retained<SettingsDictionary>> {
    let settings = SettingsDictionary::new();
    let compression = SettingsDictionary::new();
    let encoder = SettingsDictionary::new();

    let bitrate = NSNumber::numberWithUnsignedLongLong(target_bitrate);
    let frame_rate = NSNumber::numberWithUnsignedInt(fps);
    let keyframe_interval = NSNumber::numberWithDouble(MAX_KEYFRAME_INTERVAL_SECONDS);
    let width = NSNumber::numberWithUnsignedInt(width);
    let height = NSNumber::numberWithUnsignedInt(height);
    let require_hardware = NSNumber::numberWithBool(true);
    let hardware_key = NSString::from_str("RequireHardwareAcceleratedVideoEncoder");

    // SAFETY: these are immutable weak-linked AVFoundation string constants.
    let average_bitrate_key =
        unsafe { required(AVVideoAverageBitRateKey, "AVVideoAverageBitRateKey") }?;
    // SAFETY: see above.
    let source_rate_key = unsafe {
        required(
            AVVideoExpectedSourceFrameRateKey,
            "AVVideoExpectedSourceFrameRateKey",
        )
    }?;
    // SAFETY: see above.
    let keyframe_interval_key = unsafe {
        required(
            AVVideoMaxKeyFrameIntervalDurationKey,
            "AVVideoMaxKeyFrameIntervalDurationKey",
        )
    }?;
    // SAFETY: see above.
    let codec_key = unsafe { required(AVVideoCodecKey, "AVVideoCodecKey") }?;
    // SAFETY: see above.
    let width_key = unsafe { required(AVVideoWidthKey, "AVVideoWidthKey") }?;
    // SAFETY: see above.
    let height_key = unsafe { required(AVVideoHeightKey, "AVVideoHeightKey") }?;
    // SAFETY: see above.
    let compression_key = unsafe {
        required(
            AVVideoCompressionPropertiesKey,
            "AVVideoCompressionPropertiesKey",
        )
    }?;
    // SAFETY: see above.
    let encoder_key = unsafe {
        required(
            AVVideoEncoderSpecificationKey,
            "AVVideoEncoderSpecificationKey",
        )
    }?;

    compression.insert(average_bitrate_key, any(&*bitrate));
    compression.insert(source_rate_key, any(&*frame_rate));
    compression.insert(keyframe_interval_key, any(&*keyframe_interval));
    encoder.insert(&*hardware_key, any(&*require_hardware));

    let codec = match codec {
        // SAFETY: immutable weak-linked AVFoundation constants.
        VideoCodec::H264 => unsafe { required(AVVideoCodecTypeH264, "AVVideoCodecTypeH264") }?,
        // SAFETY: immutable weak-linked AVFoundation constants.
        VideoCodec::Hevc => unsafe { required(AVVideoCodecTypeHEVC, "AVVideoCodecTypeHEVC") }?,
        VideoCodec::Av1 => {
            return Err(Error::Unsupported {
                what: "AV1 recording".to_owned(),
                why: "the macOS VideoToolbox engine supports H.264 and HEVC".to_owned(),
            });
        }
        VideoCodec::Auto => unreachable!("RecordingPlan always resolves Auto"),
    };
    settings.insert(codec_key, any(codec));
    settings.insert(width_key, any(&*width));
    settings.insert(height_key, any(&*height));
    settings.insert(compression_key, any(&*compression));
    settings.insert(encoder_key, any(&*encoder));

    Ok(settings)
}

pub(crate) fn audio() -> objc2::rc::Retained<SettingsDictionary> {
    audio_for_channels(2)
}

pub(crate) fn audio_for_channels(channel_count: u16) -> objc2::rc::Retained<SettingsDictionary> {
    const MPEG4_AAC: u32 = u32::from_be_bytes(*b"aac ");

    let settings = SettingsDictionary::new();
    let format = NSNumber::numberWithUnsignedInt(MPEG4_AAC);
    let sample_rate = NSNumber::numberWithDouble(48_000.0);
    let channels = NSNumber::numberWithUnsignedInt(u32::from(channel_count));
    let bitrate =
        NSNumber::numberWithUnsignedInt(if channel_count == 1 { 96_000 } else { 192_000 });
    let format_key = NSString::from_str("AVFormatIDKey");
    let sample_rate_key = NSString::from_str("AVSampleRateKey");
    let channels_key = NSString::from_str("AVNumberOfChannelsKey");
    let bitrate_key = NSString::from_str("AVEncoderBitRateKey");

    settings.insert(&*format_key, any(&*format));
    settings.insert(&*sample_rate_key, any(&*sample_rate));
    settings.insert(&*channels_key, any(&*channels));
    settings.insert(&*bitrate_key, any(&*bitrate));
    settings
}

fn required<T>(value: Option<&'static T>, name: &str) -> Result<&'static T> {
    value.ok_or_else(|| Error::Unsupported {
        what: "macOS media encoding".to_owned(),
        why: format!("{name} is unavailable on this macOS"),
    })
}

pub(crate) fn any<T: Message + ?Sized>(value: &T) -> &AnyObject {
    // SAFETY: every `Message` is an Objective-C object pointer and `AnyObject`
    // is its type-erased representation. The returned borrow cannot outlive it.
    unsafe { &*(std::ptr::from_ref(value).cast::<AnyObject>()) }
}

#[cfg(test)]
mod tests {
    use scrozz_core::CaptureTarget;

    use super::*;
    use crate::RecordingRequest;

    #[test]
    fn muxer_settings_require_hardware_and_frequent_fragment_keyframes() {
        let request = RecordingRequest::new(CaptureTarget::AllDisplays);
        let plan = RecordingPlan::new(&request, 1_920, 1_080, 1.0, false);
        assert_eq!(plan.fragment_interval_seconds, 5);
        assert!(MAX_KEYFRAME_INTERVAL_SECONDS < plan.fragment_interval_seconds as f64);

        let settings = video(&plan, request.fps).unwrap();
        // SAFETY: immutable weak-linked AVFoundation constant.
        let key = unsafe {
            required(
                AVVideoEncoderSpecificationKey,
                "AVVideoEncoderSpecificationKey",
            )
        }
        .unwrap();
        assert!(settings.objectForKey(key).is_some());
    }
}
