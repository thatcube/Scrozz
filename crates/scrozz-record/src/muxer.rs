//! Recoverable fragmented ISO-BMFF output.
//!
//! The initial `ftyp`/`moov` pair is written before capture begins. Each call to
//! [`FragmentedMp4::write_fragment`] then writes and flushes one complete
//! `moof`/`mdat` pair. There is no end-of-file index whose absence invalidates
//! everything before it, so every fully flushed fragment survives a crash.

use std::io::{self, Write};

use crate::Salvageability;

/// Recovery state for an arbitrary byte prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverySalvageability {
    /// No complete Scrozz container metadata was found.
    None,
    /// The initialisation segment is complete, but no media fragment is.
    InitialisationOnly,
    /// At least one complete media fragment is playable.
    Playable,
}

const VIDEO_TRACK_ID: u32 = 1;
const AUDIO_TRACK_ID: u32 = 2;

/// Codec configuration stored in the video sample entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoCodecConfiguration {
    /// AVCDecoderConfigurationRecord (`avcC` payload).
    Avc(Vec<u8>),
    /// AV1CodecConfigurationRecord (`av1C` payload).
    Av1(Vec<u8>),
}

/// Static video-track description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrackConfig {
    /// Encoded width.
    pub width: u16,
    /// Encoded height.
    pub height: u16,
    /// Media time units per second.
    pub timescale: u32,
    /// Decoder configuration supplied by the selected encoder.
    pub codec: VideoCodecConfiguration,
}

/// Static AAC track description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrackConfig {
    /// Audio sample rate and media timescale.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// MPEG-4 AudioSpecificConfig bytes supplied by the AAC encoder.
    pub audio_specific_config: Vec<u8>,
    /// Decoder preroll skipped from presentation at the start of the track.
    pub priming_frames: u32,
}

/// One encoded media sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedSample {
    /// Codec payload.
    pub data: Vec<u8>,
    /// Duration in this track's time units.
    pub duration: u32,
    /// Whether this sample can be decoded without an earlier sample.
    pub keyframe: bool,
}

/// Samples for one track in one fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackFragment {
    /// Decode timestamp of the first sample.
    pub base_decode_time: u64,
    /// Consecutive encoded samples.
    pub samples: Vec<EncodedSample>,
}

/// One independently flushable media fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFragment {
    /// Video is required for a screen recording.
    pub video: TrackFragment,
    /// Mixed AAC audio, when requested.
    pub audio: Option<TrackFragment>,
}

/// Incremental fragmented-MP4 writer.
#[derive(Debug)]
pub struct FragmentedMp4<W: Write> {
    writer: W,
    has_audio: bool,
    sequence_number: u32,
    fragments_written: u64,
}

impl<W: Write> FragmentedMp4<W> {
    /// Writes the complete initialisation segment.
    ///
    /// # Errors
    ///
    /// Returns any writer error, or [`io::ErrorKind::InvalidInput`] for an
    /// invalid track description.
    pub fn new(
        mut writer: W,
        video: &VideoTrackConfig,
        audio: Option<&AudioTrackConfig>,
    ) -> io::Result<Self> {
        validate_track_config(video, audio)?;
        writer.write_all(&file_type_box()?)?;
        writer.write_all(&movie_box(video, audio)?)?;
        writer.flush()?;
        Ok(Self {
            writer,
            has_audio: audio.is_some(),
            sequence_number: 1,
            fragments_written: 0,
        })
    }

