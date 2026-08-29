//! Recording worker, lifecycle commands, and orderly finalisation.

use std::{
    hash::{BuildHasher as _, Hasher as _, RandomState},
    os::windows::ffi::OsStrExt,
    os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use scrozz_core::{CaptureTarget, Error, PhysicalSize, Result};
use windows::{
    Graphics::Capture::GraphicsCaptureSession,
    Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{
            CreateDirectoryW, CreateFileW, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_ID_INFO,
            FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FileIdInfo, FileRenameInfo, GetFileInformationByHandleEx,
            OPEN_EXISTING, SetFileInformationByHandle,
        },
    },
    core::BOOL,
    core::PCWSTR,
};

use crate::{
    CameraFeed, CameraFrame, CameraRecordingMetadata, CameraRuntimeStatus, Recording,
    RecordingMetadata, RecordingRequest, RecordingResolution, RecordingSession, RecordingState,
    SessionEvent, VideoCodec, camera::CameraCompositor, settings::CameraSettings,
};

use super::{
    audio::{AudioCapture, qpc_now_hns},
    camera::CameraCapture,
    com::{Apartment, MediaFoundation},
    device::Device,
    encoder::Encoder,
    mix::Mixer,
    plan::{self, EncoderPlan},
    salvage::{self, Outcome},
    target,
    terminal::{NativeRecording, SessionState, SharedSessionState, TerminalCache},
    timing::{
        FramePacer, HNS_PER_SECOND, Timeline, audio_drain_limit, audio_frames_to_hns,
        native_timestamp_is_plausible,
    },
    video::{Capture, FramePacket, Signal},
};

const FRAME_QUEUE_CAPACITY: usize = 3;
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_CHUNK_FRAMES: u32 = 480;
const AUDIO_SETTLE_HNS: i64 = HNS_PER_SECOND / 10;
const MAX_AUDIO_DRAIN_SPAN_HNS: i64 = 5 * HNS_PER_SECOND;
const IDLE_WAIT: Duration = Duration::from_millis(5);
const TARGET_REVALIDATION_INTERVAL: Duration = Duration::from_millis(250);
static OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Starts a worker and waits until every native subsystem is live.
pub fn start(request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
    let mut request = request.clone();
    if let Some(destination) = request.destination.as_mut() {
        *destination = absolute_path(destination)?;
    }
    let elapsed_hns = Arc::new(AtomicU64::new(0));
    let camera = request.camera.as_ref().map(CameraFeed::new).transpose()?;
    let (commands_tx, commands_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (events_tx, events_rx) = mpsc::channel();
    let state = Arc::new(SharedSessionState::new());
    let thread = thread::Builder::new()
        .name("scrozz-recording".into())
        .spawn({
            let elapsed_hns = Arc::clone(&elapsed_hns);
            let worker_state = Arc::clone(&state);
            let worker_camera = camera.clone();
            move || {
                let _ended = EndStateGuard(Arc::clone(&worker_state));
                match Worker::initialize(
                    request,
                    commands_rx,
                    Arc::clone(&elapsed_hns),
                    Arc::clone(&worker_state),
                    events_tx.clone(),
                    worker_camera,
                ) {
                    Ok(worker) => {
                        if ready_tx.send(Ok(())).is_ok() {
                            let result = worker.run();
                            worker_state.set(SessionState::Ended);
                            let _ = events_tx.send(WorkerEvent::Terminal(Box::new(result)));
                        }
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            }
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Box::new(WindowsSession {
            commands: commands_tx,
            events: events_rx,
            thread: Some(thread),
            state,
            elapsed_hns,
            terminal: TerminalCache::default(),
            camera,
        })),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(Error::Platform(
                "recording worker exited during startup".into(),
            ))
        }
    }
}

struct WindowsSession {
    commands: mpsc::Sender<Command>,
    events: Receiver<WorkerEvent>,
    thread: Option<JoinHandle<()>>,
    state: Arc<SharedSessionState>,
    elapsed_hns: Arc<AtomicU64>,
    terminal: TerminalCache,
    camera: Option<CameraFeed>,
}

struct EndStateGuard(Arc<SharedSessionState>);

impl Drop for EndStateGuard {
    fn drop(&mut self) {
        self.0.set(SessionState::Ended);
    }
}

#[derive(Debug)]
enum WorkerEvent {
    FirstFrame,
    Warning(String),
    Terminal(Box<Result<NativeRecording>>),
}

impl RecordingSession for WindowsSession {
    fn state(&self) -> RecordingState {
        self.state.recording_state()
    }

    fn pause(&mut self) -> Result<()> {
        if self.state.get() != SessionState::Running {
            return Err(Error::InvalidRequest(
                "recording is already paused or has ended".into(),
            ));
        }
        self.command_with_ack(Command::Pause)?;
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if self.state.get() != SessionState::Paused {
            return Err(Error::InvalidRequest(
                "recording is not paused or has ended".into(),
            ));
        }
        self.command_with_ack(Command::Resume)?;
        Ok(())
    }

    fn poll(&mut self) -> Option<SessionEvent> {
        if self.terminal.is_some() {
            return self.terminal.emit();
        }
        match self.events.try_recv() {
            Ok(WorkerEvent::FirstFrame) => Some(SessionEvent::FirstFrame),
            Ok(WorkerEvent::Warning(warning)) => Some(SessionEvent::Warning(warning)),
            Ok(WorkerEvent::Terminal(result)) => {
                self.terminal.cache(*result);
                self.terminal.emit()
            }
            Err(TryRecvError::Disconnected) => {
                self.cache_disconnected_terminal();
                self.terminal.emit()
            }
            Err(TryRecvError::Empty) => None,
        }
    }

    fn engine_elapsed_secs(&self) -> Option<f64> {
        Some(self.elapsed_hns.load(Ordering::Acquire) as f64 / HNS_PER_SECOND as f64)
    }

    fn camera_status(&self) -> Option<CameraRuntimeStatus> {
        self.camera.as_ref().map(CameraFeed::status)
    }

    fn camera_preview(&self) -> Option<crate::CameraPreview> {
        self.camera.as_ref().and_then(|camera| {
            let elapsed =
                Duration::from_nanos(self.elapsed_hns.load(Ordering::Acquire).saturating_mul(100));
            camera.preview(elapsed)
        })
    }

    fn update_camera(&mut self, settings: CameraSettings) -> Result<()> {
        self.camera
            .as_ref()
            .ok_or_else(|| Error::InvalidRequest("this recording has no active camera".into()))?
            .update_settings(settings)
    }

    fn stop(mut self: Box<Self>) -> Result<Recording> {
        if !self.terminal.is_some() {
            let _ = self.commands.send(Command::Stop);
            self.receive_terminal();
        }
        if let Err(error) = self.join() {
            tracing::error!(%error, "Windows recording worker did not join after finalisation");
        }
        self.terminal
            .take_result()
            .expect("terminal outcome was received")
    }
}

impl WindowsSession {
    fn command_with_ack(&self, build: fn(SyncSender<Result<()>>) -> Command) -> Result<()> {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        self.commands
            .send(build(ack_tx))
            .map_err(|_| Error::InvalidRequest("recording session has ended".into()))?;
        ack_rx
            .recv()
            .map_err(|_| Error::Platform("recording worker did not acknowledge command".into()))?
    }

    fn join(&mut self) -> Result<()> {
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| Error::Platform("recording worker panicked".into()))?;
        }
        Ok(())
    }

    fn cache_disconnected_terminal(&mut self) {
        let join_detail = self
            .join()
            .err()
            .map(|error| format!(": {error}"))
            .unwrap_or_default();
        self.terminal.cache_error(Error::Platform(format!(
            "recording worker exited without a terminal result{join_detail}"
        )));
    }

    fn receive_terminal(&mut self) {
        while !self.terminal.is_some() {
            match self.events.recv() {
                Ok(WorkerEvent::FirstFrame | WorkerEvent::Warning(_)) => {}
                Ok(WorkerEvent::Terminal(result)) => self.terminal.cache(*result),
                Err(_) => self.cache_disconnected_terminal(),
            }
        }
    }

    fn report_abandoned_terminal(&self) {
        let Some(outcome) = self.terminal.outcome() else {
            return;
        };
        if let Some(recording) = outcome.recording() {
            tracing::warn!(
                path = %recording.path().display(),
                partial = recording.is_partial(),
                "recording owner dropped the session without collecting its retained output"
            );
        } else if let Some(error) = outcome.error() {
            tracing::error!(
                %error,
                "recording owner dropped the session after native finalisation failed"
            );
        }
    }
}

