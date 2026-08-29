//! Linux recording orchestration.

#[cfg(feature = "linux-native")]
mod source;

#[cfg(feature = "linux-native")]
use crate::{EngineCapabilities, RecordingEngine, RecordingRequest, RecordingSession};
#[cfg(feature = "linux-native")]
use scrozz_core::Result;

#[cfg(feature = "linux-native")]
pub(crate) const ENGINE_NAME: &str = "Linux X11/Wayland + PipeWire/FFmpeg";

#[cfg(feature = "linux-native")]
pub(crate) struct LinuxEngine;

#[cfg(feature = "linux-native")]
impl RecordingEngine for LinuxEngine {
    fn name(&self) -> &'static str {
        ENGINE_NAME
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            video: true,
            system_audio: true,
            microphone: true,
            pause_resume: true,
            display: true,
            window: true,
            region: true,
            all_displays: true,
            cursor: true,
            mp4: true,
            h264: true,
            hevc: false,
            av1: cfg!(feature = "rav1e-fallback"),
            quality: true,
            resolution: true,
            ..EngineCapabilities::default()
        }
    }

    fn start(&self, request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
        native::start(request)
    }
}

#[cfg(feature = "linux-native")]
mod native {
    use std::collections::{BTreeMap, VecDeque};
    use std::fs::{DirBuilder, File, OpenOptions};
    use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use scrozz_core::{CaptureTarget, Error, PhysicalSize, Result};

    use super::source::{AudioBatch, PipeWireAudio, VideoSource, open_video_source};
    use crate::audio::{AudioBuffer, AudioMixer};
    use crate::config::{RecordingConfig, resolve_dimensions};
    use crate::encoder::aac::{AacEncoder, EncodedAudioPacket};
    use crate::encoder::{self, EncodedVideoPacket, VideoEncoder, VideoEncoderSettings};
    use crate::format::{Nv12Frame, PackedFrame, to_nv12};
    use crate::muxer::{
        AudioTrackConfig, EncodedSample, FragmentedMp4, MediaFragment, RecoverySalvageability,
        TrackFragment, VideoTrackConfig,
    };
    use crate::pacing::{FramePacer, PacingDecision};
    use crate::state::{RecorderCommand, RecorderState, RecordingStateMachine};
    use crate::timeline::RecordingTimeline;
    use crate::{
        Quality, Recording, RecordingMetadata, RecordingRequest, RecordingResolution,
        RecordingSession, RecordingState as PublicRecordingState, Salvageability, SessionEvent,
        VideoCodec,
    };

    static TEMP_OUTPUT_COUNTER: AtomicU64 = AtomicU64::new(0);
    const SESSION_RECORDING: u8 = 0;
    const SESSION_PAUSED: u8 = 1;
    const SESSION_STOPPED: u8 = 2;
    const MIX_LATENCY_FRAMES: u64 = 12_000;
    const MIX_CHUNK_FRAMES: u64 = 4_800;
    const MAX_MIX_CHUNKS_PER_POLL: usize = 16;
    const MAX_AUDIO_TIMELINE_GAP_FRAMES: u64 = 48_000;

    enum Control {
        Pause(SyncSender<Result<()>>),
        Resume(SyncSender<Result<()>>),
        Stop,
    }

    enum WorkerSignal {
        FirstFrame,
        Terminal(Box<Result<Recording>>),
    }

    #[derive(Debug, Clone)]
    struct TemporaryOutput {
        directory: PathBuf,
        path: PathBuf,
    }

    struct OpenedDestination {
        path: PathBuf,
        file: File,
        temporary: Option<TemporaryOutput>,
    }

    enum TerminalOutcome {
        Finished(Recording),
        Failed(CachedError),
    }

    #[derive(Clone)]
    enum CachedError {
        PermissionDenied { capability: String, remedy: String },
        Unsupported { what: String, why: String },
        TargetGone(String),
        InvalidRequest(String),
        Codec(String),
        Storage(String),
        Cancelled,
        Io { kind: ErrorKind, message: String },
        Platform(String),
    }

    impl CachedError {
        fn new(error: Error) -> Self {
            match error {
                Error::PermissionDenied { capability, remedy } => {
                    Self::PermissionDenied { capability, remedy }
                }
                Error::Unsupported { what, why } => Self::Unsupported { what, why },
                Error::TargetGone(message) => Self::TargetGone(message),
                Error::InvalidRequest(message) => Self::InvalidRequest(message),
                Error::Codec(message) => Self::Codec(message),
                Error::Storage(message) => Self::Storage(message),
                Error::Cancelled => Self::Cancelled,
                Error::Io(error) => Self::Io {
                    kind: error.kind(),
                    message: error.to_string(),
                },
                Error::Platform(message) => Self::Platform(message),
                other => Self::Platform(other.to_string()),
            }
        }

        fn to_error(&self) -> Error {
            match self {
                Self::PermissionDenied { capability, remedy } => Error::PermissionDenied {
                    capability: capability.clone(),
                    remedy: remedy.clone(),
                },
                Self::Unsupported { what, why } => Error::Unsupported {
                    what: what.clone(),
                    why: why.clone(),
                },
                Self::TargetGone(message) => Error::TargetGone(message.clone()),
                Self::InvalidRequest(message) => Error::InvalidRequest(message.clone()),
                Self::Codec(message) => Error::Codec(message.clone()),
                Self::Storage(message) => Error::Storage(message.clone()),
                Self::Cancelled => Error::Cancelled,
                Self::Io { kind, message } => {
                    Error::Io(std::io::Error::new(*kind, message.clone()))
                }
                Self::Platform(message) => Error::Platform(message.clone()),
            }
        }
    }

    impl TerminalOutcome {
        fn event(&self) -> SessionEvent {
            match self {
                Self::Finished(recording) => SessionEvent::Finished(recording.clone()),
                Self::Failed(error) => SessionEvent::Failed(Arc::new(error.to_error())),
            }
        }

        fn into_result(self) -> Result<Recording> {
            match self {
                Self::Finished(recording) => Ok(recording),
                Self::Failed(error) => Err(error.to_error()),
            }
        }
    }

    pub(super) struct LinuxRecordingSession {
        control: Sender<Control>,
        signals: Receiver<WorkerSignal>,
        worker: Option<JoinHandle<()>>,
        terminal: Option<TerminalOutcome>,
        terminal_emitted: bool,
        elapsed_nanos: Arc<AtomicU64>,
        state: Arc<AtomicU8>,
        temporary_output: Option<TemporaryOutput>,
    }

    impl LinuxRecordingSession {
        fn command(&self, command: Control) -> Result<()> {
            self.control.send(command).map_err(|_| {
                Error::Platform("the Linux recording worker has already stopped".into())
            })
        }

        fn join_worker(&mut self) -> Result<()> {
            if let Some(worker) = self.worker.take() {
                worker
                    .join()
                    .map_err(|_| Error::Platform("the Linux recording worker panicked".into()))?;
            }
            Ok(())
        }

        fn cache_terminal(&mut self, result: Result<Recording>) {
            self.terminal = Some(match result {
                Ok(recording) => TerminalOutcome::Finished(recording),
                Err(error) => TerminalOutcome::Failed(CachedError::new(error)),
            });
        }

        fn cache_disconnected_terminal(&mut self) {
            let join_detail = self
                .join_worker()
                .err()
                .map(|error| format!(": {error}"))
                .unwrap_or_default();
            self.cache_terminal(Err(Error::Platform(format!(
                "the Linux recording worker exited without a terminal result{join_detail}"
            ))));
        }

        fn receive_terminal(&mut self) {
            while self.terminal.is_none() {
                match self.signals.recv() {
                    Ok(WorkerSignal::FirstFrame) => {}
                    Ok(WorkerSignal::Terminal(result)) => self.cache_terminal(*result),
                    Err(_) => self.cache_disconnected_terminal(),
                }
            }
        }

        fn emit_terminal(&mut self) -> Option<SessionEvent> {
            if self.terminal_emitted {
                return None;
            }
            let event = self.terminal.as_ref()?.event();
            self.terminal_emitted = true;
            if matches!(self.terminal.as_ref(), Some(TerminalOutcome::Finished(_))) {
                self.temporary_output = None;
            }
            Some(event)
        }