    /// Writes and flushes one `moof`/`mdat` pair.
    ///
    /// # Errors
    ///
    /// Returns a writer error or [`io::ErrorKind::InvalidInput`] for empty
    /// tracks, zero durations, missing configured audio, or oversized boxes.
    pub fn write_fragment(&mut self, fragment: &MediaFragment) -> io::Result<()> {
        validate_fragment(fragment, self.has_audio)?;
        if self.fragments_written == 0
            && !fragment
                .video
                .samples
                .first()
                .is_some_and(|sample| sample.keyframe)
        {
            return Err(invalid_input(
                "the first media fragment must begin with a video keyframe",
            ));
        }

        let placeholder = movie_fragment_box(self.sequence_number, fragment, 0, 0)?;
        let video_offset = i32::try_from(placeholder.len().saturating_add(8))
            .map_err(|_| invalid_input("media fragment offset exceeds i32"))?;
        let video_bytes = fragment
            .video
            .samples
            .iter()
            .try_fold(0_usize, |total, sample| {
                total
                    .checked_add(sample.data.len())
                    .ok_or_else(|| invalid_input("video fragment size overflows memory"))
            })?;
        let audio_offset = i32::try_from(
            placeholder
                .len()
                .saturating_add(8)
                .saturating_add(video_bytes),
        )
        .map_err(|_| invalid_input("audio fragment offset exceeds i32"))?;
        let moof = movie_fragment_box(self.sequence_number, fragment, video_offset, audio_offset)?;

        let mut media = Vec::new();
        for sample in &fragment.video.samples {
            media.extend_from_slice(&sample.data);
        }
        if let Some(audio) = &fragment.audio {
            for sample in &audio.samples {
                media.extend_from_slice(&sample.data);
            }
        }
        let mdat = mp4_box(*b"mdat", media)?;

        self.writer.write_all(&moof)?;
        self.writer.write_all(&mdat)?;
        self.writer.flush()?;
        self.sequence_number = self.sequence_number.saturating_add(1);
        self.fragments_written = self.fragments_written.saturating_add(1);
        Ok(())
    }

    /// Recoverability represented by output flushed so far.
    #[must_use]
    pub const fn salvageability(&self) -> Salvageability {
        if self.fragments_written == 0 {
            Salvageability::InitialisationOnly
        } else {
            Salvageability::Playable
        }
    }

    /// Borrows the underlying writer, for durability operations such as
    /// [`std::fs::File::sync_data`].
    #[must_use]
    pub const fn writer(&self) -> &W {
        &self.writer
    }

    /// Flushes and returns the underlying writer.
    ///
    /// # Errors
    ///
    /// Returns a final writer flush error.
    pub fn finish(mut self) -> io::Result<W> {
        self.writer.flush()?;
        Ok(self.writer)
    }
}

/// Result of scanning a possibly truncated Scrozz MP4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Both `ftyp` and `moov` were complete.
    pub initialisation_complete: bool,
    /// Number of complete `moof`/`mdat` pairs.
    pub complete_fragments: u64,
    /// Prefix length that can be copied into a repair operation.
    pub valid_prefix_len: usize,
    /// User-facing recovery category.
    pub salvageability: RecoverySalvageability,
}

/// Finds the complete prefix of a possibly interrupted ISO-BMFF file.
///
/// The scan is intentionally narrow and allocation-free. It does not claim to
/// validate arbitrary MP4 semantics; it proves which top-level boxes and media
/// fragment pairs were written completely by this muxer.
#[must_use]
pub fn inspect_recovery(bytes: &[u8]) -> RecoveryReport {
    let mut cursor = 0_usize;
    let mut valid_prefix = 0_usize;
    let mut saw_ftyp = false;
    let mut saw_moov = false;
    let mut pending_moof = None;
    let mut complete_fragments = 0_u64;

    while cursor.saturating_add(8) <= bytes.len() {
        let start = cursor;
        let size32 = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        let kind: [u8; 4] = bytes[cursor + 4..cursor + 8].try_into().unwrap();
        let (header_size, box_size) = if size32 == 1 {
            if cursor.saturating_add(16) > bytes.len() {
                break;
            }
            let size = u64::from_be_bytes(bytes[cursor + 8..cursor + 16].try_into().unwrap());
            let Ok(size) = usize::try_from(size) else {
                break;
            };
            (16_usize, size)
        } else if size32 == 0 {
            (8_usize, bytes.len() - cursor)
        } else {
            (8_usize, size32 as usize)
        };
        if box_size < header_size || cursor.saturating_add(box_size) > bytes.len() {
            break;
        }
        cursor += box_size;

        match &kind {
            b"ftyp" => saw_ftyp = true,
            b"moov" => saw_moov = true,
            b"moof" if saw_ftyp && saw_moov => {
                if pending_moof.is_some() {
                    break;
                }
                pending_moof = Some((start, first_video_sample_is_sync(&bytes[start..cursor])));
            }
            b"mdat" => {
                if let Some((_, sync)) = pending_moof.take() {
                    if !sync && complete_fragments == 0 {
                        break;
                    }
                    complete_fragments = complete_fragments.saturating_add(1);
                    valid_prefix = cursor;
                }
            }
            _ => {
                if pending_moof.take().is_some() {
                    break;
                }
            }
        }
        if saw_ftyp && saw_moov && pending_moof.is_none() && complete_fragments == 0 {
            valid_prefix = cursor;
        }
    }

    if let Some((moof_start, _)) = pending_moof {
        valid_prefix = valid_prefix.min(moof_start);
    }
    let initialisation_complete = saw_ftyp && saw_moov;
    let salvageability = if complete_fragments > 0 {
        RecoverySalvageability::Playable
    } else if initialisation_complete {
        RecoverySalvageability::InitialisationOnly
    } else {
        RecoverySalvageability::None
    };

    RecoveryReport {
        initialisation_complete,
        complete_fragments,
        valid_prefix_len: valid_prefix,
        salvageability,
    }
}

