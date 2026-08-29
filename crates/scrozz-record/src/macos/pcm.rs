//! CoreMedia sample buffers to and from normalized PCM.
//!
//! ScreenCaptureKit and AVCapture can deliver different channel layouts and
//! sample formats. This bridge copies those transient buffers into the pure
//! mixer, then creates owned 48 kHz stereo Float32 buffers for AVAssetWriter.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{NonNull, null, null_mut};

use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, kAudioFormatFlagIsBigEndian,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsSignedInteger,
    kAudioFormatLinearPCM,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionCreate, CMAudioFormatDescriptionGetStreamBasicDescription,
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    kCMSampleBufferError_ArrayTooSmall, kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
};
use scrozz_core::{Error, Result};

use super::mix::PcmChunk;

const FLOAT32_FLAGS: u32 = kAudioFormatFlagIsFloat | (1 << 3);

pub(crate) fn decode(sample: &CMSampleBuffer) -> Result<PcmChunk> {
    // SAFETY: immutable CoreMedia sample metadata reads.
    let (format, frames, pts) = unsafe {
        (
            sample.format_description(),
            sample.num_samples(),
            sample.presentation_time_stamp(),
        )
    };
    let format = format.ok_or_else(|| {
        Error::Codec("audio sample did not include a format description".to_owned())
    })?;
    // SAFETY: the sample's retained format description is live for this read.
    let asbd = unsafe {
        CMAudioFormatDescriptionGetStreamBasicDescription(&format)
            .as_ref()
            .copied()
    }
    .ok_or_else(|| Error::Codec("audio sample did not include PCM stream metadata".to_owned()))?;
    if asbd.mFormatID != kAudioFormatLinearPCM {
        return Err(Error::Codec(format!(
            "capture returned unsupported audio format 0x{:08x}",
            asbd.mFormatID
        )));
    }
    if frames <= 0
        || asbd.mSampleRate <= 0.0
        || asbd.mChannelsPerFrame == 0
        || asbd.mBitsPerChannel == 0
    {
        return Err(Error::Codec(
            "capture returned invalid PCM stream dimensions".to_owned(),
        ));
    }
    if asbd.mFormatFlags & kAudioFormatFlagIsBigEndian != 0 {
        return Err(Error::Unsupported {
            what: "big-endian PCM recording".to_owned(),
            why: "the macOS recording mixer accepts native-endian PCM".to_owned(),
        });
    }

    let mut list_size = 0usize;
    // SAFETY: a null output list with a zero size is CoreMedia's documented
    // size-query operation.
    let query_status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            &mut list_size,
            null_mut(),
            0,
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            null_mut(),
        )
    };
    if query_status != 0 && query_status != kCMSampleBufferError_ArrayTooSmall {
        return Err(Error::Codec(format!(
            "querying captured PCM buffer-list size failed with status {query_status}"
        )));
    }
    if list_size < size_of::<AudioBufferList>() {
        return Err(Error::Codec(format!(
            "CoreMedia requested an invalid {list_size}-byte PCM buffer list"
        )));
    }
    let storage_words = list_size.div_ceil(size_of::<u64>());
    let mut storage = vec![0_u64; storage_words];
    let list = storage.as_mut_ptr().cast::<AudioBufferList>();
    let mut needed_size = 0usize;
    let mut retained_block = null_mut();
    // SAFETY: storage is aligned and allocated for the exact byte count reported
    // by CoreMedia; the retained block keeps every returned buffer pointer live.
    let status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            &mut needed_size,
            list,
            list_size,
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &mut retained_block,
        )
    };
    if status != 0 {
        return Err(Error::Codec(format!(
            "reading captured PCM buffers failed with status {status} \
             (needed {needed_size} bytes, supplied {list_size})"
        )));
    }
    let retained_block = NonNull::new(retained_block).map(|block| {
        // SAFETY: the Create-rule function transferred one retain to this output.
        unsafe { CFRetained::from_raw(block) }
    });
    let _retained_block = retained_block;

    // SAFETY: CoreMedia initialized the variable-length list inside storage.
    let buffers = unsafe {
        let count = usize::try_from((*list).mNumberBuffers)
            .map_err(|_| Error::Codec("PCM buffer count overflowed usize".to_owned()))?;
        let list_header = size_of::<AudioBufferList>() - size_of::<AudioBuffer>();
        let buffer_capacity = list_size.saturating_sub(list_header) / size_of::<AudioBuffer>();
        if count > buffer_capacity {
            return Err(Error::Codec(format!(
                "CoreMedia returned {count} PCM buffers for capacity {buffer_capacity}"
            )));
        }
        let first = std::ptr::addr_of!((*list).mBuffers).cast::<AudioBuffer>();
        std::slice::from_raw_parts(first, count)
    };
    if buffers.is_empty() {
        return Err(Error::Codec(
            "capture returned an empty PCM buffer list".to_owned(),
        ));
    }

    let frames = usize::try_from(frames)
        .map_err(|_| Error::Codec("PCM frame count overflowed usize".to_owned()))?;
    let channels = usize::try_from(asbd.mChannelsPerFrame)
        .map_err(|_| Error::Codec("PCM channel count overflowed usize".to_owned()))?;
    let mut samples = Vec::with_capacity(frames.saturating_mul(channels));
    for frame in 0..frames {
        for channel in 0..channels {
            let (buffer, channel_in_buffer) = locate_channel(buffers, channel)?;
            samples.push(read_sample(buffer, frame, channel_in_buffer, &asbd)?);
        }
    }

    Ok(PcmChunk {
        start_frame: time_to_frames(pts, asbd.mSampleRate)?,
        sample_rate: asbd.mSampleRate.round().clamp(1.0, f64::from(u32::MAX)) as u32,
        channels: u16::try_from(channels)
            .map_err(|_| Error::Codec("PCM channel count exceeded u16".to_owned()))?,
        samples,
    })
}

