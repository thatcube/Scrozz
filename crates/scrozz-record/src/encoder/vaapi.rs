//! FFmpeg VA-API H.264 encoder.
//!
//! The encoder is selected by the exact `h264_vaapi` name. There is no generic
//! H.264 lookup and therefore no path by which an installed x264 can be used.

use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr::{self, NonNull};

use ffmpeg_sys_next as ffi;
use scrozz_core::{Error, Result};

use super::{EncodedVideoPacket, VideoEncoder, VideoEncoderSettings};
use crate::format::Nv12Frame;
use crate::h264;
use crate::muxer::VideoCodecConfiguration;

pub(super) struct VaapiEncoder {
    codec_context: NonNull<ffi::AVCodecContext>,
    hardware_device: NonNull<ffi::AVBufferRef>,
    hardware_frames: NonNull<ffi::AVBufferRef>,
    decoder_configuration: Vec<u8>,
    width: u32,
    height: u32,
    next_frame: u64,
}

// FFmpeg codec and buffer contexts are not internally shared and this wrapper is
// moved once into the dedicated recording thread before it is used.
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    pub(super) fn new(settings: VideoEncoderSettings) -> Result<Self> {
        // SAFETY: Every allocation is checked before dereferencing. Ownership is
        // transferred into `Self` only after all FFmpeg initialisation succeeds;
        // the local cleanup helper releases partial allocations on every error.
        unsafe { Self::new_unsafe(settings) }
    }

    unsafe fn new_unsafe(settings: VideoEncoderSettings) -> Result<Self> {
        let name = CString::new("h264_vaapi").expect("static encoder name has no NUL");
        // SAFETY: `name` is a live NUL-terminated string.
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err(Error::Unsupported {
                what: "H.264 recording".into(),
                why: "FFmpeg does not expose the `h264_vaapi` hardware encoder; Scrozz \
                      deliberately never substitutes x264"
                    .into(),
            });
        }

        // SAFETY: `codec` points to FFmpeg-owned static storage.
        let raw_context = unsafe { ffi::avcodec_alloc_context3(codec) };
        let codec_context = NonNull::new(raw_context).ok_or_else(|| {
            Error::Platform("FFmpeg could not allocate an encoder context".into())
        })?;
        let mut hardware_device = ptr::null_mut();
        let mut hardware_frames = ptr::null_mut();

        let setup = (|| {
            // SAFETY: `codec_context` is uniquely owned until construction ends.
            let context = unsafe { codec_context.as_ptr().as_mut().unwrap() };
            context.width = c_int::try_from(settings.dimensions.width)
                .map_err(|_| Error::InvalidRequest("recording width exceeds FFmpeg".into()))?;
            context.height = c_int::try_from(settings.dimensions.height)
                .map_err(|_| Error::InvalidRequest("recording height exceeds FFmpeg".into()))?;
            context.time_base = ffi::AVRational {
                num: 1,
                den: c_int::try_from(settings.fps).unwrap_or(30),
            };
            context.framerate = ffi::AVRational {
                num: c_int::try_from(settings.fps).unwrap_or(30),
                den: 1,
            };
            context.pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            context.max_b_frames = 0;
            context.gop_size = c_int::try_from(settings.fps.saturating_mul(2)).unwrap_or(60);
            context.flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;

            // A null device string asks libavutil to choose the render node.
            // SAFETY: out-pointer is valid and FFmpeg owns the created reference.
            ffmpeg_result(unsafe {
                ffi::av_hwdevice_ctx_create(
                    &mut hardware_device,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    ptr::null(),
                    ptr::null_mut(),
                    0,
                )
            })
            .map_err(|error| Error::Unsupported {
                what: "VA-API H.264 recording".into(),
                why: format!(
                    "no usable VA-API render device could be opened ({error}); check \
                         /dev/dri/renderD* access and the installed Mesa/Intel VA driver"
                ),
            })?;

            // SAFETY: `hardware_device` is a valid reference after the call above.
            hardware_frames = unsafe { ffi::av_hwframe_ctx_alloc(hardware_device) };
            if hardware_frames.is_null() {
                return Err(Error::Platform(
                    "FFmpeg could not allocate a VA-API frame pool".into(),
                ));
            }
            // SAFETY: AVHWFramesContext is the documented data payload of the ref.
            let frames = unsafe {
                (*hardware_frames)
                    .data
                    .cast::<ffi::AVHWFramesContext>()
                    .as_mut()
                    .unwrap()
            };
            frames.format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            frames.sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            frames.width = context.width;
            frames.height = context.height;
            frames.initial_pool_size = 20;
            // SAFETY: all required AVHWFramesContext fields are populated.
            ffmpeg_result(unsafe { ffi::av_hwframe_ctx_init(hardware_frames) })
                .map_err(|error| Error::Platform(format!("VA-API frame pool failed: {error}")))?;
            // SAFETY: creates the independent reference owned by AVCodecContext.
            context.hw_frames_ctx = unsafe { ffi::av_buffer_ref(hardware_frames) };
            if context.hw_frames_ctx.is_null() {
                return Err(Error::Platform(
                    "FFmpeg could not reference the VA-API frame pool".into(),
                ));
            }

            let qp = CString::new(settings.quality.vaapi_qp().to_string())
                .expect("decimal QP has no NUL");
            let key = CString::new("qp").expect("static option has no NUL");
            // Drivers that do not expose constant-QP reject this explicitly
            // instead of silently changing the user's quality selection.
            // SAFETY: `priv_data` is created with the codec context and both
            // option strings remain alive for the duration of the call.
            ffmpeg_result(unsafe {
                ffi::av_opt_set(context.priv_data, key.as_ptr(), qp.as_ptr(), 0)
            })
            .map_err(|error| Error::Platform(format!("h264_vaapi rejected quality: {error}")))?;

            // SAFETY: context and codec are compatible and uniquely owned.
            ffmpeg_result(unsafe {
                ffi::avcodec_open2(codec_context.as_ptr(), codec, ptr::null_mut())
            })
            .map_err(|error| Error::Unsupported {
                what: "VA-API H.264 recording".into(),
                why: format!("the `h264_vaapi` encoder could not start ({error})"),
            })?;
            Ok::<(), Error>(())
        })();

        if let Err(error) = setup {
            // SAFETY: each pointer is either null or an owned FFmpeg allocation.
            unsafe {
                if !hardware_frames.is_null() {
                    ffi::av_buffer_unref(&mut hardware_frames);
                }
                if !hardware_device.is_null() {
                    ffi::av_buffer_unref(&mut hardware_device);
                }
                let mut context = codec_context.as_ptr();
                ffi::avcodec_free_context(&mut context);
            }
            return Err(error);
        }

        // SAFETY: avcodec_open2 populated immutable extradata owned by context.
        let context = unsafe { codec_context.as_ref() };
        if context.extradata.is_null() || context.extradata_size <= 0 {
            let mut context_ptr = codec_context.as_ptr();
            // SAFETY: all three pointers are live owned allocations.
            unsafe {
                ffi::av_buffer_unref(&mut hardware_frames);
                ffi::av_buffer_unref(&mut hardware_device);
                ffi::avcodec_free_context(&mut context_ptr);
            }
            return Err(Error::Platform(
                "h264_vaapi produced no global decoder configuration".into(),
            ));
        }
        // SAFETY: FFmpeg guarantees `extradata_size` readable bytes while the
        // codec context remains open.
        let extradata = unsafe {
            std::slice::from_raw_parts(
                context.extradata,
                usize::try_from(context.extradata_size).unwrap_or_default(),
            )
        };
        let decoder_configuration = match h264::decoder_configuration(extradata) {
            Ok(configuration) => configuration,
            Err(error) => {
                let mut context_ptr = codec_context.as_ptr();
                // SAFETY: all three pointers are live owned allocations.
                unsafe {
                    ffi::av_buffer_unref(&mut hardware_frames);
                    ffi::av_buffer_unref(&mut hardware_device);
                    ffi::avcodec_free_context(&mut context_ptr);
                }
                return Err(error);
            }
        };

        Ok(Self {
            codec_context,
            hardware_device: NonNull::new(hardware_device).expect("device checked after create"),
            hardware_frames: NonNull::new(hardware_frames).expect("frames checked after alloc"),
            decoder_configuration,
            width: settings.dimensions.width,
            height: settings.dimensions.height,
            next_frame: 0,
        })
    }

    fn receive_packets(&mut self, flushing: bool) -> Result<Vec<EncodedVideoPacket>> {
        let mut output = Vec::new();
        // SAFETY: allocation is checked and freed before every return.
        let mut packet = unsafe { ffi::av_packet_alloc() };
        if packet.is_null() {
            return Err(Error::Platform(
                "FFmpeg could not allocate an encoded packet".into(),
            ));
        }
        loop {
            // SAFETY: both pointers are valid for the encoder lifetime.
            let status =
                unsafe { ffi::avcodec_receive_packet(self.codec_context.as_ptr(), packet) };
            if status == 0 {
                // SAFETY: successful receive owns `size` readable data bytes.
                let packet_ref = unsafe { &*packet };
                let data = unsafe {
                    std::slice::from_raw_parts(
                        packet_ref.data,
                        usize::try_from(packet_ref.size).unwrap_or_default(),
                    )
                };
                let frame_index = if packet_ref.pts >= 0 {
                    packet_ref.pts as u64
                } else {
                    self.next_frame.saturating_sub(1)
                };
                let converted = match h264::to_length_prefixed(data) {
                    Ok(converted) => converted,
                    Err(error) => {
                        // SAFETY: packet is live.
                        unsafe { ffi::av_packet_free(&mut packet) };
                        return Err(error);
                    }
                };
                output.push(EncodedVideoPacket {
                    frame_index,
                    data: converted,
                    keyframe: packet_ref.flags & ffi::AV_PKT_FLAG_KEY as c_int != 0,
                });
                // SAFETY: releases payload but keeps the packet allocation.
                unsafe { ffi::av_packet_unref(packet) };
                continue;
            }
            if status == -11 || (flushing && status == ffi::AVERROR_EOF) {
                break;
            }
            // SAFETY: packet is live.
            unsafe { ffi::av_packet_free(&mut packet) };
            return Err(Error::Platform(format!(
                "h264_vaapi failed while receiving a packet: {}",
                ffmpeg_error(status)
            )));
        }
        // SAFETY: packet is live.
        unsafe { ffi::av_packet_free(&mut packet) };
        Ok(output)
    }
}

