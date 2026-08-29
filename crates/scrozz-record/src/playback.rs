//! Bounded native playback for recording previews.
//!
//! The platform player owns the media clock and audio output. A separate worker
//! pulls decoded video frames from [`crate::media::NativeMediaSource`] into a
//! small queue, then publishes only the frame covering the native clock's
//! current timestamp. This keeps audio and video on one authoritative timeline
//! without decoding on the UI thread or retaining an entire recording.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use scrozz_core::{Error, Result};

use crate::{
    edit::{EditPlan, PlaybackState, SourceMetadata, TrimRange, VideoDocument},
    media::{
        DecodedAudioChunk, DecodedMediaSample, DecodedVideoFrame, NativeMediaDecoder,
        NativeMediaSource,
    },
};

#[cfg(target_os = "macos")]
#[path = "playback/macos.rs"]
mod platform;

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::{path::Path, time::Duration};

    use scrozz_core::{Error, Result};

    use crate::edit::EditPlan;

    pub(super) const BACKEND_NAME: &str = if cfg!(target_os = "windows") {
        "Windows Media Foundation playback"
    } else if cfg!(target_os = "linux") {
        "linked FFmpeg/desktop audio playback"
    } else {
        "native recording playback"
    };
    pub(super) const AVAILABLE: bool = false;
    pub(super) const UNAVAILABLE_REASON: Option<&str> = Some(if cfg!(target_os = "windows") {
        "the Media Foundation source-reader and audio-renderer adapters are not included yet"
    } else if cfg!(target_os = "linux") {
        "the linked libav decoder and native desktop audio adapters are not included yet"
    } else {
        "this target has no native recording playback adapter"
    });

    pub(super) struct Clock;

    pub(super) struct Observation {
        pub(super) position: Option<Duration>,
        pub(super) running: bool,
        pub(super) buffering: bool,
        pub(super) seeking: bool,
        pub(super) audio_frames_rendered: u64,
    }

    impl Clock {
        pub(super) fn open(_path: &Path, _has_audio: bool, _plan: EditPlan) -> Result<Self> {
            Err(unavailable())
        }

        pub(super) fn configure(&self, _plan: EditPlan) -> Result<()> {
            Err(unavailable())
        }

        pub(super) fn play(&self, _rate: f32) -> Result<()> {
            Err(unavailable())
        }

        pub(super) fn pause(&self) {}

        pub(super) fn seek(&mut self, _position: Duration) -> Result<()> {
            Err(unavailable())
        }

        pub(super) fn observe(&mut self) -> Result<Observation> {
            Err(unavailable())
        }

        pub(super) const fn seeking(&self) -> bool {
            false
        }
    }

    fn unavailable() -> Error {
        Error::Unsupported {
            what: "recording preview playback".to_owned(),
            why: UNAVAILABLE_REASON
                .expect("an unavailable adapter has a reason")
                .to_owned(),
        }
    }
}

/// Longest recording accepted by the interactive preview.
///
/// Export remains streaming and may support a longer source. The editor refuses
/// a source beyond this bound instead of allowing timestamp conversion and seek
/// arithmetic to become unbounded.
pub const MAX_PREVIEW_DURATION: Duration = Duration::from_secs(48 * 60 * 60);

/// Largest decoded edge retained by the editor.
pub const MAX_PREVIEW_EDGE: u32 = 960;

/// Maximum decoded frames retained by the playback worker.
pub const MAX_BUFFERED_VIDEO_FRAMES: usize = 8;

/// Maximum bytes accepted for one decoded preview frame.
pub const MAX_PREVIEW_FRAME_BYTES: usize = 16 * 1024 * 1024;

const MAX_AUDIO_SAMPLE_RATE: u32 = 384_000;
const MAX_AUDIO_CHUNK_DURATION: Duration = Duration::from_secs(2);
const MAX_DECODE_SAMPLES_PER_PASS: usize = 64;
const WORKER_IDLE_WAIT: Duration = Duration::from_millis(20);
const END_TOLERANCE: Duration = Duration::from_millis(2);
const MAX_AV_DRIFT: Duration = Duration::from_millis(50);
static NEXT_PLAYBACK_STREAM: AtomicU64 = AtomicU64::new(1);

/// Runtime capabilities for decoded recording playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativePlaybackCapabilities {
    /// Native clock and audio backend.
    pub backend: &'static str,
    /// Whether decoded video preview is implemented.
    pub decoded_video: bool,
    /// Whether captured audio is rendered.
    pub audio_output: bool,
    /// Whether seek, pause/resume, and variable rate are implemented.
    pub transport: bool,
    /// Explicit implementation gap, when unavailable.
    pub unavailable_reason: Option<&'static str>,
}

/// Reports native recording-preview support without opening media.
#[must_use]
pub const fn native_playback_capabilities() -> NativePlaybackCapabilities {
    NativePlaybackCapabilities {
        backend: platform::BACKEND_NAME,
        decoded_video: platform::AVAILABLE,
        audio_output: platform::AVAILABLE,
        transport: platform::AVAILABLE,
        unavailable_reason: platform::UNAVAILABLE_REASON,
    }
}

/// Observable playback lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackPhase {
    /// Native player or first decoded frame is not ready yet.
    #[default]
    Loading,
    /// Playback is stopped at the current position.
    Paused,
    /// The native clock is advancing.
    Playing,
    /// Native media is temporarily waiting for local buffered data.
    Buffering,
    /// The edit plan's trim end was reached.
    Ended,
    /// Playback failed and the source remains untouched.
    Failed,
}

/// How the current edit plan treats captured audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackAudio {
    /// The source contains no audio stream.
    NoTrack,
    /// Audio is explicitly removed by mute or the selected output format.
    Muted,
    /// Captured audio is rendered by the native player.
    Active,
}

