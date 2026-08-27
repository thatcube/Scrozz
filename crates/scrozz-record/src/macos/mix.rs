//! Pure PCM normalisation and mixing.
//!
//! Audio callbacks can use different channel counts and sample rates. The
//! native layer converts their bytes into this representation, then this module
//! aligns them on a common 48 kHz stereo timeline and clips summed samples.

use std::collections::BTreeMap;

pub(crate) const MIX_SAMPLE_RATE: u32 = 48_000;
pub(crate) const MIX_CHANNELS: u16 = 2;
const MIX_LATENCY_FRAMES: i64 = MIX_SAMPLE_RATE as i64 / 10;
const CLOCK_MISMATCH_FRAMES: i64 = MIX_SAMPLE_RATE as i64 * 5;
const MAX_DRAIN_FRAMES: i64 = MIX_SAMPLE_RATE as i64 * 2;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PcmChunk {
    pub(crate) start_frame: i64,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    pub(crate) samples: Vec<f32>,
}

impl PcmChunk {
    pub(crate) fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels.max(1))
    }

    pub(crate) fn stereo_48khz(&self) -> Self {
        self.to_48khz_channels(MIX_CHANNELS)
    }

    pub(crate) fn to_48khz_channels(&self, output_channels: u16) -> Self {
        if self.samples.is_empty()
            || self.sample_rate == 0
            || self.channels == 0
            || output_channels == 0
        {
            return Self {
                start_frame: scale_frame(self.start_frame, self.sample_rate, MIX_SAMPLE_RATE),
                sample_rate: MIX_SAMPLE_RATE,
                channels: output_channels,
                samples: Vec::new(),
            };
        }

        let source_frames = self.frames();
        let output_frames = ((source_frames as u64 * u64::from(MIX_SAMPLE_RATE))
            / u64::from(self.sample_rate)) as usize;
        let mut samples = Vec::with_capacity(output_frames * usize::from(output_channels));
        for output_frame in 0..output_frames {
            let source_position =
                output_frame as f64 * f64::from(self.sample_rate) / f64::from(MIX_SAMPLE_RATE);
            let first = source_position.floor() as usize;
            let second = (first + 1).min(source_frames.saturating_sub(1));
            let blend = (source_position - first as f64) as f32;

            for channel in 0..usize::from(output_channels) {
                let first_sample = self.output_sample(first, channel, output_channels);
                let second_sample = self.output_sample(second, channel, output_channels);
                samples.push(first_sample + (second_sample - first_sample) * blend);
            }
        }

        Self {
            start_frame: scale_frame(self.start_frame, self.sample_rate, MIX_SAMPLE_RATE),
            sample_rate: MIX_SAMPLE_RATE,
            channels: output_channels,
            samples,
        }
    }

    fn output_sample(&self, frame: usize, output_channel: usize, output_channels: u16) -> f32 {
        if output_channels == 1 && self.channels > 1 {
            let channels = usize::from(self.channels);
            return (0..channels)
                .map(|channel| self.channel_sample(frame, channel))
                .sum::<f32>()
                / channels as f32;
        }
        self.channel_sample(frame, output_channel)
    }

    fn channel_sample(&self, frame: usize, output_channel: usize) -> f32 {
        let channels = usize::from(self.channels);
        let source_channel = if channels == 1 {
            0
        } else {
            output_channel.min(channels - 1)
        };
        self.samples[frame * channels + source_channel]
    }
}

pub(crate) fn mix_aligned(first: &PcmChunk, second: &PcmChunk) -> PcmChunk {
    let first = first.stereo_48khz();
    let second = second.stereo_48khz();
    let start = first.start_frame.min(second.start_frame);
    let end = (first.start_frame + first.frames() as i64)
        .max(second.start_frame + second.frames() as i64);
    let mut samples = vec![0.0; (end.saturating_sub(start) as usize) * 2];

    add_chunk(&mut samples, start, &first);
    add_chunk(&mut samples, start, &second);
    for sample in &mut samples {
        *sample = sample.clamp(-1.0, 1.0);
    }

    PcmChunk {
        start_frame: start,
        sample_rate: MIX_SAMPLE_RATE,
        channels: MIX_CHANNELS,
        samples,
    }
}

pub(crate) struct LiveMixer {
    system_enabled: bool,
    microphone_enabled: bool,
    pending: BTreeMap<i64, [f32; 2]>,
    system_end: Option<i64>,
    microphone_end: Option<i64>,
    next_frame: Option<i64>,
    system_shift: Option<i64>,
    microphone_shift: Option<i64>,
}

