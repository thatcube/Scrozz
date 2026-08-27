//! Hardware-only H.264/AAC Media Foundation sink writer.

use std::{ffi::c_void, os::windows::ffi::OsStrExt, path::Path};

use scrozz_core::{Error, Result};
use windows::{
    Win32::{
        Media::MediaFoundation::{
            IMF2DBuffer, IMFActivate, IMFAttributes, IMFByteStream, IMFDXGIDeviceManager,
            IMFMediaType, IMFSinkWriter, IMFSinkWriterEx, MF_ACCESSMODE_WRITE, MF_E_INVALIDINDEX,
            MF_FILEFLAGS_NONE, MF_LOW_LATENCY, MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION,
            MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
            MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
            MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
            MF_MT_MAJOR_TYPE, MF_MT_MAX_KEYFRAME_SPACING, MF_MT_MPEG2_PROFILE,
            MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_OPENMODE_FAIL_IF_EXIST,
            MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_D3D_MANAGER,
            MF_TRANSCODE_CONTAINERTYPE, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateAttributes,
            MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateFile, MFCreateMediaType,
            MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Audio,
            MFMediaType_Video, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
            MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_HARDWARE_URL_Attribute, MFT_REGISTER_TYPE_INFO,
            MFTEnumEx, MFTranscodeContainerType_FMPEG4, MFVideoFormat_ARGB32, MFVideoFormat_H264,
            MFVideoInterlace_Progressive, eAVEncH264VProfile_High,
        },
        System::Com::CoTaskMemFree,
    },
    core::{Interface, PCWSTR},
};

use super::{
    device::Device, mix::MixedChunk, plan::EncoderPlan, timing::HNS_PER_SECOND, video::FramePacket,
};

const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u32 = 2;
const AUDIO_BITS: u32 = 16;
const AUDIO_BLOCK_ALIGN: u32 = 4;
const AUDIO_BITRATE: u32 = 192_000;

/// One fragmented MP4 sink writer.
pub struct Encoder {
    writer: IMFSinkWriter,
    byte_stream: IMFByteStream,
    _device_manager: IMFDXGIDeviceManager,
    video_stream: u32,
    audio_stream: Option<u32>,
    frame_duration_hns: i64,
    samples_written: u64,
}

impl Encoder {
    /// Creates and starts a sink writer backed by a hardware H.264 MFT.
    pub fn new(path: &Path, device: &Device, plan: EncoderPlan, audio: bool) -> Result<Self> {
        require_hardware_h264()?;

        let manager = dxgi_manager(device)?;
        let attributes = attributes(6)?;
        let configure_attributes = || -> windows::core::Result<()> {
            unsafe {
                attributes.SetGUID(
                    &MF_TRANSCODE_CONTAINERTYPE,
                    &MFTranscodeContainerType_FMPEG4,
                )?;
                attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
                attributes.SetUINT32(&MF_LOW_LATENCY, 1)?;
                attributes.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)?;
            }
            Ok(())
        };
        configure_attributes()
            .map_err(|error| Error::Codec(format!("could not configure MP4 writer: {error}")))?;

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let byte_stream = unsafe {
            MFCreateFile(
                MF_ACCESSMODE_WRITE,
                MF_OPENMODE_FAIL_IF_EXIST,
                MF_FILEFLAGS_NONE,
                PCWSTR(wide.as_ptr()),
            )
        }
        .map_err(|error| {
            if path.exists() {
                Error::InvalidRequest(format!(
                    "recording output already exists: {}",
                    path.display()
                ))
            } else {
                Error::Codec(format!("could not create recording output: {error}"))
            }
        })?;
        let mut cleanup = OutputCleanup::new(path, &byte_stream);
        let writer =
            unsafe { MFCreateSinkWriterFromURL(PCWSTR::null(), &byte_stream, &attributes) }
                .map_err(|error| {
                    Error::Codec(format!("could not create MP4 sink writer: {error}"))
                })?;

        let video_output = video_output_type(plan)
            .map_err(|error| Error::Codec(format!("invalid H.264 media type: {error}")))?;
        let video_stream = unsafe { writer.AddStream(&video_output) }
            .map_err(|error| Error::Codec(format!("could not add H.264 stream: {error}")))?;
        let video_input = video_input_type(plan)
            .map_err(|error| Error::Codec(format!("invalid BGRA media type: {error}")))?;
        unsafe { writer.SetInputMediaType(video_stream, &video_input, None::<&IMFAttributes>) }
            .map_err(|error| {
                Error::Codec(format!("could not configure BGRA video input: {error}"))
            })?;