/// One decoded frame published to the UI.
#[derive(Clone)]
pub struct PlaybackFrame {
    /// Monotonic sequence used to avoid redundant texture uploads.
    pub sequence: u64,
    /// Decoded frame pixels and source timestamp.
    pub frame: Arc<DecodedVideoFrame>,
    /// Exclusive source timestamp through which this frame remains visible.
    pub display_until: Duration,
}

impl std::fmt::Debug for PlaybackFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlaybackFrame")
            .field("sequence", &self.sequence)
            .field("timestamp", &self.frame.timestamp)
            .field("duration", &self.frame.duration)
            .field("display_until", &self.display_until)
            .field(
                "dimensions",
                &(self.frame.image.width, self.frame.image.height),
            )
            .field("bytes", &self.frame.image.data.len())
            .finish()
    }
}

/// Small, cloneable playback state suitable for crossing the app/UI seam.
#[derive(Debug, Clone)]
pub struct PlaybackSnapshot {
    /// Process-local playback identity used to separate texture streams.
    pub stream_id: u64,
    /// Current lifecycle.
    pub phase: PlaybackPhase,
    /// Authoritative source position.
    pub position: Duration,
    /// Requested playback rate.
    pub rate: f32,
    /// Most recent decoded frame.
    pub frame: Option<PlaybackFrame>,
    /// Source timestamp through which video is decoded.
    pub buffered_until: Duration,
    /// Number of decoded frames currently retained.
    pub buffered_frames: usize,
    /// Number of stale frames deliberately skipped to catch the media clock.
    pub dropped_frames: u64,
    /// Distance from the media clock to the displayed frame interval.
    pub av_drift: Option<Duration>,
    /// Captured-audio behavior.
    pub audio: PlaybackAudio,
    /// PCM frames delivered by the native audio renderer.
    pub audio_frames_rendered: u64,
    /// Explicit terminal failure.
    pub error: Option<String>,
}

impl PlaybackSnapshot {
    fn initial(stream_id: u64, plan: EditPlan, metadata: SourceMetadata) -> Self {
        Self {
            stream_id,
            phase: PlaybackPhase::Loading,
            position: plan.trim.start,
            rate: 1.0,
            frame: None,
            buffered_until: plan.trim.start,
            buffered_frames: 0,
            dropped_frames: 0,
            av_drift: None,
            audio: playback_audio(plan, metadata),
            audio_frames_rendered: 0,
            error: None,
        }
    }
}

/// Native recording preview with deterministic worker ownership.
pub struct NativePlayback {
    clock: platform::Clock,
    shared: Arc<SharedDecode>,
    worker: Option<JoinHandle<()>>,
    plan: EditPlan,
    metadata: SourceMetadata,
    snapshot: PlaybackSnapshot,
    desired_playing: bool,
    clock_suspended_for_video: bool,
    stopped: bool,
}

impl NativePlayback {
    /// Opens a real recording for decoded preview and native audio playback.
    ///
    /// The source is never modified. Video decode happens on a named worker;
    /// the platform clock/audio renderer stays on the caller thread.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported platforms, synthetic/corrupt media,
    /// unsafe source bounds, an invalid edit plan, or native player startup
    /// failure.
    pub fn open(document: &VideoDocument, plan: EditPlan) -> Result<Self> {
        document.validate_plan(&plan)?;
        validate_source_bounds(document.metadata(), document.duration())?;
        if !platform::AVAILABLE {
            return Err(Error::Unsupported {
                what: "recording preview playback".to_owned(),
                why: platform::UNAVAILABLE_REASON
                    .expect("an unavailable adapter has a reason")
                    .to_owned(),
            });
        }

        let source = NativeMediaSource::open(document.recording().clone())?;
        validate_document_source(document, &source)?;
        let dimensions = preview_dimensions(plan, source.metadata())?;
        let mut clock =
            platform::Clock::open(source.path(), source.metadata().audio_channels > 0, plan)?;
        clock.seek(plan.trim.start)?;
        clock.pause();

        let control = DecodeControl {
            generation: 1,
            cursor: plan.trim.start,
            plan,
            dimensions,
            playing: false,
            shutdown: false,
        };
        let shared = Arc::new(SharedDecode::new(control));
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("scrozz-recording-preview".to_owned())
            .spawn(move || run_decode_worker(source, worker_shared))
            .map_err(|error| {
                Error::Platform(format!("could not start recording preview worker: {error}"))
            })?;
        let stream_id = NEXT_PLAYBACK_STREAM.fetch_add(1, Ordering::Relaxed);

        Ok(Self {
            clock,
            shared,
            worker: Some(worker),
            plan,
            metadata: document.metadata(),
            snapshot: PlaybackSnapshot::initial(stream_id, plan, document.metadata()),
            desired_playing: false,
            clock_suspended_for_video: false,
            stopped: false,
        })
    }

    /// Starts or resumes playback. Replaying an ended trim restarts at trim-in.
    ///
    /// # Errors
    ///
    /// Returns an error if the native player refuses the seek or rate.
    pub fn play(&mut self) -> Result<()> {
        self.ensure_live()?;
        if self.snapshot.phase == PlaybackPhase::Failed {
            return Err(Error::Platform(
                self.snapshot
                    .error
                    .clone()
                    .unwrap_or_else(|| "recording preview has failed".to_owned()),
            ));
        }
        if self.snapshot.position >= self.plan.trim.end {
            self.seek(self.plan.trim.start)?;
        }
        if !self.clock.seeking()
            && frame_is_ready(self.snapshot.frame.as_ref(), self.snapshot.position)
        {
            self.clock.play(self.snapshot.rate)?;
            self.clock_suspended_for_video = false;
            self.snapshot.phase = PlaybackPhase::Playing;
        } else {
            self.clock.pause();
            self.clock_suspended_for_video = true;
            self.snapshot.phase = PlaybackPhase::Buffering;
        }
        self.desired_playing = true;
        self.shared.set_playing(true);
        Ok(())
    }