impl LiveMixer {
    pub(crate) fn new(system_enabled: bool, microphone_enabled: bool) -> Self {
        Self {
            system_enabled,
            microphone_enabled,
            pending: BTreeMap::new(),
            system_end: None,
            microphone_end: None,
            next_frame: None,
            system_shift: None,
            microphone_shift: None,
        }
    }

    pub(crate) fn push_system(&mut self, chunk: PcmChunk) -> Option<PcmChunk> {
        self.push(chunk, false)
    }

    pub(crate) fn push_microphone(&mut self, chunk: PcmChunk) -> Option<PcmChunk> {
        self.push(chunk, true)
    }

    pub(crate) fn flush(&mut self) -> Option<PcmChunk> {
        let end = self
            .system_end
            .into_iter()
            .chain(self.microphone_end)
            .max()?;
        let output = self.drain_until(end);
        self.reset();
        output
    }

    pub(crate) fn reset(&mut self) {
        self.pending.clear();
        self.system_end = None;
        self.microphone_end = None;
        self.next_frame = None;
    }

    fn push(&mut self, chunk: PcmChunk, microphone: bool) -> Option<PcmChunk> {
        let mut chunk = chunk.stereo_48khz();
        if chunk.samples.is_empty() {
            return None;
        }
        let (shift, other_end) = if microphone {
            (&mut self.microphone_shift, self.system_end)
        } else {
            (&mut self.system_shift, self.microphone_end)
        };
        let shift = *shift.get_or_insert_with(|| {
            other_end.map_or(0, |reference| {
                if chunk.start_frame.abs_diff(reference) > CLOCK_MISMATCH_FRAMES as u64 {
                    reference
                        .saturating_sub(chunk.start_frame.saturating_add(chunk.frames() as i64))
                } else {
                    0
                }
            })
        });
        chunk.start_frame = chunk.start_frame.saturating_add(shift);
        let end = chunk.start_frame.saturating_add(chunk.frames() as i64);
        let watermark = if microphone {
            &mut self.microphone_end
        } else {
            &mut self.system_end
        };
        *watermark = Some(watermark.map_or(end, |current| current.max(end)));
        self.next_frame.get_or_insert(chunk.start_frame);

        let already_emitted = self.next_frame.unwrap_or(chunk.start_frame);
        for (offset, channels) in chunk.samples.as_chunks::<2>().0.iter().enumerate() {
            let frame = chunk.start_frame.saturating_add(offset as i64);
            if frame < already_emitted {
                continue;
            }
            let mixed = self.pending.entry(frame).or_default();
            mixed[0] += channels[0];
            mixed[1] += channels[1];
        }

        self.ready_end().and_then(|end| self.drain_until(end))
    }

    fn ready_end(&self) -> Option<i64> {
        match (self.system_enabled, self.microphone_enabled) {
            (true, true) => {
                let furthest = self
                    .system_end
                    .into_iter()
                    .chain(self.microphone_end)
                    .max()?;
                let latency_bound = furthest.saturating_sub(MIX_LATENCY_FRAMES);
                Some(
                    match (self.system_end, self.microphone_end) {
                        (Some(system), Some(microphone)) => system.min(microphone),
                        _ => i64::MIN,
                    }
                    .max(latency_bound),
                )
            }
            (true, false) => self.system_end,
            (false, true) => self.microphone_end,
            (false, false) => None,
        }
    }

    fn drain_until(&mut self, end: i64) -> Option<PcmChunk> {
        let mut start = self.next_frame?;
        if end <= start {
            return None;
        }
        if end.saturating_sub(start) > MAX_DRAIN_FRAMES {
            start = end.saturating_sub(MAX_DRAIN_FRAMES);
            self.pending.retain(|frame, _| *frame >= start);
        }
        let frame_count = end.saturating_sub(start) as usize;
        let mut samples = Vec::with_capacity(frame_count.saturating_mul(2));
        for frame in start..end {
            let channels = self.pending.remove(&frame).unwrap_or([0.0, 0.0]);
            samples.push(channels[0].clamp(-1.0, 1.0));
            samples.push(channels[1].clamp(-1.0, 1.0));
        }
        self.next_frame = Some(end);
        Some(PcmChunk {
            start_frame: start,
            sample_rate: MIX_SAMPLE_RATE,
            channels: MIX_CHANNELS,
            samples,
        })
    }
}

fn add_chunk(output: &mut [f32], output_start: i64, chunk: &PcmChunk) {
    let offset = chunk.start_frame.saturating_sub(output_start) as usize * 2;
    for (destination, source) in output[offset..].iter_mut().zip(&chunk.samples) {
        *destination += source;
    }
}

