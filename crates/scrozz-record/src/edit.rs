//! Pure video-document playback and non-destructive edit plans.

use std::time::Duration;

use scrozz_core::{Error, Result};
use scrozz_export::AnimationFormat;

use crate::{
    Recording,
    media::NativeMediaSource,
    settings::{Quality, ResolutionCap},
};

/// Metadata read from a source recording's media streams.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceMetadata {
    /// Encoded video width in pixels.
    pub width: u32,
    /// Encoded video height in pixels.
    pub height: u32,
    /// Average or declared video frame rate.
    pub fps: f64,
    /// Number of source audio channels; zero means no audio stream.
    pub audio_channels: u16,
}

impl SourceMetadata {
    /// Largest encoded edge accepted by the native media pipeline.
    pub const MAX_EDGE: u32 = 16_384;
    /// Largest declared/average frame rate accepted by the native media pipeline.
    pub const MAX_FPS: f64 = 240.0;
    /// Largest source channel count accepted for inspection.
    pub const MAX_AUDIO_CHANNELS: u16 = 32;

    /// Validates media stream metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for empty video geometry or a
    /// non-positive/non-finite frame rate.
    pub fn validate(self) -> Result<Self> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::InvalidRequest(format!(
                "video source dimensions {}x{} must have area",
                self.width, self.height
            )));
        }
        if self.width > Self::MAX_EDGE || self.height > Self::MAX_EDGE {
            return Err(Error::Unsupported {
                what: format!("{}x{} video source", self.width, self.height),
                why: format!(
                    "native media dimensions are bounded to {} pixels per edge",
                    Self::MAX_EDGE
                ),
            });
        }
        if !self.fps.is_finite() || self.fps <= 0.0 || self.fps > Self::MAX_FPS {
            return Err(Error::InvalidRequest(format!(
                "video source frame rate {} must be positive, finite, and at most {}",
                self.fps,
                Self::MAX_FPS
            )));
        }
        if self.audio_channels > Self::MAX_AUDIO_CHANNELS {
            return Err(Error::Unsupported {
                what: format!("{}-channel video source", self.audio_channels),
                why: format!(
                    "native media inspection is bounded to {} audio channels",
                    Self::MAX_AUDIO_CHANNELS
                ),
            });
        }
        Ok(self)
    }
}

/// Playback state for a video document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackState {
    /// Playback is stopped at the current position.
    #[default]
    Paused,
    /// Virtual playback advances on [`VideoDocument::tick`].
    Playing,
}

/// A loaded recording and its deterministic playback cursor.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoDocument {
    recording: Recording,
    metadata: SourceMetadata,
    duration: Duration,
    position: Duration,
    playback: PlaybackState,
}

impl VideoDocument {
    /// Inspects native media on disk and opens it with source-derived metadata.
    ///
    /// Unlike [`VideoDocument::open`], this constructor does not trust summary
    /// metadata carried beside the recording. It opens the encoded file through
    /// the platform media stack and uses the actual stream dimensions, frame
    /// rate, audio channels, and duration.
    ///
    /// # Errors
    ///
    /// Returns an error for synthetic/unplayable recording reports, missing or
    /// empty files, unsupported platform media backends, or undecodable media.
    pub fn open_native(recording: Recording) -> Result<Self> {
        let source = NativeMediaSource::open(recording.clone())?;
        Self::from_validated_recording_with_duration(
            recording,
            source.metadata(),
            source.inspection().duration,
        )
    }

    /// Opens real native recording output for editing.
    ///
    /// Partial native recordings are accepted because editing is a recovery path
    /// for salvageable media.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for synthetic output, invalid metadata,
    /// or a zero/non-representable duration.
    pub fn open(recording: Recording, metadata: SourceMetadata) -> Result<Self> {
        recording.require_native()?;
        Self::from_validated_recording(recording, metadata)
    }

    /// Opens an explicitly synthetic fixture for tests or previews.
    ///
    /// This separate constructor prevents mock output from accidentally entering
    /// the ordinary user-real path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for invalid metadata or duration.
    pub fn open_fixture(recording: Recording, metadata: SourceMetadata) -> Result<Self> {
        recording.validate()?;
        Self::from_validated_recording(recording, metadata)
    }