    /// Pauses at the native clock's current position.
    pub fn pause(&mut self) {
        self.clock.pause();
        self.desired_playing = false;
        self.clock_suspended_for_video = false;
        if self.snapshot.phase != PlaybackPhase::Failed {
            self.snapshot.phase = PlaybackPhase::Paused;
        }
        self.shared.set_playing(false);
    }

    /// Seeks inside the retained trim interval.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] outside the current trim or a platform
    /// error if the native clock rejects the exact seek.
    pub fn seek(&mut self, position: Duration) -> Result<()> {
        self.ensure_live()?;
        if position < self.plan.trim.start || position > self.plan.trim.end {
            return Err(Error::InvalidRequest(format!(
                "preview seek {:.3} s is outside trim {:.3}..{:.3} s",
                position.as_secs_f64(),
                self.plan.trim.start.as_secs_f64(),
                self.plan.trim.end.as_secs_f64()
            )));
        }
        let resume_clock = self.desired_playing && !self.clock_suspended_for_video;
        self.clock.pause();
        if let Err(seek_error) = self.clock.seek(position) {
            return match resume_clock.then(|| self.clock.play(self.snapshot.rate)) {
                None | Some(Ok(())) => Err(seek_error),
                Some(Err(resume_error)) => Err(Error::Platform(format!(
                    "seeking recording preview failed: {seek_error}; resuming its previous position also failed: {resume_error}"
                ))),
            };
        }
        self.clock_suspended_for_video = self.desired_playing;
        self.snapshot.position = position;
        self.snapshot.phase = if position == self.plan.trim.end {
            PlaybackPhase::Ended
        } else if self.desired_playing {
            PlaybackPhase::Buffering
        } else {
            PlaybackPhase::Loading
        };
        self.shared
            .reconfigure(position, self.plan, self.desired_playing);
        Ok(())
    }

    /// Changes playback rate within the native, pitch-correct range.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] unless `rate` is finite and in
    /// `0.5..=2.0`, or a platform error if the player refuses it.
    pub fn set_rate(&mut self, rate: f32) -> Result<()> {
        self.ensure_live()?;
        if !rate.is_finite() || !(0.5..=2.0).contains(&rate) {
            return Err(Error::InvalidRequest(format!(
                "preview playback rate {rate} is outside 0.5..=2.0"
            )));
        }
        if self.desired_playing && !self.clock_suspended_for_video && !self.clock.seeking() {
            self.clock.play(rate)?;
        }
        self.snapshot.rate = rate;
        Ok(())
    }

    /// Applies the exact non-destructive plan used by export.
    ///
    /// Trim, resolution, mute, gain, and mono behavior are updated together.
    /// A cursor outside the new trim moves to trim-in.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid plan, unsafe preview dimensions, or
    /// native player reconfiguration failure.
    pub fn set_plan(&mut self, document: &VideoDocument, plan: EditPlan) -> Result<()> {
        self.ensure_live()?;
        document.validate_plan(&plan)?;
        let dimensions = preview_dimensions(plan, self.metadata)?;
        let resume_old_clock = self.desired_playing && !self.clock_suspended_for_video;
        self.clock.pause();
        if let Err(error) = self.clock.configure(plan) {
            let recovery = self.clock.configure(self.plan).and_then(|()| {
                if resume_old_clock {
                    self.clock.play(self.snapshot.rate)
                } else {
                    Ok(())
                }
            });
            return match recovery {
                Ok(()) => Err(error),
                Err(recovery_error) => Err(Error::Platform(format!(
                    "changing the native preview plan failed: {error}; restoring its previous plan also failed: {recovery_error}"
                ))),
            };
        }
        let position =
            if self.snapshot.position < plan.trim.start || self.snapshot.position > plan.trim.end {
                plan.trim.start
            } else {
                self.snapshot.position
            };
        if position != self.snapshot.position
            && let Err(seek_error) = self.clock.seek(position)
        {
            let recovery = self.clock.configure(self.plan).and_then(|()| {
                if resume_old_clock {
                    self.clock.play(self.snapshot.rate)
                } else {
                    Ok(())
                }
            });
            return match recovery {
                Ok(()) => Err(seek_error),
                Err(recovery_error) => Err(Error::Platform(format!(
                    "changing preview trim failed: {seek_error}; restoring the previous native playback plan also failed: {recovery_error}"
                ))),
            };
        }
        self.clock_suspended_for_video = self.desired_playing;
        self.plan = plan;
        self.snapshot.position = position;
        self.snapshot.audio = playback_audio(plan, self.metadata);
        self.snapshot.phase = if self.desired_playing {
            PlaybackPhase::Buffering
        } else {
            PlaybackPhase::Loading
        };
        self.shared
            .reconfigure_with_dimensions(position, plan, dimensions, self.desired_playing);
        Ok(())
    }