        let audio_stream = if audio {
            let audio_output = audio_output_type()
                .map_err(|error| Error::Codec(format!("invalid AAC media type: {error}")))?;
            let stream = unsafe { writer.AddStream(&audio_output) }
                .map_err(|error| Error::Codec(format!("could not add AAC stream: {error}")))?;
            let audio_input = audio_input_type()
                .map_err(|error| Error::Codec(format!("invalid PCM media type: {error}")))?;
            unsafe { writer.SetInputMediaType(stream, &audio_input, None::<&IMFAttributes>) }
                .map_err(|error| {
                    Error::Codec(format!("could not configure PCM audio input: {error}"))
                })?;
            Some(stream)
        } else {
            None
        };

        unsafe { writer.BeginWriting() }
            .map_err(|error| Error::Codec(format!("sink writer could not begin: {error}")))?;
        verify_hardware_transform(&writer, video_stream)?;
        cleanup.keep();
        drop(cleanup);

        Ok(Self {
            writer,
            byte_stream,
            _device_manager: manager,
            video_stream,
            audio_stream,
            frame_duration_hns: HNS_PER_SECOND / i64::from(plan.fps),
            samples_written: 0,
        })
    }

    /// Sends one GPU texture directly to the sink writer.
    pub fn write_video(&mut self, packet: FramePacket, stream_hns: i64) -> Result<()> {
        let buffer =
            unsafe { MFCreateDXGISurfaceBuffer(&ID3D11_TEXTURE2D_IID, &packet.texture, 0, false) }
                .map_err(|error| Error::Codec(format!("could not wrap D3D11 frame: {error}")))?;
        let two_dimensional: IMF2DBuffer = buffer.cast().map_err(|error| {
            Error::Codec(format!("frame buffer is not two-dimensional: {error}"))
        })?;
        let length = unsafe { two_dimensional.GetContiguousLength() }.map_err(|error| {
            Error::Codec(format!("could not determine frame buffer length: {error}"))
        })?;
        unsafe { buffer.SetCurrentLength(length) }
            .map_err(|error| Error::Codec(format!("could not size frame buffer: {error}")))?;
        let sample = unsafe { MFCreateSample() }
            .map_err(|error| Error::Codec(format!("could not allocate video sample: {error}")))?;
        let write = || -> windows::core::Result<()> {
            unsafe {
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(stream_hns)?;
                sample.SetSampleDuration(self.frame_duration_hns)?;
                self.writer.WriteSample(self.video_stream, &sample)?;
            }
            Ok(())
        };
        write().map_err(|error| Error::Codec(format!("could not write video sample: {error}")))?;
        self.samples_written = self.samples_written.saturating_add(1);
        Ok(())
    }

    /// Writes one aligned stereo PCM chunk into the AAC stream.
    pub fn write_audio(&mut self, chunk: MixedChunk) -> Result<()> {
        let Some(stream) = self.audio_stream else {
            return Ok(());
        };
        let length = u32::try_from(chunk.pcm.len())
            .map_err(|_| Error::Codec("audio packet exceeds Media Foundation limits".into()))?;
        let buffer = unsafe { MFCreateMemoryBuffer(length) }
            .map_err(|error| Error::Codec(format!("could not allocate audio buffer: {error}")))?;
        let mut destination = core::ptr::null_mut();
        unsafe { buffer.Lock(&raw mut destination, None, None) }
            .map_err(|error| Error::Codec(format!("could not lock audio buffer: {error}")))?;
        if destination.is_null() {
            let _ = unsafe { buffer.Unlock() };
            return Err(Error::Codec(
                "Media Foundation returned a null audio buffer".into(),
            ));
        }
        unsafe {
            core::ptr::copy_nonoverlapping(chunk.pcm.as_ptr(), destination, chunk.pcm.len());
        }
        unsafe { buffer.Unlock() }
            .map_err(|error| Error::Codec(format!("could not unlock audio buffer: {error}")))?;
        unsafe { buffer.SetCurrentLength(length) }
            .map_err(|error| Error::Codec(format!("could not size audio buffer: {error}")))?;

        let sample = unsafe { MFCreateSample() }
            .map_err(|error| Error::Codec(format!("could not allocate audio sample: {error}")))?;
        let write = || -> windows::core::Result<()> {
            unsafe {
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(chunk.time_hns)?;
                sample.SetSampleDuration(chunk.duration_hns)?;
                self.writer.WriteSample(stream, &sample)?;
            }
            Ok(())
        };
        write().map_err(|error| Error::Codec(format!("could not write audio sample: {error}")))?;
        self.samples_written = self.samples_written.saturating_add(1);
        Ok(())
    }

    /// Finalises every fragmented stream.
    pub fn finalize(&self) -> windows::core::Result<()> {
        let finalized = unsafe { self.writer.Finalize() };
        let flushed = unsafe { self.byte_stream.Flush() };
        let closed = unsafe { self.byte_stream.Close() };
        finalized.and(flushed).and(closed)
    }

    /// Releases the writer and closes an output that never reached startup.
    pub fn discard(self) {
        let Self {
            writer,
            byte_stream,
            ..
        } = self;
        drop(writer);
        let _ = unsafe { byte_stream.Close() };
    }

    /// Number of video and audio samples accepted by the sink writer.
    #[must_use]
    pub const fn samples_written(&self) -> u64 {
        self.samples_written
    }
}