fn first_video_sample_is_sync(moof: &[u8]) -> bool {
    let mut cursor = 0;
    let Some((kind, payload)) = next_box(moof, &mut cursor) else {
        return false;
    };
    if kind != *b"moof" || cursor != moof.len() {
        return false;
    }

    let mut children = 0;
    while children < payload.len() {
        let Some((kind, traf)) = next_box(payload, &mut children) else {
            return false;
        };
        if kind != *b"traf" {
            continue;
        }
        let mut track_id = None;
        let mut sample_flags = None;
        let mut fields = 0;
        while fields < traf.len() {
            let Some((kind, field)) = next_box(traf, &mut fields) else {
                return false;
            };
            match &kind {
                b"tfhd" if field.len() >= 8 => {
                    track_id = read_u32(field, 4);
                }
                b"trun" => {
                    sample_flags = first_track_run_sample_flags(field);
                }
                _ => {}
            }
        }
        if track_id == Some(VIDEO_TRACK_ID) {
            return sample_flags.is_some_and(sample_is_sync);
        }
    }
    false
}

fn first_track_run_sample_flags(payload: &[u8]) -> Option<u32> {
    if payload.len() < 8 {
        return None;
    }
    let flags = u32::from(payload[1]) << 16 | u32::from(payload[2]) << 8 | u32::from(payload[3]);
    if read_u32(payload, 4)? == 0 {
        return None;
    }
    let mut cursor = 8;
    if flags & 0x0000_0001 != 0 {
        cursor += 4;
    }
    let first_sample_flags = if flags & 0x0000_0004 != 0 {
        let value = read_u32(payload, cursor)?;
        cursor += 4;
        Some(value)
    } else {
        None
    };
    if flags & 0x0000_0100 != 0 {
        cursor += 4;
    }
    if flags & 0x0000_0200 != 0 {
        cursor += 4;
    }
    if flags & 0x0000_0400 != 0 {
        read_u32(payload, cursor)
    } else {
        first_sample_flags
    }
}

fn sample_is_sync(flags: u32) -> bool {
    flags & 0x0001_0000 == 0 && flags & 0x0300_0000 == 0x0200_0000
}

