//! Recording worker, lifecycle commands, and orderly finalisation.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use scrozz_core::{Error, Result};
use windows::Graphics::Capture::GraphicsCaptureSession;

use crate::{Recording, RecordingRequest, RecordingSession};

use super::{
    audio::{AudioCapture, qpc_now_hns},
    com::{Apartment, MediaFoundation},
    device::Device,
    encoder::Encoder,
    mix::Mixer,
    plan,
    salvage::{self, Outcome},
    target,
    timing::{FramePacer, HNS_PER_SECOND, Timeline, audio_drain_limit, audio_frames_to_hns},
    video::{Capture, FramePacket, Signal},
};

const FRAME_QUEUE_CAPACITY: usize = 3;
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_CHUNK_FRAMES: u32 = 480;
const AUDIO_SETTLE_HNS: i64 = HNS_PER_SECOND / 10;
const IDLE_WAIT: Duration = Duration::from_millis(5);
static OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Starts a worker and waits until every native subsystem is live.
pub fn start(request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
    let request = request.clone();
    let (commands_tx, commands_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (completion_tx, completion_rx) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("scrozz-recording".into())
        .spawn(move || match Worker::initialize(request, commands_rx) {
            Ok(worker) => {
                if ready_tx.send(Ok(())).is_ok() {
                    let _ = completion_tx.send(worker.run());
                }
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error));
            }
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Box::new(WindowsSession {
            commands: commands_tx,
            completion: completion_rx,
            thread: Some(thread),
            state: SessionState::Running,
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
    completion: Receiver<Result<Recording>>,
    thread: Option<JoinHandle<()>>,
    state: SessionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Running,
    Paused,
    Ended,
}

impl RecordingSession for WindowsSession {
    fn pause(&mut self) -> Result<()> {
        if self.state != SessionState::Running {
            return Err(Error::InvalidRequest(
                "recording is already paused or has ended".into(),
            ));
        }
        self.command_with_ack(Command::Pause)?;
        self.state = SessionState::Paused;
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if self.state != SessionState::Paused {
            return Err(Error::InvalidRequest(
                "recording is not paused or has ended".into(),
            ));
        }
        self.command_with_ack(Command::Resume)?;
        self.state = SessionState::Running;
        Ok(())
    }

    fn stop(mut self: Box<Self>) -> Result<Recording> {
        self.state = SessionState::Ended;
        let _ = self.commands.send(Command::Stop);
        let result = self
            .completion
            .recv()
            .map_err(|_| Error::Platform("recording worker ended without a result".into()))?;
        self.join()?;
        result
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
}

impl Drop for WindowsSession {
    fn drop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        let _ = self.commands.send(Command::Stop);
        let _ = self.completion.recv();
        let _ = self.join();
    }
}

enum Command {
    Pause(SyncSender<Result<()>>),
    Resume(SyncSender<Result<()>>),
    Stop,
}

struct Worker {
    commands: Receiver<Command>,
    paused: Arc<AtomicBool>,
    capture: Option<Capture>,
    frames: Receiver<FramePacket>,
    signals: Receiver<Signal>,
    audio: Option<AudioCapture>,
    mixer: Option<Mixer>,
    encoder: Option<Encoder>,
    output: PathBuf,
    timeline: Timeline,
    pacer: FramePacer,
    frame_duration_hns: i64,
    media_end_hns: i64,
    latest_video_hns: i64,
    started: bool,
    min_raw_hns: i64,
    _device: Device,
    _media_foundation: MediaFoundation,
    _apartment: Apartment,
}

impl Worker {
    fn initialize(request: RecordingRequest, commands: Receiver<Command>) -> Result<Self> {
        let output = output_path(request.output.as_deref())?;
        Self::initialize_at(request, commands, output)
    }

    fn initialize_at(
        request: RecordingRequest,
        commands: Receiver<Command>,
        output: PathBuf,
    ) -> Result<Self> {
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
        let source = target::resolve(&request.target)?;
        let encoder_plan = plan::build(
            source.crop.width,
            source.crop.height,
            request.fps,
            request.quality,
            request.max_height,
        )
        .map_err(|error| Error::InvalidRequest(format!("invalid recording plan: {error:?}")))?;
        let device = Device::new()?;
        let wants_audio = request.system_audio || request.microphone;
        let encoder = Encoder::new(&output, &device, encoder_plan, wants_audio)?;
        let audio = match wants_audio
            .then(|| AudioCapture::start(request.system_audio, request.microphone))
            .transpose()
        {
            Ok(audio) => audio,
            Err(error) => {
                encoder.discard();
                return Err(discard_output(&output, error));
            }
        };

        let paused = Arc::new(AtomicBool::new(false));
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
                return Err(discard_output(&output, error));
            }
        };

        Ok(Self {
            commands,
            paused,
            capture: Some(capture),
            frames,
            signals,
            audio,
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
            timeline: Timeline::default(),
            pacer: FramePacer::new(encoder_plan.fps),
            frame_duration_hns: HNS_PER_SECOND / i64::from(encoder_plan.fps),
            media_end_hns: 0,
            latest_video_hns: 0,
            started: false,
            min_raw_hns: i64::MIN,
            _device: device,
            _media_foundation: media_foundation,
            _apartment: apartment,
        })
    }

    fn run(mut self) -> Result<Recording> {
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
            if let Err(error) = self.drain_audio(false) {
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
        self.timeline.pause(now);
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if !self.timeline.is_paused() {
            return Err(Error::InvalidRequest("recording is not paused".into()));
        }
        let now = qpc_now_hns()
            .map_err(|error| Error::Platform(format!("could not timestamp resume: {error}")))?;
        self.min_raw_hns = now;
        self.timeline.resume(now);
        self.paused.store(false, Ordering::Release);
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

    fn write_frame(&mut self, frame: FramePacket) -> Result<()> {
        if frame.raw_hns < self.min_raw_hns || self.timeline.is_paused() {
            return Ok(());
        }
        if !self.started {
            self.timeline.start(frame.raw_hns);
            self.min_raw_hns = frame.raw_hns;
            self.started = true;
        }
        let Some(stream_hns) = self.timeline.map(frame.raw_hns) else {
            return Ok(());
        };
        self.latest_video_hns = self.latest_video_hns.max(stream_hns);
        if !self.pacer.accept(stream_hns) {
            return Ok(());
        }
        self.encoder
            .as_mut()
            .expect("encoder lives until finalisation")
            .write_video(frame, stream_hns)?;
        self.media_end_hns = self
            .media_end_hns
            .max(stream_hns.saturating_add(self.frame_duration_hns));
        Ok(())
    }

    fn drain_audio(&mut self, include_partial: bool) -> Result<()> {
        if !self.started {
            return Ok(());
        }
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
            let Some(stream_hns) = self.timeline.map(raw.qpc_hns) else {
                continue;
            };
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
            let through_hns = audio_drain_limit(
                self.media_end_hns,
                self.latest_video_hns,
                AUDIO_SETTLE_HNS,
                include_partial,
            );
            for chunk in mixer.drain_through(through_hns, include_partial) {
                self.encoder
                    .as_mut()
                    .expect("encoder lives until finalisation")
                    .write_audio(chunk)?;
            }
        }
        Ok(())
    }

    fn finish(mut self, cause: EndCause) -> Result<Recording> {
        if let Some(capture) = self.capture.take() {
            capture.close();
        }

        let mut runtime_error = match cause {
            EndCause::Requested | EndCause::TargetClosed => None,
            EndCause::Failed(error) => Some(error),
        };
        if let Some(audio) = self.audio.as_mut()
            && let Err(error) = audio.shutdown()
        {
            append_error(&mut runtime_error, error.to_string());
        }
        if let Some(error) = self.audio.as_ref().and_then(AudioCapture::try_failure) {
            append_error(&mut runtime_error, error);
        }
        if let Err(error) = self.drain_audio(true) {
            append_error(&mut runtime_error, error.to_string());
        }
        self.audio.take();

        let encoder = self
            .encoder
            .take()
            .expect("encoder lives until finalisation");
        let samples_written = encoder.samples_written();
        let finalize_error = encoder.finalize().err().map(|error| error.to_string());
        drop(encoder);
        if let Some(error) = finalize_error {
            append_error(
                &mut runtime_error,
                format!("Media Foundation finalisation failed: {error}"),
            );
        }

        let file_bytes = std::fs::metadata(&self.output)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let mut inspection = None;
        if runtime_error.is_some() {
            match salvage::inspect_file(&self.output) {
                Ok(found) if found.playable() => {
                    if let Err(error) = salvage::repair_file(&self.output, found) {
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
        let duration_secs = self.media_end_hns.max(0) as f64 / HNS_PER_SECOND as f64;
        match salvage::classify(
            runtime_error.as_deref(),
            file_bytes,
            samples_written,
            inspection,
        ) {
            Outcome::Complete if samples_written != 0 && file_bytes != 0 => {
                Ok(Recording::complete(self.output, duration_secs))
            }
            Outcome::Complete => Err(discard_output(
                &self.output,
                Error::Codec("recording ended before any media reached the output file".into()),
            )),
            Outcome::Salvaged(reason) => {
                Ok(Recording::salvaged(self.output, duration_secs, reason))
            }
            Outcome::Unusable(reason) => Err(discard_output(&self.output, Error::Codec(reason))),
        }
    }
}

enum EndCause {
    Requested,
    TargetClosed,
    Failed(String),
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

fn discard_output(path: &Path, error: Error) -> Error {
    match std::fs::remove_file(path) {
        Ok(()) => error,
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => error,
        Err(cleanup) => Error::Platform(format!(
            "{error}; could not remove incomplete output {}: {cleanup}",
            path.display()
        )),
    }
}

fn output_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        validate_output(path)?;
        return Ok(path.to_owned());
    }

    let directory = std::env::current_dir()?;
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