    fn from_validated_recording(recording: Recording, metadata: SourceMetadata) -> Result<Self> {
        let duration = Duration::try_from_secs_f64(recording.duration_secs).map_err(|_| {
            Error::InvalidRequest(format!(
                "recording duration {} cannot be represented for playback",
                recording.duration_secs
            ))
        })?;
        Self::from_validated_recording_with_duration(recording, metadata, duration)
    }

    fn from_validated_recording_with_duration(
        recording: Recording,
        metadata: SourceMetadata,
        duration: Duration,
    ) -> Result<Self> {
        metadata.validate()?;
        if duration.is_zero() {
            return Err(Error::InvalidRequest(
                "a zero-duration recording cannot be opened for editing".to_owned(),
            ));
        }
        Ok(Self {
            recording,
            metadata,
            duration,
            position: Duration::ZERO,
            playback: PlaybackState::Paused,
        })
    }

    /// Starts playback. At the end, play restarts from zero.
    pub fn play(&mut self) {
        if self.position == self.duration {
            self.position = Duration::ZERO;
        }
        self.playback = PlaybackState::Playing;
    }

    /// Pauses at the current position.
    pub fn pause(&mut self) {
        self.playback = PlaybackState::Paused;
    }

    /// Seeks to an exact position.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] instead of silently clamping a position
    /// beyond the source duration.
    pub fn seek(&mut self, position: Duration) -> Result<()> {
        if position > self.duration {
            return Err(Error::InvalidRequest(format!(
                "seek to {:.3} s exceeds video duration {:.3} s",
                position.as_secs_f64(),
                self.duration.as_secs_f64()
            )));
        }
        self.position = position;
        Ok(())
    }

    /// Advances playback by explicit virtual time.
    ///
    /// Paused documents do not move. Reaching the end clamps exactly to the
    /// duration and pauses.
    pub fn tick(&mut self, delta: Duration) {
        if self.playback == PlaybackState::Paused || delta.is_zero() {
            return;
        }
        self.position = self.position.saturating_add(delta).min(self.duration);
        if self.position == self.duration {
            self.playback = PlaybackState::Paused;
        }
    }

    /// Source recording.
    #[must_use]
    pub const fn recording(&self) -> &Recording {
        &self.recording
    }

    /// Source stream metadata.
    #[must_use]
    pub const fn metadata(&self) -> SourceMetadata {
        self.metadata
    }

    /// Source duration.
    #[must_use]
    pub const fn duration(&self) -> Duration {
        self.duration
    }

    /// Current playback cursor.
    #[must_use]
    pub const fn position(&self) -> Duration {
        self.position
    }

    /// Current playback state.
    #[must_use]
    pub const fn playback(&self) -> PlaybackState {
        self.playback
    }

    /// Validates an edit plan against this source.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an incompatible plan.
    pub fn validate_plan(&self, plan: &EditPlan) -> Result<()> {
        plan.validate(self.metadata, self.duration)
    }
}

/// A non-empty half-open source interval `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrimRange {
    /// First retained timestamp.
    pub start: Duration,
    /// Timestamp immediately after the retained media.
    pub end: Duration,
}

impl TrimRange {
    /// Creates a validated trim range within `source_duration`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] unless `start < end <=
    /// source_duration`.
    pub fn new(start: Duration, end: Duration, source_duration: Duration) -> Result<Self> {
        if start >= end {
            return Err(Error::InvalidRequest(format!(
                "trim start {:.3} s must be before end {:.3} s",
                start.as_secs_f64(),
                end.as_secs_f64()
            )));
        }
        if end > source_duration {
            return Err(Error::InvalidRequest(format!(
                "trim end {:.3} s exceeds source duration {:.3} s",
                end.as_secs_f64(),
                source_duration.as_secs_f64()
            )));
        }
        Ok(Self { start, end })
    }

    /// Full source range.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for zero duration.
    pub fn full(source_duration: Duration) -> Result<Self> {
        Self::new(Duration::ZERO, source_duration, source_duration)
    }