fn next_box<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<([u8; 4], &'a [u8])> {
    let start = *cursor;
    let size32 = read_u32(bytes, start)?;
    let kind = bytes.get(start + 4..start + 8)?.try_into().ok()?;
    let (header, size) = if size32 == 1 {
        let size = usize::try_from(read_u64(bytes, start + 8)?).ok()?;
        (16, size)
    } else if size32 == 0 {
        (8, bytes.len().checked_sub(start)?)
    } else {
        (8, usize::try_from(size32).ok()?)
    };
    if size < header {
        return None;
    }
    let end = start.checked_add(size)?;
    let payload_start = start.checked_add(header)?;
    if end > bytes.len() {
        return None;
    }
    *cursor = end;
    Some((kind, &bytes[payload_start..end]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset.checked_add(4)?)?
        .try_into()
        .ok()
        .map(u32::from_be_bytes)
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    bytes
        .get(offset..offset.checked_add(8)?)?
        .try_into()
        .ok()
        .map(u64::from_be_bytes)
}

fn validate_track_config(
    video: &VideoTrackConfig,
    audio: Option<&AudioTrackConfig>,
) -> io::Result<()> {
    if video.width == 0 || video.height == 0 || video.timescale == 0 {
        return Err(invalid_input(
            "video dimensions and timescale must be non-zero",
        ));
    }
    let decoder_config = match &video.codec {
        VideoCodecConfiguration::Avc(config) | VideoCodecConfiguration::Av1(config) => config,
    };
    if decoder_config.is_empty() {
        return Err(invalid_input("video decoder configuration is empty"));
    }
    if let Some(audio) = audio {
        if audio.sample_rate == 0 || audio.channels == 0 {
            return Err(invalid_input(
                "audio sample rate and channel count must be non-zero",
            ));
        }
        if audio.audio_specific_config.is_empty() {
            return Err(invalid_input("AAC AudioSpecificConfig is empty"));
        }
    }
    Ok(())
}

fn validate_fragment(fragment: &MediaFragment, has_audio: bool) -> io::Result<()> {
    if fragment.video.samples.is_empty() {
        return Err(invalid_input("a media fragment needs a video sample"));
    }
    if fragment.audio.is_some() && !has_audio {
        return Err(invalid_input(
            "fragment has audio but the initialisation segment has no audio track",
        ));
    }
    for sample in fragment
        .video
        .samples
        .iter()
        .chain(fragment.audio.iter().flat_map(|audio| &audio.samples))
    {
        if sample.duration == 0 {
            return Err(invalid_input("encoded sample duration must be non-zero"));
        }
        if sample.data.is_empty() {
            return Err(invalid_input("encoded sample payload must not be empty"));
        }
    }
    if fragment
        .audio
        .as_ref()
        .is_some_and(|audio| audio.samples.is_empty())
    {
        return Err(invalid_input("configured audio fragment is empty"));
    }
    Ok(())
}

fn file_type_box() -> io::Result<Vec<u8>> {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"isom");
    push_u32(&mut payload, 0x0000_0200);
    payload.extend_from_slice(b"isom");
    payload.extend_from_slice(b"iso6");
    payload.extend_from_slice(b"mp41");
    payload.extend_from_slice(b"avc1");
    payload.extend_from_slice(b"av01");
    mp4_box(*b"ftyp", payload)
}

fn movie_box(video: &VideoTrackConfig, audio: Option<&AudioTrackConfig>) -> io::Result<Vec<u8>> {
    let mut payload = movie_header_box(if audio.is_some() { 3 } else { 2 })?;
    payload.extend_from_slice(&track_box_video(video)?);
    if let Some(audio) = audio {
        payload.extend_from_slice(&track_box_audio(audio)?);
    }
    payload.extend_from_slice(&movie_extends_box(audio.is_some())?);
    mp4_box(*b"moov", payload)
}

fn movie_header_box(next_track_id: u32) -> io::Result<Vec<u8>> {
    let mut payload = full_box_header(0, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 1_000);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0x0001_0000);
    push_u16(&mut payload, 0x0100);
    push_u16(&mut payload, 0);
    payload.extend_from_slice(&[0; 8]);
    push_identity_matrix(&mut payload);
    payload.extend_from_slice(&[0; 24]);
    push_u32(&mut payload, next_track_id);
    mp4_box(*b"mvhd", payload)
}

fn track_box_video(config: &VideoTrackConfig) -> io::Result<Vec<u8>> {
    let mut payload = track_header_box(VIDEO_TRACK_ID, config.width, config.height, false)?;
    payload.extend_from_slice(&media_box_video(config)?);
    mp4_box(*b"trak", payload)
}

fn track_box_audio(config: &AudioTrackConfig) -> io::Result<Vec<u8>> {
    let mut payload = track_header_box(AUDIO_TRACK_ID, 0, 0, true)?;
    if config.priming_frames > 0 {
        payload.extend_from_slice(&audio_edit_box(config.priming_frames)?);
    }
    payload.extend_from_slice(&media_box_audio(config)?);
    mp4_box(*b"trak", payload)
}

fn audio_edit_box(priming_frames: u32) -> io::Result<Vec<u8>> {
    let mut payload = full_box_header(1, 0);
    push_u32(&mut payload, 1);
    // Fragmented output has no final duration when the initialization segment
    // is written. All ones is ISO-BMFF's unknown-duration sentinel.
    push_u64(&mut payload, u64::MAX);
    payload.extend_from_slice(&i64::from(priming_frames).to_be_bytes());
    push_u16(&mut payload, 1);
    push_u16(&mut payload, 0);
    mp4_box(*b"edts", mp4_box(*b"elst", payload)?)
}