struct OutputCleanup<'a> {
    path: &'a Path,
    stream: &'a IMFByteStream,
    keep: bool,
}

impl<'a> OutputCleanup<'a> {
    const fn new(path: &'a Path, stream: &'a IMFByteStream) -> Self {
        Self {
            path,
            stream,
            keep: false,
        }
    }

    const fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for OutputCleanup<'_> {
    fn drop(&mut self) {
        if !self.keep {
            let _ = unsafe { self.stream.Close() };
            let _ = std::fs::remove_file(self.path);
        }
    }
}

fn verify_hardware_transform(writer: &IMFSinkWriter, stream: u32) -> Result<()> {
    let extended: IMFSinkWriterEx = writer
        .cast()
        .map_err(|error| Error::Codec(format!("sink writer cannot expose transforms: {error}")))?;
    for index in 0..32 {
        let mut category = windows::core::GUID::zeroed();
        let mut transform = None;
        let result = unsafe {
            extended.GetTransformForStream(
                stream,
                index,
                Some(&raw mut category),
                &raw mut transform,
            )
        };
        match result {
            Err(error) if error.code() == MF_E_INVALIDINDEX => break,
            Err(error) => {
                return Err(Error::Codec(format!(
                    "could not inspect video transform {index}: {error}"
                )));
            }
            Ok(()) if category != MFT_CATEGORY_VIDEO_ENCODER => continue,
            Ok(()) => {
                let transform = transform.ok_or_else(|| {
                    Error::Codec("sink writer returned no H.264 transform".into())
                })?;
                let attributes = unsafe { transform.GetAttributes() }.map_err(|error| {
                    Error::Codec(format!("could not inspect H.264 transform: {error}"))
                })?;
                let hardware_url =
                    unsafe { attributes.GetStringLength(&MFT_ENUM_HARDWARE_URL_Attribute) };
                return match hardware_url {
                    Ok(length) if length != 0 => Ok(()),
                    _ => Err(Error::Unsupported {
                        what: "H.264 screen recording".into(),
                        why: "Media Foundation selected a software H.264 encoder even though Scrozz requires hardware encoding"
                            .into(),
                    }),
                };
            }
        }
    }
    Err(Error::Codec(
        "sink writer exposed no H.264 encoder transform".into(),
    ))
}

// Avoid importing the entire D3D11 module just for one interface IID in the
// already-large Media Foundation import list.
const ID3D11_TEXTURE2D_IID: windows::core::GUID =
    <windows::Win32::Graphics::Direct3D11::ID3D11Texture2D as Interface>::IID;