    /// Polls native time and the latest decoded frame without blocking.
    ///
    /// # Errors
    ///
    /// Returns a platform failure from the native player. The same failure is
    /// retained in [`PlaybackSnapshot`] and subsequent polling remains safe.
    pub fn poll(&mut self) -> Result<&PlaybackSnapshot> {
        self.ensure_live()?;
        if self.snapshot.phase == PlaybackPhase::Failed {
            return Ok(&self.snapshot);
        }

        let observation = match self.clock.observe() {
            Ok(observation) => observation,
            Err(error) => {
                self.fail(error.to_string());
                return Err(error);
            }
        };
        if let Some(position) = observation.position {
            self.snapshot.position = position.clamp(self.plan.trim.start, self.plan.trim.end);
        }
        self.snapshot.audio_frames_rendered = observation.audio_frames_rendered;

        self.shared
            .set_cursor(self.snapshot.position, self.desired_playing);
        let decoded = self.shared.output();
        if let Some(error) = decoded.error {
            self.fail(error.clone());
            return Err(Error::Codec(error));
        }
        self.snapshot.frame = decoded.frame;
        self.snapshot.buffered_until = decoded.buffered_until;
        self.snapshot.buffered_frames = decoded.buffered_frames;
        self.snapshot.dropped_frames = decoded.dropped_frames;
        self.snapshot.av_drift = self
            .snapshot
            .frame
            .as_ref()
            .map(|frame| frame_drift(frame, self.snapshot.position));
        if self.snapshot.frame.is_some() {
            self.snapshot.error = None;
        }

        if observation.seeking {
            self.snapshot.phase = if self.desired_playing {
                PlaybackPhase::Buffering
            } else {
                PlaybackPhase::Loading
            };
        } else if self.desired_playing
            && self.snapshot.position.saturating_add(END_TOLERANCE) >= self.plan.trim.end
        {
            self.clock.pause();
            self.desired_playing = false;
            self.clock_suspended_for_video = false;
            self.snapshot.position = self.plan.trim.end;
            self.snapshot.phase = PlaybackPhase::Ended;
            self.shared.set_playing(false);
        } else if self.desired_playing
            && !frame_is_ready(self.snapshot.frame.as_ref(), self.snapshot.position)
        {
            if !self.clock_suspended_for_video {
                self.clock.pause();
                self.clock_suspended_for_video = true;
            }
            self.snapshot.phase = PlaybackPhase::Buffering;
        } else if self.desired_playing && self.clock_suspended_for_video {
            if let Err(error) = self.clock.play(self.snapshot.rate) {
                self.fail(error.to_string());
                return Err(error);
            }
            self.clock_suspended_for_video = false;
            self.snapshot.phase = PlaybackPhase::Buffering;
        } else if self.desired_playing {
            self.snapshot.phase = if observation.buffering || !observation.running {
                PlaybackPhase::Buffering
            } else {
                PlaybackPhase::Playing
            };
        } else if self.snapshot.phase == PlaybackPhase::Loading && self.snapshot.frame.is_some() {
            self.snapshot.phase = PlaybackPhase::Paused;
        } else if self.snapshot.phase != PlaybackPhase::Loading {
            self.snapshot.phase = if self.snapshot.position == self.plan.trim.end {
                PlaybackPhase::Ended
            } else {
                PlaybackPhase::Paused
            };
        }
        Ok(&self.snapshot)
    }

    /// Last observed state.
    #[must_use]
    pub const fn snapshot(&self) -> &PlaybackSnapshot {
        &self.snapshot
    }

    /// Stops the decoder, joins its worker, and silences native audio.
    ///
    /// This method is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error if the decode worker panicked.
    pub fn shutdown(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        self.clock.pause();
        self.desired_playing = false;
        self.clock_suspended_for_video = false;
        self.shared.shutdown();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            return Err(Error::Platform(
                "recording preview worker panicked during shutdown".to_owned(),
            ));
        }
        Ok(())
    }

    fn ensure_live(&self) -> Result<()> {
        if self.stopped {
            Err(Error::InvalidRequest(
                "recording preview is already shut down".to_owned(),
            ))
        } else {
            Ok(())
        }
    }

    fn fail(&mut self, error: String) {
        self.clock.pause();
        self.desired_playing = false;
        self.clock_suspended_for_video = false;
        self.shared.set_playing(false);
        self.snapshot.phase = PlaybackPhase::Failed;
        self.snapshot.error = Some(error);
    }
}

impl Drop for NativePlayback {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            tracing::error!(%error, "recording preview did not shut down cleanly");
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DecodeControl {
    generation: u64,
    cursor: Duration,
    plan: EditPlan,
    dimensions: (u32, u32),
    playing: bool,
    shutdown: bool,
}

#[derive(Debug, Clone)]
struct DecodeOutput {
    generation: u64,
    frame: Option<PlaybackFrame>,
    buffered_until: Duration,
    buffered_frames: usize,
    dropped_frames: u64,
    error: Option<String>,
}

impl DecodeOutput {
    fn initial(control: DecodeControl) -> Self {
        Self {
            generation: control.generation,
            frame: None,
            buffered_until: control.cursor,
            buffered_frames: 0,
            dropped_frames: 0,
            error: None,
        }
    }
}

struct SharedDecode {
    control: Mutex<DecodeControl>,
    output: Mutex<DecodeOutput>,
    changed: Condvar,
}

impl SharedDecode {
    fn new(control: DecodeControl) -> Self {
        Self {
            control: Mutex::new(control),
            output: Mutex::new(DecodeOutput::initial(control)),
            changed: Condvar::new(),
        }
    }