fn track_header_box(track_id: u32, width: u16, height: u16, audio: bool) -> io::Result<Vec<u8>> {
    let mut payload = full_box_header(0, 0x0000_0007);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, track_id);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    payload.extend_from_slice(&[0; 8]);
    push_u16(&mut payload, 0);
    push_u16(&mut payload, 0);
    push_u16(&mut payload, if audio { 0x0100 } else { 0 });
    push_u16(&mut payload, 0);
    push_identity_matrix(&mut payload);
    push_u32(&mut payload, u32::from(width) << 16);
    push_u32(&mut payload, u32::from(height) << 16);
    mp4_box(*b"tkhd", payload)
}

fn media_box_video(config: &VideoTrackConfig) -> io::Result<Vec<u8>> {
    let mut payload = media_header_box(config.timescale)?;
    payload.extend_from_slice(&handler_box(*b"vide", b"VideoHandler\0")?);
    payload.extend_from_slice(&media_information_box(
        sample_description_video(config)?,
        false,
    )?);
    mp4_box(*b"mdia", payload)
}

fn media_box_audio(config: &AudioTrackConfig) -> io::Result<Vec<u8>> {
    let mut payload = media_header_box(config.sample_rate)?;
    payload.extend_from_slice(&handler_box(*b"soun", b"SoundHandler\0")?);
    payload.extend_from_slice(&media_information_box(
        sample_description_audio(config)?,
        true,
    )?);
    mp4_box(*b"mdia", payload)
}

fn media_header_box(timescale: u32) -> io::Result<Vec<u8>> {
    let mut payload = full_box_header(0, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, timescale);
    push_u32(&mut payload, 0);
    push_u16(&mut payload, 0x55c4);
    push_u16(&mut payload, 0);
    mp4_box(*b"mdhd", payload)
}

fn handler_box(handler_type: [u8; 4], name: &[u8]) -> io::Result<Vec<u8>> {
    let mut payload = full_box_header(0, 0);
    push_u32(&mut payload, 0);
    payload.extend_from_slice(&handler_type);
    payload.extend_from_slice(&[0; 12]);
    payload.extend_from_slice(name);
    mp4_box(*b"hdlr", payload)
}

fn media_information_box(stsd: Vec<u8>, audio: bool) -> io::Result<Vec<u8>> {
    let mut payload = if audio {
        sound_media_header_box()?
    } else {
        video_media_header_box()?
    };
    payload.extend_from_slice(&data_information_box()?);
    payload.extend_from_slice(&sample_table_box(stsd)?);
    mp4_box(*b"minf", payload)
}

fn video_media_header_box() -> io::Result<Vec<u8>> {
    let mut payload = full_box_header(0, 1);
    payload.extend_from_slice(&[0; 8]);
    mp4_box(*b"vmhd", payload)
}

fn sound_media_header_box() -> io::Result<Vec<u8>> {
    let mut payload = full_box_header(0, 0);
    payload.extend_from_slice(&[0; 4]);
    mp4_box(*b"smhd", payload)
}

fn data_information_box() -> io::Result<Vec<u8>> {
    let url = mp4_box(*b"url ", full_box_header(0, 1))?;
    let mut dref_payload = full_box_header(0, 0);
    push_u32(&mut dref_payload, 1);
    dref_payload.extend_from_slice(&url);
    let dref = mp4_box(*b"dref", dref_payload)?;
    mp4_box(*b"dinf", dref)
}

fn sample_table_box(stsd: Vec<u8>) -> io::Result<Vec<u8>> {
    let mut payload = stsd;
    for kind in [*b"stts", *b"stsc", *b"stco"] {
        let mut empty = full_box_header(0, 0);
        push_u32(&mut empty, 0);
        payload.extend_from_slice(&mp4_box(kind, empty)?);
    }
    let mut stsz = full_box_header(0, 0);
    push_u32(&mut stsz, 0);
    push_u32(&mut stsz, 0);
    payload.extend_from_slice(&mp4_box(*b"stsz", stsz)?);
    mp4_box(*b"stbl", payload)
}

fn sample_description_video(config: &VideoTrackConfig) -> io::Result<Vec<u8>> {
    let (entry_kind, config_kind, decoder_config) = match &config.codec {
        VideoCodecConfiguration::Avc(bytes) => (*b"avc1", *b"avcC", bytes),
        VideoCodecConfiguration::Av1(bytes) => (*b"av01", *b"av1C", bytes),
    };
    let mut entry = vec![0; 6];
    push_u16(&mut entry, 1);
    entry.extend_from_slice(&[0; 16]);
    push_u16(&mut entry, config.width);
    push_u16(&mut entry, config.height);
    push_u32(&mut entry, 0x0048_0000);
    push_u32(&mut entry, 0x0048_0000);
    push_u32(&mut entry, 0);
    push_u16(&mut entry, 1);
    entry.extend_from_slice(&[0; 32]);
    push_u16(&mut entry, 0x0018);
    push_u16(&mut entry, 0xffff);
    entry.extend_from_slice(&mp4_box(config_kind, decoder_config.clone())?);
    let entry = mp4_box(entry_kind, entry)?;

    let mut stsd = full_box_header(0, 0);
    push_u32(&mut stsd, 1);
    stsd.extend_from_slice(&entry);
    mp4_box(*b"stsd", stsd)
}

