//! Pure video-document playback and non-destructive edit plans.

use std::time::Duration;

use scrozz_core::{Error, Result};
pub use scrozz_export::{AnimationFormat, AnimationRepeat, GifDither};

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
    /// Software AV1 in a WebM container.
    WebM,
    /// A reusable animation format from `scrozz-export`.
    Animation(AnimationFormat),
}

impl EditOutput {
    /// Whether this output format can carry audio.
    #[must_use]
    pub const fn supports_audio(self) -> bool {
        matches!(self, Self::Video)
    }

    /// Conventional filename extension without a leading dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Video => "mp4",
            Self::WebM => "webm",
            Self::Animation(format) => format.extension(),
        }
    }

    /// IANA media type for the completed artifact.
    #[must_use]
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Video => "video/mp4",
            Self::WebM => "video/webm",
            Self::Animation(format) => format.media_type(),
        }
    }

    /// Stable codec spelling stored in capture metadata.
    #[must_use]
    pub const fn codec_slug(self) -> &'static str {
        match self {
            Self::Video => "h264",
            Self::WebM => "av1",
            Self::Animation(AnimationFormat::Gif) => "gif",
        }
    }

    /// Human-readable name for actionable validation messages.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Video => "MP4 (hardware H.264)",
            Self::WebM => "WebM (software AV1)",
            Self::Animation(AnimationFormat::Gif) => "GIF",
        }
    }
}

/// GIF-specific controls and hard resource limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GifExportSettings {
    /// Requested output frame rate. Source cadence remains the upper bound.
    pub frame_rate: u16,
    /// Playback repetition behavior.
    pub repeat: AnimationRepeat,
    /// Palette error diffusion.
    pub dither: GifDither,
}

impl GifExportSettings {
    /// Frame rates offered by the recording editor.
    pub const FRAME_RATES: [u16; 5] = [5, 10, 15, 24, 30];
    /// Default frame rate chosen for a new GIF plan.
    pub const DEFAULT_FRAME_RATE: u16 = 15;
    /// Largest accepted GIF frame rate.
    pub const MAX_FRAME_RATE: u16 = 30;
    /// Longest accepted GIF timeline.
    pub const MAX_DURATION: Duration = Duration::from_secs(120);
    /// Largest accepted GIF pixel count (1280x720).
    pub const MAX_PIXELS: u64 = 1_280 * 720;
    /// Largest accepted GIF edge.
    pub const MAX_EDGE: u32 = 1_920;
    /// Largest estimated or written GIF.
    pub const MAX_OUTPUT_BYTES: u64 = 512 * 1024 * 1024;
    /// Largest estimated live working set.
    pub const MAX_WORKING_SET_BYTES: u64 = 64 * 1024 * 1024;

    /// Effective output rate after respecting the source cadence.
    #[must_use]
    pub fn effective_frame_rate(self, source_fps: f64) -> u16 {
        let source = source_fps
            .ceil()
            .clamp(1.0, f64::from(Self::MAX_FRAME_RATE)) as u16;
        self.frame_rate.min(source)
    }
}

impl Default for GifExportSettings {
    fn default() -> Self {
        Self {
            frame_rate: Self::DEFAULT_FRAME_RATE,
            repeat: AnimationRepeat::Infinite,
            dither: GifDither::FloydSteinberg,
        }
    }
}

/// Software AV1/WebM controls and hard resource limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebmExportSettings {
    /// Requested output frame rate. Source cadence remains the upper bound.
    pub frame_rate: u16,
}

