//! Pure WASAPI conversion, QPC alignment, resampling, and mixing.

use std::collections::VecDeque;

use super::timing::{audio_frames_to_hns, hns_to_audio_frame};

/// One input to the recording mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The default render endpoint captured through WASAPI loopback.
    System,
    /// The default capture endpoint.
    Microphone,
}

impl Source {
    const fn index(self) -> usize {
        match self {
            Self::System => 0,
            Self::Microphone => 1,
        }
    }
}

/// A decoded WASAPI packet.
#[derive(Debug, Clone)]
pub struct Packet {
    /// Timestamp of the first sample on the pause-free stream timeline.
    pub stream_hns: i64,
    /// Input sample rate.
    pub sample_rate: u32,
    /// Interleaved input channel count.
    pub channels: u16,
    /// `WAVEFORMATEXTENSIBLE` speaker positions, or zero when unspecified.
    pub channel_mask: u32,
    /// Interleaved normalized samples.
    pub samples: Vec<f32>,
}

/// PCM ready for an MF sink-writer sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixedChunk {
    /// Sample timestamp.
    pub time_hns: i64,
    /// Sample duration.
    pub duration_hns: i64,
    /// Stereo, signed 16-bit, little-endian PCM.
    pub pcm: Vec<u8>,
}

#[derive(Debug, Clone)]
struct Segment {
    start: u64,
    samples: Vec<[f32; 2]>,
}

#[derive(Debug, Default, Clone)]
struct Track {
    segments: VecDeque<Segment>,
    last_end: u64,
}

impl Track {
    fn push(&mut self, mut start: u64, mut samples: Vec<[f32; 2]>, cursor: u64) {
        if samples.is_empty() {
            return;
        }

        let discard = cursor.saturating_sub(start).min(samples.len() as u64) as usize;
        if discard != 0 {
            samples.drain(..discard);
            start = start.saturating_add(discard as u64);
        }

        let overlap = self
            .last_end
            .saturating_sub(start)
            .min(samples.len() as u64) as usize;
        if overlap != 0 {
            samples.drain(..overlap);
            start = start.saturating_add(overlap as u64);
        }
        if samples.is_empty() {
            return;
        }

        self.last_end = start.saturating_add(samples.len() as u64);
        self.segments.push_back(Segment { start, samples });
    }

    fn sample_at(&mut self, frame: u64) -> [f32; 2] {
        while self
            .segments
            .front()
            .is_some_and(|s| frame >= s.start.saturating_add(s.samples.len() as u64))
        {
            self.segments.pop_front();
        }

        let Some(segment) = self.segments.front() else {
            return [0.0; 2];
        };
        if frame < segment.start {
            return [0.0; 2];
        }
        segment.samples[(frame - segment.start) as usize]
    }

    fn buffered_frames(&self, cursor: u64) -> u64 {
        self.segments.iter().fold(0, |total, segment| {
            let end = segment.start.saturating_add(segment.samples.len() as u64);
            total.saturating_add(end.saturating_sub(segment.start.max(cursor)))
        })
    }
}

/// Aligns two independently clocked WASAPI streams and fills absent time with
/// silence so loopback going quiet cannot shorten the recording.
#[derive(Debug, Clone)]
pub struct Mixer {
    sample_rate: u32,
    chunk_frames: u32,
    enabled: [bool; 2],
    tracks: [Track; 2],
    cursor: u64,
}

impl Mixer {
    /// Creates a stereo mix at a fixed output rate.
    #[must_use]
    pub fn new(sample_rate: u32, chunk_frames: u32, system_audio: bool, microphone: bool) -> Self {
        Self {
            sample_rate,
            chunk_frames: chunk_frames.max(1),
            enabled: [system_audio, microphone],
            tracks: [Track::default(), Track::default()],
            cursor: 0,
        }
    }

    /// Converts and queues one source packet.
    pub fn ingest(&mut self, source: Source, packet: Packet) {
        if !self.enabled[source.index()] || packet.sample_rate == 0 || packet.channels == 0 {
            return;
        }
        let stereo = downmix_stereo(&packet.samples, packet.channels, packet.channel_mask);
        let stereo = resample_linear(&stereo, packet.sample_rate, self.sample_rate);
        let start = hns_to_audio_frame(packet.stream_hns, self.sample_rate);
        self.tracks[source.index()].push(start, stereo, self.cursor);
    }

    /// Emits every whole chunk before `through_hns`.
    ///
    /// Missing source samples are emitted as silence. That is essential for
    /// loopback capture: Windows sends no packet at all while the endpoint is
    /// quiet.
    pub fn drain_through(&mut self, through_hns: i64, include_partial: bool) -> Vec<MixedChunk> {
        let mut chunks = Vec::new();
        while let Some(chunk) = self.drain_next(through_hns, include_partial) {
            chunks.push(chunk);
        }
        chunks
    }