impl Drop for WindowsSession {
    fn drop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        if !self.terminal.is_some() {
            let _ = self.commands.send(Command::Stop);
            self.receive_terminal();
        }
        self.report_abandoned_terminal();
        if let Err(error) = self.join() {
            tracing::error!(%error, "Windows recording worker did not join during drop");
        }
    }
}

enum Command {
    Pause(SyncSender<Result<()>>),
    Resume(SyncSender<Result<()>>),
    Stop,
}

struct Worker {
    commands: Receiver<Command>,
    events: Sender<WorkerEvent>,
    elapsed_hns: Arc<AtomicU64>,
    state: Arc<SharedSessionState>,
    paused: Arc<AtomicBool>,
    capture: Option<Capture>,
    frames: Receiver<FramePacket>,
    signals: Receiver<Signal>,
    audio: Option<AudioCapture>,
    camera: Option<CameraCapture>,
    camera_feed: Option<CameraFeed>,
    camera_compositor: CameraCompositor,
    mixer: Option<Mixer>,
    encoder: Option<Encoder>,
    output: OutputPaths,
    target: CaptureTarget,
    quality: crate::Quality,
    resolution: RecordingResolution,
    encoder_plan: EncoderPlan,
    audio_requested: bool,
    timeline: Timeline,
    pacer: FramePacer,
    media_end_hns: i64,
    latest_video_hns: i64,
    started: bool,
    first_frame_emitted: bool,
    min_raw_hns: i64,
    target_validator: target::TargetValidator,
    next_target_validation: Instant,
    device: Device,
    _media_foundation: MediaFoundation,
    _apartment: Apartment,
}