fn require_hardware_h264() -> Result<()> {
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut found: *mut Option<IMFActivate> = core::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&raw const output),
            &raw mut found,
            &raw mut count,
        )
    }
    .map_err(|error| Error::Codec(format!("hardware H.264 probe failed: {error}")))?;

    if !found.is_null() {
        let activations = unsafe { core::slice::from_raw_parts_mut(found, count as usize) };
        for activation in activations {
            drop(activation.take());
        }
        unsafe { CoTaskMemFree(Some(found.cast::<c_void>())) };
    }

    if count == 0 {
        Err(Error::Unsupported {
            what: "H.264 screen recording".into(),
            why: "Media Foundation reported no hardware H.264 encoder".into(),
        })
    } else {
        Ok(())
    }
}

fn dxgi_manager(device: &Device) -> Result<IMFDXGIDeviceManager> {
    let mut token = 0;
    let mut manager = None;
    unsafe { MFCreateDXGIDeviceManager(&raw mut token, &raw mut manager) }
        .map_err(|error| Error::Platform(format!("MFCreateDXGIDeviceManager failed: {error}")))?;
    let manager = manager
        .ok_or_else(|| Error::Platform("Media Foundation returned no DXGI manager".into()))?;
    unsafe { manager.ResetDevice(device.native(), token) }
        .map_err(|error| Error::Platform(format!("DXGI manager rejected D3D11 device: {error}")))?;
    Ok(manager)
}

fn attributes(capacity: u32) -> Result<IMFAttributes> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&raw mut attributes, capacity) }
        .map_err(|error| Error::Platform(format!("MFCreateAttributes failed: {error}")))?;
    attributes.ok_or_else(|| Error::Platform("Media Foundation returned no attributes".into()))
}

fn video_output_type(plan: EncoderPlan) -> windows::core::Result<IMFMediaType> {
    let media = unsafe { MFCreateMediaType()? };
    let configure = || -> windows::core::Result<()> {
        unsafe {
            media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            media.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            media.SetUINT32(&MF_MT_AVG_BITRATE, plan.bitrate)?;
            media.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0.cast_unsigned(),
            )?;
            media.SetUINT64(
                &MF_MT_FRAME_SIZE,
                pack_pair(plan.output_width, plan.output_height),
            )?;
            media.SetUINT64(&MF_MT_FRAME_RATE, pack_pair(plan.fps, 1))?;
            media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_pair(1, 1))?;
            media.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, plan.gop)?;
            media.SetUINT32(
                &MF_MT_MPEG2_PROFILE,
                eAVEncH264VProfile_High.0.cast_unsigned(),
            )?;
        }
        Ok(())
    };
    configure()?;
    Ok(media)
}

fn video_input_type(plan: EncoderPlan) -> windows::core::Result<IMFMediaType> {
    let media = unsafe { MFCreateMediaType()? };
    let configure = || -> windows::core::Result<()> {
        unsafe {
            media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            media.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
            media.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0.cast_unsigned(),
            )?;
            media.SetUINT64(
                &MF_MT_FRAME_SIZE,
                pack_pair(plan.output_width, plan.output_height),
            )?;
            media.SetUINT64(&MF_MT_FRAME_RATE, pack_pair(plan.fps, 1))?;
            media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_pair(1, 1))?;
        }
        Ok(())
    };
    configure()?;
    Ok(media)
}

fn audio_output_type() -> windows::core::Result<IMFMediaType> {
    let media = unsafe { MFCreateMediaType()? };
    let configure = || -> windows::core::Result<()> {
        unsafe {
            media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            media.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            media.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
            media.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
            media.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_BITRATE / 8)?;
            media.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, AUDIO_BITS)?;
            media.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 1)?;
            media.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            media.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
        }
        Ok(())
    };
    configure()?;
    Ok(media)
}

fn audio_input_type() -> windows::core::Result<IMFMediaType> {
    let media = unsafe { MFCreateMediaType()? };
    let configure = || -> windows::core::Result<()> {
        unsafe {
            media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            media.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            media.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
            media.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
            media.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, AUDIO_BITS)?;
            media.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, AUDIO_BLOCK_ALIGN)?;
            media.SetUINT32(
                &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
                AUDIO_SAMPLE_RATE * AUDIO_BLOCK_ALIGN,
            )?;
        }
        Ok(())
    };
    configure()?;
    Ok(media)
}

/// `MFSetAttributeSize` and `MFSetAttributeRatio` are C macros, not exports.
const fn pack_pair(high: u32, low: u32) -> u64 {
    (high as u64) << 32 | low as u64
}