impl VideoEncoder for VaapiEncoder {
    fn codec(&self) -> crate::VideoCodec {
        crate::VideoCodec::H264
    }

    fn decoder_configuration(&self) -> VideoCodecConfiguration {
        VideoCodecConfiguration::Avc(self.decoder_configuration.clone())
    }

    fn encode(&mut self, frame: &Nv12Frame) -> Result<Vec<EncodedVideoPacket>> {
        if frame.width != self.width || frame.height != self.height {
            return Err(Error::InvalidRequest(format!(
                "h264_vaapi expected {}x{}, got {}x{}",
                self.width, self.height, frame.width, frame.height
            )));
        }
        let expected_y = self.width as usize * self.height as usize;
        let expected_uv = expected_y / 2;
        if frame.y.len() < expected_y || frame.uv.len() < expected_uv {
            return Err(Error::InvalidRequest(
                "NV12 frame storage is shorter than its dimensions".into(),
            ));
        }
        // SAFETY: the helper checks allocations and owns both frames until send
        // has retained the hardware frame.
        unsafe {
            let mut software = ffi::av_frame_alloc();
            let mut hardware = ffi::av_frame_alloc();
            if software.is_null() || hardware.is_null() {
                ffi::av_frame_free(&mut software);
                ffi::av_frame_free(&mut hardware);
                return Err(Error::Platform(
                    "FFmpeg could not allocate VA-API upload frames".into(),
                ));
            }
            (*software).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            (*software).width = self.width as c_int;
            (*software).height = self.height as c_int;
            let status = ffi::av_frame_get_buffer(software, 32);
            if status < 0 {
                ffi::av_frame_free(&mut software);
                ffi::av_frame_free(&mut hardware);
                return Err(Error::Platform(format!(
                    "FFmpeg could not allocate an NV12 upload frame: {}",
                    ffmpeg_error(status)
                )));
            }
            copy_plane(
                (*software).data[0],
                (*software).linesize[0],
                &frame.y,
                self.width as usize,
                self.height as usize,
            );
            copy_plane(
                (*software).data[1],
                (*software).linesize[1],
                &frame.uv,
                self.width as usize,
                self.height as usize / 2,
            );
            let status = ffi::av_hwframe_get_buffer(
                (*self.codec_context.as_ptr()).hw_frames_ctx,
                hardware,
                0,
            );
            if status >= 0 {
                (*hardware).pts = self.next_frame as i64;
            }
            let status = if status < 0 {
                status
            } else {
                ffi::av_hwframe_transfer_data(hardware, software, 0)
            };
            if status < 0 {
                ffi::av_frame_free(&mut software);
                ffi::av_frame_free(&mut hardware);
                return Err(Error::Platform(format!(
                    "VA-API frame upload failed: {}",
                    ffmpeg_error(status)
                )));
            }
            (*hardware).pts = self.next_frame as i64;
            self.next_frame = self.next_frame.saturating_add(1);
            let status = ffi::avcodec_send_frame(self.codec_context.as_ptr(), hardware);
            ffi::av_frame_free(&mut software);
            ffi::av_frame_free(&mut hardware);
            if status < 0 {
                return Err(Error::Platform(format!(
                    "h264_vaapi rejected a frame: {}",
                    ffmpeg_error(status)
                )));
            }
        }
        self.receive_packets(false)
    }