impl Worker {
    fn initialize(
        request: RecordingRequest,
        commands: Receiver<Command>,
        elapsed_hns: Arc<AtomicU64>,
        state: Arc<SharedSessionState>,
        events: Sender<WorkerEvent>,
        camera_feed: Option<CameraFeed>,
    ) -> Result<Self> {
        let output = output_paths(request.destination.as_deref())?;
        Self::initialize_at(
            request,
            commands,
            elapsed_hns,
            state,
            events,
            camera_feed,
            output,
        )
    }

    fn initialize_at(
        request: RecordingRequest,
        commands: Receiver<Command>,
        elapsed_hns: Arc<AtomicU64>,
        state: Arc<SharedSessionState>,
        events: Sender<WorkerEvent>,
        camera_feed: Option<CameraFeed>,
        mut output: OutputPaths,
    ) -> Result<Self> {
        resolve_video_codec(request.video_codec)?;
        let apartment = Apartment::enter()?;
        let supported =
            GraphicsCaptureSession::IsSupported().map_err(|error| Error::Unsupported {
                what: "Windows screen recording".into(),
                why: format!("Windows.Graphics.Capture support could not be queried: {error}"),
            })?;
        if !supported {
            return Err(Error::Unsupported {
                what: "Windows screen recording".into(),
                why: "Windows.Graphics.Capture is unavailable; Windows 10 version 1903 or newer is required"
                    .into(),
            });
        }

        let media_foundation = MediaFoundation::start()?;
        let startup_qpc = qpc_now_hns()
            .map_err(|error| Error::Platform(format!("could not timestamp startup: {error}")))?;
        let source = target::resolve(&request.target)?;
        let target_validator = source.validator.clone();
        let encoder_plan = plan::build(
            source.crop.width,
            source.crop.height,
            source.backing_scale,
            request.fps,
            request.quality,
            request.resolution,
        )
        .map_err(plan_error)?;
        if let Some(camera) = &camera_feed {
            camera.set_output_size(encoder_plan.output_width, encoder_plan.output_height)?;
        }
        let device = Device::new()?;
        let wants_audio = request.system_audio || request.microphone;
        let encoder = Encoder::new(&output.temporary_path, &device, encoder_plan, wants_audio)?;
        output.mark_owned();
        if let Err(error) = output.pin_identity() {
            encoder.discard();
            return Err(output.discard(error));
        }
        let audio = match wants_audio
            .then(|| AudioCapture::start(request.system_audio, request.microphone))
            .transpose()
        {
            Ok(audio) => audio,
            Err(error) => {
                encoder.discard();
                return Err(output.discard(error));
            }
        };

        let paused = Arc::new(AtomicBool::new(false));
        let camera = match (request.camera.clone(), camera_feed.clone()) {
            (Some(camera_request), Some(feed)) => {
                match CameraCapture::start_with_pause(
                    camera_request,
                    feed.clone(),
                    Arc::clone(&paused),
                ) {
                    Ok(camera) => {
                        feed.activate();
                        Some(camera)
                    }
                    Err(error) => {
                        drop(audio);
                        encoder.discard();
                        return Err(output.discard(error));
                    }
                }
            }
            _ => None,
        };
        let (frames_tx, frames) = mpsc::sync_channel(FRAME_QUEUE_CAPACITY);
        let (signals_tx, signals) = mpsc::channel();
        let capture = match Capture::start(
            &device,
            source,
            encoder_plan,
            request.show_cursor,
            Arc::clone(&paused),
            frames_tx,
            signals_tx,
        ) {
            Ok(capture) => capture,
            Err(error) => {
                drop(audio);
                encoder.discard();
                return Err(output.discard(error));
            }
        };
        Ok(Self {
            commands,
            events,
            elapsed_hns,
            state,
            paused,
            capture: Some(capture),
            frames,
            signals,
            audio,
            camera,
            camera_feed,
            camera_compositor: CameraCompositor::default(),
            mixer: wants_audio.then(|| {
                Mixer::new(
                    AUDIO_SAMPLE_RATE,
                    AUDIO_CHUNK_FRAMES,
                    request.system_audio,
                    request.microphone,
                )
            }),
            encoder: Some(encoder),
            output,
            target: request.target,
            quality: request.quality,
            resolution: request.resolution,
            encoder_plan,
            audio_requested: wants_audio,
            timeline: Timeline::default(),
            pacer: FramePacer::new(encoder_plan.fps),
            media_end_hns: 0,
            latest_video_hns: 0,
            started: false,
            first_frame_emitted: false,
            min_raw_hns: startup_qpc.saturating_sub(AUDIO_SETTLE_HNS),
            target_validator,
            next_target_validation: Instant::now() + TARGET_REVALIDATION_INTERVAL,
            device,
            _media_foundation: media_foundation,
            _apartment: apartment,
        })
    }