    fn control(&self) -> DecodeControl {
        *self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn output(&self) -> DecodeOutput {
        self.output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_cursor(&self, cursor: Duration, playing: bool) {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if control.cursor == cursor && control.playing == playing {
            return;
        }
        control.cursor = cursor;
        control.playing = playing;
        self.changed.notify_one();
    }

    fn set_playing(&self, playing: bool) {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        control.playing = playing;
        self.changed.notify_one();
    }

    fn reconfigure(&self, cursor: Duration, plan: EditPlan, playing: bool) {
        let dimensions = self.control().dimensions;
        self.reconfigure_with_dimensions(cursor, plan, dimensions, playing);
    }

    fn reconfigure_with_dimensions(
        &self,
        cursor: Duration,
        plan: EditPlan,
        dimensions: (u32, u32),
        playing: bool,
    ) {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        control.generation = control.generation.wrapping_add(1).max(1);
        control.cursor = cursor;
        control.plan = plan;
        control.dimensions = dimensions;
        control.playing = playing;
        let next = *control;
        drop(control);
        *self
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = DecodeOutput::initial(next);
        self.changed.notify_one();
    }

    fn shutdown(&self) {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        control.shutdown = true;
        self.changed.notify_all();
    }

    fn wait(&self, generation: u64) {
        let control = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if control.shutdown || control.generation != generation {
            return;
        }
        let _guard = self
            .changed
            .wait_timeout(control, WORKER_IDLE_WAIT)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    fn wait_for_generation_change(&self, generation: u64) -> bool {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !control.shutdown && control.generation == generation {
            control = self
                .changed
                .wait(control)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        !control.shutdown
    }

    fn publish(
        &self,
        generation: u64,
        frame: Option<(Arc<DecodedVideoFrame>, Duration)>,
        buffered_until: Duration,
        buffered_frames: usize,
        dropped_frames: u64,
        sequence: &mut u64,
    ) {
        let mut output = self
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if output.generation != generation {
            return;
        }
        if let Some((frame, display_until)) = frame {
            let changed = output.frame.as_ref().is_none_or(|current| {
                current.frame.timestamp != frame.timestamp || current.display_until != display_until
            });
            if changed {
                *sequence = sequence.saturating_add(1);
                output.frame = Some(PlaybackFrame {
                    sequence: *sequence,
                    frame,
                    display_until,
                });
            }
        }
        output.buffered_until = buffered_until;
        output.buffered_frames = buffered_frames;
        output.dropped_frames = dropped_frames;
    }

    fn fail(&self, generation: u64, error: Error) {
        let mut output = self
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if output.generation == generation {
            output.error = Some(error.to_string());
        }
    }
}

trait PreviewDecoder {
    fn next_sample(&mut self) -> Result<Option<DecodedMediaSample>>;
    fn cancel(&mut self);
}

impl PreviewDecoder for NativeMediaDecoder {
    fn next_sample(&mut self) -> Result<Option<DecodedMediaSample>> {
        NativeMediaDecoder::next_sample(self)
    }

    fn cancel(&mut self) {
        NativeMediaDecoder::cancel(self);
    }
}

trait PreviewSource {
    type Decoder: PreviewDecoder;

    fn decoder(&self, range: TrimRange, dimensions: (u32, u32)) -> Result<Self::Decoder>;
}

impl PreviewSource for NativeMediaSource {
    type Decoder = NativeMediaDecoder;

    fn decoder(&self, range: TrimRange, dimensions: (u32, u32)) -> Result<Self::Decoder> {
        self.decoder_with_dimensions(range, dimensions)
    }
}

fn run_decode_worker(source: impl PreviewSource, shared: Arc<SharedDecode>) {
    let mut frame_sequence = 0_u64;
    loop {
        let control = shared.control();
        if control.shutdown {
            return;
        }
        let generation = control.generation;
        let range = decode_range(control);
        let mut decoder = match source.decoder(range, control.dimensions) {
            Ok(decoder) => decoder,
            Err(error) => {
                shared.fail(generation, error);
                if !shared.wait_for_generation_change(generation) {
                    return;
                }
                continue;
            }
        };
        let mut queue = VecDeque::with_capacity(MAX_BUFFERED_VIDEO_FRAMES);
        let mut dropped_frames = 0_u64;
        let mut buffered_until = range.start;
        let mut ended = false;
        let mut last_video_timestamp = None;

        loop {
            let current = shared.control();
            if current.shutdown {
                decoder.cancel();
                return;
            }
            if current.generation != generation {
                decoder.cancel();
                break;
            }

            select_frame(&mut queue, current.cursor, &mut dropped_frames);
            buffered_until = queue
                .back()
                .map_or(buffered_until, |frame| {
                    frame.timestamp.saturating_add(frame.duration)
                })
                .min(current.plan.trim.end);
            shared.publish(
                generation,
                frame_for_cursor(&queue, ended, current.plan.trim.end),
                buffered_until,
                queue.len(),
                dropped_frames,
                &mut frame_sequence,
            );

            if ended || queue.len() >= MAX_BUFFERED_VIDEO_FRAMES {
                shared.wait(generation);
                continue;
            }

            let mut decoded_this_pass = 0;
            while decoded_this_pass < MAX_DECODE_SAMPLES_PER_PASS
                && queue.len() < MAX_BUFFERED_VIDEO_FRAMES
            {
                let latest = shared.control();
                if latest.shutdown || latest.generation != generation {
                    break;
                }
                let sample = match decoder.next_sample() {
                    Ok(Some(sample)) => sample,
                    Ok(None) => {
                        ended = true;
                        break;
                    }
                    Err(error) => {
                        if shared.control().generation == generation {
                            shared.fail(generation, error);
                            decoder.cancel();
                            if !shared.wait_for_generation_change(generation) {
                                return;
                            }
                        }
                        break;
                    }
                };
                decoded_this_pass += 1;
                match sample {
                    DecodedMediaSample::Video(frame) => {
                        if let Err(error) = validate_preview_frame(
                            &frame,
                            current.dimensions,
                            current.plan.trim,
                            last_video_timestamp,
                        ) {
                            if shared.control().generation == generation {
                                shared.fail(generation, error);
                                decoder.cancel();
                                if !shared.wait_for_generation_change(generation) {
                                    return;
                                }
                            }
                            break;
                        }
                        last_video_timestamp = Some(frame.timestamp);
                        queue.push_back(Arc::new(frame));
                    }
                    DecodedMediaSample::Audio(chunk) => {
                        if let Err(error) = validate_audio_chunk(&chunk) {
                            if shared.control().generation == generation {
                                shared.fail(generation, error);
                                decoder.cancel();
                                if !shared.wait_for_generation_change(generation) {
                                    return;
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
}

fn decode_range(control: DecodeControl) -> TrimRange {
    if control.cursor < control.plan.trim.end {
        return TrimRange {
            start: control.cursor.max(control.plan.trim.start),
            end: control.plan.trim.end,
        };
    }
    let fallback = Duration::try_from_secs_f64(1.0 / 30.0)
        .unwrap_or(Duration::from_millis(34))
        .saturating_mul(2);
    TrimRange {
        start: control
            .plan
            .trim
            .end
            .saturating_sub(fallback)
            .max(control.plan.trim.start),
        end: control.plan.trim.end,
    }
}

fn select_frame(
    queue: &mut VecDeque<Arc<DecodedVideoFrame>>,
    cursor: Duration,
    dropped_frames: &mut u64,
) {
    let mut skipped = 0_u64;
    while queue.len() > 1 && queue.get(1).is_some_and(|next| next.timestamp <= cursor) {
        queue.pop_front();
        skipped = skipped.saturating_add(1);
    }
    *dropped_frames = dropped_frames.saturating_add(skipped.saturating_sub(1));
}

fn validate_preview_frame(
    frame: &DecodedVideoFrame,
    dimensions: (u32, u32),
    trim: TrimRange,
    previous_timestamp: Option<Duration>,
) -> Result<()> {
    if (frame.image.width, frame.image.height) != dimensions {
        return Err(Error::Codec(format!(
            "decoded preview frame is {}x{}, expected {}x{}",
            frame.image.width, frame.image.height, dimensions.0, dimensions.1
        )));
    }
    let expected = usize::try_from(frame.image.width)
        .ok()
        .and_then(|width| {
            usize::try_from(frame.image.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| Error::Codec("decoded preview frame size overflowed memory".to_owned()))?;
    if expected > MAX_PREVIEW_FRAME_BYTES || frame.image.data.len() != expected {
        return Err(Error::Codec(format!(
            "decoded preview frame has {} bytes; expected {expected} within the {MAX_PREVIEW_FRAME_BYTES}-byte bound",
            frame.image.data.len()
        )));
    }
    if frame.duration.is_zero() || frame.duration > MAX_PREVIEW_DURATION {
        return Err(Error::Codec(format!(
            "decoded preview frame has invalid {:.3} s duration",
            frame.duration.as_secs_f64()
        )));
    }
    if frame.timestamp < trim.start || frame.timestamp >= trim.end {
        return Err(Error::Codec(format!(
            "decoded preview timestamp {:.3} s is outside trim {:.3}..{:.3} s",
            frame.timestamp.as_secs_f64(),
            trim.start.as_secs_f64(),
            trim.end.as_secs_f64()
        )));
    }
    if previous_timestamp.is_some_and(|previous| frame.timestamp < previous) {
        return Err(Error::Codec(
            "decoded preview timestamps moved backwards".to_owned(),
        ));
    }
    Ok(())
}

fn validate_audio_chunk(chunk: &DecodedAudioChunk) -> Result<()> {
    if chunk.sample_rate == 0
        || chunk.sample_rate > MAX_AUDIO_SAMPLE_RATE
        || chunk.channels == 0
        || chunk.channels > SourceMetadata::MAX_AUDIO_CHANNELS
        || chunk.duration.is_zero()
        || chunk.duration > MAX_AUDIO_CHUNK_DURATION
        || !chunk
            .samples
            .len()
            .is_multiple_of(usize::from(chunk.channels))
        || chunk.samples.iter().any(|sample| !sample.is_finite())
    {
        return Err(Error::Codec(format!(
            "decoded audio chunk is malformed: {} Hz, {} channels, {} samples, {:.3} s",
            chunk.sample_rate,
            chunk.channels,
            chunk.samples.len(),
            chunk.duration.as_secs_f64()
        )));
    }
    Ok(())
}

fn validate_source_bounds(metadata: SourceMetadata, duration: Duration) -> Result<()> {
    metadata.validate()?;
    if metadata.width > SourceMetadata::MAX_EDGE || metadata.height > SourceMetadata::MAX_EDGE {
        return Err(Error::Unsupported {
            what: format!("{}x{} recording preview", metadata.width, metadata.height),
            why: format!(
                "decoded source edges are bounded to {} pixels",
                SourceMetadata::MAX_EDGE
            ),
        });
    }
    if metadata.fps > SourceMetadata::MAX_FPS {
        return Err(Error::Unsupported {
            what: format!("{:.3} fps recording preview", metadata.fps),
            why: format!(
                "preview frame rate is bounded to {:.0} fps",
                SourceMetadata::MAX_FPS
            ),
        });
    }
    if duration > MAX_PREVIEW_DURATION {
        return Err(Error::Unsupported {
            what: format!("{:.0}-second recording preview", duration.as_secs_f64()),
            why: format!(
                "interactive preview is bounded to {} hours; the source remains untouched",
                MAX_PREVIEW_DURATION.as_secs() / 3_600
            ),
        });
    }
    Ok(())
}

fn preview_dimensions(plan: EditPlan, metadata: SourceMetadata) -> Result<(u32, u32)> {
    let (width, height) = plan.output_dimensions(metadata);
    if width == 0 || height == 0 {
        return Err(Error::InvalidRequest(
            "recording preview dimensions must have area".to_owned(),
        ));
    }
    let edge = width.max(height);
    let dimensions = if edge <= MAX_PREVIEW_EDGE {
        (width, height)
    } else {
        let scale = f64::from(MAX_PREVIEW_EDGE) / f64::from(edge);
        (
            (f64::from(width) * scale).round().max(1.0) as u32,
            (f64::from(height) * scale).round().max(1.0) as u32,
        )
    };
    let bytes = usize::try_from(dimensions.0)
        .ok()
        .and_then(|width| {
            usize::try_from(dimensions.1)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| Error::InvalidRequest("preview dimensions overflow memory".to_owned()))?;
    if bytes > MAX_PREVIEW_FRAME_BYTES {
        return Err(Error::Unsupported {
            what: format!("{}x{} decoded preview", dimensions.0, dimensions.1),
            why: format!("one preview frame is bounded to {MAX_PREVIEW_FRAME_BYTES} bytes"),
        });
    }
    Ok(dimensions)
}

fn playback_audio(plan: EditPlan, metadata: SourceMetadata) -> PlaybackAudio {
    if metadata.audio_channels == 0 {
        PlaybackAudio::NoTrack
    } else if plan.audio.mute || !plan.output.supports_audio() {
        PlaybackAudio::Muted
    } else {
        PlaybackAudio::Active
    }
}

fn frame_for_cursor(
    queue: &VecDeque<Arc<DecodedVideoFrame>>,
    ended: bool,
    trim_end: Duration,
) -> Option<(Arc<DecodedVideoFrame>, Duration)> {
    let frame = queue.front()?.clone();
    let display_until = queue
        .get(1)
        .map_or_else(
            || {
                if ended {
                    trim_end
                } else {
                    frame.timestamp.saturating_add(frame.duration)
                }
            },
            |next| next.timestamp,
        )
        .max(frame.timestamp.saturating_add(frame.duration))
        .min(trim_end);
    Some((frame, display_until))
}

fn frame_drift(frame: &PlaybackFrame, position: Duration) -> Duration {
    if position < frame.frame.timestamp {
        frame.frame.timestamp - position
    } else {
        position.saturating_sub(frame.display_until)
    }
}

fn frame_is_ready(frame: Option<&PlaybackFrame>, position: Duration) -> bool {
    frame.is_some_and(|frame| frame_drift(frame, position) <= MAX_AV_DRIFT)
}

fn validate_document_source(document: &VideoDocument, source: &NativeMediaSource) -> Result<()> {
    let expected = document.metadata();
    let actual = source.metadata();
    if (expected.width, expected.height, expected.audio_channels)
        != (actual.width, actual.height, actual.audio_channels)
        || (expected.fps - actual.fps).abs() > actual.fps.max(1.0) * 0.01
    {
        return Err(Error::InvalidRequest(
            "recording document metadata changed; reopen it before playback".to_owned(),
        ));
    }
    let tolerance = Duration::try_from_secs_f64((1.0 / actual.fps).max(0.050))
        .unwrap_or(Duration::from_millis(50));
    if document.duration().abs_diff(source.inspection().duration) > tolerance {
        return Err(Error::InvalidRequest(
            "recording duration changed; reopen it before playback".to_owned(),
        ));
    }
    Ok(())
}

/// Applies native playback state to the editor's deterministic document model.
///
/// # Errors
///
/// Returns an error only if the native position no longer fits the document.
pub fn sync_document(document: &mut VideoDocument, snapshot: &PlaybackSnapshot) -> Result<()> {
    document.seek(snapshot.position)?;
    match snapshot.phase {
        PlaybackPhase::Playing | PlaybackPhase::Buffering => document.play(),
        PlaybackPhase::Loading
        | PlaybackPhase::Paused
        | PlaybackPhase::Ended
        | PlaybackPhase::Failed => document.pause(),
    }
    Ok(())
}

/// Whether the deterministic document and native snapshot agree about play/pause.
#[must_use]
pub fn document_state_matches(document: &VideoDocument, snapshot: &PlaybackSnapshot) -> bool {
    match snapshot.phase {
        PlaybackPhase::Playing | PlaybackPhase::Buffering => {
            document.playback() == PlaybackState::Playing
        }
        _ => document.playback() == PlaybackState::Paused,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Instant,
    };

    use scrozz_export::RgbaImage;

    use super::*;

    fn metadata() -> SourceMetadata {
        SourceMetadata {
            width: 1920,
            height: 1080,
            fps: 30.0,
            audio_channels: 2,
        }
    }

    fn plan() -> EditPlan {
        let document = VideoDocument::open_fixture(
            crate::Recording::synthetic("preview.mp4", 10.0, "playback test").unwrap(),
            metadata(),
        )
        .unwrap();
        EditPlan::video(&document).unwrap()
    }

    fn frame(timestamp_ms: u64, duration_ms: u64) -> Arc<DecodedVideoFrame> {
        Arc::new(DecodedVideoFrame {
            timestamp: Duration::from_millis(timestamp_ms),
            duration: Duration::from_millis(duration_ms),
            image: RgbaImage {
                width: 2,
                height: 2,
                data: vec![0; 16],
            },
        })
    }

    fn published_frame(timestamp_ms: u64, duration_ms: u64, until_ms: u64) -> PlaybackFrame {
        PlaybackFrame {
            sequence: 1,
            frame: frame(timestamp_ms, duration_ms),
            display_until: Duration::from_millis(until_ms),
        }
    }

    #[test]
    fn capabilities_are_explicit_on_every_target() {
        let capabilities = native_playback_capabilities();
        assert!(!capabilities.backend.is_empty());
        if capabilities.decoded_video {
            assert!(capabilities.audio_output);
            assert!(capabilities.transport);
            assert!(capabilities.unavailable_reason.is_none());
        } else {
            assert!(capabilities.unavailable_reason.is_some());
        }
    }

    #[test]
    fn preview_dimensions_apply_export_resolution_then_memory_bound() {
        let mut plan = plan();
        assert_eq!(preview_dimensions(plan, metadata()).unwrap(), (960, 540));
        plan.resolution = crate::settings::ResolutionCap::Hd720;
        assert_eq!(preview_dimensions(plan, metadata()).unwrap(), (960, 540));
        plan.resolution = crate::settings::ResolutionCap::Half;
        assert_eq!(preview_dimensions(plan, metadata()).unwrap(), (960, 540));
    }

    #[test]
    fn frame_selection_uses_timestamps_for_variable_frame_rate() {
        let mut queue = VecDeque::from([frame(0, 17), frame(17, 50), frame(67, 11), frame(78, 40)]);
        let mut dropped = 0;
        select_frame(&mut queue, Duration::from_millis(70), &mut dropped);
        assert_eq!(queue.front().unwrap().timestamp, Duration::from_millis(67));
        let (frame, display_until) =
            frame_for_cursor(&queue, false, Duration::from_millis(200)).unwrap();
        let current = PlaybackFrame {
            sequence: 1,
            frame,
            display_until,
        };
        assert_eq!(
            frame_drift(&current, Duration::from_millis(70)),
            Duration::ZERO
        );
        assert_eq!(dropped, 1);
    }

    #[test]
    fn av_drift_is_distance_outside_the_displayed_frame_interval() {
        let frame = published_frame(100, 40, 140);
        assert_eq!(
            frame_drift(&frame, Duration::from_millis(90)),
            Duration::from_millis(10)
        );
        assert_eq!(
            frame_drift(&frame, Duration::from_millis(120)),
            Duration::ZERO
        );
        assert_eq!(
            frame_drift(&frame, Duration::from_millis(160)),
            Duration::from_millis(20)
        );
    }

    #[test]
    fn sparse_vfr_frame_covers_gap_until_the_next_timestamp() {
        let queue = VecDeque::from([frame(0, 33), frame(1_000, 33)]);
        let (current_frame, display_until) =
            frame_for_cursor(&queue, false, Duration::from_secs(2)).unwrap();
        let current = PlaybackFrame {
            sequence: 1,
            frame: current_frame,
            display_until,
        };
        assert_eq!(current.display_until, Duration::from_secs(1));
        assert_eq!(
            frame_drift(&current, Duration::from_millis(750)),
            Duration::ZERO
        );

        let final_queue = VecDeque::from([frame(1_000, 33)]);
        let (final_image, display_until) =
            frame_for_cursor(&final_queue, true, Duration::from_secs(2)).unwrap();
        let final_frame = PlaybackFrame {
            sequence: 2,
            frame: final_image,
            display_until,
        };
        assert_eq!(final_frame.display_until, Duration::from_secs(2));
        assert_eq!(
            frame_drift(&final_frame, Duration::from_millis(1_900)),
            Duration::ZERO
        );
    }

    #[test]
    fn source_and_audio_bounds_fail_explicitly() {
        let mut huge = metadata();
        huge.width = SourceMetadata::MAX_EDGE + 1;
        assert!(validate_source_bounds(huge, Duration::from_secs(1)).is_err());
        assert!(
            validate_source_bounds(metadata(), MAX_PREVIEW_DURATION + Duration::from_secs(1))
                .is_err()
        );
        assert!(
            validate_audio_chunk(&DecodedAudioChunk {
                timestamp: Duration::ZERO,
                duration: Duration::from_millis(10),
                sample_rate: 48_000,
                channels: 2,
                samples: vec![f32::NAN, 0.0],
            })
            .is_err()
        );
    }

    #[test]
    fn audio_state_names_no_track_and_explicit_mute() {
        let plan = plan();
        assert_eq!(playback_audio(plan, metadata()), PlaybackAudio::Active);
        let mut muted = plan;
        muted.audio.mute = true;
        assert_eq!(playback_audio(muted, metadata()), PlaybackAudio::Muted);
        let mut silent = metadata();
        silent.audio_channels = 0;
        assert_eq!(playback_audio(plan, silent), PlaybackAudio::NoTrack);
    }

    struct RecoveringSource {
        attempts: Arc<AtomicUsize>,
    }

    struct OneFrameDecoder {
        emitted: bool,
    }

    impl PreviewSource for RecoveringSource {
        type Decoder = OneFrameDecoder;

        fn decoder(&self, _range: TrimRange, _dimensions: (u32, u32)) -> Result<Self::Decoder> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(Error::Codec("injected decoder-open failure".to_owned()))
            } else {
                Ok(OneFrameDecoder { emitted: false })
            }
        }
    }

    impl PreviewDecoder for OneFrameDecoder {
        fn next_sample(&mut self) -> Result<Option<DecodedMediaSample>> {
            if self.emitted {
                Ok(None)
            } else {
                self.emitted = true;
                Ok(Some(DecodedMediaSample::Video((*frame(0, 33)).clone())))
            }
        }

        fn cancel(&mut self) {}
    }

    #[test]
    fn failed_generation_waits_and_recovers_after_reconfiguration() {
        let plan = plan();
        let shared = Arc::new(SharedDecode::new(DecodeControl {
            generation: 1,
            cursor: Duration::ZERO,
            plan,
            dimensions: (2, 2),
            playing: false,
            shutdown: false,
        }));
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_shared = Arc::clone(&shared);
        let worker_attempts = Arc::clone(&attempts);
        let worker = thread::spawn(move || {
            run_decode_worker(
                RecoveringSource {
                    attempts: worker_attempts,
                },
                worker_shared,
            );
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        while shared.output().error.is_none() {
            assert!(
                Instant::now() < deadline,
                "injected failure was not published"
            );
            thread::sleep(Duration::from_millis(2));
        }
        thread::sleep(Duration::from_millis(30));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "a failed generation must not spin-retry"
        );

        shared.reconfigure_with_dimensions(Duration::ZERO, plan, (2, 2), false);
        let deadline = Instant::now() + Duration::from_secs(1);
        while shared.output().frame.is_none() {
            assert!(Instant::now() < deadline, "new generation did not recover");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        shared.shutdown();
        worker.join().unwrap();
    }
}
