//! LGPL FFmpeg AAC encoder for mixed PipeWire audio.

use std::ffi::{CStr, CString, c_int};
use std::ptr::{self, NonNull};

use ffmpeg_sys_next as ffi;
use scrozz_core::{Error, Result};

use crate::config::EncoderQuality;

/// One encoded AAC access unit.
#[derive(Debug, Clone)]
pub(crate) struct EncodedAudioPacket {
    pub start_frame: u64,
    pub duration: u32,
    pub data: Vec<u8>,
}

/// Fixed-rate stereo AAC encoder.
pub(crate) struct AacEncoder {
    context: NonNull<ffi::AVCodecContext>,
    decoder_configuration: Vec<u8>,
    frame_size: usize,
    pending: Vec<f32>,
    next_input_frame: u64,
}

// The context moves into and remains on one recording worker thread.
unsafe impl Send for AacEncoder {}

impl AacEncoder {
    pub(crate) const SAMPLE_RATE: u32 = 48_000;
    pub(crate) const CHANNELS: u16 = 2;

    pub(crate) fn new(quality: EncoderQuality) -> Result<Self> {
        // SAFETY: all allocations and FFmpeg return values are checked.
        unsafe { Self::new_unsafe(quality) }
    }

    unsafe fn new_unsafe(quality: EncoderQuality) -> Result<Self> {
        let name = CString::new("aac").expect("static encoder name has no NUL");
        // SAFETY: name is live and NUL-terminated.
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err(Error::Unsupported {
                what: "recording audio".into(),
                why: "the LGPL FFmpeg AAC encoder is unavailable".into(),
            });
        }
        // SAFETY: codec is FFmpeg-owned static storage.
        let raw = unsafe { ffi::avcodec_alloc_context3(codec) };
        let context = NonNull::new(raw)
            .ok_or_else(|| Error::Platform("FFmpeg could not allocate an AAC context".into()))?;
        // SAFETY: context is uniquely owned.
        let context_ref = unsafe { context.as_ptr().as_mut().unwrap() };
        context_ref.sample_rate = Self::SAMPLE_RATE as c_int;
        context_ref.sample_fmt = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP;
        context_ref.time_base = ffi::AVRational {
            num: 1,
            den: Self::SAMPLE_RATE as c_int,
        };
        context_ref.bit_rate = 96_000 + i64::from(quality.get().saturating_sub(1)) * 160_000 / 99;
        context_ref.flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
        // SAFETY: ch_layout is writable and starts zero-initialised by FFmpeg.
        unsafe { ffi::av_channel_layout_default(&mut context_ref.ch_layout, 2) };
        // SAFETY: context and codec are compatible.
        let status = unsafe { ffi::avcodec_open2(context.as_ptr(), codec, ptr::null_mut()) };
        if status < 0 {
            let mut raw = context.as_ptr();
            // SAFETY: raw is the owned context allocation.
            unsafe { ffi::avcodec_free_context(&mut raw) };
            return Err(Error::Platform(format!(
                "FFmpeg AAC encoder could not start: {}",
                ffmpeg_error(status)
            )));
        }
        // SAFETY: context remains live and avcodec_open2 populated these fields.
        let context_ref = unsafe { context.as_ref() };
        if context_ref.extradata.is_null() || context_ref.extradata_size <= 0 {
            let mut raw = context.as_ptr();
            // SAFETY: raw is the owned context allocation.
            unsafe { ffi::avcodec_free_context(&mut raw) };
            return Err(Error::Platform(
                "FFmpeg AAC encoder produced no AudioSpecificConfig".into(),
            ));
        }
        // SAFETY: extradata_size bytes are valid for the context lifetime.
        let decoder_configuration = unsafe {
            std::slice::from_raw_parts(
                context_ref.extradata,
                usize::try_from(context_ref.extradata_size).unwrap_or_default(),
            )
            .to_vec()
        };
        let frame_size = usize::try_from(context_ref.frame_size).unwrap_or(0);
        if frame_size == 0 {
            let mut raw = context.as_ptr();
            // SAFETY: raw is the owned context allocation.
            unsafe { ffi::avcodec_free_context(&mut raw) };
            return Err(Error::Platform(
                "FFmpeg AAC encoder reported a zero frame size".into(),
            ));
        }

