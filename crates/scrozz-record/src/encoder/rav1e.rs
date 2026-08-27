//! Optional pure-Rust AV1 fallback.

use std::sync::Arc;

use rav1e::prelude::{
    ChromaSampling, Config, Context, EncoderConfig, EncoderStatus, FrameType, PixelRange, Rational,
};
use scrozz_core::{Error, Result};

use super::{EncodedVideoPacket, VideoEncoder, VideoEncoderSettings};
use crate::format::Nv12Frame;
use crate::muxer::VideoCodecConfiguration;

pub(super) struct Rav1eEncoder {
    context: Context<u8>,
    decoder_configuration: Vec<u8>,
    width: u32,
    height: u32,
}

impl Rav1eEncoder {
    pub(super) fn new(settings: VideoEncoderSettings) -> Result<Self> {
        let mut encoder = EncoderConfig::with_speed_preset(8);
        encoder.width = settings.dimensions.width as usize;
        encoder.height = settings.dimensions.height as usize;
        encoder.time_base = Rational {
            num: 1,
            den: u64::from(settings.fps),
        };
        encoder.bit_depth = 8;
        encoder.chroma_sampling = ChromaSampling::Cs420;
        encoder.pixel_range = PixelRange::Limited;
        encoder.low_latency = true;
        encoder.min_key_frame_interval = u64::from(settings.fps);
        encoder.max_key_frame_interval = u64::from(settings.fps).saturating_mul(2);
        encoder.quantizer = usize::from(settings.quality.rav1e_quantizer());

        let config = Config::new().with_encoder_config(encoder);
        let context: Context<u8> = config
            .new_context()
            .map_err(|err| Error::Platform(format!("could not configure rav1e: {err}")))?;
        let decoder_configuration = context.container_sequence_header();
        Ok(Self {
            context,
            decoder_configuration,
            width: settings.dimensions.width,
            height: settings.dimensions.height,
        })
    }

    fn drain(&mut self, flushing: bool) -> Result<Vec<EncodedVideoPacket>> {
        let mut packets = Vec::new();
        loop {
            match self.context.receive_packet() {
                Ok(packet) => packets.push(EncodedVideoPacket {
                    frame_index: packet.input_frameno,
                    data: packet.data,
                    keyframe: matches!(
                        packet.frame_type,
                        FrameType::KEY | FrameType::INTRA_ONLY | FrameType::SWITCH
                    ),
                }),
                Err(EncoderStatus::Encoded) => {}
                Err(EncoderStatus::NeedMoreData) if !flushing => break,
                Err(EncoderStatus::LimitReached) if flushing => break,
                Err(status) => {
                    return Err(Error::Platform(format!(
                        "rav1e stopped while encoding: {status}"
                    )));
                }
            }
        }
        Ok(packets)
    }
}

impl VideoEncoder for Rav1eEncoder {
    fn codec(&self) -> crate::VideoCodec {
        crate::VideoCodec::Av1
    }

    fn decoder_configuration(&self) -> VideoCodecConfiguration {
        VideoCodecConfiguration::Av1(self.decoder_configuration.clone())
    }

    fn encode(&mut self, frame: &Nv12Frame) -> Result<Vec<EncodedVideoPacket>> {
        let expected_width = self.width;
        let expected_height = self.height;
        if frame.width != expected_width || frame.height != expected_height {
            return Err(Error::InvalidRequest(format!(
                "rav1e expected {expected_width}x{expected_height}, got {}x{}",
                frame.width, frame.height
            )));
        }

        let mut input = self.context.new_frame();
        input.planes[0].copy_from_raw_u8(&frame.y, frame.width as usize, 1);
        let chroma_pixels = frame.uv.len() / 2;
        let mut u = Vec::with_capacity(chroma_pixels);
        let mut v = Vec::with_capacity(chroma_pixels);
        for pair in frame.uv.as_chunks::<2>().0 {
            u.push(pair[0]);
            v.push(pair[1]);
        }
        input.planes[1].copy_from_raw_u8(&u, frame.width as usize / 2, 1);
        input.planes[2].copy_from_raw_u8(&v, frame.width as usize / 2, 1);
        self.context
            .send_frame(Arc::new(input))
            .map_err(|status| Error::Platform(format!("rav1e rejected a frame: {status}")))?;
        self.drain(false)
    }

    fn finish(&mut self) -> Result<Vec<EncodedVideoPacket>> {
        self.context.flush();
        self.drain(true)
    }
}