    fn run(mut self) -> Result<NativeRecording> {
        let cause = loop {
            if let Some(cause) = self.process_commands() {
                break cause;
            }
            if let Some(cause) = self.capture_signal() {
                break cause;
            }
            if let Some(error) = self.audio.as_ref().and_then(AudioCapture::try_failure) {
                break EndCause::Failed(error);
            }
            if let Some(warning) = self.camera.as_ref().and_then(CameraCapture::try_warning) {
                let _ = self.events.send(WorkerEvent::Warning(warning));
            }
            if let Err(error) = self.validate_target() {
                break EndCause::Failed(error.to_string());
            }

            match self.frames.recv_timeout(IDLE_WAIT) {
                Ok(frame) => {
                    if let Err(error) = self.write_frame(frame) {
                        break EndCause::Failed(error.to_string());
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break EndCause::Failed("WGC frame queue disconnected".into());
                }
            }
            if let Err(error) = self.drain_audio(None) {
                break EndCause::Failed(error.to_string());
            }
        };
        self.finish(cause)
    }

    fn process_commands(&mut self) -> Option<EndCause> {
        loop {
            match self.commands.try_recv() {
                Ok(Command::Pause(ack)) => {
                    let result = self.pause();
                    let _ = ack.send(result);
                }
                Ok(Command::Resume(ack)) => {
                    let result = self.resume();
                    let _ = ack.send(result);
                }
                Ok(Command::Stop) | Err(TryRecvError::Disconnected) => {
                    return Some(EndCause::Requested);
                }
                Err(TryRecvError::Empty) => return None,
            }
        }
    }

    fn pause(&mut self) -> Result<()> {
        if self.timeline.is_paused() {
            return Err(Error::InvalidRequest("recording is already paused".into()));
        }
        let now = qpc_now_hns()
            .map_err(|error| Error::Platform(format!("could not timestamp pause: {error}")))?;
        self.paused.store(true, Ordering::Release);
        if let Some(camera) = &self.camera {
            let (pending, superseded) = camera.take_latest_frame();
            if let Some(feed) = &self.camera_feed {
                feed.note_drops(superseded + usize::from(pending.is_some()));
                feed.clear_frames();
            }
        }
        self.timeline.pause(now);
        self.state.set(SessionState::Paused);
        self.publish_elapsed();
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if !self.timeline.is_paused() {
            return Err(Error::InvalidRequest("recording is not paused".into()));
        }
        let now = qpc_now_hns()
            .map_err(|error| Error::Platform(format!("could not timestamp resume: {error}")))?;
        self.min_raw_hns = now;
        if let Some(camera) = &self.camera {
            let (pending, superseded) = camera.take_latest_frame();
            if let Some(feed) = &self.camera_feed {
                feed.note_drops(superseded + usize::from(pending.is_some()));
                feed.clear_frames();
            }
        }
        self.timeline.resume(now);
        self.paused.store(false, Ordering::Release);
        self.state.set(SessionState::Running);
        self.publish_elapsed();
        Ok(())
    }

    fn capture_signal(&self) -> Option<EndCause> {
        match self.signals.try_recv() {
            Ok(Signal::TargetClosed) => Some(EndCause::TargetClosed),
            Ok(Signal::Failed(error)) => Some(EndCause::Failed(error)),
            Err(TryRecvError::Disconnected) => {
                Some(EndCause::Failed("WGC signal channel disconnected".into()))
            }
            Err(TryRecvError::Empty) => None,
        }
    }

    fn write_frame(&mut self, mut frame: FramePacket) -> Result<()> {
        if frame.raw_hns < self.min_raw_hns || self.timeline.is_paused() {
            return Ok(());
        }
        self.validate_native_timestamp(frame.raw_hns, "WGC frame")?;
        if !self.started {
            self.timeline.start(frame.raw_hns);
            self.min_raw_hns = frame.raw_hns;
            self.started = true;
        }
        let Some(stream_hns) = self.timeline.map(frame.raw_hns) else {
            return Ok(());
        };
        self.latest_video_hns = self.latest_video_hns.max(stream_hns);
        let Some(schedule) = self.pacer.schedule(stream_hns) else {
            self.publish_elapsed();
            return Ok(());
        };
        self.drain_camera_frames()?;
        if let Some(camera) = &self.camera_feed {
            let settings = camera.settings();
            let camera_frame = camera.frame_for(hns_duration(stream_hns));
            if camera_frame.is_some() || settings.presenter {
                let mut screen = self.device.download_bgra(&frame.texture)?;
                let width = screen.width();
                let height = screen.height();
                let stride = screen.stride;
                self.camera_compositor.compose_optional(
                    &mut screen.data,
                    width,
                    height,
                    stride,
                    camera_frame.as_ref(),
                    settings,
                )?;
                frame.texture = self.device.upload_bgra(&screen)?;
            }
        }
        self.encoder
            .as_mut()
            .expect("encoder lives until finalisation")
            .write_video(frame, schedule.timestamp_hns, schedule.duration_hns)?;
        if !self.first_frame_emitted {
            self.first_frame_emitted = true;
            let _ = self.events.send(WorkerEvent::FirstFrame);
        }
        self.media_end_hns = self
            .media_end_hns
            .max(schedule.timestamp_hns.saturating_add(schedule.duration_hns));
        self.publish_elapsed();
        Ok(())
    }

    fn drain_camera_frames(&mut self) -> Result<()> {
        let (Some(camera), Some(feed)) = (&self.camera, &self.camera_feed) else {
            return Ok(());
        };
        let (packet, superseded) = camera.take_latest_frame();
        feed.note_drops(superseded);
        let Some(packet) = packet else {
            return Ok(());
        };
        if packet.raw_hns < self.min_raw_hns {
            feed.note_drop();
            return Ok(());
        }
        self.validate_native_timestamp(packet.raw_hns, "Media Foundation camera frame")?;
        let Some(stream_hns) = self.timeline.project(packet.raw_hns) else {
            return Ok(());
        };
        let frame = CameraFrame::new(packet.pixels, hns_duration(stream_hns), packet.orientation)?;
        let _ = feed.push(frame)?;
        Ok(())
    }

    fn drain_audio(&mut self, final_horizon: Option<i64>) -> Result<()> {
        if !self.started {
            return Ok(());
        }
        let include_partial = final_horizon.is_some();
        loop {
            let raw = match self.audio.as_ref().map(AudioCapture::try_packet) {
                Some(Ok(raw)) => raw,
                Some(Err(TryRecvError::Empty)) | None => break,
                Some(Err(TryRecvError::Disconnected)) if include_partial => break,
                Some(Err(TryRecvError::Disconnected)) => {
                    return Err(Error::Platform("WASAPI packet channel disconnected".into()));
                }
            };
            if raw.qpc_hns < self.min_raw_hns || self.timeline.is_paused() {
                continue;
            }
            self.validate_native_timestamp(raw.qpc_hns, "WASAPI packet")?;
            let Some(stream_hns) = self.timeline.map(raw.qpc_hns) else {
                continue;
            };
            if final_horizon
                .is_some_and(|horizon| stream_hns > horizon.saturating_add(AUDIO_SETTLE_HNS))
            {
                return Err(Error::Platform(format!(
                    "WASAPI packet timestamp {:.3}s exceeds the trusted stop horizon {:.3}s",
                    stream_hns as f64 / HNS_PER_SECOND as f64,
                    final_horizon.unwrap_or_default() as f64 / HNS_PER_SECOND as f64
                )));
            }
            let frames = raw.samples.len() / usize::from(raw.channels.max(1));
            let packet_duration = audio_frames_to_hns(frames as u64, raw.sample_rate);
            self.media_end_hns = self
                .media_end_hns
                .max(stream_hns.saturating_add(packet_duration));
            let source = raw.source;
            self.mixer
                .as_mut()
                .expect("audio capture has a mixer")
                .ingest(source, raw.at_stream_time(stream_hns));
        }

        if let Some(mixer) = self.mixer.as_mut() {
            let qpc_watermark_hns = if include_partial {
                final_horizon.unwrap_or(self.latest_video_hns)
            } else {
                let now = qpc_now_hns().map_err(|error| {
                    Error::Platform(format!("could not timestamp audio watermark: {error}"))
                })?;
                self.timeline.project(now).unwrap_or(self.latest_video_hns)
            };
            let through_hns = audio_drain_limit(
                self.media_end_hns,
                self.latest_video_hns,
                qpc_watermark_hns,
                AUDIO_SETTLE_HNS.max(HNS_PER_SECOND / i64::from(self.encoder_plan.fps.max(1))),
                include_partial,
            );
            let drain_span = through_hns.saturating_sub(mixer.cursor_hns());
            if drain_span > MAX_AUDIO_DRAIN_SPAN_HNS {
                return Err(Error::Platform(format!(
                    "refusing to synthesize {:.3}s of audio in one drain",
                    drain_span as f64 / HNS_PER_SECOND as f64
                )));
            }
            while let Some(chunk) = mixer.drain_next(through_hns, include_partial) {
                self.encoder
                    .as_mut()
                    .expect("encoder lives until finalisation")
                    .write_audio(chunk)?;
            }
        }
        self.publish_elapsed();
        Ok(())
    }

    fn finish(mut self, cause: EndCause) -> Result<NativeRecording> {
        let stop_qpc = qpc_now_hns();
        let trusted_stop_hns = stop_qpc
            .as_ref()
            .ok()
            .and_then(|raw| self.timeline.project_final(*raw))
            .unwrap_or(self.latest_video_hns)
            .max(self.latest_video_hns);
        if let Some(capture) = self.capture.take() {
            capture.close();
        }
        if let Some(mut camera) = self.camera.take() {
            camera.stop();
        }
        let camera_metadata = self.camera_feed.as_ref().map(|camera| {
            let metadata = Box::new(CameraRecordingMetadata::from_runtime(
                camera.settings(),
                &camera.status(),
            ));
            camera.stop();
            metadata
        });

        let mut runtime_error = match cause {
            EndCause::Requested | EndCause::TargetClosed => None,
            EndCause::Failed(error) => Some(error),
        };
        if let Err(error) = stop_qpc {
            append_error(
                &mut runtime_error,
                format!("could not timestamp recording stop: {error}"),
            );
        }
        if let Some(audio) = self.audio.as_mut()
            && let Err(error) = audio.shutdown()
        {
            append_error(&mut runtime_error, error.to_string());
        }
        if let Some(error) = self.audio.as_ref().and_then(AudioCapture::try_failure) {
            append_error(&mut runtime_error, error);
        }
        if let Err(error) = self.drain_audio(Some(trusted_stop_hns)) {
            append_error(&mut runtime_error, error.to_string());
        }
        self.audio.take();

        let encoder = self
            .encoder
            .take()
            .expect("encoder lives until finalisation");
        let samples_written = encoder.samples_written();
        let video_frames = encoder.video_frames();
        let audio_channels = encoder.audio_channels();
        let finalize_error = encoder.finalize().err().map(|error| error.to_string());
        drop(encoder);
        if let Some(error) = finalize_error {
            append_error(
                &mut runtime_error,
                format!("Media Foundation finalisation failed: {error}"),
            );
        }

        let file_bytes = std::fs::metadata(&self.output.temporary_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let mut inspection = None;
        if runtime_error.is_some() {
            match salvage::inspect_file(&self.output.temporary_path) {
                Ok(found) if found.playable() => {
                    if let Err(error) = salvage::repair_file(&self.output.temporary_path, found) {
                        append_error(
                            &mut runtime_error,
                            format!("could not trim incomplete MP4 data: {error}"),
                        );
                    } else {
                        inspection = Some(found);
                    }
                }
                Ok(_) => {}
                Err(error) => append_error(
                    &mut runtime_error,
                    format!("could not inspect fragmented MP4 output: {error}"),
                ),
            }
        }

        let retained_bytes = inspection.map_or(file_bytes, |value| value.truncate_to);
        let duration_secs = self.current_elapsed_hns() as f64 / HNS_PER_SECOND as f64;
        self.publish_elapsed();

        match salvage::classify(
            runtime_error.as_deref(),
            file_bytes,
            samples_written,
            inspection,
        ) {
            Outcome::Complete if samples_written != 0 && file_bytes != 0 => self.finish_recording(
                duration_secs,
                retained_bytes,
                video_frames,
                audio_channels,
                None,
                camera_metadata,
            ),
            Outcome::Complete => Err(self.output.discard(Error::Codec(
                "recording ended before any media reached the output file".into(),
            ))),
            Outcome::Salvaged(reason) => self.finish_recording(
                duration_secs,
                retained_bytes,
                video_frames,
                audio_channels,
                Some(reason),
                camera_metadata,
            ),
            Outcome::Unusable(reason) => Err(self.output.discard(Error::Codec(reason))),
        }
    }

    fn finish_recording(
        &mut self,
        duration_secs: f64,
        retained_bytes: u64,
        video_frames: u64,
        audio_channels: u16,
        mut partial_reason: Option<String>,
        camera: Option<Box<CameraRecordingMetadata>>,
    ) -> Result<NativeRecording> {
        let metadata = RecordingMetadata {
            size: Some(PhysicalSize::new(
                f64::from(self.encoder_plan.output_width),
                f64::from(self.encoder_plan.output_height),
            )),
            frames: Some(video_frames),
            audio_channels: Some(if self.audio_requested {
                audio_channels
            } else {
                0
            }),
            file_size_bytes: Some(retained_bytes),
            video_codec: Some(VideoCodec::H264),
            quality: Some(self.quality),
            resolution: Some(self.resolution),
            camera,
        };
        let path = match promote_output(&mut self.output) {
            Ok(()) => self.output.final_path.clone(),
            Err(error) => {
                append_error(
                    &mut partial_reason,
                    format!(
                        "could not atomically move the finished recording into place at {}: {error}; retained the owned temporary output at {}",
                        self.output.final_path.display(),
                        self.output.temporary_path.display()
                    ),
                );
                self.output.temporary_path.clone()
            }
        };
        let mut recording = Recording::native(path, duration_secs, super::ENGINE_NAME)?
            .with_native_details(self.target.clone(), metadata)?;
        if let Some(reason) = partial_reason {
            recording = recording.into_partial(reason)?;
        }
        let recording = NativeRecording::new(recording, super::ENGINE_NAME)?;
        self.output.mark_reported();
        Ok(recording)
    }

    fn current_elapsed_hns(&self) -> i64 {
        self.media_end_hns.max(self.timeline.duration_hns()).max(0)
    }

    fn publish_elapsed(&self) {
        self.elapsed_hns
            .store(self.current_elapsed_hns() as u64, Ordering::Release);
    }

    fn validate_native_timestamp(&self, raw_hns: i64, source: &str) -> Result<()> {
        let now = qpc_now_hns().map_err(|error| {
            Error::Platform(format!("could not validate {source} time: {error}"))
        })?;
        if native_timestamp_is_plausible(raw_hns, self.min_raw_hns, now) {
            Ok(())
        } else {
            Err(Error::Platform(format!(
                "{source} timestamp {raw_hns} is outside the trusted QPC window {}..={}",
                self.min_raw_hns,
                now.saturating_add(super::timing::MAX_NATIVE_FUTURE_HNS)
            )))
        }
    }

    fn validate_target(&mut self) -> Result<()> {
        let now = Instant::now();
        if now < self.next_target_validation {
            return Ok(());
        }
        self.next_target_validation = now + TARGET_REVALIDATION_INTERVAL;
        self.target_validator.validate()
    }
}

fn hns_duration(value: i64) -> Duration {
    Duration::from_nanos(value.max(0).cast_unsigned().saturating_mul(100))
}

enum EndCause {
    Requested,
    TargetClosed,
    Failed(String),
}

struct OutputPaths {
    final_path: PathBuf,
    temporary_path: PathBuf,
    working_directory: PathBuf,
    identity: Option<OwnedHandle>,
    identity_info: Option<FILE_ID_INFO>,
    owns_output: bool,
    promoted: bool,
    reported: bool,
}

impl OutputPaths {
    fn owned_path(&self) -> &Path {
        if self.promoted {
            &self.final_path
        } else {
            &self.temporary_path
        }
    }

    fn mark_reported(&mut self) {
        self.reported = true;
    }

    fn mark_owned(&mut self) {
        self.owns_output = true;
    }

    fn pin_identity(&mut self) -> Result<()> {
        let path: Vec<u16> = self
            .temporary_path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|error| {
            Error::Storage(format!(
                "could not pin recording staging identity {}: {error}",
                self.temporary_path.display()
            ))
        })?;
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        self.identity_info = Some(file_identity(&handle)?);
        self.identity = Some(handle);
        Ok(())
    }

    fn discard(&mut self, error: Error) -> Error {
        self.identity.take();
        let path = self.owned_path().to_owned();
        let cleanup = if self.owns_output {
            std::fs::remove_file(&path)
        } else {
            Ok(())
        };
        let directory_cleanup = std::fs::remove_dir(&self.working_directory);
        self.reported = true;
        if removal_succeeded(&cleanup) && removal_succeeded(&directory_cleanup) {
            error
        } else {
            Error::Platform(format!(
                "{error}; could not fully remove incomplete output {} ({}) or private working directory {} ({})",
                path.display(),
                io_result_detail(cleanup),
                self.working_directory.display(),
                io_result_detail(directory_cleanup)
            ))
        }
    }
}

impl Drop for OutputPaths {
    fn drop(&mut self) {
        if self.reported {
            return;
        }
        self.identity.take();
        let path = self.owned_path().to_owned();
        if self.owns_output {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::error!(
                        path = %path.display(),
                        %error,
                        "could not remove abandoned Windows recording output"
                    );
                }
            }
        }
        if let Err(error) = std::fs::remove_dir(&self.working_directory)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::error!(
                path = %self.working_directory.display(),
                %error,
                "could not remove abandoned private Windows recording directory"
            );
        }
    }
}