    /// Emits at most one chunk before `through_hns`.
    pub fn drain_next(&mut self, through_hns: i64, include_partial: bool) -> Option<MixedChunk> {
        let end = hns_to_audio_frame(through_hns, self.sample_rate);
        if self.cursor >= end {
            return None;
        }
        let remaining = end - self.cursor;
        if !include_partial && remaining < u64::from(self.chunk_frames) {
            return None;
        }
        let frames = remaining.min(u64::from(self.chunk_frames));
        let start = self.cursor;
        let mut pcm = Vec::with_capacity(frames as usize * 4);
        let active = self.enabled.iter().filter(|enabled| **enabled).count();
        let gain = if active > 1 { 0.5 } else { 1.0 };

        for frame in start..start + frames {
            let system = self.tracks[0].sample_at(frame);
            let microphone = self.tracks[1].sample_at(frame);
            let left = (system[0] + microphone[0]) * gain;
            let right = (system[1] + microphone[1]) * gain;
            pcm.extend_from_slice(&f32_to_i16(left).to_le_bytes());
            pcm.extend_from_slice(&f32_to_i16(right).to_le_bytes());
        }

        self.cursor += frames;
        Some(MixedChunk {
            time_hns: audio_frames_to_hns(start, self.sample_rate),
            duration_hns: audio_frames_to_hns(frames, self.sample_rate),
            pcm,
        })
    }

    /// Current committed output time.
    #[must_use]
    pub fn cursor_hns(&self) -> i64 {
        audio_frames_to_hns(self.cursor, self.sample_rate)
    }

    /// Number of source frames still retained across both input tracks.
    #[must_use]
    pub fn buffered_frames(&self) -> u64 {
        self.tracks
            .iter()
            .map(|track| track.buffered_frames(self.cursor))
            .fold(0, u64::saturating_add)
    }
}

/// Downmixes interleaved input to stereo using the endpoint's speaker layout.
#[must_use]
pub fn downmix_stereo(samples: &[f32], channels: u16, channel_mask: u32) -> Vec<[f32; 2]> {
    let channels = usize::from(channels);
    if channels == 0 {
        return Vec::new();
    }

    samples
        .chunks_exact(channels)
        .map(|frame| {
            if channels == 1 {
                [frame[0], frame[0]]
            } else if channel_mask != 0 && channel_mask.count_ones() as usize == channels {
                downmix_masked(frame, channel_mask)
            } else {
                downmix_by_index(frame)
            }
        })
        .collect()
}

fn downmix_masked(frame: &[f32], mut mask: u32) -> [f32; 2] {
    let mut left = 0.0;
    let mut right = 0.0;
    for sample in frame {
        let speaker = 1u32 << mask.trailing_zeros();
        mask &= !speaker;
        match speaker {
            0x0001 | 0x0010 | 0x0040 | 0x0200 | 0x1000 | 0x8000 => left += sample,
            0x0002 | 0x0020 | 0x0080 | 0x0400 | 0x4000 | 0x20_000 => right += sample,
            0x0004 => {
                left += sample * 0.707;
                right += sample * 0.707;
            }
            0x0008 => {
                left += sample * 0.25;
                right += sample * 0.25;
            }
            _ => {
                left += sample * 0.5;
                right += sample * 0.5;
            }
        }
    }
    [left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0)]
}

fn downmix_by_index(frame: &[f32]) -> [f32; 2] {
    if frame.len() == 2 {
        return [frame[0], frame[1]];
    }
    let center = frame.get(2).copied().unwrap_or_default() * 0.707;
    let lfe = frame.get(3).copied().unwrap_or_default() * 0.25;
    let mut left = frame[0] + center + lfe;
    let mut right = frame[1] + center + lfe;
    for (index, sample) in frame.iter().copied().enumerate().skip(4) {
        if index % 2 == 0 {
            left += sample * 0.5;
        } else {
            right += sample * 0.5;
        }
    }
    [left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0)]
}

/// Resamples stereo frames with a fractional source position and linear
/// interpolation.
#[must_use]
pub fn resample_linear(input: &[[f32; 2]], input_rate: u32, output_rate: u32) -> Vec<[f32; 2]> {
    if input.is_empty() || input_rate == 0 || output_rate == 0 {
        return Vec::new();
    }
    if input_rate == output_rate {
        return input.to_vec();
    }

    let output_len = ((input.len() as u128 * u128::from(output_rate) + u128::from(input_rate) / 2)
        / u128::from(input_rate)) as usize;
    let mut output = Vec::with_capacity(output_len);
    for index in 0..output_len {
        let source = index as f64 * f64::from(input_rate) / f64::from(output_rate);
        let left = source.floor() as usize;
        let right = (left + 1).min(input.len() - 1);
        let fraction = (source - left as f64) as f32;
        output.push([
            input[left][0] + (input[right][0] - input[left][0]) * fraction,
            input[left][1] + (input[right][1] - input[left][1]) * fraction,
        ]);
    }
    output
}

/// Converts a normalized float sample to signed PCM without wraparound.
#[must_use]
pub fn f32_to_i16(sample: f32) -> i16 {
    let sample = if sample.is_finite() { sample } else { 0.0 };
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}