    /// Retained duration.
    #[must_use]
    pub fn duration(self) -> Duration {
        self.end - self.start
    }
}

/// Audio channel behavior during export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelBehavior {
    /// Keep source channel count.
    #[default]
    Preserve,
    /// Downmix a multi-channel source to one mono channel.
    StereoToMono,
}

/// Audio edits applied during video export.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioEdit {
    /// Linear gain, where `1.0` preserves source volume.
    pub volume: f32,
    /// Remove audio from the output.
    pub mute: bool,
    /// Channel mapping.
    pub channels: ChannelBehavior,
}

impl AudioEdit {
    /// Largest supported gain (200%).
    pub const MAX_VOLUME: f32 = 2.0;

    /// Validates the gain.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] unless volume is finite and in
    /// `0.0..=2.0`.
    pub fn validate(self) -> Result<Self> {
        if !self.volume.is_finite() || !(0.0..=Self::MAX_VOLUME).contains(&self.volume) {
            return Err(Error::InvalidRequest(format!(
                "audio volume {} is outside 0.0..={}",
                self.volume,
                Self::MAX_VOLUME
            )));
        }
        Ok(self)
    }

    /// Output channel count for a source.
    #[must_use]
    pub const fn output_channels(self, source_channels: u16) -> u16 {
        if self.mute || source_channels == 0 {
            0
        } else if matches!(self.channels, ChannelBehavior::StereoToMono) && source_channels > 1 {
            1
        } else {
            source_channels
        }
    }

    /// Effective linear gain.
    #[must_use]
    pub const fn effective_gain(self) -> f32 {
        if self.mute { 0.0 } else { self.volume }
    }
}

impl Default for AudioEdit {
    fn default() -> Self {
        Self {
            volume: 1.0,
            mute: false,
            channels: ChannelBehavior::Preserve,
        }
    }
}

/// Container/codec family requested by an edit plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditOutput {
    /// Platform-native video output.
    #[default]
    Video,
    /// A reusable animation format from `scrozz-export`.
    Animation(AnimationFormat),
}

impl EditOutput {
    /// Whether this output format can carry audio.
    #[must_use]
    pub const fn supports_audio(self) -> bool {
        matches!(self, Self::Video)
    }
}

/// Non-destructive operations applied by a transcoder.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EditPlan {
    /// Retained source interval.
    pub trim: TrimRange,
    /// Encoder quality rung.
    pub quality: Quality,
    /// Output dimension ceiling.
    pub resolution: ResolutionCap,
    /// Audio gain/mute/channel behavior.
    pub audio: AudioEdit,
    /// Video or animation output.
    pub output: EditOutput,
}

impl EditPlan {
    /// Creates a default full-length video plan.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a zero-duration document.
    pub fn video(document: &VideoDocument) -> Result<Self> {
        Ok(Self {
            trim: TrimRange::full(document.duration)?,
            quality: Quality::Balanced,
            resolution: ResolutionCap::Native,
            audio: AudioEdit::default(),
            output: EditOutput::Video,
        })
    }

    /// Creates a default full-length GIF plan with audio explicitly removed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a zero-duration document.
    pub fn gif(document: &VideoDocument) -> Result<Self> {
        let mut plan = Self::video(document)?;
        plan.audio.mute = true;
        plan.output = EditOutput::Animation(AnimationFormat::Gif);
        Ok(plan)
    }

    /// Validates this plan against source metadata and duration.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for trim/audio errors or a GIF plan
    /// that attempts to retain audio.
    pub fn validate(&self, metadata: SourceMetadata, source_duration: Duration) -> Result<()> {
        metadata.validate()?;
        TrimRange::new(self.trim.start, self.trim.end, source_duration)?;
        self.audio.validate()?;
        if !self.output.supports_audio() && metadata.audio_channels > 0 && !self.audio.mute {
            return Err(Error::InvalidRequest(
                "GIF output cannot carry audio; set the edit plan to mute".to_owned(),
            ));
        }
        Ok(())
    }

    /// Output dimensions after applying the resolution cap.
    #[must_use]
    pub fn output_dimensions(self, metadata: SourceMetadata) -> (u32, u32) {
        self.resolution.apply(metadata.width, metadata.height)
    }