pub(crate) fn encode(chunk: &PcmChunk) -> Result<CFRetained<CMSampleBuffer>> {
    let chunk = chunk.stereo_48khz();
    encode_normalized(&chunk)
}

pub(crate) fn encode_normalized(chunk: &PcmChunk) -> Result<CFRetained<CMSampleBuffer>> {
    if chunk.samples.is_empty() {
        return Err(Error::Codec(
            "cannot create a CoreMedia buffer from empty PCM".to_owned(),
        ));
    }
    if chunk.sample_rate == 0
        || chunk.channels == 0
        || !chunk
            .samples
            .len()
            .is_multiple_of(usize::from(chunk.channels))
    {
        return Err(Error::Codec(
            "cannot create a CoreMedia buffer from malformed PCM".to_owned(),
        ));
    }
    let data_length = chunk
        .samples
        .len()
        .checked_mul(size_of::<f32>())
        .ok_or_else(|| Error::Codec("mixed PCM byte count overflowed usize".to_owned()))?;
    let mut block = null_mut();
    // SAFETY: CoreMedia allocates and owns a block of the requested size because
    // memory_block and custom_block_source are null.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            null_mut(),
            data_length,
            None,
            null(),
            0,
            data_length,
            0,
            NonNull::from(&mut block),
        )
    };
    if status != 0 {
        return Err(Error::Codec(format!(
            "allocating mixed PCM storage failed with status {status}"
        )));
    }
    let block = NonNull::new(block)
        .ok_or_else(|| Error::Codec("CoreMedia returned no storage for mixed PCM".to_owned()))?;
    // SAFETY: the Create rule transfers one owned retain.
    let block = unsafe { CFRetained::from_raw(block) };
    let source = NonNull::new(chunk.samples.as_ptr().cast_mut().cast::<c_void>())
        .expect("a non-empty slice has a non-null pointer");
    // SAFETY: source covers data_length bytes and the destination was allocated
    // for exactly that many bytes.
    let status = unsafe { CMBlockBuffer::replace_data_bytes(source, &block, 0, data_length) };
    if status != 0 {
        return Err(Error::Codec(format!(
            "copying mixed PCM failed with status {status}"
        )));
    }

    let bytes_per_frame = u32::from(chunk.channels) * size_of::<f32>() as u32;
    let stream = AudioStreamBasicDescription {
        mSampleRate: f64::from(chunk.sample_rate),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: FLOAT32_FLAGS,
        mBytesPerPacket: bytes_per_frame,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes_per_frame,
        mChannelsPerFrame: u32::from(chunk.channels),
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    let mut format: *const CMFormatDescription = null();
    // SAFETY: the ASBD fully describes interleaved stereo Float32 PCM and all
    // optional cookie/layout parameters are intentionally absent.
    let status = unsafe {
        CMAudioFormatDescriptionCreate(
            None,
            NonNull::from(&stream),
            0,
            null(),
            0,
            null(),
            None,
            NonNull::from(&mut format),
        )
    };
    if status != 0 {
        return Err(Error::Codec(format!(
            "creating mixed PCM format failed with status {status}"
        )));
    }
    let format = NonNull::new(format.cast_mut())
        .ok_or_else(|| Error::Codec("CoreMedia returned no format for mixed PCM".to_owned()))?;
    // SAFETY: the Create rule transfers one owned retain.
    let format = unsafe { CFRetained::from_raw(format) };

    let timing = CMSampleTimingInfo {
        // SAFETY: positive sample rate is a valid timescale.
        duration: unsafe { CMTime::new(1, chunk.sample_rate as i32) },
        // SAFETY: start_frame is expressed in the same positive timescale.
        presentationTimeStamp: unsafe { CMTime::new(chunk.start_frame, chunk.sample_rate as i32) },
        // SAFETY: zeroed flags represent CoreMedia's invalid decode-time sentinel.
        decodeTimeStamp: unsafe { std::mem::zeroed() },
    };
    let sample_count = isize::try_from(chunk.frames())
        .map_err(|_| Error::Codec("mixed PCM frame count overflowed isize".to_owned()))?;
    let mut sample = null_mut();
    // SAFETY: block, format and timing remain live for the call; CoreMedia retains
    // what the returned sample needs.
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(&format),
            sample_count,
            1,
            &timing,
            0,
            null(),
            NonNull::from(&mut sample),
        )
    };
    if status != 0 {
        return Err(Error::Codec(format!(
            "creating mixed PCM sample failed with status {status}"
        )));
    }
    let sample = NonNull::new(sample)
        .ok_or_else(|| Error::Codec("CoreMedia returned no mixed PCM sample".to_owned()))?;
    // SAFETY: the Create rule transfers one owned retain.
    Ok(unsafe { CFRetained::from_raw(sample) })
}