fn sample_description_audio(config: &AudioTrackConfig) -> io::Result<Vec<u8>> {
    let mut entry = vec![0; 6];
    push_u16(&mut entry, 1);
    entry.extend_from_slice(&[0; 8]);
    push_u16(&mut entry, config.channels);
    push_u16(&mut entry, 16);
    push_u16(&mut entry, 0);
    push_u16(&mut entry, 0);
    push_u32(
        &mut entry,
        config
            .sample_rate
            .checked_shl(16)
            .ok_or_else(|| invalid_input("audio sample rate exceeds 16.16 storage"))?,
    );
    entry.extend_from_slice(&elementary_stream_descriptor(config)?);
    let entry = mp4_box(*b"mp4a", entry)?;

    let mut stsd = full_box_header(0, 0);
    push_u32(&mut stsd, 1);
    stsd.extend_from_slice(&entry);
    mp4_box(*b"stsd", stsd)
}

fn elementary_stream_descriptor(config: &AudioTrackConfig) -> io::Result<Vec<u8>> {
    let decoder_specific = descriptor(0x05, config.audio_specific_config.clone())?;
    let mut decoder = vec![0x40, 0x15, 0, 0, 0];
    push_u32(&mut decoder, 128_000);
    push_u32(&mut decoder, 128_000);
    decoder.extend_from_slice(&decoder_specific);
    let decoder = descriptor(0x04, decoder)?;
    let sl = descriptor(0x06, vec![0x02])?;
    let mut es = Vec::new();
    push_u16(&mut es, AUDIO_TRACK_ID as u16);
    es.push(0);
    es.extend_from_slice(&decoder);
    es.extend_from_slice(&sl);
    let es = descriptor(0x03, es)?;
    let mut payload = full_box_header(0, 0);
    payload.extend_from_slice(&es);
    mp4_box(*b"esds", payload)
}

fn descriptor(tag: u8, payload: Vec<u8>) -> io::Result<Vec<u8>> {
    let length =
        u32::try_from(payload.len()).map_err(|_| invalid_input("MPEG-4 descriptor exceeds u32"))?;
    let mut encoded_length = [0_u8; 5];
    let mut cursor = encoded_length.len();
    let mut remaining = length;
    cursor -= 1;
    encoded_length[cursor] = (remaining & 0x7f) as u8;
    remaining >>= 7;
    while remaining > 0 {
        cursor -= 1;
        encoded_length[cursor] = ((remaining & 0x7f) as u8) | 0x80;
        remaining >>= 7;
    }

    let mut output = Vec::with_capacity(payload.len().saturating_add(6));
    output.push(tag);
    output.extend_from_slice(&encoded_length[cursor..]);
    output.extend_from_slice(&payload);
    Ok(output)
}

fn movie_extends_box(has_audio: bool) -> io::Result<Vec<u8>> {
    let mut payload = track_extends_box(VIDEO_TRACK_ID)?;
    if has_audio {
        payload.extend_from_slice(&track_extends_box(AUDIO_TRACK_ID)?);
    }
    mp4_box(*b"mvex", payload)
}

fn track_extends_box(track_id: u32) -> io::Result<Vec<u8>> {
    let mut payload = full_box_header(0, 0);
    push_u32(&mut payload, track_id);
    push_u32(&mut payload, 1);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    mp4_box(*b"trex", payload)
}

fn movie_fragment_box(
    sequence_number: u32,
    fragment: &MediaFragment,
    video_offset: i32,
    audio_offset: i32,
) -> io::Result<Vec<u8>> {
    let mut payload = movie_fragment_header_box(sequence_number)?;
    payload.extend_from_slice(&track_fragment_box(
        VIDEO_TRACK_ID,
        &fragment.video,
        video_offset,
    )?);
    if let Some(audio) = &fragment.audio {
        payload.extend_from_slice(&track_fragment_box(AUDIO_TRACK_ID, audio, audio_offset)?);
    }
    mp4_box(*b"moof", payload)
}

