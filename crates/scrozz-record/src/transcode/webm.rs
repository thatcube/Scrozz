//! Bounded software AV1 output in a seekable WebM container.

use std::{
    fs::{File, OpenOptions},
    io::{BufReader, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;

use oxideav_core::{Demuxer as _, Error as OxideError, NullCodecResolver};
use oxideav_mkv::demux;
use scrozz_core::{Error, Result};
use scrozz_export::RgbaImage;

use crate::{
    Quality,
    config::{Dimensions, EncoderQuality},
    encoder::{self, VideoEncoder, VideoEncoderSettings},
    format::{PackedFrame, PackedPixelFormat, to_nv12},
};

use super::{BoundedWriter, WebmInspection};

pub(super) const TRANSCODER_NAME: &str = "rav1e + Scrozz WebM";

const TIMECODE_SCALE_NS: u64 = 1_000_000;
const VIDEO_TRACK: u64 = 1;

const EBML_HEADER: u32 = 0x1A45_DFA3;
const EBML_VERSION: u32 = 0x4286;
const EBML_READ_VERSION: u32 = 0x42F7;
const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
const DOC_TYPE: u32 = 0x4282;
const DOC_TYPE_VERSION: u32 = 0x4287;
const DOC_TYPE_READ_VERSION: u32 = 0x4285;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TIMECODE_SCALE: u32 = 0x002A_D7B1;
const DURATION: u32 = 0x4489;
const MUXING_APP: u32 = 0x4D80;
const WRITING_APP: u32 = 0x5741;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_UID: u32 = 0x73C5;
const TRACK_TYPE: u32 = 0x83;
const FLAG_LACING: u32 = 0x9C;
const DEFAULT_DURATION: u32 = 0x0023_E383;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const CLUSTER: u32 = 0x1F43_B675;
const CLUSTER_TIMECODE: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;

#[derive(Debug, Clone, Copy)]
struct Cue {
    timecode: u64,
    cluster_position: u64,
}

pub(super) struct Av1WebmWriter {
    path: PathBuf,
    writer: BoundedWriter<File>,
    encoder: Box<dyn VideoEncoder>,
    frame_rate: u16,
    duration_offset: u64,
    segment_data_start: u64,
    submitted_frames: u64,
    written_packets: u64,
    last_timestamp_ns: Option<u64>,
    cues: Vec<Cue>,
}

impl Av1WebmWriter {
    pub(super) fn new(
        path: &Path,
        dimensions: (u32, u32),
        frame_rate: u16,
        quality: Quality,
        byte_limit: u64,
    ) -> Result<Self> {
        if frame_rate == 0 {
            return Err(Error::InvalidRequest(
                "WebM frame rate must be positive".to_owned(),
            ));
        }
        let width = u16::try_from(dimensions.0).map_err(|_| Error::Unsupported {
            what: format!("{}x{} WebM export", dimensions.0, dimensions.1),
            why: "the WebM track dimensions exceed 65535 pixels".to_owned(),
        })?;
        let height = u16::try_from(dimensions.1).map_err(|_| Error::Unsupported {
            what: format!("{}x{} WebM export", dimensions.0, dimensions.1),
            why: "the WebM track dimensions exceed 65535 pixels".to_owned(),
        })?;
        let settings = VideoEncoderSettings {
            dimensions: Dimensions {
                width: u32::from(width),
                height: u32::from(height),
            },
            fps: u32::from(frame_rate),
            quality: EncoderQuality::from(quality),
        };
        let encoder = encoder::open_software_av1(settings)?;
        let decoder_configuration = match encoder.decoder_configuration() {
            crate::muxer::VideoCodecConfiguration::Av1(configuration) => configuration,
            crate::muxer::VideoCodecConfiguration::Avc(_) => {
                return Err(Error::Codec(
                    "software WebM encoder returned H.264 configuration".to_owned(),
                ));
            }
        };
        if decoder_configuration.len() < 4 {
            return Err(Error::Codec(
                "rav1e returned an incomplete AV1 codec configuration".to_owned(),
            ));
        }

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut writer = BoundedWriter::new(options.open(path)?, byte_limit);
        let (duration_offset, segment_data_start) = write_header(
            &mut writer,
            u32::from(width),
            u32::from(height),
            frame_rate,
            &decoder_configuration,
        )?;
        writer.flush()?;

        Ok(Self {
            path: path.to_owned(),
            writer,
            encoder,
            frame_rate,
            duration_offset,
            segment_data_start,
            submitted_frames: 0,
            written_packets: 0,
            last_timestamp_ns: None,
            cues: Vec::new(),
        })
    }

    pub(super) fn append_frame(&mut self, image: &RgbaImage) -> Result<()> {
        let packed = PackedFrame {
            width: image.width,
            height: image.height,
            stride: usize::try_from(image.width)
                .ok()
                .and_then(|width| width.checked_mul(4))
                .ok_or_else(|| Error::Codec("WebM frame row size overflowed".to_owned()))?,
            format: PackedPixelFormat::Rgba,
            data: image.data.clone(),
        };
        let frame = to_nv12(
            &packed,
            Dimensions {
                width: image.width,
                height: image.height,
            },
        )?;
        for packet in self.encoder.encode(&frame)? {
            self.write_packet(packet)?;
        }
        self.submitted_frames = self.submitted_frames.saturating_add(1);
        Ok(())
    }

    pub(super) const fn frames(&self) -> u64 {
        self.submitted_frames
    }

    pub(super) fn media_end(&self) -> Duration {
        let nanos = u128::from(self.submitted_frames).saturating_mul(1_000_000_000)
            / u128::from(self.frame_rate);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    pub(super) fn finish(mut self, duration: Duration) -> Result<u64> {
        for packet in self.encoder.finish()? {
            self.write_packet(packet)?;
        }
        if self.submitted_frames == 0 || self.written_packets == 0 {
            return Err(Error::Codec(
                "software AV1 export ended before any video frame was encoded".to_owned(),
            ));
        }
        write_cues(&mut self.writer, &self.cues)?;
        let end = self.writer.stream_position()?;
        self.writer.seek(SeekFrom::Start(self.duration_offset))?;
        self.writer
            .write_all(&(duration.as_secs_f64() * 1_000.0).to_be_bytes())?;
        self.writer.seek(SeekFrom::Start(end))?;
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(self.writer.get_ref().metadata()?.len())
    }

    fn write_packet(&mut self, encoded: encoder::EncodedVideoPacket) -> Result<()> {
        let timestamp_ns = u128::from(encoded.frame_index).saturating_mul(1_000_000_000)
            / u128::from(self.frame_rate);
        let timestamp_ns = u64::try_from(timestamp_ns)
            .map_err(|_| Error::Codec("AV1 frame timestamp exceeded u64".to_owned()))?;
        if self
            .last_timestamp_ns
            .is_some_and(|previous| timestamp_ns < previous)
        {
            return Err(Error::Codec(
                "rav1e returned non-monotonic presentation timestamps".to_owned(),
            ));
        }
        let timecode = timestamp_ns / TIMECODE_SCALE_NS;
        let cluster_position = self
            .writer
            .stream_position()?
            .saturating_sub(self.segment_data_start);
        write_cluster(&mut self.writer, timecode, encoded.keyframe, &encoded.data)?;
        self.writer.flush()?;
        if encoded.keyframe {
            self.cues.push(Cue {
                timecode,
                cluster_position,
            });
        }
        self.last_timestamp_ns = Some(timestamp_ns);
        self.written_packets = self.written_packets.saturating_add(1);
        Ok(())
    }
}

pub(super) fn inspect_file(path: &Path) -> Result<WebmInspection> {
    let input = File::open(path)?;
    let mut demuxer = demux::open_typed(Box::new(BufReader::new(input)), &NullCodecResolver)
        .map_err(inspect_error)?;
    let video_index = demuxer
        .streams()
        .iter()
        .position(|stream| stream.params.codec_id.as_str() == "av1")
        .ok_or_else(|| Error::Codec("WebM contains no AV1 video track".to_owned()))?;
    let stream = demuxer.streams()[video_index].clone();
    let width = stream
        .params
        .width
        .ok_or_else(|| Error::Codec("WebM AV1 track has no pixel width".to_owned()))?;
    let height = stream
        .params
        .height
        .ok_or_else(|| Error::Codec("WebM AV1 track has no pixel height".to_owned()))?;
    let default_duration = demuxer
        .track_timing(u32::try_from(video_index).unwrap_or(u32::MAX))
        .and_then(|timing| timing.default_duration())
        .map(Duration::from_nanos)
        .unwrap_or(Duration::ZERO);
    let declared_duration = stream
        .duration
        .map(|ticks| stream.time_base.seconds_of(ticks))
        .and_then(|seconds| Duration::try_from_secs_f64(seconds.max(0.0)).ok())
        .unwrap_or_default();
    let mut frames = 0_u64;
    let mut end = Duration::ZERO;
    loop {
        match demuxer.next_packet() {
            Ok(packet) if packet.stream_index as usize == video_index => {
                let pts = packet
                    .pts
                    .map(|value| packet.time_base.seconds_of(value))
                    .unwrap_or_default()
                    .max(0.0);
                let duration = packet.duration.map_or(default_duration, |value| {
                    Duration::try_from_secs_f64(packet.time_base.seconds_of(value).max(0.0))
                        .unwrap_or_default()
                });
                end = end.max(
                    Duration::try_from_secs_f64(pts)
                        .unwrap_or_default()
                        .saturating_add(duration),
                );
                frames = frames.saturating_add(1);
            }
            Ok(_) => {}
            Err(OxideError::Eof) => break,
            Err(error) => return Err(inspect_error(error)),
        }
    }
    if frames == 0 {
        return Err(Error::Codec(
            "WebM contains no decodable AV1 packet".to_owned(),
        ));
    }
    let keyframe_cues = demuxer
        .cue_points()
        .iter()
        .flat_map(|cue| &cue.track_positions)
        .filter(|position| position.track == VIDEO_TRACK)
        .count();
    Ok(WebmInspection {
        dimensions: (width, height),
        frames,
        duration: end.max(declared_duration),
        keyframe_cues: u64::try_from(keyframe_cues).unwrap_or(u64::MAX),
    })
}

fn write_header(
    writer: &mut BoundedWriter<File>,
    width: u32,
    height: u32,
    frame_rate: u16,
    codec_private: &[u8],
) -> Result<(u64, u64)> {
    let mut ebml = Vec::new();
    write_uint(&mut ebml, EBML_VERSION, 1)?;
    write_uint(&mut ebml, EBML_READ_VERSION, 1)?;
    write_uint(&mut ebml, EBML_MAX_ID_LENGTH, 4)?;
    write_uint(&mut ebml, EBML_MAX_SIZE_LENGTH, 8)?;
    write_string(&mut ebml, DOC_TYPE, "webm")?;
    write_uint(&mut ebml, DOC_TYPE_VERSION, 4)?;
    write_uint(&mut ebml, DOC_TYPE_READ_VERSION, 2)?;
    write_master(writer, EBML_HEADER, &ebml)?;

    write_id(writer, SEGMENT)?;
    writer.write_all(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff])?;
    let segment_data_start = writer.stream_position()?;

    let mut info = Vec::new();
    write_uint(&mut info, TIMECODE_SCALE, TIMECODE_SCALE_NS)?;
    write_string(&mut info, MUXING_APP, "Scrozz")?;
    write_string(&mut info, WRITING_APP, "Scrozz")?;
    write_id(&mut info, DURATION)?;
    write_size(&mut info, 8)?;
    let duration_in_info = u64::try_from(info.len())
        .map_err(|_| Error::Codec("WebM Info size exceeded u64".to_owned()))?;
    info.extend_from_slice(&0_f64.to_be_bytes());
    let info_start = writer.stream_position()?;
    write_id(writer, INFO)?;
    let info_size_len = write_size(writer, info.len())?;
    writer.write_all(&info)?;
    let duration_offset = info_start
        .saturating_add(u64::try_from(id_width(INFO)).unwrap_or(u64::MAX))
        .saturating_add(u64::try_from(info_size_len).unwrap_or(u64::MAX))
        .saturating_add(duration_in_info);

    let mut video = Vec::new();
    write_uint(&mut video, PIXEL_WIDTH, u64::from(width))?;
    write_uint(&mut video, PIXEL_HEIGHT, u64::from(height))?;
    let mut track = Vec::new();
    write_uint(&mut track, TRACK_NUMBER, VIDEO_TRACK)?;
    write_uint(&mut track, TRACK_UID, VIDEO_TRACK)?;
    write_uint(&mut track, TRACK_TYPE, 1)?;
    write_uint(&mut track, FLAG_LACING, 0)?;
    write_uint(
        &mut track,
        DEFAULT_DURATION,
        (1_000_000_000_u64 + u64::from(frame_rate) / 2) / u64::from(frame_rate),
    )?;
    write_string(&mut track, CODEC_ID, "V_AV1")?;
    write_binary(&mut track, CODEC_PRIVATE, codec_private)?;
    write_master(&mut track, VIDEO, &video)?;
    let mut tracks = Vec::new();
    write_master(&mut tracks, TRACK_ENTRY, &track)?;
    write_master(writer, TRACKS, &tracks)?;
    Ok((duration_offset, segment_data_start))
}

fn write_cluster(
    writer: &mut BoundedWriter<File>,
    timecode: u64,
    keyframe: bool,
    payload: &[u8],
) -> Result<()> {
    let mut timecode_element = Vec::new();
    write_uint(&mut timecode_element, CLUSTER_TIMECODE, timecode)?;
    let block_size = payload
        .len()
        .checked_add(4)
        .ok_or_else(|| Error::Codec("WebM block size overflowed".to_owned()))?;
    let block_header = element_header(SIMPLE_BLOCK, block_size)?;
    let cluster_size = timecode_element
        .len()
        .checked_add(block_header.len())
        .and_then(|size| size.checked_add(block_size))
        .ok_or_else(|| Error::Codec("WebM cluster size overflowed".to_owned()))?;
    write_id(writer, CLUSTER)?;
    write_size(writer, cluster_size)?;
    writer.write_all(&timecode_element)?;
    writer.write_all(&block_header)?;
    writer.write_all(&[0x81, 0, 0, if keyframe { 0x80 } else { 0 }])?;
    writer.write_all(payload)?;
    Ok(())
}

fn write_cues(writer: &mut BoundedWriter<File>, cues: &[Cue]) -> Result<()> {
    if cues.is_empty() {
        return Ok(());
    }
    let mut body = Vec::new();
    for cue in cues {
        let mut position = Vec::new();
        write_uint(&mut position, CUE_TRACK, VIDEO_TRACK)?;
        write_uint(&mut position, CUE_CLUSTER_POSITION, cue.cluster_position)?;
        let mut point = Vec::new();
        write_uint(&mut point, CUE_TIME, cue.timecode)?;
        write_master(&mut point, CUE_TRACK_POSITIONS, &position)?;
        write_master(&mut body, CUE_POINT, &point)?;
    }
    write_master(writer, CUES, &body)
}

fn write_master(writer: &mut impl std::io::Write, id: u32, body: &[u8]) -> Result<()> {
    write_id(writer, id)?;
    write_size(writer, body.len())?;
    writer.write_all(body)?;
    Ok(())
}

fn write_uint(writer: &mut impl std::io::Write, id: u32, value: u64) -> Result<()> {
    let bytes = value.to_be_bytes();
    let first = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len() - 1);
    write_binary(writer, id, &bytes[first..])
}

fn write_string(writer: &mut impl std::io::Write, id: u32, value: &str) -> Result<()> {
    write_binary(writer, id, value.as_bytes())
}

fn write_binary(writer: &mut impl std::io::Write, id: u32, value: &[u8]) -> Result<()> {
    write_id(writer, id)?;
    write_size(writer, value.len())?;
    writer.write_all(value)?;
    Ok(())
}

fn write_id(writer: &mut impl std::io::Write, id: u32) -> Result<()> {
    let bytes = id.to_be_bytes();
    writer.write_all(&bytes[bytes.len() - id_width(id)..])?;
    Ok(())
}

fn id_width(id: u32) -> usize {
    (usize::try_from(u32::BITS - id.leading_zeros())
        .unwrap_or(4)
        .saturating_add(7)
        / 8)
    .max(1)
}

fn write_size(writer: &mut impl std::io::Write, size: usize) -> Result<usize> {
    let size = u64::try_from(size)
        .map_err(|_| Error::Codec("WebM element size exceeded u64".to_owned()))?;
    for width in 1..=8 {
        let value_bits = width * 7;
        let maximum = (1_u128 << value_bits) - 2;
        if u128::from(size) <= maximum {
            let encoded = size | (1_u64 << value_bits);
            let bytes = encoded.to_be_bytes();
            writer.write_all(&bytes[8 - width..])?;
            return Ok(width);
        }
    }
    Err(Error::Codec(
        "WebM element exceeds the EBML size limit".to_owned(),
    ))
}

fn element_header(id: u32, size: usize) -> Result<Vec<u8>> {
    let mut header = Vec::with_capacity(12);
    write_id(&mut header, id)?;
    write_size(&mut header, size)?;
    Ok(header)
}

fn inspect_error(error: OxideError) -> Error {
    Error::Codec(format!("WebM container error: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "scrozz-{label}-{}-{nonce}-{}.webm",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn frame(index: u8) -> RgbaImage {
        let mut data = Vec::with_capacity(64 * 48 * 4);
        for pixel in 0..64 * 48 {
            let value = (pixel as u8).wrapping_add(index.wrapping_mul(17));
            data.extend_from_slice(&[value, value.wrapping_add(53), 180, 255]);
        }
        RgbaImage {
            width: 64,
            height: 48,
            data,
        }
    }

    #[test]
    fn tiny_av1_webm_round_trips_frames_timing_and_dimensions() {
        let path = path("webm-round-trip");
        let mut writer =
            Av1WebmWriter::new(&path, (64, 48), 10, Quality::Low, 8 * 1024 * 1024).unwrap();
        for index in 0..6 {
            writer.append_frame(&frame(index)).unwrap();
        }
        let bytes = writer.finish(Duration::from_millis(600)).unwrap();
        let inspection = inspect_file(&path).unwrap();
        assert_eq!(inspection.dimensions, (64, 48));
        assert_eq!(inspection.frames, 6);
        assert_eq!(inspection.duration, Duration::from_millis(600));
        assert!(inspection.keyframe_cues >= 1);
        assert_eq!(bytes, std::fs::metadata(&path).unwrap().len());
        assert!(bytes < 256 * 1024);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_webm_is_rejected() {
        let path = path("webm-corrupt");
        std::fs::write(&path, b"not webm").unwrap();
        assert!(inspect_file(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn partial_finalization_declares_only_submitted_media_time() {
        let path = path("webm-partial-duration");
        let mut writer =
            Av1WebmWriter::new(&path, (64, 48), 10, Quality::Low, 8 * 1024 * 1024).unwrap();
        for index in 0..3 {
            writer.append_frame(&frame(index)).unwrap();
        }
        let media_end = writer.media_end();
        assert_eq!(media_end, Duration::from_millis(300));
        writer.finish(media_end).unwrap();
        assert_eq!(inspect_file(&path).unwrap().duration, media_end);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn output_limit_fails_closed() {
        let path = path("webm-limit");
        let result = (|| {
            let mut writer = Av1WebmWriter::new(&path, (64, 48), 10, Quality::Low, 256)?;
            writer.append_frame(&frame(0))?;
            writer.finish(Duration::from_millis(100))
        })();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("staged-output limit")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ffprobe_accepts_the_av1_webm_when_available() {
        if std::process::Command::new("ffprobe")
            .arg("-version")
            .output()
            .is_err()
        {
            return;
        }
        let path = path("webm-ffprobe");
        let mut writer =
            Av1WebmWriter::new(&path, (64, 48), 10, Quality::Low, 8 * 1024 * 1024).unwrap();
        for index in 0..6 {
            writer.append_frame(&frame(index)).unwrap();
        }
        writer.finish(Duration::from_millis(600)).unwrap();
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name,width,height,r_frame_rate:format=format_name,duration",
                "-of",
                "json",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = String::from_utf8(output.stdout).unwrap();
        assert!(report.contains("\"codec_name\": \"av1\""), "{report}");
        assert!(report.contains("\"width\": 64"), "{report}");
        assert!(report.contains("\"height\": 48"), "{report}");
        assert!(report.contains("matroska,webm"), "{report}");
        assert!(report.contains("\"duration\": \"0.600000\""), "{report}");
        std::fs::remove_file(path).unwrap();
    }
}