fn locate_channel(buffers: &[AudioBuffer], channel: usize) -> Result<(&AudioBuffer, usize)> {
    let mut first_channel = 0usize;
    for buffer in buffers {
        let buffer_channels = usize::try_from(buffer.mNumberChannels)
            .map_err(|_| Error::Codec("PCM buffer channel count overflowed usize".to_owned()))?;
        if channel < first_channel.saturating_add(buffer_channels) {
            return Ok((buffer, channel - first_channel));
        }
        first_channel = first_channel.saturating_add(buffer_channels);
    }
    Err(Error::Codec(
        "PCM buffer list did not contain every declared channel".to_owned(),
    ))
}

fn read_sample(
    buffer: &AudioBuffer,
    frame: usize,
    channel: usize,
    asbd: &AudioStreamBasicDescription,
) -> Result<f32> {
    let buffer_channels = usize::try_from(buffer.mNumberChannels)
        .map_err(|_| Error::Codec("PCM buffer channel count overflowed usize".to_owned()))?;
    let bytes_per_sample = usize::try_from(asbd.mBitsPerChannel.div_ceil(8))
        .map_err(|_| Error::Codec("PCM sample width overflowed usize".to_owned()))?;
    let bytes_per_frame = if asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 {
        usize::try_from(asbd.mBytesPerFrame)
            .map_err(|_| Error::Codec("PCM frame size overflowed usize".to_owned()))?
    } else {
        usize::try_from(asbd.mBytesPerFrame)
            .map_err(|_| Error::Codec("PCM frame size overflowed usize".to_owned()))?
            .max(bytes_per_sample.saturating_mul(buffer_channels))
    };
    let offset = frame
        .checked_mul(bytes_per_frame)
        .and_then(|offset| offset.checked_add(channel.saturating_mul(bytes_per_sample)))
        .ok_or_else(|| Error::Codec("PCM sample offset overflowed usize".to_owned()))?;
    let byte_size = usize::try_from(buffer.mDataByteSize)
        .map_err(|_| Error::Codec("PCM buffer size overflowed usize".to_owned()))?;
    if offset.saturating_add(bytes_per_sample) > byte_size || buffer.mData.is_null() {
        return Err(Error::Codec(
            "PCM sample data was shorter than its format description".to_owned(),
        ));
    }
    // SAFETY: the range check above proves this sample lies within mDataByteSize.
    let bytes = unsafe {
        std::slice::from_raw_parts(buffer.mData.cast::<u8>().add(offset), bytes_per_sample)
    };
    if asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0 {
        return match bytes_per_sample {
            4 => Ok(f32::from_ne_bytes(bytes.try_into().expect("four bytes"))),
            8 => Ok(f64::from_ne_bytes(bytes.try_into().expect("eight bytes")) as f32),
            _ => Err(Error::Unsupported {
                what: format!("{}-bit floating-point PCM", asbd.mBitsPerChannel),
                why: "the recording mixer supports Float32 and Float64 input".to_owned(),
            }),
        };
    }
    if asbd.mFormatFlags & kAudioFormatFlagIsSignedInteger == 0 {
        return Err(Error::Unsupported {
            what: "unsigned PCM recording".to_owned(),
            why: "the recording mixer supports signed integer and floating-point PCM".to_owned(),
        });
    }
    match bytes_per_sample {
        2 => Ok(f32::from(i16::from_ne_bytes(bytes.try_into().expect("two bytes"))) / 32_768.0),
        3 => {
            let value = if cfg!(target_endian = "little") {
                i32::from_le_bytes([
                    bytes[0],
                    bytes[1],
                    bytes[2],
                    if bytes[2] & 0x80 == 0 { 0 } else { 0xff },
                ])
            } else {
                i32::from_be_bytes([
                    if bytes[0] & 0x80 == 0 { 0 } else { 0xff },
                    bytes[0],
                    bytes[1],
                    bytes[2],
                ])
            };
            Ok(value as f32 / 8_388_608.0)
        }
        4 => Ok(i32::from_ne_bytes(bytes.try_into().expect("four bytes")) as f32 / 2_147_483_648.0),
        _ => Err(Error::Unsupported {
            what: format!("{}-bit integer PCM", asbd.mBitsPerChannel),
            why: "the recording mixer supports signed 16-, 24-, and 32-bit PCM".to_owned(),
        }),
    }
}

fn time_to_frames(time: CMTime, sample_rate: f64) -> Result<i64> {
    if !time.flags.contains(CMTimeFlags::Valid)
        || time.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
        || time.timescale <= 0
    {
        return Err(Error::Codec(
            "audio sample did not include a numeric presentation timestamp".to_owned(),
        ));
    }
    let sample_rate = sample_rate.round();
    if !sample_rate.is_finite() || sample_rate < 1.0 || sample_rate > u32::MAX as f64 {
        return Err(Error::Codec(
            "audio sample rate could not be represented on the mixer timeline".to_owned(),
        ));
    }
    let frames = i128::from(time.value)
        .saturating_mul(sample_rate as i128)
        .checked_div(i128::from(time.timescale))
        .ok_or_else(|| Error::Codec("audio sample timescale was zero".to_owned()))?;
    if frames < i128::from(i64::MIN) || frames > i128::from(i64::MAX) {
        return Err(Error::Codec(
            "audio sample timestamp overflowed the mixer timeline".to_owned(),
        ));
    }
    Ok(frames as i64)
}