        fn abandon_temporary_output(&mut self) {
            let Some(output) = self.temporary_output.take() else {
                return;
            };
            if let Err(error) = remove_temporary_output(&output) {
                record_abandoned_output(&output, &error);
            }
        }
    }

    impl RecordingSession for LinuxRecordingSession {
        fn state(&self) -> PublicRecordingState {
            if self.terminal.is_some() {
                return PublicRecordingState::Stopped;
            }
            match self.state.load(Ordering::Acquire) {
                SESSION_PAUSED => PublicRecordingState::Paused,
                SESSION_STOPPED => PublicRecordingState::Stopped,
                _ => PublicRecordingState::Recording,
            }
        }

        fn pause(&mut self) -> Result<()> {
            if self.terminal.is_some() {
                return Err(Error::InvalidRequest(
                    "cannot pause a recording that has already ended".into(),
                ));
            }
            let (reply, response) = mpsc::sync_channel(0);
            self.command(Control::Pause(reply))?;
            response.recv().map_err(|_| {
                Error::Platform("the recording worker stopped before pausing".into())
            })?
        }

        fn resume(&mut self) -> Result<()> {
            if self.terminal.is_some() {
                return Err(Error::InvalidRequest(
                    "cannot resume a recording that has already ended".into(),
                ));
            }
            let (reply, response) = mpsc::sync_channel(0);
            self.command(Control::Resume(reply))?;
            response.recv().map_err(|_| {
                Error::Platform("the recording worker stopped before resuming".into())
            })?
        }

        fn poll(&mut self) -> Option<SessionEvent> {
            if self.terminal.is_some() {
                return self.emit_terminal();
            }
            match self.signals.try_recv() {
                Ok(WorkerSignal::FirstFrame) => Some(SessionEvent::FirstFrame),
                Ok(WorkerSignal::Terminal(result)) => {
                    self.cache_terminal(*result);
                    self.emit_terminal()
                }
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    self.cache_disconnected_terminal();
                    self.emit_terminal()
                }
            }
        }

        fn engine_elapsed_secs(&self) -> Option<f64> {
            Some(self.elapsed_nanos.load(Ordering::Acquire) as f64 / 1_000_000_000.0)
        }