fn movie_fragment_header_box(sequence_number: u32) -> io::Result<Vec<u8>> {
    let mut payload = full_box_header(0, 0);
    push_u32(&mut payload, sequence_number);
    mp4_box(*b"mfhd", payload)
}

fn track_fragment_box(
    track_id: u32,
    fragment: &TrackFragment,
    data_offset: i32,
) -> io::Result<Vec<u8>> {
    let mut payload = track_fragment_header_box(track_id)?;
    payload.extend_from_slice(&track_fragment_decode_time_box(fragment.base_decode_time)?);
    payload.extend_from_slice(&track_run_box(fragment, data_offset)?);
    mp4_box(*b"traf", payload)
}

fn track_fragment_header_box(track_id: u32) -> io::Result<Vec<u8>> {
    let mut payload = full_box_header(0, 0x0002_0000);
    push_u32(&mut payload, track_id);
    mp4_box(*b"tfhd", payload)
}

fn track_fragment_decode_time_box(base_decode_time: u64) -> io::Result<Vec<u8>> {
    let mut payload = full_box_header(1, 0);
    push_u64(&mut payload, base_decode_time);
    mp4_box(*b"tfdt", payload)
}

fn track_run_box(fragment: &TrackFragment, data_offset: i32) -> io::Result<Vec<u8>> {
    const FLAGS: u32 = 0x0000_0701;
    let mut payload = full_box_header(0, FLAGS);
    push_u32(
        &mut payload,
        u32::try_from(fragment.samples.len())
            .map_err(|_| invalid_input("fragment sample count exceeds u32"))?,
    );
    push_i32(&mut payload, data_offset);
    for sample in &fragment.samples {
        push_u32(&mut payload, sample.duration);
        push_u32(
            &mut payload,
            u32::try_from(sample.data.len())
                .map_err(|_| invalid_input("encoded sample exceeds u32"))?,
        );
        push_u32(
            &mut payload,
            if sample.keyframe {
                0x0200_0000
            } else {
                0x0101_0000
            },
        );
    }
    mp4_box(*b"trun", payload)
}

fn full_box_header(version: u8, flags: u32) -> Vec<u8> {
    vec![
        version,
        ((flags >> 16) & 0xff) as u8,
        ((flags >> 8) & 0xff) as u8,
        (flags & 0xff) as u8,
    ]
}

fn push_identity_matrix(output: &mut Vec<u8>) {
    for value in [0x0001_0000_u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        push_u32(output, value);
    }
}