    fn finish(&mut self) -> Result<Vec<EncodedVideoPacket>> {
        // SAFETY: null frame is FFmpeg's documented flush signal.
        let status = unsafe { ffi::avcodec_send_frame(self.codec_context.as_ptr(), ptr::null()) };
        if status < 0 && status != ffi::AVERROR_EOF {
            return Err(Error::Platform(format!(
                "h264_vaapi flush failed: {}",
                ffmpeg_error(status)
            )));
        }
        self.receive_packets(true)
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        // SAFETY: each pointer is uniquely owned by this wrapper. The codec
        // context releases its own reference to the hardware frame pool.
        unsafe {
            let mut context = self.codec_context.as_ptr();
            ffi::avcodec_free_context(&mut context);
            let mut frames = self.hardware_frames.as_ptr();
            ffi::av_buffer_unref(&mut frames);
            let mut device = self.hardware_device.as_ptr();
            ffi::av_buffer_unref(&mut device);
        }
    }
}

unsafe fn copy_plane(
    destination: *mut u8,
    destination_stride: c_int,
    source: &[u8],
    row_bytes: usize,
    rows: usize,
) {
    let destination_stride = usize::try_from(destination_stride).unwrap_or_default();
    for row in 0..rows {
        // SAFETY: AVFrame allocated at least `rows * linesize`; source sizes were
        // validated by Nv12Frame construction.
        unsafe {
            ptr::copy_nonoverlapping(
                source.as_ptr().add(row * row_bytes),
                destination.add(row * destination_stride),
                row_bytes,
            );
        }
    }
}

fn ffmpeg_result(code: c_int) -> std::result::Result<(), String> {
    if code < 0 {
        Err(ffmpeg_error(code))
    } else {
        Ok(())
    }
}

fn ffmpeg_error(code: c_int) -> String {
    let mut buffer = [0 as c_char; ffi::AV_ERROR_MAX_STRING_SIZE];
    // SAFETY: buffer is writable and its exact length is supplied.
    let status = unsafe { ffi::av_strerror(code, buffer.as_mut_ptr(), buffer.len()) };
    if status < 0 {
        return format!("FFmpeg error {code}");
    }
    // SAFETY: av_strerror NUL-terminates on success.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}