        fn stop(mut self: Box<Self>) -> Result<Recording> {
            if self.terminal.is_none() {
                let _ = self.control.send(Control::Stop);
                self.receive_terminal();
            }
            if let Err(error) = self.join_worker() {
                tracing::error!(%error, "Linux recording worker did not join after finalisation");
            }
            match self.terminal.take().expect("terminal outcome was received") {
                TerminalOutcome::Finished(recording) => {
                    self.temporary_output = None;
                    Ok(recording)
                }
                TerminalOutcome::Failed(error) => {
                    self.abandon_temporary_output();
                    Err(error.to_error())
                }
            }
        }
    }

    impl Drop for LinuxRecordingSession {
        fn drop(&mut self) {
            if self.worker.is_some() {
                if self.terminal.is_none() {
                    let _ = self.control.send(Control::Stop);
                }
                self.receive_terminal();
                if let Err(error) = self.join_worker() {
                    tracing::error!(%error, "Linux recording worker did not join during drop");
                }
            }
            self.abandon_temporary_output();
        }
    }

    pub(super) fn start(request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
        let config = RecordingConfig::try_from(request)?;
        let (control_tx, control_rx) = mpsc::channel();
        let (initialised_tx, initialised_rx) = mpsc::sync_channel(0);
        let (signal_tx, signal_rx) = mpsc::channel();
        let elapsed_nanos = Arc::new(AtomicU64::new(0));
        let worker_elapsed = Arc::clone(&elapsed_nanos);
        let state = Arc::new(AtomicU8::new(SESSION_RECORDING));
        let worker_state = Arc::clone(&state);
        let worker = thread::Builder::new()
            .name("scrozz-linux-recording".into())
            .spawn(move || {
                worker_entry(
                    config,
                    control_rx,
                    initialised_tx,
                    signal_tx,
                    worker_elapsed,
                    worker_state,
                )
            })
            .map_err(Error::Io)?;

        match initialised_rx.recv() {
            Ok(Ok(temporary_output)) => Ok(Box::new(LinuxRecordingSession {
                control: control_tx,
                signals: signal_rx,
                worker: Some(worker),
                terminal: None,
                terminal_emitted: false,
                elapsed_nanos,
                state,
                temporary_output,
            })),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let panic = worker.join().is_err();
                Err(Error::Platform(if panic {
                    "the Linux recording worker panicked during initialisation".into()
                } else {
                    "the Linux recording worker exited during initialisation".into()
                }))
            }
        }
    }

    fn worker_entry(
        config: RecordingConfig,
        controls: Receiver<Control>,
        initialised: SyncSender<Result<Option<TemporaryOutput>>>,
        signals: Sender<WorkerSignal>,
        elapsed_nanos: Arc<AtomicU64>,
        session_state: Arc<AtomicU8>,
    ) {
        match Worker::initialize(config) {
            Ok(WorkerStartup::Ready(mut worker)) => {
                let temporary_output = worker.temporary_output.clone();
                session_state.store(SESSION_RECORDING, Ordering::Release);
                if initialised.send(Ok(temporary_output.clone())).is_ok() {
                    let result = worker.run(&controls, &signals, &elapsed_nanos, &session_state);
                    drop(worker);
                    session_state.store(SESSION_STOPPED, Ordering::Release);
                    let _ = signals.send(WorkerSignal::Terminal(Box::new(result)));
                } else {
                    drop(worker);
                    if let Some(output) = temporary_output
                        && let Err(error) = remove_temporary_output(&output)
                    {
                        record_abandoned_output(&output, &error);
                    }
                }
            }
            Ok(WorkerStartup::Retained {
                recording,
                temporary_output,
            }) => {
                session_state.store(SESSION_STOPPED, Ordering::Release);
                if initialised.send(Ok(temporary_output.clone())).is_ok() {
                    let _ = signals.send(WorkerSignal::Terminal(Box::new(Ok(*recording))));
                } else if let Some(output) = temporary_output
                    && let Err(error) = remove_temporary_output(&output)
                {
                    record_abandoned_output(&output, &error);
                }
            }
            Err(error) => {
                session_state.store(SESSION_STOPPED, Ordering::Release);
                let _ = initialised.send(Err(error));
            }
        }
    }

    enum WorkerStartup {
        Ready(Box<Worker>),
        Retained {
            recording: Box<Recording>,
            temporary_output: Option<TemporaryOutput>,
        },
    }

    struct Worker {
        config: RecordingConfig,
        source: Box<dyn VideoSource>,
        engine_name: String,
        resolved_target: Option<CaptureTarget>,
        first_frame: Option<PackedFrame>,
        dimensions: crate::config::Dimensions,
        video_encoder: Box<dyn VideoEncoder>,
        resolved_codec: VideoCodec,
        audio_source: Option<PipeWireAudio>,
        audio_encoder: Option<AacEncoder>,
        audio_watermark: AudioWatermarkMixer,
        audio_cursor: u64,
        audio_frame_offset: Option<i128>,
        microphone_samples: bool,
        system_audio_samples: bool,
        audio_suspended: bool,
        muxer: Option<FragmentedMp4<File>>,
        temporary_output: Option<TemporaryOutput>,
        state: RecordingStateMachine,
        pending_video: Option<TimedVideoPacket>,
        submitted_video_times: BTreeMap<u64, u64>,
        last_timeline_frame_end: u64,
        pending_audio: VecDeque<EncodedAudioPacket>,
        fragments_written: u64,
        frames_submitted: u64,
    }

    struct TimedVideoPacket {
        packet: EncodedVideoPacket,
        timeline_frame: u64,
    }

    struct AudioWatermarkMixer {
        mixer: AudioMixer,
        enabled: [bool; 2],
        tracks: [VecDeque<AudioBuffer>; 2],
        cursor: Option<u64>,
        committed: bool,
    }

    impl AudioWatermarkMixer {
        fn new(microphone: bool, system: bool) -> Self {
            Self {
                mixer: AudioMixer::new(AacEncoder::SAMPLE_RATE),
                enabled: [microphone, system],
                tracks: std::array::from_fn(|_| VecDeque::new()),
                cursor: None,
                committed: false,
            }
        }

        fn push(&mut self, batch: AudioBatch) -> Result<()> {
            self.push_track(0, batch.microphone)?;
            self.push_track(1, batch.system)
        }

        fn push_track(&mut self, index: usize, buffers: Vec<AudioBuffer>) -> Result<()> {
            if !self.enabled[index] {
                return Ok(());
            }
            for buffer in buffers {
                validate_linux_audio(&buffer)?;
                if self.tracks[index]
                    .back()
                    .is_some_and(|previous| audio_buffer_end(previous) > buffer.start_frame)
                {
                    return Err(Error::Platform(
                        "PipeWire delivered out-of-order audio buffers".into(),
                    ));
                }
                if !self.committed {
                    self.cursor = Some(
                        self.cursor
                            .map_or(buffer.start_frame, |cursor| cursor.min(buffer.start_frame)),
                    );
                }
                self.tracks[index].push_back(buffer);
            }
            Ok(())
        }

        fn next(&mut self, finalising: bool) -> Result<Option<AudioBuffer>> {
            let Some(cursor) = self.cursor else {
                return Ok(None);
            };
            let ends: [Option<u64>; 2] = std::array::from_fn(|index| {
                self.enabled[index]
                    .then(|| self.tracks[index].back().map(audio_buffer_end))
                    .flatten()
            });
            let Some(leader) = ends.iter().flatten().copied().max() else {
                return Ok(None);
            };
            let every_source_ready = self
                .enabled
                .iter()
                .enumerate()
                .all(|(index, enabled)| !enabled || ends[index].is_some());
            let natural = every_source_ready.then(|| {
                ends.iter()
                    .enumerate()
                    .filter_map(|(index, end)| self.enabled[index].then_some(*end).flatten())
                    .min()
                    .unwrap_or(cursor)
            });
            let deadline = leader.saturating_sub(MIX_LATENCY_FRAMES);
            let watermark = if finalising {
                leader
            } else {
                natural.map_or(deadline, |natural| natural.max(deadline))
            };
            if watermark <= cursor {
                return Ok(None);
            }
            let end = watermark.min(cursor.saturating_add(MIX_CHUNK_FRAMES));
            let microphone = self.enabled[0]
                .then(|| audio_window(&self.tracks[0], cursor, end))
                .transpose()?;
            let system = self.enabled[1]
                .then(|| audio_window(&self.tracks[1], cursor, end))
                .transpose()?;
            let mixed = self.mixer.mix(microphone.as_ref(), system.as_ref())?;
            self.cursor = Some(end);
            self.committed = true;
            for track in &mut self.tracks {
                while track
                    .front()
                    .is_some_and(|buffer| audio_buffer_end(buffer) <= end)
                {
                    track.pop_front();
                }
            }
            Ok(Some(mixed))
        }

        fn reset(&mut self) {
            for track in &mut self.tracks {
                track.clear();
            }
            self.cursor = None;
            self.committed = false;
        }
    }

    fn validate_linux_audio(buffer: &AudioBuffer) -> Result<()> {
        if buffer.sample_rate != AacEncoder::SAMPLE_RATE
            || !matches!(buffer.channels, 1 | 2)
            || !buffer
                .samples
                .len()
                .is_multiple_of(usize::from(buffer.channels))
        {
            return Err(Error::Platform(
                "PipeWire delivered malformed or non-48kHz audio".into(),
            ));
        }
        Ok(())
    }

    fn audio_buffer_end(buffer: &AudioBuffer) -> u64 {
        buffer
            .start_frame
            .saturating_add(buffer.samples.len() as u64 / u64::from(buffer.channels.max(1)))
    }

    fn audio_window(buffers: &VecDeque<AudioBuffer>, start: u64, end: u64) -> Result<AudioBuffer> {
        let frames = usize::try_from(end.saturating_sub(start))
            .map_err(|_| Error::Platform("PipeWire audio mix window exceeds memory".into()))?;
        let mut samples = vec![0.0_f32; frames.saturating_mul(2)];
        for buffer in buffers {
            let buffer_end = audio_buffer_end(buffer);
            let overlap_start = start.max(buffer.start_frame);
            let overlap_end = end.min(buffer_end);
            if overlap_start >= overlap_end {
                continue;
            }
            for source_frame in overlap_start..overlap_end {
                let source_index = usize::try_from(source_frame - buffer.start_frame)
                    .map_err(|_| Error::Platform("PipeWire audio offset exceeds memory".into()))?;
                let output_index = usize::try_from(source_frame - start)
                    .map_err(|_| Error::Platform("PipeWire audio offset exceeds memory".into()))?;
                if buffer.channels == 1 {
                    let sample = buffer.samples[source_index];
                    samples[output_index * 2] = sample;
                    samples[output_index * 2 + 1] = sample;
                } else {
                    samples[output_index * 2] = buffer.samples[source_index * 2];
                    samples[output_index * 2 + 1] = buffer.samples[source_index * 2 + 1];
                }
            }
        }
        Ok(AudioBuffer {
            sample_rate: AacEncoder::SAMPLE_RATE,
            channels: 2,
            start_frame: start,
            samples,
        })
    }

    impl Worker {
        fn initialize(mut config: RecordingConfig) -> Result<WorkerStartup> {
            let mut source = open_video_source(&config.target, config.show_cursor)?;
            let first_frame = source.next_frame(Duration::from_secs(15))?.ok_or_else(|| {
                Error::Platform("the capture source delivered no frame within 15 seconds".into())
            })?;
            first_frame.validate()?;
            let backing_scale = source.backing_scale();
            let resolved_target = source.resolved_target();
            if config.resolution == RecordingResolution::LogicalPoints && backing_scale.is_none() {
                tracing::warn!(
                    "the Linux source exposes physical buffers without a logical backing scale; \
                     LogicalPoints assumes 1:1"
                );
            }
            let dimensions = resolve_dimensions(
                config.resolution,
                first_frame.width,
                first_frame.height,
                backing_scale,
            )?;
            let video_encoder = encoder::open(
                config.codec,
                VideoEncoderSettings {
                    dimensions,
                    fps: config.fps.get(),
                    quality: config.encoder_quality,
                },
            )?;
            let resolved_codec = video_encoder.codec();
            let mut audio_encoder = if config.microphone || config.system_audio {
                Some(AacEncoder::new(config.encoder_quality)?)
            } else {
                None
            };
            let audio_source = PipeWireAudio::new(config.microphone, config.system_audio)?;
            let video_track = VideoTrackConfig {
                width: u16::try_from(dimensions.width).map_err(|_| {
                    Error::InvalidRequest("video width exceeds ISO-BMFF storage".into())
                })?,
                height: u16::try_from(dimensions.height).map_err(|_| {
                    Error::InvalidRequest("video height exceeds ISO-BMFF storage".into())
                })?,
                timescale: config.fps.get(),
                codec: video_encoder.decoder_configuration(),
            };
            let audio_track = audio_encoder.as_ref().map(|encoder| AudioTrackConfig {
                sample_rate: AacEncoder::SAMPLE_RATE,
                channels: AacEncoder::CHANNELS,
                audio_specific_config: encoder.decoder_configuration().to_vec(),
                priming_frames: encoder.priming_frames(),
            });
            let OpenedDestination {
                path: destination,
                file,
                temporary,
            } = open_destination(config.destination.as_deref())?;
            config.destination = Some(destination.clone());
            let engine_name = format!("{} via {}", super::ENGINE_NAME, source.name());
            let muxer = match FragmentedMp4::new(file, &video_track, audio_track.as_ref()) {
                Ok(muxer) => muxer,
                Err(error) => {
                    return startup_write_failure(
                        FinalReport {
                            path: destination,
                            duration_secs: 0.0,
                            engine_name,
                            target: resolved_target,
                            dimensions,
                            frames: 0,
                            audio_channels: u16::from(audio_encoder.is_some()) * 2,
                            codec: resolved_codec,
                            quality: config.quality,
                            resolution: config.resolution,
                            partial: None,
                        },
                        temporary,
                        Error::Io(error),
                    );
                }
            };
            if let Err(error) = muxer.writer().sync_data() {
                drop(muxer);
                return startup_write_failure(
                    FinalReport {
                        path: destination,
                        duration_secs: 0.0,
                        engine_name,
                        target: resolved_target,
                        dimensions,
                        frames: 0,
                        audio_channels: u16::from(audio_encoder.is_some()) * 2,
                        codec: resolved_codec,
                        quality: config.quality,
                        resolution: config.resolution,
                        partial: None,
                    },
                    temporary,
                    Error::Io(error),
                );
            }
            let mut state = RecordingStateMachine::new();
            state
                .apply(RecorderCommand::Started)
                .expect("a new recording state accepts Started");
            tracing::info!(
                backend = source.name(),
                width = dimensions.width,
                height = dimensions.height,
                fps = config.fps.get(),
                audio = audio_encoder.is_some(),
                path = %destination.display(),
                "Linux recording started"
            );
            let audio_watermark = AudioWatermarkMixer::new(config.microphone, config.system_audio);

            Ok(WorkerStartup::Ready(Box::new(Self {
                config,
                source,
                engine_name,
                resolved_target,
                first_frame: Some(first_frame),
                dimensions,
                video_encoder,
                resolved_codec,
                audio_source,
                audio_encoder: audio_encoder.take(),
                audio_watermark,
                audio_cursor: 0,
                audio_frame_offset: None,
                microphone_samples: false,
                system_audio_samples: false,
                audio_suspended: false,
                muxer: Some(muxer),
                temporary_output: temporary,
                state,
                pending_video: None,
                submitted_video_times: BTreeMap::new(),
                last_timeline_frame_end: 0,
                pending_audio: VecDeque::new(),
                fragments_written: 0,
                frames_submitted: 0,
            })))
        }

        fn run(
            &mut self,
            controls: &Receiver<Control>,
            signals: &Sender<WorkerSignal>,
            elapsed_nanos: &AtomicU64,
            session_state: &AtomicU8,
        ) -> Result<Recording> {
            let started = Instant::now();
            let mut timeline = RecordingTimeline::new(Duration::ZERO);
            let mut pacer = FramePacer::new(self.config.fps.get());
            let interval =
                Duration::from_nanos(1_000_000_000_u64 / u64::from(self.config.fps.get()));
            let mut next_capture = started;
            let mut failure: Option<String> = None;
            let mut user_stopped = false;
            let mut first_frame_signalled = false;

            loop {
                if self.state.state() == RecorderState::Paused {
                    match controls.recv_timeout(Duration::from_millis(50)) {
                        Ok(control) => {
                            if self.handle_control(
                                control,
                                started,
                                &mut timeline,
                                elapsed_nanos,
                                session_state,
                            ) {
                                user_stopped = true;
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if let Some(audio) = &mut self.audio_source
                                && let Err(error) = audio.poll()
                            {
                                failure = Some(error.to_string());
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            let _ = self.state.apply(RecorderCommand::Stop);
                            break;
                        }
                    }
                    continue;
                }

                loop {
                    match controls.try_recv() {
                        Ok(control) => {
                            if self.handle_control(
                                control,
                                started,
                                &mut timeline,
                                elapsed_nanos,
                                session_state,
                            ) {
                                user_stopped = true;
                                break;
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            let _ = self.state.apply(RecorderCommand::Stop);
                            break;
                        }
                    }
                }
                if user_stopped {
                    break;
                }
                if self.state.state() == RecorderState::Stopping {
                    break;
                }
                if self.state.state() == RecorderState::Paused {
                    continue;
                }

                let now = Instant::now();
                if now < next_capture {
                    match controls.recv_timeout(next_capture - now) {
                        Ok(control) => {
                            if self.handle_control(
                                control,
                                started,
                                &mut timeline,
                                elapsed_nanos,
                                session_state,
                            ) {
                                user_stopped = true;
                                break;
                            }
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            let _ = self.state.apply(RecorderCommand::Stop);
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                if user_stopped {
                    break;
                }

                let captured = if let Some(first) = self.first_frame.take() {
                    Some(first)
                } else {
                    match self.source.next_frame(interval) {
                        Ok(frame) => frame,
                        Err(error) => {
                            failure = Some(error.to_string());
                            break;
                        }
                    }
                };
                let elapsed = started.elapsed();
                next_capture = next_capture
                    .checked_add(interval)
                    .unwrap_or_else(Instant::now);
                if next_capture + interval < Instant::now() {
                    next_capture = Instant::now();
                }

                if let Err(error) = self.capture_audio(false) {
                    failure = Some(error.to_string());
                    break;
                }
                let Some(captured) = captured else {
                    continue;
                };
                let media_time = match timeline.media_time(elapsed) {
                    Ok(time) => time,
                    Err(error) => {
                        failure = Some(error.to_string());
                        break;
                    }
                };
                publish_elapsed(elapsed_nanos, media_time);
                match pacer.observe(media_time) {
                    PacingDecision::Drop => {}
                    PacingDecision::Emit { frame_index, .. } => {
                        self.last_timeline_frame_end = self
                            .last_timeline_frame_end
                            .max(frame_index.saturating_add(1));
                        match to_nv12(&captured, self.dimensions) {
                            Ok(frame) => {
                                if let Err(error) = self.encode_video(&frame, frame_index) {
                                    failure = Some(error.to_string());
                                    break;
                                }
                                if !first_frame_signalled {
                                    let _ = signals.send(WorkerSignal::FirstFrame);
                                    first_frame_signalled = true;
                                }
                            }
                            Err(error) => {
                                failure = Some(error.to_string());
                                break;
                            }
                        }
                    }
                }
            }

            if failure.is_some() {
                let _ = self.state.apply(RecorderCommand::Fail);
            }
            let duration = timeline
                .media_time(started.elapsed())
                .unwrap_or(Duration::ZERO)
                .as_secs_f64();
            publish_elapsed(
                elapsed_nanos,
                Duration::try_from_secs_f64(duration).unwrap_or(Duration::ZERO),
            );
            self.finalise(duration, user_stopped, failure)
        }

        fn handle_control(
            &mut self,
            control: Control,
            started: Instant,
            timeline: &mut RecordingTimeline,
            elapsed_nanos: &AtomicU64,
            session_state: &AtomicU8,
        ) -> bool {
            let now = started.elapsed();
            match control {
                Control::Pause(reply) => {
                    let result = self
                        .state
                        .apply(RecorderCommand::Pause)
                        .and_then(|_| timeline.pause(now))
                        .and_then(|()| timeline.media_time(now))
                        .map(|elapsed| publish_elapsed(elapsed_nanos, elapsed));
                    if result.is_ok() {
                        self.audio_suspended = true;
                        session_state.store(SESSION_PAUSED, Ordering::Release);
                    }
                    let _ = reply.send(result);
                    false
                }
                Control::Resume(reply) => {
                    let result = self
                        .state
                        .apply(RecorderCommand::Resume)
                        .and_then(|_| timeline.resume(now))
                        .and_then(|()| timeline.media_time(now))
                        .map(|elapsed| publish_elapsed(elapsed_nanos, elapsed));
                    if result.is_ok() {
                        self.audio_suspended = false;
                        self.audio_frame_offset = None;
                        self.audio_watermark.reset();
                        session_state.store(SESSION_RECORDING, Ordering::Release);
                    }
                    let _ = reply.send(result);
                    false
                }
                Control::Stop => {
                    let _ = self.state.apply(RecorderCommand::Stop);
                    true
                }
            }
        }

        fn capture_audio(&mut self, finalising: bool) -> Result<()> {
            let Some(source) = &mut self.audio_source else {
                return Ok(());
            };
            let batch = source.poll()?;
            self.microphone_samples |= batch
                .microphone
                .iter()
                .any(|buffer| !buffer.samples.is_empty());
            self.system_audio_samples |=
                batch.system.iter().any(|buffer| !buffer.samples.is_empty());
            self.audio_watermark.push(batch)?;
            let mut emitted = 0;
            while let Some(mixed) = self.audio_watermark.next(finalising)? {
                emitted += 1;
                if emitted > MAX_MIX_CHUNKS_PER_POLL {
                    return Err(Error::Platform(
                        "PipeWire audio drain exceeded its per-poll work bound".into(),
                    ));
                }
                let samples = align_audio_to_timeline(
                    mixed,
                    &mut self.audio_cursor,
                    &mut self.audio_frame_offset,
                )?;
                if samples.is_empty() {
                    continue;
                }
                if let Some(encoder) = &mut self.audio_encoder {
                    self.pending_audio
                        .extend(encoder.push_interleaved(&samples)?);
                }
            }
            Ok(())
        }

        fn encode_video(&mut self, frame: &Nv12Frame, timeline_frame: u64) -> Result<()> {
            let input_frame = self.frames_submitted;
            self.submitted_video_times
                .insert(input_frame, timeline_frame);
            let packets = self.video_encoder.encode(frame)?;
            self.frames_submitted = self.frames_submitted.saturating_add(1);
            for packet in packets {
                self.queue_video_packet(packet)?;
            }
            Ok(())
        }

        fn queue_video_packet(&mut self, packet: EncodedVideoPacket) -> Result<()> {
            let timeline_frame = self
                .submitted_video_times
                .remove(&packet.frame_index)
                .ok_or_else(|| {
                    Error::Codec(format!(
                        "video encoder returned unknown input frame {}",
                        packet.frame_index
                    ))
                })?;
            let current = TimedVideoPacket {
                packet,
                timeline_frame,
            };
            if let Some(previous) = self.pending_video.replace(current) {
                let duration = timeline_frame
                    .saturating_sub(previous.timeline_frame)
                    .max(1);
                self.write_fragment(previous, duration)?;
            }
            Ok(())
        }

        fn write_fragment(&mut self, video: TimedVideoPacket, duration: u64) -> Result<()> {
            let audio = if self.pending_audio.is_empty() {
                None
            } else {
                let first_timestamp = self.pending_audio.front().unwrap().start_frame;
                Some(TrackFragment {
                    base_decode_time: first_timestamp,
                    samples: self
                        .pending_audio
                        .drain(..)
                        .map(|packet| EncodedSample {
                            data: packet.data,
                            duration: packet.duration,
                            keyframe: true,
                        })
                        .collect(),
                })
            };
            let fragment = MediaFragment {
                video: TrackFragment {
                    base_decode_time: video.timeline_frame,
                    samples: vec![EncodedSample {
                        data: video.packet.data,
                        duration: u32::try_from(duration).map_err(|_| {
                            Error::Codec("video sample duration exceeds ISO-BMFF storage".into())
                        })?,
                        keyframe: video.packet.keyframe,
                    }],
                },
                audio,
            };
            let muxer = self
                .muxer
                .as_mut()
                .ok_or_else(|| Error::Platform("recording muxer is already closed".into()))?;
            muxer.write_fragment(&fragment).map_err(Error::Io)?;
            muxer.writer().sync_data().map_err(Error::Io)?;
            self.fragments_written = self.fragments_written.saturating_add(1);
            Ok(())
        }

        fn finalise(
            &mut self,
            duration_secs: f64,
            user_stopped: bool,
            mut failure: Option<String>,
        ) -> Result<Recording> {
            if !self.audio_suspended {
                match self.capture_audio(true) {
                    Ok(()) => {}
                    Err(error) => append_failure(&mut failure, error),
                }
            }
            if let Some(error) = self.missing_audio_error() {
                append_failure(&mut failure, error);
            }
            match self.video_encoder.finish() {
                Ok(packets) => {
                    for packet in packets {
                        if let Err(error) = self.queue_video_packet(packet) {
                            append_failure(&mut failure, error);
                            break;
                        }
                    }
                }
                Err(error) => append_failure(&mut failure, error),
            }
            if let Some(audio) = &mut self.audio_encoder {
                match audio.finish() {
                    Ok(packets) => self.pending_audio.extend(packets),
                    Err(error) => append_failure(&mut failure, error),
                }
            }
            if let Some(video) = self.pending_video.take() {
                let duration = self
                    .last_timeline_frame_end
                    .saturating_sub(video.timeline_frame)
                    .max(1);
                if let Err(error) = self.write_fragment(video, duration) {
                    append_failure(&mut failure, error);
                }
            }

            if let Some(muxer) = self.muxer.take() {
                match muxer.finish() {
                    Ok(file) => {
                        if let Err(error) = file.sync_all() {
                            append_failure(&mut failure, Error::Io(error));
                        }
                    }
                    Err(error) => append_failure(&mut failure, Error::Io(error)),
                }
            }

            let destination = self
                .config
                .destination
                .as_ref()
                .expect("destination is resolved before worker construction")
                .clone();
            let partial_reason = match failure {
                Some(reason) => Some(reason),
                None if self.fragments_written == 0 => {
                    Some("recording ended before a complete media fragment reached disk".into())
                }
                None if !user_stopped => {
                    Some("the recording owner disconnected before requesting a stop".into())
                }
                None => None,
            };
            let partial = if let Some(reason) = partial_reason {
                let salvageability = recover_partial_output(&destination).map_err(|error| {
                    Error::Storage(format!(
                        "{reason}; no recoverable recording output was retained: {error}"
                    ))
                })?;
                Some((salvageability, reason))
            } else {
                None
            };
            if partial.is_some() {
                let _ = self.state.apply(RecorderCommand::Fail);
            } else {
                let _ = self.state.apply(RecorderCommand::Finish);
            }
            build_recording_report(FinalReport {
                path: destination,
                duration_secs,
                engine_name: self.engine_name.clone(),
                target: self.resolved_target.clone(),
                dimensions: self.dimensions,
                frames: self.frames_submitted,
                audio_channels: if self.audio_encoder.is_some() { 2 } else { 0 },
                codec: self.resolved_codec,
                quality: self.config.quality,
                resolution: self.config.resolution,
                partial,
            })
        }

        fn missing_audio_error(&self) -> Option<Error> {
            requested_audio_missing(
                self.config.microphone,
                self.config.system_audio,
                self.microphone_samples,
                self.system_audio_samples,
            )
        }
    }

    struct FinalReport {
        path: PathBuf,
        duration_secs: f64,
        engine_name: String,
        target: Option<CaptureTarget>,
        dimensions: crate::config::Dimensions,
        frames: u64,
        audio_channels: u16,
        codec: VideoCodec,
        quality: Quality,
        resolution: RecordingResolution,
        partial: Option<(Salvageability, String)>,
    }

    fn startup_write_failure(
        mut report: FinalReport,
        temporary_output: Option<TemporaryOutput>,
        error: Error,
    ) -> Result<WorkerStartup> {
        let salvageability = match recover_partial_output(&report.path) {
            Ok(salvageability) => salvageability,
            Err(recovery_error) => {
                if let Some(output) = temporary_output.as_ref()
                    && let Err(cleanup_error) = remove_temporary_output(output)
                {
                    record_abandoned_output(output, &cleanup_error);
                }
                return Err(Error::Storage(format!(
                    "recording initialisation failed ({error}); no recoverable output was retained: \
                     {recovery_error}"
                )));
            }
        };
        report.partial = Some((
            salvageability,
            format!("recording initialisation failed: {error}"),
        ));
        let recording = build_recording_report(report)?;
        Ok(WorkerStartup::Retained {
            recording: Box::new(recording),
            temporary_output,
        })
    }

    fn build_recording_report(report: FinalReport) -> Result<Recording> {
        let metadata = RecordingMetadata {
            size: Some(PhysicalSize::new(
                f64::from(report.dimensions.width),
                f64::from(report.dimensions.height),
            )),
            frames: Some(report.frames),
            audio_channels: Some(report.audio_channels),
            file_size_bytes: std::fs::metadata(&report.path)
                .ok()
                .map(|metadata| metadata.len()),
            video_codec: Some(report.codec),
            quality: Some(report.quality),
            resolution: Some(report.resolution),
            interaction_editable: None,
        };
        let mut recording =
            Recording::native(report.path, report.duration_secs, report.engine_name)?;
        if let Some(target) = report.target {
            recording = recording.with_native_details(target, metadata)?;
        } else {
            recording.metadata = metadata;
            recording.validate()?;
        }
        if let Some((salvageability, reason)) = report.partial {
            recording = recording.into_partial_with_salvageability(salvageability, reason)?;
        }
        Ok(recording)
    }

    fn open_destination(requested: Option<&Path>) -> Result<OpenedDestination> {
        if let Some(path) = requested {
            let file = create_private_file(path)?;
            return Ok(OpenedDestination {
                path: path.to_path_buf(),
                file,
                temporary: None,
            });
        }

        let epoch_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        for _ in 0..1_024 {
            let counter = TEMP_OUTPUT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "scrozz-recording-{}-{epoch_nanos}-{counter}",
                std::process::id()
            ));
            let mut builder = DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(Error::Io(error)),
            }
            if let Err(error) =
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            {
                let _ = std::fs::remove_dir(&directory);
                return Err(Error::Io(error));
            }
            let path = directory.join("recording.mp4");
            match create_private_file(&path) {
                Ok(file) => {
                    return Ok(OpenedDestination {
                        path: path.clone(),
                        file,
                        temporary: Some(TemporaryOutput { directory, path }),
                    });
                }
                Err(error) => {
                    let _ = std::fs::remove_dir(&directory);
                    return Err(error);
                }
            }
        }
        Err(Error::Platform(
            "could not reserve a collision-safe temporary recording path".into(),
        ))
    }

    fn create_private_file(path: &Path) -> Result<File> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(Error::Io)?;
        if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(Error::Io(error));
        }
        Ok(file)
    }

    fn recover_partial_output(path: &Path) -> Result<Salvageability> {
        let (salvageability, valid_prefix_len) = inspect_recovery_file(path).map_err(|error| {
            remove_unreportable_output(
                path,
                format!("could not inspect partial recording output: {error}"),
            )
        })?;
        let salvageability = match salvageability {
            RecoverySalvageability::InitialisationOnly => Salvageability::InitialisationOnly,
            RecoverySalvageability::Playable => Salvageability::Playable,
            RecoverySalvageability::None => {
                return Err(remove_unreportable_output(
                    path,
                    "the partial recording has no complete initialisation segment".into(),
                ));
            }
        };
        let file_len = std::fs::metadata(path)
            .map_err(|error| {
                remove_unreportable_output(
                    path,
                    format!("could not inspect partial recording length: {error}"),
                )
            })?
            .len();
        if valid_prefix_len < file_len {
            let truncate = OpenOptions::new().write(true).open(path).and_then(|file| {
                file.set_len(valid_prefix_len)?;
                file.sync_all()
            });
            if let Err(error) = truncate {
                return Err(remove_unreportable_output(
                    path,
                    format!(
                        "could not truncate the partial recording to a playable prefix: {error}"
                    ),
                ));
            }
        }
        Ok(salvageability)
    }

    fn inspect_recovery_file(path: &Path) -> std::io::Result<(RecoverySalvageability, u64)> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let mut cursor = 0_u64;
        let mut valid_prefix = 0_u64;
        let mut saw_ftyp = false;
        let mut saw_moov = false;
        let mut pending_moof = None;
        let mut complete_fragments = 0_u64;
        let mut header = [0_u8; 16];

        while cursor.saturating_add(8) <= file_len {
            let start = cursor;
            file.seek(SeekFrom::Start(cursor))?;
            file.read_exact(&mut header[..8])?;
            let size32 = u32::from_be_bytes(header[..4].try_into().unwrap());
            let kind: [u8; 4] = header[4..8].try_into().unwrap();
            let (header_size, box_size) = if size32 == 1 {
                if cursor.saturating_add(16) > file_len {
                    break;
                }
                file.read_exact(&mut header[8..16])?;
                (
                    16_u64,
                    u64::from_be_bytes(header[8..16].try_into().unwrap()),
                )
            } else if size32 == 0 {
                (8_u64, file_len - cursor)
            } else {
                (8_u64, u64::from(size32))
            };
            if box_size < header_size || cursor.saturating_add(box_size) > file_len {
                break;
            }
            cursor += box_size;
            match &kind {
                b"ftyp" => saw_ftyp = true,
                b"moov" => saw_moov = true,
                b"moof" if saw_ftyp && saw_moov => pending_moof = Some(start),
                b"mdat" if pending_moof.take().is_some() => {
                    complete_fragments = complete_fragments.saturating_add(1);
                    valid_prefix = cursor;
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
        if let Some(moof_start) = pending_moof {
            valid_prefix = valid_prefix.min(moof_start);
        }
        let salvageability = if complete_fragments > 0 {
            RecoverySalvageability::Playable
        } else if saw_ftyp && saw_moov {
            RecoverySalvageability::InitialisationOnly
        } else {
            RecoverySalvageability::None
        };
        Ok((salvageability, valid_prefix))
    }

    fn remove_unreportable_output(path: &Path, reason: String) -> Error {
        match std::fs::remove_file(path) {
            Ok(()) => Error::Storage(reason),
            Err(error) if error.kind() == ErrorKind::NotFound => Error::Storage(reason),
            Err(error) => Error::Storage(format!(
                "{reason}; invalid output remains at {} because it could not be removed: {error}",
                path.display()
            )),
        }
    }

    fn remove_temporary_output(output: &TemporaryOutput) -> std::io::Result<()> {
        match std::fs::remove_file(&output.path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match std::fs::remove_dir(&output.directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn record_abandoned_output(output: &TemporaryOutput, cleanup_error: &std::io::Error) {
        let Some(state_home) = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local").join("state"))
            })
        else {
            tracing::error!(
                path = %output.path.display(),
                %cleanup_error,
                "abandoned recording could not be removed and no state directory is available"
            );
            return;
        };
        let directory = state_home.join("scrozz");
        let persist = (|| -> std::io::Result<()> {
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(&directory)?;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
            let marker = directory.join("abandoned-recordings.log");
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&marker)?;
            std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600))?;
            writeln!(file, "{:?}\tcleanup failed: {}", output.path, cleanup_error)?;
            file.sync_all()
        })();
        if let Err(error) = persist {
            tracing::error!(
                path = %output.path.display(),
                %cleanup_error,
                persist_error = %error,
                "abandoned recording could not be removed or written to the recovery log"
            );
        }
    }

    fn align_audio_to_timeline(
        mut buffer: AudioBuffer,
        cursor: &mut u64,
        frame_offset: &mut Option<i128>,
    ) -> Result<Vec<f32>> {
        if buffer.channels != 2 || buffer.sample_rate != AacEncoder::SAMPLE_RATE {
            return Err(Error::InvalidRequest(
                "Linux mixed audio must be 48 kHz interleaved stereo".into(),
            ));
        }
        let source_frames = buffer.samples.len() / usize::from(buffer.channels);
        if source_frames == 0 {
            return Ok(Vec::new());
        }
        let offset = frame_offset
            .get_or_insert_with(|| i128::from(*cursor) - i128::from(buffer.start_frame));
        let mapped_start = i128::from(buffer.start_frame) + *offset;
        let mut mapped_start = u64::try_from(mapped_start).map_err(|_| {
            Error::Platform("PipeWire audio timestamp precedes the timeline".into())
        })?;
        if mapped_start < *cursor {
            let overlap_frames = *cursor - mapped_start;
            let overlap_samples = usize::try_from(overlap_frames)
                .unwrap_or(usize::MAX)
                .saturating_mul(usize::from(buffer.channels));
            if overlap_samples >= buffer.samples.len() {
                return Ok(Vec::new());
            }
            buffer.samples.drain(..overlap_samples);
            mapped_start = *cursor;
        }
        let gap_frames = mapped_start - *cursor;
        if gap_frames > MAX_AUDIO_TIMELINE_GAP_FRAMES {
            return Err(Error::Platform(format!(
                "PipeWire audio timeline jumped forward by {gap_frames} frames"
            )));
        }
        let gap_samples = usize::try_from(gap_frames)
            .ok()
            .and_then(|frames| frames.checked_mul(usize::from(buffer.channels)))
            .ok_or_else(|| Error::Platform("PipeWire audio timeline gap exceeds memory".into()))?;
        let output_len = gap_samples
            .checked_add(buffer.samples.len())
            .ok_or_else(|| Error::Platform("PipeWire audio timeline exceeds memory".into()))?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(output_len)
            .map_err(|_| Error::Platform("PipeWire audio timeline exceeds memory".into()))?;
        output.resize(gap_samples, 0.0);
        output.extend(buffer.samples);
        let output_frames = output.len() / usize::from(buffer.channels);
        *cursor = cursor.saturating_add(output_frames as u64);
        Ok(output)
    }

    fn requested_audio_missing(
        microphone_requested: bool,
        system_requested: bool,
        microphone_samples: bool,
        system_samples: bool,
    ) -> Option<Error> {
        let mut missing = Vec::new();
        if microphone_requested && !microphone_samples {
            missing.push("microphone");
        }
        if system_requested && !system_samples {
            missing.push("system audio");
        }
        (!missing.is_empty()).then(|| {
            Error::Platform(format!(
                "requested {} capture produced no samples",
                missing.join(" and ")
            ))
        })
    }

    fn publish_elapsed(shared: &AtomicU64, elapsed: Duration) {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        shared.store(nanos, Ordering::Release);
    }

    fn append_failure(failure: &mut Option<String>, error: Error) {
        match failure {
            Some(existing) => {
                existing.push_str("; ");
                existing.push_str(&error.to_string());
            }
            None => *failure = Some(error.to_string()),
        }
    }

    #[cfg(test)]
    mod tests {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU8, AtomicU64};
        use std::sync::{Arc, mpsc};
        use std::thread;
        use std::time::Duration;

        use scrozz_core::CaptureTarget;

        use super::super::source::AudioBatch;
        use super::{
            AudioWatermarkMixer, FinalReport, LinuxRecordingSession, OpenedDestination,
            SESSION_PAUSED, SESSION_RECORDING, SESSION_STOPPED, TerminalOutcome, WorkerSignal,
            align_audio_to_timeline, build_recording_report, open_destination,
            recover_partial_output, remove_temporary_output, requested_audio_missing,
        };
        use crate::audio::AudioBuffer;
        use crate::config::Dimensions;
        use crate::muxer::{
            EncodedSample, FragmentedMp4, MediaFragment, TrackFragment, VideoCodecConfiguration,
            VideoTrackConfig,
        };
        use crate::{
            Quality, RecordingCompletion, RecordingEngine, RecordingProvenance,
            RecordingResolution, RecordingSession, RecordingState, Salvageability, SessionEvent,
            VideoCodec,
        };

        #[test]
        fn engine_capabilities_are_declarative_and_truthful() {
            let engine = super::super::LinuxEngine;
            assert_eq!(engine.name(), super::super::ENGINE_NAME);
            let capabilities = engine.capabilities();
            assert!(capabilities.video);
            assert!(capabilities.system_audio);
            assert!(capabilities.microphone);
            assert!(capabilities.pause_resume);
            assert!(capabilities.display);
            assert!(capabilities.window);
            assert!(capabilities.region);
            assert!(capabilities.all_displays);
            assert!(capabilities.cursor);
            assert!(capabilities.mp4);
            assert!(capabilities.h264);
            assert!(!capabilities.hevc);
            assert_eq!(capabilities.av1, cfg!(feature = "rav1e-fallback"));
        }

        #[test]
        fn temporary_destinations_are_private_mp4_files_in_private_directories() {
            let first = open_destination(None).unwrap();
            let second = open_destination(None).unwrap();
            assert_ne!(first.path, second.path);
            assert_eq!(
                first.path.extension().and_then(|value| value.to_str()),
                Some("mp4")
            );
            assert_eq!(
                second.path.extension().and_then(|value| value.to_str()),
                Some("mp4")
            );
            for destination in [&first, &second] {
                let temporary = destination.temporary.as_ref().unwrap();
                assert_eq!(
                    std::fs::metadata(&temporary.directory)
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
                assert_eq!(
                    std::fs::metadata(&temporary.path)
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
            let first_temporary = first.temporary.clone().unwrap();
            let second_temporary = second.temporary.clone().unwrap();
            drop((first.file, second.file));
            remove_temporary_output(&first_temporary).unwrap();
            remove_temporary_output(&second_temporary).unwrap();
        }

        #[test]
        fn report_contains_native_provenance_metadata_and_initialisation_partial() {
            let OpenedDestination {
                path,
                file,
                temporary,
            } = open_destination(None).unwrap();
            drop(file);
            std::fs::write(&path, b"test").unwrap();
            let target = CaptureTarget::AllDisplays;
            let recording = build_recording_report(FinalReport {
                path: path.clone(),
                duration_secs: 1.25,
                engine_name: "Linux test source".into(),
                target: Some(target.clone()),
                dimensions: Dimensions {
                    width: 1280,
                    height: 720,
                },
                frames: 37,
                audio_channels: 2,
                codec: VideoCodec::Av1,
                quality: Quality::High,
                resolution: RecordingResolution::MaxShortestEdge(720),
                partial: Some((
                    Salvageability::InitialisationOnly,
                    "no complete media fragment".into(),
                )),
            })
            .unwrap();

            assert_eq!(recording.path, path);
            assert_eq!(
                recording.provenance,
                RecordingProvenance::Native {
                    engine: "Linux test source".into(),
                    target: Some(target),
                }
            );
            assert_eq!(
                recording.completion,
                RecordingCompletion::Partial {
                    salvageability: Salvageability::InitialisationOnly,
                    reason: "no complete media fragment".into(),
                }
            );
            assert_eq!(
                recording.metadata.size,
                Some(scrozz_core::PhysicalSize::new(1280.0, 720.0))
            );
            assert_eq!(recording.metadata.frames, Some(37));
            assert_eq!(recording.metadata.audio_channels, Some(2));
            assert_eq!(recording.metadata.file_size_bytes, Some(4));
            assert_eq!(recording.metadata.video_codec, Some(VideoCodec::Av1));
            assert_eq!(recording.metadata.quality, Some(Quality::High));
            assert_eq!(
                recording.metadata.resolution,
                Some(RecordingResolution::MaxShortestEdge(720))
            );
            remove_temporary_output(&temporary.unwrap()).unwrap();
        }

        #[test]
        fn report_does_not_invent_a_native_target() {
            let OpenedDestination {
                path,
                file,
                temporary,
            } = open_destination(None).unwrap();
            drop(file);
            let recording = build_recording_report(FinalReport {
                path,
                duration_secs: 0.0,
                engine_name: "Wayland test source".into(),
                target: None,
                dimensions: Dimensions {
                    width: 2,
                    height: 2,
                },
                frames: 0,
                audio_channels: 0,
                codec: VideoCodec::Av1,
                quality: Quality::Balanced,
                resolution: RecordingResolution::Native,
                partial: Some((Salvageability::InitialisationOnly, "test partial".into())),
            })
            .unwrap();
            assert_eq!(
                recording.provenance,
                RecordingProvenance::Native {
                    engine: "Wayland test source".into(),
                    target: None,
                }
            );
            remove_temporary_output(&temporary.unwrap()).unwrap();
        }

        #[test]
        fn partial_output_is_truncated_to_the_last_complete_fragment() {
            let OpenedDestination {
                path,
                file,
                temporary,
            } = open_destination(None).unwrap();
            let video = VideoTrackConfig {
                width: 2,
                height: 2,
                timescale: 30,
                codec: VideoCodecConfiguration::Av1(vec![0x81, 0, 0, 0]),
            };
            let mut muxer = FragmentedMp4::new(file, &video, None).unwrap();
            muxer
                .write_fragment(&MediaFragment {
                    video: TrackFragment {
                        base_decode_time: 0,
                        samples: vec![EncodedSample {
                            data: vec![1, 2, 3],
                            duration: 1,
                            keyframe: true,
                        }],
                    },
                    audio: None,
                })
                .unwrap();
            let mut file = muxer.finish().unwrap();
            file.sync_all().unwrap();
            let valid_len = file.metadata().unwrap().len();
            file.write_all(&[0, 0, 0, 16, b'm', b'o', b'o', b'f'])
                .unwrap();
            file.sync_all().unwrap();
            drop(file);

            assert_eq!(
                recover_partial_output(&path).unwrap(),
                Salvageability::Playable
            );
            assert_eq!(std::fs::metadata(&path).unwrap().len(), valid_len);
            remove_temporary_output(&temporary.unwrap()).unwrap();
        }

        #[test]
        fn audio_timeline_preserves_pts_gaps_and_rebases_after_pause() {
            let mut cursor = 0;
            let mut offset = None;
            let first = align_audio_to_timeline(
                AudioBuffer {
                    sample_rate: 48_000,
                    channels: 2,
                    start_frame: 100,
                    samples: vec![0.25, -0.25, 0.5, -0.5],
                },
                &mut cursor,
                &mut offset,
            )
            .unwrap();
            assert_eq!(first, vec![0.25, -0.25, 0.5, -0.5]);
            assert_eq!(cursor, 2);

            let second = align_audio_to_timeline(
                AudioBuffer {
                    sample_rate: 48_000,
                    channels: 2,
                    start_frame: 104,
                    samples: vec![0.75, -0.75],
                },
                &mut cursor,
                &mut offset,
            )
            .unwrap();
            assert_eq!(second, vec![0.0, 0.0, 0.0, 0.0, 0.75, -0.75]);
            assert_eq!(cursor, 5);

            offset = None;
            let resumed = align_audio_to_timeline(
                AudioBuffer {
                    sample_rate: 48_000,
                    channels: 2,
                    start_frame: 48_000,
                    samples: vec![1.0, -1.0],
                },
                &mut cursor,
                &mut offset,
            )
            .unwrap();
            assert_eq!(resumed, vec![1.0, -1.0]);
            assert_eq!(cursor, 6);
        }

        #[test]
        fn late_audio_within_the_watermark_window_is_mixed_instead_of_dropped() {
            let mut mixer = AudioWatermarkMixer::new(true, true);
            mixer
                .push(AudioBatch {
                    microphone: vec![AudioBuffer {
                        sample_rate: 48_000,
                        channels: 1,
                        start_frame: 0,
                        samples: vec![0.25; 4_800],
                    }],
                    system: Vec::new(),
                })
                .unwrap();
            assert!(mixer.next(false).unwrap().is_none());

            mixer
                .push(AudioBatch {
                    microphone: Vec::new(),
                    system: vec![AudioBuffer {
                        sample_rate: 48_000,
                        channels: 1,
                        start_frame: 0,
                        samples: vec![0.5; 4_800],
                    }],
                })
                .unwrap();
            let mixed = mixer
                .next(false)
                .unwrap()
                .expect("both sources reached watermark");
            assert_eq!(mixed.start_frame, 0);
            assert_eq!(mixed.samples.len(), 4_800 * 2);
            assert!(mixed.samples.iter().all(|sample| *sample == 0.75));
        }

        #[test]
        fn audio_timeline_refuses_unbounded_silence() {
            let mut cursor = 0;
            let mut offset = Some(0);
            let error = align_audio_to_timeline(
                AudioBuffer {
                    sample_rate: 48_000,
                    channels: 2,
                    start_frame: 48_001,
                    samples: vec![0.25, -0.25],
                },
                &mut cursor,
                &mut offset,
            )
            .expect_err("a discontinuity larger than one second must fail");
            assert!(error.to_string().contains("jumped forward"));
        }

        #[test]
        fn requested_audio_without_samples_is_a_product_failure() {
            let error = requested_audio_missing(true, true, false, true).unwrap();
            assert!(error.to_string().contains("microphone"));
            assert!(!error.to_string().contains("system audio"));
            assert!(requested_audio_missing(true, false, true, false).is_none());
        }

        #[test]
        fn poll_is_one_shot_and_stop_reuses_the_finished_recording() {
            let (control, _controls) = mpsc::channel();
            let (signals, signal_rx) = mpsc::channel();
            let elapsed_nanos = Arc::new(AtomicU64::new(1_250_000_000));
            let recording = crate::Recording::native("finished.mp4", 1.25, "Linux test").unwrap();
            let expected = recording.clone();
            let worker = thread::spawn(move || {
                signals.send(WorkerSignal::FirstFrame).unwrap();
                signals
                    .send(WorkerSignal::Terminal(Box::new(Ok(recording))))
                    .unwrap();
            });
            let mut session = LinuxRecordingSession {
                control,
                signals: signal_rx,
                worker: Some(worker),
                terminal: None,
                terminal_emitted: false,
                elapsed_nanos,
                state: Arc::new(AtomicU8::new(SESSION_RECORDING)),
                temporary_output: None,
            };

            assert!(matches!(poll_until(&mut session), SessionEvent::FirstFrame));
            let SessionEvent::Finished(polled) = poll_until(&mut session) else {
                panic!("expected terminal finished event");
            };
            assert_eq!(polled, expected);
            assert!(session.poll().is_none());
            assert_eq!(session.engine_elapsed_secs(), Some(1.25));
            assert_eq!(session.state(), RecordingState::Stopped);
            assert_eq!(Box::new(session).stop().unwrap(), expected);
        }

        #[test]
        fn failed_terminal_event_remains_coherent_for_stop() {
            let (control, _controls) = mpsc::channel();
            let (signals, signal_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                signals
                    .send(WorkerSignal::Terminal(Box::new(Err(
                        scrozz_core::Error::Platform("terminal failure".into()),
                    ))))
                    .unwrap();
            });
            let mut session = LinuxRecordingSession {
                control,
                signals: signal_rx,
                worker: Some(worker),
                terminal: None,
                terminal_emitted: false,
                elapsed_nanos: Arc::new(AtomicU64::new(0)),
                state: Arc::new(AtomicU8::new(SESSION_RECORDING)),
                temporary_output: None,
            };
            let SessionEvent::Failed(error) = poll_until(&mut session) else {
                panic!("expected terminal failure event");
            };
            assert!(error.to_string().contains("terminal failure"));
            assert!(session.poll().is_none());
            let stop_error = Box::new(session).stop().unwrap_err();
            assert!(stop_error.to_string().contains("terminal failure"));
        }

        #[test]
        fn cached_terminal_failure_preserves_error_classification() {
            let (control, _controls) = mpsc::channel();
            let (signals, signal_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                signals
                    .send(WorkerSignal::Terminal(Box::new(Err(
                        scrozz_core::Error::Cancelled,
                    ))))
                    .unwrap();
            });
            let mut session = LinuxRecordingSession {
                control,
                signals: signal_rx,
                worker: Some(worker),
                terminal: None,
                terminal_emitted: false,
                elapsed_nanos: Arc::new(AtomicU64::new(0)),
                state: Arc::new(AtomicU8::new(SESSION_RECORDING)),
                temporary_output: None,
            };
            let SessionEvent::Failed(error) = poll_until(&mut session) else {
                panic!("expected terminal failure event");
            };
            assert!(error.is_cancellation());
            assert!(Box::new(session).stop().unwrap_err().is_cancellation());
        }

        #[test]
        fn state_reports_paused_and_ended_workers() {
            let (control, _controls) = mpsc::channel();
            let (_signals, signal_rx) = mpsc::channel();
            let state = Arc::new(AtomicU8::new(SESSION_PAUSED));
            let session = LinuxRecordingSession {
                control,
                signals: signal_rx,
                worker: None,
                terminal: None,
                terminal_emitted: false,
                elapsed_nanos: Arc::new(AtomicU64::new(0)),
                state: Arc::clone(&state),
                temporary_output: None,
            };
            assert_eq!(session.state(), RecordingState::Paused);
            state.store(SESSION_STOPPED, std::sync::atomic::Ordering::Release);
            assert_eq!(session.state(), RecordingState::Stopped);
        }

        #[test]
        fn dropping_an_unreported_session_removes_temporary_output() {
            let OpenedDestination {
                path,
                file,
                temporary,
            } = open_destination(None).unwrap();
            drop(file);
            std::fs::write(&path, b"abandoned").unwrap();
            let temporary = temporary.unwrap();
            let directory = temporary.directory.clone();
            let recording = crate::Recording::native(&path, 0.0, "Linux test").unwrap();
            let (control, _controls) = mpsc::channel();
            let (signals, signal_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                signals
                    .send(WorkerSignal::Terminal(Box::new(Ok(recording))))
                    .unwrap();
            });
            drop(LinuxRecordingSession {
                control,
                signals: signal_rx,
                worker: Some(worker),
                terminal: None,
                terminal_emitted: false,
                elapsed_nanos: Arc::new(AtomicU64::new(0)),
                state: Arc::new(AtomicU8::new(SESSION_RECORDING)),
                temporary_output: Some(temporary),
            });
            assert!(!path.exists());
            assert!(!directory.exists());
        }

        fn poll_until(session: &mut LinuxRecordingSession) -> SessionEvent {
            for _ in 0..1_000 {
                if let Some(event) = session.poll() {
                    return event;
                }
                thread::sleep(Duration::from_millis(1));
            }
            panic!("session produced no event");
        }

        #[test]
        fn terminal_outcome_conversion_preserves_success() {
            let recording = crate::Recording::native("cached.mp4", 0.0, "Linux test").unwrap();
            assert_eq!(
                TerminalOutcome::Finished(recording.clone())
                    .into_result()
                    .unwrap(),
                recording
            );
        }
    }
}
