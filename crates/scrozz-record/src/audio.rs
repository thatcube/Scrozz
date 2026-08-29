//! Timestamp-aligned microphone and desktop-audio mixing.

use scrozz_core::{Error, Result};

/// Largest allocation one mixer call may request.
pub const MAX_MIX_FRAMES: u64 = 48_000;

/// One interleaved floating-point audio buffer.
#[derive(Debug, Clone)]
pub struct AudioBuffer {
    /// Sample rate in frames per second.
    pub sample_rate: u32,
    /// Number of interleaved channels. Mono and stereo are supported.
    pub channels: u8,
    /// Start position in sample frames on the recording timeline.
    pub start_frame: u64,
    /// Interleaved `[-1.0, 1.0]` samples.
    pub samples: Vec<f32>,
}

impl AudioBuffer {
    fn validate(&self) -> Result<usize> {
        if self.sample_rate == 0 {
            return Err(Error::InvalidRequest(
                "audio sample rate must be non-zero".into(),
            ));
        }
        if !matches!(self.channels, 1 | 2) {
            return Err(Error::Unsupported {
                what: "audio channel layout".into(),
                why: format!(
                    "the recorder mixer accepts mono or stereo input, got {} channels",
                    self.channels
                ),
            });
        }
        if !self
            .samples
            .len()
            .is_multiple_of(usize::from(self.channels))
        {
            return Err(Error::InvalidRequest(
                "interleaved audio length is not divisible by its channel count".into(),
            ));
        }
        Ok(self.samples.len() / usize::from(self.channels))
    }
}

/// Stereo mixer with independent source gain.
#[derive(Debug, Clone)]
pub struct AudioMixer {
    sample_rate: u32,
    microphone_gain: f32,
    system_gain: f32,
}

impl AudioMixer {
    /// Creates a mixer at one encoder sample rate.
    #[must_use]
    pub const fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            microphone_gain: 1.0,
            system_gain: 1.0,
        }
    }

    /// Sets finite, non-negative source gains.
    ///
    /// # Errors
    ///
    /// Returns an error for NaN, infinity or negative gain.
    pub fn set_gains(&mut self, microphone: f32, system: f32) -> Result<()> {
        if !microphone.is_finite() || !system.is_finite() || microphone < 0.0 || system < 0.0 {
            return Err(Error::InvalidRequest(
                "audio gains must be finite and non-negative".into(),
            ));
        }
        self.microphone_gain = microphone;
        self.system_gain = system;
        Ok(())
    }

    /// Mixes optional timestamped sources into one stereo buffer.
    ///
    /// Gaps are preserved as silence, mono inputs are centred, and saturation is
    /// clipped rather than wrapped.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed input or a mismatched sample rate.
    pub fn mix(
        &self,
        microphone: Option<&AudioBuffer>,
        system: Option<&AudioBuffer>,
    ) -> Result<AudioBuffer> {
        let sources = [
            microphone.map(|buffer| (buffer, self.microphone_gain)),
            system.map(|buffer| (buffer, self.system_gain)),
        ];
        let mut start = u64::MAX;
        let mut end = 0_u64;
        for (buffer, _) in sources.iter().flatten() {
            let frames = buffer.validate()?;
            if buffer.sample_rate != self.sample_rate {
                return Err(Error::InvalidRequest(format!(
                    "audio source is {} Hz but the mixer is {} Hz",
                    buffer.sample_rate, self.sample_rate
                )));
            }
            start = start.min(buffer.start_frame);
            end = end.max(buffer.start_frame.saturating_add(frames as u64));
        }

        if start == u64::MAX {
            return Ok(AudioBuffer {
                sample_rate: self.sample_rate,
                channels: 2,
                start_frame: 0,
                samples: Vec::new(),
            });
        }

        let span = end - start;
        if span > MAX_MIX_FRAMES {
            return Err(Error::InvalidRequest(format!(
                "mixed audio span {span} frames exceeds the {MAX_MIX_FRAMES}-frame bound"
            )));
        }
        let frames = usize::try_from(span)
            .map_err(|_| Error::InvalidRequest("mixed audio duration exceeds memory".into()))?;
        let mut samples = vec![0.0_f32; frames.saturating_mul(2)];
        for (buffer, gain) in sources.into_iter().flatten() {
            let source_frames = buffer.samples.len() / usize::from(buffer.channels);
            let offset = usize::try_from(buffer.start_frame - start).map_err(|_| {
                Error::InvalidRequest("audio timestamp offset exceeds memory".into())
            })?;
            for frame in 0..source_frames {
                let (left, right) = if buffer.channels == 1 {
                    let sample = buffer.samples[frame];
                    (sample, sample)
                } else {
                    (buffer.samples[frame * 2], buffer.samples[frame * 2 + 1])
                };
                samples[(offset + frame) * 2] += left * gain;
                samples[(offset + frame) * 2 + 1] += right * gain;
            }
        }
        for sample in &mut samples {
            *sample = sample.clamp(-1.0, 1.0);
        }

        Ok(AudioBuffer {
            sample_rate: self.sample_rate,
            channels: 2,
            start_frame: start,
            samples,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioBuffer, AudioMixer};

    #[test]
    fn aligns_sources_and_centres_mono() {
        let microphone = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            start_frame: 0,
            samples: vec![0.25, 0.5],
        };
        let system = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            start_frame: 1,
            samples: vec![0.25, -0.25, 0.5, -0.5],
        };
        let output = AudioMixer::new(48_000)
            .mix(Some(&microphone), Some(&system))
            .unwrap();
        assert_eq!(output.start_frame, 0);
        assert_eq!(output.samples, vec![0.25, 0.25, 0.75, 0.25, 0.5, -0.5]);
    }

    #[test]
    fn clips_saturation_and_preserves_silence_gaps() {
        let first = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            start_frame: 2,
            samples: vec![0.75],
        };
        let second = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            start_frame: 2,
            samples: vec![0.75],
        };
        let output = AudioMixer::new(48_000)
            .mix(Some(&first), Some(&second))
            .unwrap();
        assert_eq!(output.samples, vec![1.0, 1.0]);
    }

    #[test]
    fn refuses_an_unbounded_timestamp_span() {
        let first = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            start_frame: 0,
            samples: vec![0.25],
        };
        let distant = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            start_frame: super::MAX_MIX_FRAMES + 1,
            samples: vec![0.25],
        };
        assert!(
            AudioMixer::new(48_000)
                .mix(Some(&first), Some(&distant))
                .is_err()
        );
    }
}