    /// Output audio channel count. Animation formats always return zero.
    #[must_use]
    pub const fn output_audio_channels(self, metadata: SourceMetadata) -> u16 {
        if self.output.supports_audio() {
            self.audio.output_channels(metadata.audio_channels)
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> SourceMetadata {
        SourceMetadata {
            width: 3840,
            height: 2160,
            fps: 59.94,
            audio_channels: 2,
        }
    }

    fn fixture() -> VideoDocument {
        VideoDocument::open_fixture(
            Recording::synthetic("fixture.mp4", 10.0, "edit test").unwrap(),
            metadata(),
        )
        .unwrap()
    }

    #[test]
    fn normal_open_rejects_synthetic_but_fixture_open_is_explicit() {
        let recording = Recording::synthetic("fixture.mp4", 10.0, "test").unwrap();
        assert!(VideoDocument::open(recording.clone(), metadata()).is_err());
        assert!(VideoDocument::open_fixture(recording, metadata()).is_ok());
    }

    #[test]
    fn native_partial_output_remains_editable() {
        let recording =
            Recording::native_partial("partial.mp4", 4.0, "native", "trailer missing").unwrap();
        let document = VideoDocument::open(recording, metadata()).unwrap();
        assert!(document.recording().is_partial());
    }

    #[test]
    fn playback_moves_only_when_playing_and_stops_at_the_end() {
        let mut document = fixture();
        document.tick(Duration::from_secs(2));
        assert_eq!(document.position(), Duration::ZERO);
        document.play();
        document.tick(Duration::from_secs(3));
        assert_eq!(document.position(), Duration::from_secs(3));
        document.pause();
        document.tick(Duration::from_secs(3));
        assert_eq!(document.position(), Duration::from_secs(3));
        document.play();
        document.tick(Duration::from_secs(20));
        assert_eq!(document.position(), Duration::from_secs(10));
        assert_eq!(document.playback(), PlaybackState::Paused);
        document.play();
        assert_eq!(document.position(), Duration::ZERO);
    }

    #[test]
    fn seek_and_trim_reject_out_of_range_values() {
        let mut document = fixture();
        assert!(document.seek(Duration::from_secs(11)).is_err());
        assert!(document.seek(Duration::from_secs(4)).is_ok());
        assert!(
            TrimRange::new(
                Duration::from_secs(5),
                Duration::from_secs(5),
                document.duration()
            )
            .is_err()
        );
        assert!(
            TrimRange::new(
                Duration::from_secs(1),
                Duration::from_secs(11),
                document.duration()
            )
            .is_err()
        );
    }

    #[test]
    fn video_plan_applies_quality_resolution_volume_and_mono() {
        let document = fixture();
        let mut plan = EditPlan::video(&document).unwrap();
        plan.quality = Quality::High;
        plan.resolution = ResolutionCap::Fhd1080;
        plan.audio.volume = 1.5;
        plan.audio.channels = ChannelBehavior::StereoToMono;
        document.validate_plan(&plan).unwrap();
        assert_eq!(plan.output_dimensions(metadata()), (1920, 1080));
        assert_eq!(plan.output_audio_channels(metadata()), 1);
        assert_eq!(plan.audio.effective_gain(), 1.5);
    }

    #[test]
    fn gif_plan_has_no_audio_and_unmuted_gif_is_rejected() {
        let document = fixture();
        let mut plan = EditPlan::gif(&document).unwrap();
        assert_eq!(plan.output, EditOutput::Animation(AnimationFormat::Gif));
        assert_eq!(plan.output_audio_channels(metadata()), 0);
        document.validate_plan(&plan).unwrap();
        plan.audio.mute = false;
        assert!(document.validate_plan(&plan).is_err());
    }

    #[test]
    fn source_metadata_and_volume_are_validated() {
        let mut source = metadata();
        source.fps = f64::NAN;
        assert!(source.validate().is_err());
        assert!(
            AudioEdit {
                volume: 2.1,
                ..AudioEdit::default()
            }
            .validate()
            .is_err()
        );
    }
}