impl WebmExportSettings {
    /// Frame rates offered by the recording editor.
    pub const FRAME_RATES: [u16; 5] = [15, 24, 30, 48, 60];
    /// Default software-encoding frame rate.
    pub const DEFAULT_FRAME_RATE: u16 = 30;
    /// Largest accepted software-encoding frame rate.
    pub const MAX_FRAME_RATE: u16 = 60;
    /// Longest accepted software fallback timeline.
    pub const MAX_DURATION: Duration = Duration::from_secs(30 * 60);
    /// Largest accepted software fallback frame (3840x2160).
    pub const MAX_PIXELS: u64 = 3_840 * 2_160;
    /// Largest accepted software fallback edge.
    pub const MAX_EDGE: u32 = 3_840;
    /// Largest estimated or written WebM artifact.
    pub const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
    /// Largest estimated live working set.
    pub const MAX_WORKING_SET_BYTES: u64 = 256 * 1024 * 1024;

    /// Effective output rate after respecting the source cadence.
    #[must_use]
    pub fn effective_frame_rate(self, source_fps: f64) -> u16 {
        let source = source_fps
            .ceil()
            .clamp(1.0, f64::from(Self::MAX_FRAME_RATE)) as u16;
        self.frame_rate.min(source)
    }
}

impl Default for WebmExportSettings {
    fn default() -> Self {
        Self {
            frame_rate: Self::DEFAULT_FRAME_RATE,
        }
    }
}

/// Deterministic resource estimate shown before export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportEstimate {
    /// Expected encoded bytes, suitable for capacity planning rather than billing.
    pub output_bytes: u64,
    /// Maximum live memory attributable to frame conversion and palette work.
    pub working_set_bytes: u64,
    /// Frames the selected cadence will submit.
    pub frame_count: u64,
}

/// Aspect-preserving custom output geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputDimensions {
    /// Even encoded width.
    pub width: u32,
    /// Even encoded height.
    pub height: u32,
}

impl OutputDimensions {
    /// Creates an aspect-preserving custom size from a requested width.
    ///
    /// # Errors
    ///
    /// Returns an error when the width would upscale or cannot produce a
    /// hardware-encoder-compatible frame.
    pub fn from_width(width: u32, source: SourceMetadata) -> Result<Self> {
        source.validate()?;
        if width < 2 || width > source.width {
            return Err(Error::InvalidRequest(format!(
                "custom width {width} must be between 2 and source width {}",
                source.width
            )));
        }
        let width = nearest_even_bounded(f64::from(width), source.width).ok_or_else(|| {
            Error::InvalidRequest(
                "custom width cannot produce an even hardware-encoder dimension".to_owned(),
            )
        })?;
        let height = scaled_even(width, source.width, source.height)?;
        Self::new(width, height, source)
    }

    /// Creates an aspect-preserving custom size from a requested height.
    ///
    /// # Errors
    ///
    /// Returns an error when the height would upscale or cannot produce a
    /// hardware-encoder-compatible frame.
    pub fn from_height(height: u32, source: SourceMetadata) -> Result<Self> {
        source.validate()?;
        if height < 2 || height > source.height {
            return Err(Error::InvalidRequest(format!(
                "custom height {height} must be between 2 and source height {}",
                source.height
            )));
        }
        let height = nearest_even_bounded(f64::from(height), source.height).ok_or_else(|| {
            Error::InvalidRequest(
                "custom height cannot produce an even hardware-encoder dimension".to_owned(),
            )
        })?;
        let width = scaled_even(height, source.height, source.width)?;
        Self::new(width, height, source)
    }