fn append_error(current: &mut Option<String>, next: String) {
    match current {
        Some(current) => {
            current.push_str("; ");
            current.push_str(&next);
        }
        None => *current = Some(next),
    }
}

fn output_paths(explicit: Option<&Path>) -> Result<OutputPaths> {
    let final_path = if let Some(path) = explicit {
        validate_output(path)?;
        path.to_owned()
    } else {
        reserve_default_output()?
    };
    let (working_directory, temporary_path) = reserve_temporary_output(&final_path)?;
    Ok(OutputPaths {
        final_path,
        temporary_path,
        working_directory,
        identity: None,
        identity_info: None,
        owns_output: false,
        promoted: false,
        reported: false,
    })
}

fn reserve_default_output() -> Result<PathBuf> {
    let directory = std::env::temp_dir();
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    for _ in 0..1_000 {
        let sequence = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "scrozz-recording-{epoch}-{}-{sequence}.mp4",
            std::process::id()
        ));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a unique recording path",
    )))
}

fn reserve_temporary_output(final_path: &Path) -> Result<(PathBuf, PathBuf)> {
    let parent = final_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    for _ in 0..1_000 {
        let sequence = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut entropy = RandomState::new().build_hasher();
        entropy.write_u64(sequence);
        entropy.write_u64(u64::from(std::process::id()));
        entropy.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        let directory = parent.join(format!(
            ".scrozz-recording-{}-{:016x}",
            std::process::id(),
            entropy.finish()
        ));
        if directory.exists() {
            continue;
        }
        create_private_directory(&directory)?;
        let candidate = directory.join("recording.mp4");
        if candidate != final_path {
            return Ok((directory, candidate));
        }
    }
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a temporary recording path",
    )))
}

