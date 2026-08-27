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
    use std::collections::VecDeque;
    use std::fs::{File, OpenOptions};
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use scrozz_core::{CaptureTarget, Error, PhysicalSize, Result};

    use super::source::{PipeWireAudio, VideoSource, open_video_source};
    use crate::audio::AudioMixer;
    use crate::config::{RecordingConfig, resolve_dimensions};
    use crate::encoder::aac::{AacEncoder, EncodedAudioPacket};
    use crate::encoder::{self, EncodedVideoPacket, VideoEncoder, VideoEncoderSettings};
    use crate::format::{Nv12Frame, PackedFrame, to_nv12};
    use crate::muxer::{
        AudioTrackConfig, EncodedSample, FragmentedMp4, MediaFragment, TrackFragment,
        VideoTrackConfig,
    };
    use crate::pacing::{FramePacer, PacingDecision};
    use crate::state::{RecorderCommand, RecorderState, RecordingStateMachine};
    use crate::timeline::RecordingTimeline;
    use crate::{
        Quality, Recording, RecordingMetadata, RecordingRequest, RecordingResolution,
        RecordingSession, Salvageability, SessionEvent, VideoCodec,
    };

    static TEMP_OUTPUT_COUNTER: AtomicU64 = AtomicU64::new(0);

    enum Control {
        Pause(SyncSender<Result<()>>),
        Resume(SyncSender<Result<()>>),
        Stop,
    }

    enum WorkerSignal {
        FirstFrame,
        Terminal(Box<Result<Recording>>),
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
            Some(event)
        }
    }

    impl RecordingSession for LinuxRecordingSession {
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
            self.terminal
                .take()
                .expect("terminal outcome was received")
                .into_result()
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
        }
    }

    pub(super) fn start(request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
        let config = RecordingConfig::try_from(request)?;
        let (control_tx, control_rx) = mpsc::channel();
        let (initialised_tx, initialised_rx) = mpsc::sync_channel(0);
        let (signal_tx, signal_rx) = mpsc::channel();
        let elapsed_nanos = Arc::new(AtomicU64::new(0));
        let worker_elapsed = Arc::clone(&elapsed_nanos);
        let worker = thread::Builder::new()
            .name("scrozz-linux-recording".into())
            .spawn(move || {
                worker_entry(
                    config,
                    control_rx,
                    initialised_tx,
                    signal_tx,
                    worker_elapsed,
                )
            })
            .map_err(Error::Io)?;

        match initialised_rx.recv() {
            Ok(Ok(())) => Ok(Box::new(LinuxRecordingSession {
                control: control_tx,
                signals: signal_rx,
                worker: Some(worker),
                terminal: None,
                terminal_emitted: false,
                elapsed_nanos,
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
        initialised: SyncSender<Result<()>>,
        signals: Sender<WorkerSignal>,
        elapsed_nanos: Arc<AtomicU64>,
    ) {
        match Worker::initialize(config) {
            Ok(WorkerStartup::Ready(mut worker)) => {
                if initialised.send(Ok(())).is_ok() {
                    let result = worker.run(&controls, &signals, &elapsed_nanos);
                    drop(worker);
                    let _ = signals.send(WorkerSignal::Terminal(Box::new(result)));
                }
            }
            Ok(WorkerStartup::Retained(recording)) => {
                if initialised.send(Ok(())).is_ok() {
                    let _ = signals.send(WorkerSignal::Terminal(Box::new(Ok(*recording))));
                }
            }
            Err(error) => {
                let _ = initialised.send(Err(error));
            }
        }
    }

    enum WorkerStartup {
        Ready(Box<Worker>),
        Retained(Box<Recording>),
    }

    struct Worker {
        config: RecordingConfig,
        source: Box<dyn VideoSource>,
        engine_name: String,
        first_frame: Option<PackedFrame>,
        dimensions: crate::config::Dimensions,
        video_encoder: Box<dyn VideoEncoder>,
        resolved_codec: VideoCodec,
        audio_source: Option<PipeWireAudio>,
        audio_encoder: Option<AacEncoder>,
        audio_mixer: AudioMixer,
        muxer: Option<FragmentedMp4<File>>,
        state: RecordingStateMachine,
        pending_video: Option<EncodedVideoPacket>,
        pending_audio: VecDeque<EncodedAudioPacket>,
        fragments_written: u64,
        frames_submitted: u64,
    }

    impl Worker {
        fn initialize(mut config: RecordingConfig) -> Result<WorkerStartup> {
            let mut source = open_video_source(&config.target, config.show_cursor)?;
            let first_frame = source.next_frame(Duration::from_secs(15))?.ok_or_else(|| {
                Error::Platform("the capture source delivered no frame within 15 seconds".into())
            })?;
            first_frame.validate()?;
            let backing_scale = source.backing_scale();
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
            });
            let (destination, file) = open_destination(config.destination.as_deref())?;
            config.destination = Some(destination.clone());
            let engine_name = format!("{} via {}", super::ENGINE_NAME, source.name());
            let muxer = match FragmentedMp4::new(file, &video_track, audio_track.as_ref()) {
                Ok(muxer) => muxer,
                Err(error) => {
                    return startup_write_failure(
                        &config,
                        destination,
                        dimensions,
                        resolved_codec,
                        engine_name,
                        u16::from(audio_encoder.is_some()) * 2,
                        Error::Io(error),
                    );
                }
            };
            if let Err(error) = muxer.writer().sync_data() {
                drop(muxer);
                return startup_write_failure(
                    &config,
                    destination,
                    dimensions,
                    resolved_codec,
                    engine_name,
                    u16::from(audio_encoder.is_some()) * 2,
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

            Ok(WorkerStartup::Ready(Box::new(Self {
                config,
                source,
                engine_name,
                first_frame: Some(first_frame),
                dimensions,
                video_encoder,
                resolved_codec,
                audio_source,
                audio_encoder: audio_encoder.take(),
                audio_mixer: AudioMixer::new(AacEncoder::SAMPLE_RATE),
                muxer: Some(muxer),
                state,
                pending_video: None,
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
        ) -> Result<Recording> {
            let started = Instant::now();
            let mut timeline = RecordingTimeline::new(Duration::ZERO);
            let mut pacer = FramePacer::new(self.config.fps.get());
            let interval =
                Duration::from_nanos(1_000_000_000_u64 / u64::from(self.config.fps.get()));
            let mut next_capture = started;
            let mut last_frame: Option<Nv12Frame> = None;
            let mut failure: Option<String> = None;
            let mut user_stopped = false;
            let mut first_frame_signalled = false;

            loop {
                if self.state.state() == RecorderState::Paused {
                    match controls.recv_timeout(Duration::from_millis(50)) {
                        Ok(control) => {
                            if self.handle_control(control, started, &mut timeline, elapsed_nanos) {
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
                            if self.handle_control(control, started, &mut timeline, elapsed_nanos) {
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
                            if self.handle_control(control, started, &mut timeline, elapsed_nanos) {
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

                if let Err(error) = self.capture_audio() {
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
                    PacingDecision::Emit {
                        repeat_previous, ..
                    } => {
                        if let Some(previous) = &last_frame {
                            for _ in 0..repeat_previous {
                                if let Err(error) = self.encode_video(previous) {
                                    failure = Some(error.to_string());
                                    break;
                                }
                            }
                        }
                        if failure.is_some() {
                            break;
                        }
                        match to_nv12(&captured, self.dimensions) {
                            Ok(frame) => {
                                if let Err(error) = self.encode_video(&frame) {
                                    failure = Some(error.to_string());
                                    break;
                                }
                                if !first_frame_signalled {
                                    let _ = signals.send(WorkerSignal::FirstFrame);
                                    first_frame_signalled = true;
                                }
                                last_frame = Some(frame);
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
                    let _ = reply.send(result);
                    false
                }
                Control::Stop => {
                    let _ = self.state.apply(RecorderCommand::Stop);
                    true
                }
            }
        }

        fn capture_audio(&mut self) -> Result<()> {
            let Some(source) = &mut self.audio_source else {
                return Ok(());
            };
            let (microphone, system) = source.poll()?;
            let mixed = self.audio_mixer.mix(microphone.as_ref(), system.as_ref())?;
            if mixed.samples.is_empty() {
                return Ok(());
            }
            if let Some(encoder) = &mut self.audio_encoder {
                self.pending_audio
                    .extend(encoder.push_interleaved(&mixed.samples)?);
            }
            Ok(())
        }

        fn encode_video(&mut self, frame: &Nv12Frame) -> Result<()> {
            let packets = self.video_encoder.encode(frame)?;
            self.frames_submitted = self.frames_submitted.saturating_add(1);
            for packet in packets {
                if let Some(previous) = self.pending_video.replace(packet) {
                    self.write_fragment(previous)?;
                }
            }
            Ok(())
        }

        fn write_fragment(&mut self, video: EncodedVideoPacket) -> Result<()> {
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
                    base_decode_time: video.frame_index,
                    samples: vec![EncodedSample {
                        data: video.data,
                        duration: 1,
                        keyframe: video.keyframe,
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
            match self.capture_audio() {
                Ok(()) => {}
                Err(error) => append_failure(&mut failure, error),
            }
            match self.video_encoder.finish() {
                Ok(packets) => {
                    for packet in packets {
                        if let Some(previous) = self.pending_video.replace(packet)
                            && let Err(error) = self.write_fragment(previous)
                        {
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
            if let Some(video) = self.pending_video.take()
                && let Err(error) = self.write_fragment(video)
            {
                append_failure(&mut failure, error);
            }

            let salvageability = if self.fragments_written > 0 {
                Salvageability::Playable
            } else {
                Salvageability::InitialisationOnly
            };
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
            if partial_reason.is_some() {
                let _ = self.state.apply(RecorderCommand::Fail);
            } else {
                let _ = self.state.apply(RecorderCommand::Finish);
            }
            build_recording_report(FinalReport {
                path: destination,
                duration_secs,
                engine_name: self.engine_name.clone(),
                target: self.config.target.clone(),
                dimensions: self.dimensions,
                frames: self.frames_submitted,
                audio_channels: if self.audio_encoder.is_some() { 2 } else { 0 },
                codec: self.resolved_codec,
                quality: self.config.quality,
                resolution: self.config.resolution,
                partial: partial_reason.map(|reason| (salvageability, reason)),
            })
        }
    }

    struct FinalReport {
        path: PathBuf,
        duration_secs: f64,
        engine_name: String,
        target: CaptureTarget,
        dimensions: crate::config::Dimensions,
        frames: u64,
        audio_channels: u16,
        codec: VideoCodec,
        quality: Quality,
        resolution: RecordingResolution,
        partial: Option<(Salvageability, String)>,
    }

    fn startup_write_failure(
        config: &RecordingConfig,
        path: PathBuf,
        dimensions: crate::config::Dimensions,
        codec: VideoCodec,
        engine_name: String,
        audio_channels: u16,
        error: Error,
    ) -> Result<WorkerStartup> {
        match std::fs::remove_file(&path) {
            Ok(()) => Err(error),
            Err(remove_error) if remove_error.kind() == ErrorKind::NotFound => Err(error),
            Err(remove_error) => {
                let recording = build_recording_report(FinalReport {
                    path,
                    duration_secs: 0.0,
                    engine_name,
                    target: config.target.clone(),
                    dimensions,
                    frames: 0,
                    audio_channels,
                    codec,
                    quality: config.quality,
                    resolution: config.resolution,
                    partial: Some((
                        Salvageability::InitialisationOnly,
                        format!(
                            "recording initialisation failed ({error}); the retained output could not be removed ({remove_error})"
                        ),
                    )),
                })?;
                Ok(WorkerStartup::Retained(Box::new(recording)))
            }
        }
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
        };
        let mut recording =
            Recording::native(report.path, report.duration_secs, report.engine_name)?
                .with_native_details(report.target, metadata)?;
        if let Some((salvageability, reason)) = report.partial {
            recording = recording.into_partial_with_salvageability(salvageability, reason)?;
        }
        Ok(recording)
    }

    fn open_destination(requested: Option<&Path>) -> Result<(PathBuf, File)> {
        if let Some(path) = requested {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(Error::Io)?;
            return Ok((path.to_path_buf(), file));
        }

        let epoch_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        for _ in 0..1_024 {
            let counter = TEMP_OUTPUT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "scrozz-recording-{}-{epoch_nanos}-{counter}.mp4",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(Error::Io(error)),
            }
        }
        Err(Error::Platform(
            "could not reserve a collision-safe temporary recording path".into(),
        ))
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
        use std::sync::atomic::AtomicU64;
        use std::sync::{Arc, mpsc};
        use std::thread;
        use std::time::Duration;

        use scrozz_core::CaptureTarget;

        use super::{
            FinalReport, LinuxRecordingSession, TerminalOutcome, WorkerSignal,
            build_recording_report, open_destination,
        };
        use crate::config::Dimensions;
        use crate::{
            Quality, RecordingCompletion, RecordingEngine, RecordingProvenance,
            RecordingResolution, RecordingSession, Salvageability, SessionEvent, VideoCodec,
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
        fn temporary_destinations_are_mp4_and_collision_safe() {
            let (first_path, first_file) = open_destination(None).unwrap();
            let (second_path, second_file) = open_destination(None).unwrap();
            assert_ne!(first_path, second_path);
            assert_eq!(
                first_path.extension().and_then(|value| value.to_str()),
                Some("mp4")
            );
            assert_eq!(
                second_path.extension().and_then(|value| value.to_str()),
                Some("mp4")
            );
            drop((first_file, second_file));
            std::fs::remove_file(first_path).unwrap();
            std::fs::remove_file(second_path).unwrap();
        }

        #[test]
        fn report_contains_native_provenance_metadata_and_initialisation_partial() {
            let (path, file) = open_destination(None).unwrap();
            drop(file);
            std::fs::write(&path, b"test").unwrap();
            let target = CaptureTarget::AllDisplays;
            let recording = build_recording_report(FinalReport {
                path: path.clone(),
                duration_secs: 1.25,
                engine_name: "Linux test source".into(),
                target: target.clone(),
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
            std::fs::remove_file(path).unwrap();
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
            };

            assert!(matches!(poll_until(&mut session), SessionEvent::FirstFrame));
            let SessionEvent::Finished(polled) = poll_until(&mut session) else {
                panic!("expected terminal finished event");
            };
            assert_eq!(polled, expected);
            assert!(session.poll().is_none());
            assert_eq!(session.engine_elapsed_secs(), Some(1.25));
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
            };
            let SessionEvent::Failed(error) = poll_until(&mut session) else {
                panic!("expected terminal failure event");
            };
            assert!(error.is_cancellation());
            assert!(Box::new(session).stop().unwrap_err().is_cancellation());
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