        Ok(Self {
            context,
            decoder_configuration,
            frame_size,
            pending: Vec::new(),
            next_input_frame: 0,
        })
    }

    pub(crate) fn decoder_configuration(&self) -> &[u8] {
        &self.decoder_configuration
    }

    pub(crate) fn push_interleaved(&mut self, samples: &[f32]) -> Result<Vec<EncodedAudioPacket>> {
        if !samples.len().is_multiple_of(usize::from(Self::CHANNELS)) {
            return Err(Error::InvalidRequest(
                "AAC input is not interleaved stereo".into(),
            ));
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            return Err(Error::InvalidRequest(
                "AAC input contains a non-finite sample".into(),
            ));
        }
        self.pending
            .extend(samples.iter().map(|sample| sample.clamp(-1.0, 1.0)));
        self.encode_complete_frames()
    }

    fn encode_complete_frames(&mut self) -> Result<Vec<EncodedAudioPacket>> {
        let interleaved_frame_size = self.frame_size * usize::from(Self::CHANNELS);
        let mut packets = Vec::new();
        while self.pending.len() >= interleaved_frame_size {
            let frame_samples: Vec<f32> = self.pending.drain(..interleaved_frame_size).collect();
            self.send_frame(&frame_samples)?;
            packets.extend(self.receive(false)?);
        }
        Ok(packets)
    }

    fn send_frame(&mut self, samples: &[f32]) -> Result<()> {
        // SAFETY: frame allocation, buffers and channel layout copies are checked.
        unsafe {
            let mut frame = ffi::av_frame_alloc();
            if frame.is_null() {
                return Err(Error::Platform(
                    "FFmpeg could not allocate an AAC frame".into(),
                ));
            }
            (*frame).nb_samples = self.frame_size as c_int;
            (*frame).format = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as c_int;
            (*frame).sample_rate = Self::SAMPLE_RATE as c_int;
            let layout_status = ffi::av_channel_layout_copy(
                &mut (*frame).ch_layout,
                &(*self.context.as_ptr()).ch_layout,
            );
            if layout_status < 0 {
                ffi::av_frame_free(&mut frame);
                return Err(Error::Platform(format!(
                    "FFmpeg could not copy the AAC channel layout: {}",
                    ffmpeg_error(layout_status)
                )));
            }
            let status = ffi::av_frame_get_buffer(frame, 0);
            if status < 0 {
                ffi::av_frame_free(&mut frame);
                return Err(Error::Platform(format!(
                    "FFmpeg could not allocate AAC sample planes: {}",
                    ffmpeg_error(status)
                )));
            }
            let left =
                std::slice::from_raw_parts_mut((*frame).data[0].cast::<f32>(), self.frame_size);
            let right =
                std::slice::from_raw_parts_mut((*frame).data[1].cast::<f32>(), self.frame_size);
            for (index, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
                left[index] = pair[0];
                right[index] = pair[1];
            }
            (*frame).pts = self.next_input_frame as i64;
            self.next_input_frame = self.next_input_frame.saturating_add(self.frame_size as u64);
            let status = ffi::avcodec_send_frame(self.context.as_ptr(), frame);
            ffi::av_frame_free(&mut frame);
            if status < 0 {
                return Err(Error::Platform(format!(
                    "FFmpeg AAC encoder rejected samples: {}",
                    ffmpeg_error(status)
                )));
            }
        }
        Ok(())
    }

    fn receive(&mut self, flushing: bool) -> Result<Vec<EncodedAudioPacket>> {
        let mut output = Vec::new();
        // SAFETY: allocation is checked and freed before every return.
        let mut packet = unsafe { ffi::av_packet_alloc() };
        if packet.is_null() {
            return Err(Error::Platform(
                "FFmpeg could not allocate an AAC packet".into(),
            ));
        }
        loop {
            // SAFETY: packet and context are live.
            let status = unsafe { ffi::avcodec_receive_packet(self.context.as_ptr(), packet) };
            if status == 0 {
                // SAFETY: successful receive exposes size readable bytes.
                let packet_ref = unsafe { &*packet };
                let data = unsafe {
                    std::slice::from_raw_parts(
                        packet_ref.data,
                        usize::try_from(packet_ref.size).unwrap_or_default(),
                    )
                    .to_vec()
                };
                output.push(EncodedAudioPacket {
                    start_frame: packet_ref.pts.max(0) as u64,
                    duration: u32::try_from(packet_ref.duration)
                        .ok()
                        .filter(|duration| *duration > 0)
                        .unwrap_or(self.frame_size as u32),
                    data,
                });
                // SAFETY: keeps packet allocation for reuse.
                unsafe { ffi::av_packet_unref(packet) };
                continue;
            }
            if status == -11 || (flushing && status == ffi::AVERROR_EOF) {
                break;
            }
            // SAFETY: packet is live.
            unsafe { ffi::av_packet_free(&mut packet) };
            return Err(Error::Platform(format!(
                "AAC packet receive failed: {}",
                ffmpeg_error(status)
            )));
        }
        // SAFETY: packet is live.
        unsafe { ffi::av_packet_free(&mut packet) };
        Ok(output)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<EncodedAudioPacket>> {
        let mut packets = Vec::new();
        if !self.pending.is_empty() {
            let target = self.frame_size * usize::from(Self::CHANNELS);
            self.pending.resize(target, 0.0);
            packets.extend(self.encode_complete_frames()?);
        }
        // SAFETY: null frame is the documented flush signal.
        let status = unsafe { ffi::avcodec_send_frame(self.context.as_ptr(), ptr::null()) };
        if status < 0 && status != ffi::AVERROR_EOF {
            return Err(Error::Platform(format!(
                "AAC flush failed: {}",
                ffmpeg_error(status)
            )));
        }
        packets.extend(self.receive(true)?);
        Ok(packets)
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        // SAFETY: context is uniquely owned.
        unsafe {
            let mut context = self.context.as_ptr();
            ffi::avcodec_free_context(&mut context);
        }
    }
}

fn ffmpeg_error(code: c_int) -> String {
    let mut buffer = [0_u8; ffi::AV_ERROR_MAX_STRING_SIZE];
    // SAFETY: buffer is valid writable storage.
    let status = unsafe { ffi::av_strerror(code, buffer.as_mut_ptr(), buffer.len()) };
    if status < 0 {
        return format!("FFmpeg error {code}");
    }
    // SAFETY: av_strerror NUL-terminates on success.
    unsafe { CStr::from_ptr(buffer.as_ptr().cast()) }
        .to_string_lossy()
        .into_owned()
}