fn validate_output(path: &Path) -> Result<()> {
    if path.exists() {
        return Err(Error::InvalidRequest(format!(
            "recording output already exists: {}",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(Error::InvalidRequest(format!(
            "recording output directory does not exist: {}",
            parent.display()
        )));
    }
    Ok(())
}

fn promote_output(paths: &mut OutputPaths) -> Result<()> {
    if !paths.owns_output {
        return Err(Error::Storage(
            "cannot promote a recording output that this worker does not own".into(),
        ));
    }
    paths.identity.take();
    let temporary: Vec<u16> = paths
        .temporary_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let final_path: Vec<u16> = paths
        .final_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(temporary.as_ptr()),
            (FILE_READ_ATTRIBUTES | DELETE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|error| {
        Error::Storage(format!(
            "could not reopen recording staging output for identity-bound promotion: {error}"
        ))
    })?;
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let actual = file_identity(&handle)?;
    if paths.identity_info != Some(actual) {
        return Err(Error::Storage(
            "recording staging identity changed before promotion; refusing substituted output"
                .into(),
        ));
    }
    rename_handle_no_replace(&handle, &final_path).map_err(|error| {
        Error::Storage(format!(
            "could not atomically move recording without replacement from {} to {}: {error}",
            paths.temporary_path.display(),
            paths.final_path.display()
        ))
    })?;
    paths.promoted = true;
    if let Err(error) = std::fs::remove_dir(&paths.working_directory) {
        tracing::error!(
            path = %paths.working_directory.display(),
            %error,
            "recording was promoted but its empty private working directory remains"
        );
    }
    Ok(())
}

fn file_identity(handle: &OwnedHandle) -> Result<FILE_ID_INFO> {
    let mut identity = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            windows::Win32::Foundation::HANDLE(handle.as_raw_handle()),
            FileIdInfo,
            (&raw mut identity).cast(),
            u32::try_from(std::mem::size_of::<FILE_ID_INFO>()).expect("FILE_ID_INFO fits u32"),
        )
    }
    .map_err(|error| Error::Storage(format!("could not read recording file identity: {error}")))?;
    Ok(identity)
}

fn rename_handle_no_replace(handle: &OwnedHandle, destination: &[u16]) -> Result<()> {
    let name = destination.strip_suffix(&[0]).unwrap_or(destination);
    let name_bytes = name
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| Error::Storage("recording destination name is too long".into()))?;
    let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let allocation = offset
        .checked_add(name_bytes)
        .ok_or_else(|| Error::Storage("recording rename buffer is too large".into()))?;
    let mut storage = vec![0_usize; allocation.div_ceil(std::mem::size_of::<usize>())];
    let rename = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*rename).Anonymous.ReplaceIfExists = false;
        (*rename).RootDirectory = windows::Win32::Foundation::HANDLE::default();
        (*rename).FileNameLength = u32::try_from(name_bytes)
            .map_err(|_| Error::Storage("recording destination name is too long".into()))?;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*rename).FileName.as_mut_ptr(), name.len());
        SetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(handle.as_raw_handle()),
            FileRenameInfo,
            rename.cast(),
            u32::try_from(allocation)
                .map_err(|_| Error::Storage("recording rename buffer is too large".into()))?,
        )
        .map_err(|error| Error::Storage(format!("handle-based rename failed: {error}")))
    }
}