fn scale_frame(frame: i64, source_rate: u32, destination_rate: u32) -> i64 {
    if source_rate == 0 {
        return 0;
    }
    (i128::from(frame) * i128::from(destination_rate) / i128::from(source_rate)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_audio_is_duplicated_into_stereo() {
        let mono = PcmChunk {
            start_frame: 0,
            sample_rate: MIX_SAMPLE_RATE,
            channels: 1,
            samples: vec![0.25, -0.5],
        };
        assert_eq!(mono.stereo_48khz().samples, vec![0.25, 0.25, -0.5, -0.5]);
    }

    #[test]
    fn mixing_aligns_chunks_and_clips_the_sum() {
        let first = PcmChunk {
            start_frame: 0,
            sample_rate: MIX_SAMPLE_RATE,
            channels: 2,
            samples: vec![0.75, 0.75, 0.75, 0.75],
        };
        let second = PcmChunk {
            start_frame: 1,
            sample_rate: MIX_SAMPLE_RATE,
            channels: 2,
            samples: vec![0.75, -0.75],
        };

        let mixed = mix_aligned(&first, &second);
        assert_eq!(mixed.start_frame, 0);
        assert_eq!(mixed.samples, vec![0.75, 0.75, 1.0, 0.0]);
    }

    #[test]
    fn live_mixer_waits_for_both_sources_and_emits_aligned_audio() {
        let mut mixer = LiveMixer::new(true, true);
        let system = PcmChunk {
            start_frame: 10,
            sample_rate: MIX_SAMPLE_RATE,
            channels: 2,
            samples: vec![0.75, 0.75, 0.25, 0.25],
        };
        let microphone = PcmChunk {
            start_frame: 11,
            sample_rate: MIX_SAMPLE_RATE,
            channels: 1,
            samples: vec![0.5, 0.5],
        };

        assert!(mixer.push_system(system).is_none());
        let first = mixer.push_microphone(microphone).unwrap();
        assert_eq!(first.start_frame, 10);
        assert_eq!(first.samples, vec![0.75, 0.75, 0.75, 0.75]);
        let tail = mixer.flush().unwrap();
        assert_eq!(tail.start_frame, 12);
        assert_eq!(tail.samples, vec![0.5, 0.5]);
    }

    #[test]
    fn live_mixer_resets_without_bridging_a_pause_gap() {
        let mut mixer = LiveMixer::new(true, false);
        let before_pause = PcmChunk {
            start_frame: 0,
            sample_rate: MIX_SAMPLE_RATE,
            channels: 2,
            samples: vec![0.25, 0.25],
        };
        assert!(mixer.push_system(before_pause).is_some());
        mixer.reset();
        let after_pause = PcmChunk {
            start_frame: 48_000,
            sample_rate: MIX_SAMPLE_RATE,
            channels: 2,
            samples: vec![0.5, 0.5],
        };
        let mixed = mixer.push_system(after_pause).unwrap();
        assert_eq!(mixed.start_frame, 48_000);
        assert_eq!(mixed.samples, vec![0.5, 0.5]);
    }

    #[test]
    fn live_mixer_does_not_wait_forever_for_a_missing_source() {
        let mut mixer = LiveMixer::new(true, true);
        let system = PcmChunk {
            start_frame: 0,
            sample_rate: MIX_SAMPLE_RATE,
            channels: 2,
            samples: vec![0.25; (MIX_LATENCY_FRAMES as usize + 2) * 2],
        };

        let mixed = mixer.push_system(system).unwrap();
        assert_eq!(mixed.start_frame, 0);
        assert_eq!(mixed.frames(), 2);
    }

    #[test]
    fn live_mixer_aligns_sources_that_use_different_clock_epochs() {
        let mut mixer = LiveMixer::new(true, true);
        let system = PcmChunk {
            start_frame: 48_000_000,
            sample_rate: MIX_SAMPLE_RATE,
            channels: 2,
            samples: vec![0.25, 0.25],
        };
        let microphone = PcmChunk {
            start_frame: 0,
            sample_rate: MIX_SAMPLE_RATE,
            channels: 2,
            samples: vec![0.5, 0.5],
        };

        assert!(mixer.push_system(system).is_none());
        let mixed = mixer.push_microphone(microphone).unwrap();
        assert_eq!(mixed.start_frame, 48_000_000);
        assert_eq!(mixed.samples, vec![0.75, 0.75]);
    }
}