    /// Validates a custom encoded size against its source.
    ///
    /// # Errors
    ///
    /// Returns an error for odd/empty/upscaled dimensions or aspect distortion.
    pub fn new(width: u32, height: u32, source: SourceMetadata) -> Result<Self> {
        source.validate()?;
        if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(Error::InvalidRequest(format!(
                "custom output {width}x{height} must use even dimensions of at least 2x2"
            )));
        }
        if width > source.width || height > source.height {
            return Err(Error::InvalidRequest(format!(
                "custom output {width}x{height} cannot upscale source {}x{}",
                source.width, source.height
            )));
        }
        let source_aspect = f64::from(source.width) / f64::from(source.height);
        let output_aspect = f64::from(width) / f64::from(height);
        let relative_error = ((output_aspect / source_aspect) - 1.0).abs();
        if relative_error > 0.02 {
            return Err(Error::InvalidRequest(format!(
                "custom output {width}x{height} must preserve source aspect ratio {}x{}",
                source.width, source.height
            )));
        }
        Ok(Self { width, height })
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn nearest_even_bounded(value: f64, maximum: u32) -> Option<u32> {
    let maximum = maximum & !1;
    if !value.is_finite() || value < 2.0 || maximum < 2 {
        return None;
    }
    let lower = ((value.floor() as u32) & !1).max(2).min(maximum);
    let upper = lower.saturating_add(2).min(maximum & !1);
    if (value - f64::from(lower)).abs() <= (value - f64::from(upper)).abs() {
        Some(lower)
    } else {
        Some(upper)
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn scaled_even(value: u32, source_axis: u32, other_axis: u32) -> Result<u32> {
    let scaled = f64::from(value) * f64::from(other_axis) / f64::from(source_axis);
    if !scaled.is_finite() || scaled < 2.0 || scaled > f64::from(u32::MAX) {
        return Err(Error::InvalidRequest(
            "custom output dimensions cannot preserve this source aspect ratio".to_owned(),
        ));
    }
    nearest_even_bounded(scaled, other_axis).ok_or_else(|| {
        Error::InvalidRequest(
            "custom output dimensions cannot produce an even hardware-encoder frame".to_owned(),
        )
    })
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
    /// Explicit aspect-preserving dimensions, overriding the named ceiling.
    pub custom_dimensions: Option<OutputDimensions>,
    /// Audio gain/mute/channel behavior.
    pub audio: AudioEdit,
    /// Video or animation output.
    pub output: EditOutput,
    /// GIF-only cadence, loop, and palette controls.
    pub gif: GifExportSettings,
    /// Software AV1/WebM cadence.
    pub webm: WebmExportSettings,
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
            custom_dimensions: None,
            audio: AudioEdit::default(),
            output: EditOutput::Video,
            gif: GifExportSettings::default(),
            webm: WebmExportSettings::default(),
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
        plan.resolution = ResolutionCap::Hd720;
        plan.custom_dimensions = bounded_dimensions(
            document.metadata(),
            plan.resolution,
            GifExportSettings::MAX_EDGE,
            GifExportSettings::MAX_PIXELS,
        )?;
        plan.output = EditOutput::Animation(AnimationFormat::Gif);
        Ok(plan)
    }

    /// Creates a software AV1/WebM plan with bounded defaults.
    ///
    /// WebM audio is intentionally disabled until a reviewed Opus encoder is
    /// available; the container is never mislabeled with AAC.
    pub fn webm(document: &VideoDocument) -> Result<Self> {
        let mut plan = Self::video(document)?;
        plan.audio.mute = true;
        plan.resolution = ResolutionCap::Fhd1080;
        plan.custom_dimensions = bounded_dimensions(
            document.metadata(),
            plan.resolution,
            WebmExportSettings::MAX_EDGE,
            WebmExportSettings::MAX_PIXELS,
        )?;
        plan.output = EditOutput::WebM;
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
        if let Some(dimensions) = self.custom_dimensions {
            OutputDimensions::new(dimensions.width, dimensions.height, metadata)?;
        }
        if !self.output.supports_audio() && metadata.audio_channels > 0 && !self.audio.mute {
            return Err(Error::InvalidRequest(format!(
                "{} output cannot carry audio in this build; set the edit plan to mute",
                self.output.label()
            )));
        }
        let estimate = self.export_estimate(metadata);
        match self.output {
            EditOutput::Animation(AnimationFormat::Gif) => {
                validate_bounded_output(
                    "GIF",
                    self.trim.duration(),
                    self.gif.frame_rate,
                    self.output_dimensions(metadata),
                    estimate,
                    GifExportSettings::MAX_DURATION,
                    GifExportSettings::MAX_FRAME_RATE,
                    GifExportSettings::MAX_EDGE,
                    GifExportSettings::MAX_PIXELS,
                    GifExportSettings::MAX_OUTPUT_BYTES,
                    GifExportSettings::MAX_WORKING_SET_BYTES,
                )?;
            }
            EditOutput::WebM => {
                validate_bounded_output(
                    "software AV1/WebM",
                    self.trim.duration(),
                    self.webm.frame_rate,
                    self.output_dimensions(metadata),
                    estimate,
                    WebmExportSettings::MAX_DURATION,
                    WebmExportSettings::MAX_FRAME_RATE,
                    WebmExportSettings::MAX_EDGE,
                    WebmExportSettings::MAX_PIXELS,
                    WebmExportSettings::MAX_OUTPUT_BYTES,
                    WebmExportSettings::MAX_WORKING_SET_BYTES,
                )?;
            }
            EditOutput::Video => {}
        }
        Ok(())
    }

    /// Output dimensions after applying the resolution cap.
    #[must_use]
    pub fn output_dimensions(self, metadata: SourceMetadata) -> (u32, u32) {
        self.custom_dimensions.map_or_else(
            || self.resolution.apply(metadata.width, metadata.height),
            |dimensions| (dimensions.width, dimensions.height),
        )
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

    /// Effective GIF cadence after respecting source and product caps.
    #[must_use]
    pub fn gif_frame_rate(self, metadata: SourceMetadata) -> u16 {
        self.gif.effective_frame_rate(metadata.fps)
    }

    /// Effective software AV1 cadence after respecting source and product caps.
    #[must_use]
    pub fn webm_frame_rate(self, metadata: SourceMetadata) -> u16 {
        self.webm.effective_frame_rate(metadata.fps)
    }

    /// Estimates output size, working memory, and submitted frames.
    #[must_use]
    pub fn export_estimate(self, metadata: SourceMetadata) -> ExportEstimate {
        let (width, height) = self.output_dimensions(metadata);
        let pixels = u64::from(width).saturating_mul(u64::from(height));
        let duration = self
            .trim
            .end
            .checked_sub(self.trim.start)
            .unwrap_or_default();
        let duration_millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        let frame_rate = match self.output {
            EditOutput::Video => metadata.fps.round().clamp(1.0, SourceMetadata::MAX_FPS) as u64,
            EditOutput::WebM => u64::from(self.webm_frame_rate(metadata)),
            EditOutput::Animation(AnimationFormat::Gif) => u64::from(self.gif_frame_rate(metadata)),
        };
        let frame_count = duration_millis
            .saturating_mul(frame_rate)
            .saturating_add(999)
            / 1_000;
        let output_bytes = match self.output {
            EditOutput::Video | EditOutput::WebM => {
                let video_bps = self
                    .quality
                    .target_bitrate(width, height, frame_rate as u32);
                let audio_bps = if self.output_audio_channels(metadata) == 0 {
                    0
                } else {
                    192_000
                };
                (video_bps.saturating_add(audio_bps)).saturating_mul(duration_millis) / 8_000
            }
            EditOutput::Animation(AnimationFormat::Gif) => {
                let encoded_percent = match self.quality {
                    Quality::Low => 18,
                    Quality::Balanced => 28,
                    Quality::High => 40,
                };
                pixels
                    .saturating_mul(frame_count)
                    .saturating_mul(encoded_percent)
                    / 100
            }
        };
        let frame_bytes = pixels.saturating_mul(4);
        let working_set_bytes = match self.output {
            EditOutput::Video => frame_bytes.saturating_mul(2),
            EditOutput::WebM => frame_bytes
                .saturating_mul(3)
                .saturating_add(pixels.saturating_mul(3) / 2),
            EditOutput::Animation(AnimationFormat::Gif) => {
                frame_bytes.saturating_mul(3).saturating_add(pixels)
            }
        };
        ExportEstimate {
            output_bytes,
            working_set_bytes,
            frame_count,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_bounded_output(
    label: &str,
    duration: Duration,
    frame_rate: u16,
    dimensions: (u32, u32),
    estimate: ExportEstimate,
    max_duration: Duration,
    max_frame_rate: u16,
    max_edge: u32,
    max_pixels: u64,
    max_output_bytes: u64,
    max_working_set_bytes: u64,
) -> Result<()> {
    if duration > max_duration {
        return Err(Error::Unsupported {
            what: format!(
                "{label} export lasting {:.1} seconds",
                duration.as_secs_f64()
            ),
            why: format!(
                "the bounded exporter allows at most {} seconds; shorten the trim",
                max_duration.as_secs()
            ),
        });
    }
    if frame_rate == 0 || frame_rate > max_frame_rate {
        return Err(Error::InvalidRequest(format!(
            "{label} frame rate must be between 1 and {max_frame_rate}, got {frame_rate}"
        )));
    }
    let pixels = u64::from(dimensions.0).saturating_mul(u64::from(dimensions.1));
    if dimensions.0 > max_edge || dimensions.1 > max_edge || pixels > max_pixels {
        return Err(Error::Unsupported {
            what: format!("{label} output at {}x{}", dimensions.0, dimensions.1),
            why: format!(
                "the bounded exporter allows at most {max_edge} pixels per edge and {max_pixels} pixels per frame; choose a smaller resolution"
            ),
        });
    }

    if estimate.output_bytes > max_output_bytes {
        return Err(Error::Unsupported {
            what: format!(
                "{label} export estimated at {} MiB",
                estimate.output_bytes / (1024 * 1024)
            ),
            why: format!(
                "the staged output cap is {} MiB; reduce duration, frame rate, resolution, or quality",
                max_output_bytes / (1024 * 1024)
            ),
        });
    }
    if estimate.working_set_bytes > max_working_set_bytes {
        return Err(Error::Unsupported {
            what: format!(
                "{label} working set estimated at {} MiB",
                estimate.working_set_bytes / (1024 * 1024)
            ),
            why: format!(
                "the exporter working-set cap is {} MiB; choose a smaller resolution",
                max_working_set_bytes / (1024 * 1024)
            ),
        });
    }
    Ok(())
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn bounded_dimensions(
    metadata: SourceMetadata,
    resolution: ResolutionCap,
    max_edge: u32,
    max_pixels: u64,
) -> Result<Option<OutputDimensions>> {
    let (width, height) = resolution.apply(metadata.width, metadata.height);
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width <= max_edge && height <= max_edge && pixels <= max_pixels {
        return Ok(None);
    }
    let edge_scale = f64::from(max_edge) / f64::from(width.max(height));
    let pixel_scale = (max_pixels as f64 / pixels as f64).sqrt();
    let scale = edge_scale.min(pixel_scale).min(1.0);
    let target_width = (f64::from(width) * scale).floor() as u32 & !1;
    let target_height = (f64::from(height) * scale).floor() as u32 & !1;
    OutputDimensions::new(target_width.max(2), target_height.max(2), metadata).map(Some)
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
    fn custom_dimensions_preserve_aspect_and_never_upscale() {
        let source = metadata();
        assert_eq!(
            OutputDimensions::from_width(1920, source).unwrap(),
            OutputDimensions {
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(
            OutputDimensions::from_height(720, source).unwrap(),
            OutputDimensions {
                width: 1280,
                height: 720
            }
        );
        assert_eq!(
            OutputDimensions::from_width(70, source).unwrap(),
            OutputDimensions {
                width: 70,
                height: 40
            }
        );
        assert!(OutputDimensions::from_width(4_000, source).is_err());
        assert!(OutputDimensions::new(1281, 720, source).is_err());
        assert!(OutputDimensions::new(1280, 700, source).is_err());
        assert!(
            OutputDimensions::from_width(
                6,
                SourceMetadata {
                    width: 1920,
                    height: 1080,
                    fps: 30.0,
                    audio_channels: 0,
                }
            )
            .is_err()
        );

        let document = fixture();
        let mut plan = EditPlan::video(&document).unwrap();
        plan.custom_dimensions = Some(OutputDimensions::from_width(1920, source).unwrap());
        assert_eq!(plan.output_dimensions(source), (1920, 1080));
        document.validate_plan(&plan).unwrap();
    }

    #[test]
    fn gif_plan_has_no_audio_and_unmuted_gif_is_rejected() {
        let document = fixture();
        let mut plan = EditPlan::gif(&document).unwrap();
        assert_eq!(plan.output, EditOutput::Animation(AnimationFormat::Gif));
        assert_eq!(plan.output.extension(), "gif");
        assert_eq!(plan.output.media_type(), "image/gif");
        assert_eq!(plan.output.codec_slug(), "gif");
        assert_eq!(plan.output_dimensions(metadata()), (1280, 720));
        assert_eq!(plan.gif_frame_rate(metadata()), 15);
        assert_eq!(plan.export_estimate(metadata()).frame_count, 150);
        assert_eq!(plan.output_audio_channels(metadata()), 0);
        document.validate_plan(&plan).unwrap();
        plan.audio.mute = false;
        assert!(document.validate_plan(&plan).is_err());
    }

    #[test]
    fn gif_guards_reject_oversized_fast_and_long_exports() {
        let document = fixture();
        let mut plan = EditPlan::gif(&document).unwrap();
        plan.resolution = ResolutionCap::Native;
        assert!(
            document
                .validate_plan(&plan)
                .unwrap_err()
                .to_string()
                .contains("smaller resolution")
        );

        plan.resolution = ResolutionCap::Hd720;
        plan.gif.frame_rate = GifExportSettings::MAX_FRAME_RATE + 1;
        assert!(
            document
                .validate_plan(&plan)
                .unwrap_err()
                .to_string()
                .contains("frame rate")
        );

        let long = VideoDocument::open_fixture(
            Recording::synthetic("long.mp4", 121.0, "long GIF guard").unwrap(),
            SourceMetadata {
                width: 640,
                height: 360,
                fps: 30.0,
                audio_channels: 0,
            },
        )
        .unwrap();
        let long_plan = EditPlan::gif(&long).unwrap();
        assert!(
            long.validate_plan(&long_plan)
                .unwrap_err()
                .to_string()
                .contains("shorten the trim")
        );

        let ultrawide = VideoDocument::open_fixture(
            Recording::synthetic("ultrawide.mp4", 5.0, "safe GIF defaults").unwrap(),
            SourceMetadata {
                width: 5_120,
                height: 1_440,
                fps: 60.0,
                audio_channels: 0,
            },
        )
        .unwrap();
        let safe = EditPlan::gif(&ultrawide).unwrap();
        ultrawide.validate_plan(&safe).unwrap();
        let dimensions = safe.output_dimensions(ultrawide.metadata());
        assert!(dimensions.0 <= GifExportSettings::MAX_EDGE);
        assert!(u64::from(dimensions.0) * u64::from(dimensions.1) <= GifExportSettings::MAX_PIXELS);
    }

    #[test]
    fn webm_is_an_explicit_bounded_silent_av1_choice() {
        let document = fixture();
        let plan = EditPlan::webm(&document).unwrap();
        assert_eq!(plan.output, EditOutput::WebM);
        assert_eq!(plan.output.extension(), "webm");
        assert_eq!(plan.output.media_type(), "video/webm");
        assert_eq!(plan.output.codec_slug(), "av1");
        assert_eq!(plan.output_audio_channels(metadata()), 0);
        assert_eq!(plan.webm_frame_rate(metadata()), 30);
        document.validate_plan(&plan).unwrap();
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