fn create_private_directory(path: &Path) -> Result<()> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            windows::core::w!("D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)"),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    }
    .map_err(|error| {
        Error::Storage(format!(
            "could not construct private recording permissions: {error}"
        ))
    })?;
    let descriptor_guard = SecurityDescriptorGuard(descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())
            .expect("SECURITY_ATTRIBUTES fits u32"),
        lpSecurityDescriptor: descriptor_guard.0.0,
        bInheritHandle: BOOL(0),
    };
    unsafe { CreateDirectoryW(PCWSTR(wide.as_ptr()), Some(&raw const attributes)) }.map_err(
        |error| {
            Error::Storage(format!(
                "could not create private recording directory {}: {error}",
                path.display()
            ))
        },
    )
}

struct SecurityDescriptorGuard(PSECURITY_DESCRIPTOR);

impl Drop for SecurityDescriptorGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

fn io_result_detail(result: std::io::Result<()>) -> String {
    match result {
        Ok(()) => "removed".to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "already absent".to_owned(),
        Err(error) => error.to_string(),
    }
}

fn removal_succeeded(result: &std::io::Result<()>) -> bool {
    matches!(result, Ok(()))
        || matches!(result, Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(Error::Io)
}

fn resolve_video_codec(codec: VideoCodec) -> Result<VideoCodec> {
    match codec {
        VideoCodec::Auto | VideoCodec::H264 => Ok(VideoCodec::H264),
        VideoCodec::Hevc => Err(Error::Unsupported {
            what: "HEVC encoding".into(),
            why: "the Windows recording engine currently supports H.264 only".into(),
        }),
        VideoCodec::Av1 => Err(Error::Unsupported {
            what: "AV1 encoding".into(),
            why: "the Windows recording engine currently supports H.264 only".into(),
        }),
    }
}

fn plan_error(error: plan::PlanError) -> Error {
    match error {
        plan::PlanError::EmptySource => {
            Error::InvalidRequest("the recording area has no pixels".into())
        }
        plan::PlanError::InvalidFrameRate(fps) => Error::InvalidRequest(format!(
            "recording frame rate {fps} must be between 1 and 240"
        )),
        plan::PlanError::InvalidScale(scale) => Error::Platform(format!(
            "capture target reported an invalid DPI scale factor {scale}"
        )),
        plan::PlanError::ResolutionTooSmall { width, height } => Error::InvalidRequest(format!(
            "recording resolution resolved to {width}x{height}; the Windows encoder requires at least 2 by 2 pixels"
        )),
        plan::PlanError::UnsupportedBitrate(bitrate) => Error::Unsupported {
            what: "recording quality".into(),
            why: format!(
                "the shared target bitrate {bitrate} bps does not fit the Windows encoder API limits"
            ),
        },
    }
}
