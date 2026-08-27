//! AVAssetWriter output settings.

use objc2::Message;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVVideoAverageBitRateKey, AVVideoCodecKey, AVVideoCodecTypeH264, AVVideoCodecTypeHEVC,
    AVVideoCompressionPropertiesKey, AVVideoEncoderSpecificationKey,
    AVVideoExpectedSourceFrameRateKey, AVVideoHeightKey, AVVideoWidthKey,
};
use objc2_foundation::{NSMutableDictionary, NSNumber, NSString};
use scrozz_core::{Error, Result};

use crate::VideoCodec;

use super::plan::RecordingPlan;

pub(crate) type SettingsDictionary = NSMutableDictionary<NSString, AnyObject>;

pub(crate) fn video(
    plan: &RecordingPlan,
    fps: u32,
) -> Result<objc2::rc::Retained<SettingsDictionary>> {
    let settings = SettingsDictionary::new();
    let compression = SettingsDictionary::new();
    let encoder = SettingsDictionary::new();

    let bitrate = NSNumber::numberWithUnsignedLongLong(plan.bitrate);
    let frame_rate = NSNumber::numberWithUnsignedInt(fps);
    let width = NSNumber::numberWithUnsignedInt(plan.size.width.round() as u32);
    let height = NSNumber::numberWithUnsignedInt(plan.size.height.round() as u32);
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
    encoder.insert(&*hardware_key, any(&*require_hardware));

    let codec = match plan.codec {
        // SAFETY: immutable weak-linked AVFoundation constants.
        VideoCodec::H264 => unsafe { required(AVVideoCodecTypeH264, "AVVideoCodecTypeH264") }?,
        // SAFETY: immutable weak-linked AVFoundation constants.
        VideoCodec::Hevc => unsafe { required(AVVideoCodecTypeHEVC, "AVVideoCodecTypeHEVC") }?,
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
    const MPEG4_AAC: u32 = u32::from_be_bytes(*b"aac ");

    let settings = SettingsDictionary::new();
    let format = NSNumber::numberWithUnsignedInt(MPEG4_AAC);
    let sample_rate = NSNumber::numberWithDouble(48_000.0);
    let channels = NSNumber::numberWithUnsignedInt(2);
    let bitrate = NSNumber::numberWithUnsignedInt(192_000);
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

fn any<T: Message + ?Sized>(value: &T) -> &AnyObject {
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
    fn muxer_settings_require_hardware_and_use_five_second_fragments() {
        let request = RecordingRequest::new(CaptureTarget::AllDisplays);
        let plan = RecordingPlan::new(&request, 1_920, 1_080, 1.0, false);
        assert_eq!(plan.fragment_interval_seconds, 5);

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