fn mp4_box(kind: [u8; 4], payload: Vec<u8>) -> io::Result<Vec<u8>> {
    let size = payload
        .len()
        .checked_add(8)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| invalid_input("ISO-BMFF box exceeds u32"))?;
    let mut output = Vec::with_capacity(size as usize);
    push_u32(&mut output, size);
    output.extend_from_slice(&kind);
    output.extend_from_slice(&payload);
    Ok(output)
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn push_i32(output: &mut Vec<u8>, value: i32) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn push_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        EncodedSample, FragmentedMp4, MediaFragment, RecoverySalvageability, TrackFragment,
        VideoCodecConfiguration, VideoTrackConfig, inspect_recovery, movie_fragment_box, mp4_box,
    };
    use crate::Salvageability;

    fn video_config() -> VideoTrackConfig {
        VideoTrackConfig {
            width: 1280,
            height: 720,
            timescale: 30,
            codec: VideoCodecConfiguration::Avc(vec![
                1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 1, 0x67, 1, 0, 1, 0x68,
            ]),
        }
    }

    fn fragment(timestamp: u64) -> MediaFragment {
        MediaFragment {
            video: TrackFragment {
                base_decode_time: timestamp,
                samples: vec![EncodedSample {
                    data: vec![0, 0, 0, 2, 0x65, 0x88],
                    duration: 1,
                    keyframe: true,
                }],
            },
            audio: None,
        }
    }

    fn dependent_fragment(timestamp: u64) -> MediaFragment {
        let mut fragment = fragment(timestamp);
        fragment.video.samples[0].keyframe = false;
        fragment.video.samples[0].data[4] = 0x41;
        fragment
    }

    #[test]
    fn initialisation_without_media_is_explicitly_non_playable() {
        let writer = FragmentedMp4::new(Cursor::new(Vec::new()), &video_config(), None).unwrap();
        assert_eq!(writer.salvageability(), Salvageability::InitialisationOnly);
        let bytes = writer.finish().unwrap().into_inner();
        let report = inspect_recovery(&bytes);
        assert!(report.initialisation_complete);
        assert_eq!(report.complete_fragments, 0);
        assert_eq!(
            report.salvageability,
            RecoverySalvageability::InitialisationOnly
        );
        assert_eq!(report.valid_prefix_len, bytes.len());
    }

    #[test]
    fn every_flushed_fragment_is_recoverable() {
        let mut writer =
            FragmentedMp4::new(Cursor::new(Vec::new()), &video_config(), None).unwrap();
        writer.write_fragment(&fragment(0)).unwrap();
        writer.write_fragment(&fragment(1)).unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let report = inspect_recovery(&bytes);
        assert_eq!(report.complete_fragments, 2);
        assert_eq!(report.salvageability, RecoverySalvageability::Playable);
        assert_eq!(report.valid_prefix_len, bytes.len());
    }

    #[test]
    fn the_first_live_fragment_must_begin_with_a_keyframe() {
        let mut writer =
            FragmentedMp4::new(Cursor::new(Vec::new()), &video_config(), None).unwrap();

        let error = writer
            .write_fragment(&dependent_fragment(0))
            .expect_err("dependent first frame is not independently playable");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(writer.salvageability(), Salvageability::InitialisationOnly);
    }

    #[test]
    fn recovery_does_not_call_a_dependent_first_fragment_playable() {
        let writer = FragmentedMp4::new(Cursor::new(Vec::new()), &video_config(), None).unwrap();
        let mut bytes = writer.finish().unwrap().into_inner();
        let initialisation_len = bytes.len();
        let fragment = dependent_fragment(0);
        bytes.extend_from_slice(&movie_fragment_box(1, &fragment, 0, 0).unwrap());
        bytes
            .extend_from_slice(&mp4_box(*b"mdat", fragment.video.samples[0].data.clone()).unwrap());

        let report = inspect_recovery(&bytes);

        assert!(report.initialisation_complete);
        assert_eq!(report.complete_fragments, 0);
        assert_eq!(
            report.salvageability,
            RecoverySalvageability::InitialisationOnly
        );
        assert_eq!(report.valid_prefix_len, initialisation_len);
    }

    #[test]
    fn dependent_fragments_after_the_first_keyframe_remain_recoverable() {
        let mut writer =
            FragmentedMp4::new(Cursor::new(Vec::new()), &video_config(), None).unwrap();
        writer.write_fragment(&fragment(0)).unwrap();
        writer.write_fragment(&dependent_fragment(1)).unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let report = inspect_recovery(&bytes);

        assert_eq!(report.complete_fragments, 2);
        assert_eq!(report.salvageability, RecoverySalvageability::Playable);
        assert_eq!(report.valid_prefix_len, bytes.len());
    }

    #[test]
    fn interrupted_final_fragment_preserves_the_previous_pair() {
        let mut first = FragmentedMp4::new(Cursor::new(Vec::new()), &video_config(), None).unwrap();
        first.write_fragment(&fragment(0)).unwrap();
        let first_bytes = first.finish().unwrap().into_inner();

        let mut two = FragmentedMp4::new(Cursor::new(Vec::new()), &video_config(), None).unwrap();
        two.write_fragment(&fragment(0)).unwrap();
        two.write_fragment(&fragment(1)).unwrap();
        let mut interrupted = two.finish().unwrap().into_inner();
        interrupted.truncate(interrupted.len() - 2);

        let report = inspect_recovery(&interrupted);
        assert_eq!(report.complete_fragments, 1);
        assert_eq!(report.salvageability, RecoverySalvageability::Playable);
        assert_eq!(report.valid_prefix_len, first_bytes.len());
    }

    #[test]
    fn truncated_initialisation_is_not_reported_as_salvageable() {
        let writer = FragmentedMp4::new(Cursor::new(Vec::new()), &video_config(), None).unwrap();
        let mut bytes = writer.finish().unwrap().into_inner();
        bytes.truncate(bytes.len() / 2);
        let report = inspect_recovery(&bytes);
        assert_eq!(report.complete_fragments, 0);
        assert_eq!(report.salvageability, RecoverySalvageability::None);
    }
}
