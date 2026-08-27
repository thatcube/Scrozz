//! Linux recording orchestration.

#[cfg(feature = "linux-native")]
mod source;

#[cfg(not(feature = "linux-native"))]
use crate::{RecordingRequest, RecordingSession};
#[cfg(not(feature = "linux-native"))]
use scrozz_core::{Error, Result};

#[cfg(not(feature = "linux-native"))]
pub(super) fn start(_request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
    Err(Error::Unsupported {
        what: "Linux screen recording".into(),
        why: "this binary was built without the non-default `scrozz-record/linux-native` feature; \
              enable it on Linux after installing PipeWire, FFmpeg and VA-API development libraries"
            .into(),
    })
}

#[cfg(feature = "linux-native")]
mod native {
    use std::collections::VecDeque;
    use std::fs::{File, OpenOptions};
    use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use scrozz_core::{Error, Result};

    use super::source::{PipeWireAudio, VideoSource, open_video_source};
    use crate::audio::AudioMixer;
    use crate::config::RecordingConfig;
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
        Recording, RecordingCompletion, RecordingRequest, RecordingSession, Salvageability,
    };

    enum Control {
        Pause(SyncSender<Result<()>>),
        Resume(SyncSender<Result<()>>),
        Stop,
    }

    pub(super) struct LinuxRecordingSession {
        control: Sender<Control>,
        result: Receiver<Result<Recording>>,
        worker: Option<JoinHandle<()>>,
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
    }

    impl RecordingSession for LinuxRecordingSession {
        fn pause(&mut self) -> Result<()> {
            let (reply, response) = mpsc::sync_channel(0);
            self.command(Control::Pause(reply))?;
            response.recv().map_err(|_| {
                Error::Platform("the recording worker stopped before pausing".into())
            })?
        }

        fn resume(&mut self) -> Result<()> {
            let (reply, response) = mpsc::sync_channel(0);
            self.command(Control::Resume(reply))?;
            response.recv().map_err(|_| {
                Error::Platform("the recording worker stopped before resuming".into())
            })?
        }

        fn stop(mut self: Box<Self>) -> Result<Recording> {
            let _ = self.control.send(Control::Stop);
            let recording = self.result.recv().map_err(|_| {
                Error::Platform("the recording worker exited without a result".into())
            })?;
            self.join_worker()?;
            recording
        }
    }

    impl Drop for LinuxRecordingSession {
        fn drop(&mut self) {
            if self.worker.is_some() {
                let _ = self.control.send(Control::Stop);
                if let Err(error) = self.join_worker() {
                    tracing::error!(%error, "Linux recording worker did not stop cleanly");
                }
            }
        }
    }

    pub(super) fn start(request: &RecordingRequest) -> Result<Box<dyn RecordingSession>> {
        let config = RecordingConfig::try_from(request)?;
        let (control_tx, control_rx) = mpsc::channel();
        let (initialised_tx, initialised_rx) = mpsc::sync_channel(0);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("scrozz-linux-recording".into())
            .spawn(move || worker_entry(config, control_rx, initialised_tx, result_tx))
            .map_err(Error::Io)?;

        match initialised_rx.recv() {
            Ok(Ok(())) => Ok(Box::new(LinuxRecordingSession {
                control: control_tx,
                result: result_rx,
                worker: Some(worker),
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
        result: SyncSender<Result<Recording>>,
    ) {
        match Worker::new(config) {
            Ok(mut worker) => {
                if initialised.send(Ok(())).is_ok() {
                    let _ = result.send(worker.run(&controls));
                }
            }
            Err((error, destination)) => {
                if let Some(destination) = destination
                    && let Err(remove_error) = std::fs::remove_file(&destination)
                {
                    tracing::warn!(
                        %remove_error,
                        path = %destination.display(),
                        "could not remove an uninitialised recording file"
                    );
                }
                let _ = initialised.send(Err(error));
            }
        }
    }

    struct Worker {
        config: RecordingConfig,
        source: Box<dyn VideoSource>,
        first_frame: Option<PackedFrame>,
        dimensions: crate::config::Dimensions,
        video_encoder: Box<dyn VideoEncoder>,
        audio_source: Option<PipeWireAudio>,
        audio_encoder: Option<AacEncoder>,
        audio_mixer: AudioMixer,
        muxer: Option<FragmentedMp4<File>>,
        state: RecordingStateMachine,
        pending_video: Option<EncodedVideoPacket>,
        pending_audio: VecDeque<EncodedAudioPacket>,
        fragments_written: u64,
    }

    impl Worker {
        fn new(
            config: RecordingConfig,
        ) -> std::result::Result<Self, (Error, Option<std::path::PathBuf>)> {
            let destination = config.destination.clone();
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|error| (Error::Io(error), None))?;

            let setup = Self::new_with_file(config, file);
            setup.map_err(|error| (error, Some(destination)))
        }

        fn new_with_file(config: RecordingConfig, file: File) -> Result<Self> {
            let mut source = open_video_source(&config.target, config.show_cursor)?;
            let first_frame = source.next_frame(Duration::from_secs(15))?.ok_or_else(|| {
                Error::Platform("the capture source delivered no frame within 15 seconds".into())
            })?;
            first_frame.validate()?;
            let dimensions = config
                .resolution
                .resolve(first_frame.width, first_frame.height)?;
            let video_encoder = encoder::open(
                config.codec,
                VideoEncoderSettings {
                    dimensions,
                    fps: config.fps.get(),
                    quality: config.quality,
                },
            )?;
            let mut audio_encoder = if config.microphone || config.system_audio {
                Some(AacEncoder::new(config.quality)?)
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
            let muxer =
                FragmentedMp4::new(file, &video_track, audio_track.as_ref()).map_err(Error::Io)?;
            muxer.writer().sync_data().map_err(Error::Io)?;
            let mut state = RecordingStateMachine::new();
            state.apply(RecorderCommand::Started)?;
            tracing::info!(
                backend = source.name(),
                width = dimensions.width,
                height = dimensions.height,
                fps = config.fps.get(),
                audio = audio_encoder.is_some(),
                path = %config.destination.display(),
                "Linux recording started"
            );

            Ok(Self {
                config,
                source,
                first_frame: Some(first_frame),
                dimensions,
                video_encoder,
                audio_source,
                audio_encoder: audio_encoder.take(),
                audio_mixer: AudioMixer::new(AacEncoder::SAMPLE_RATE),
                muxer: Some(muxer),
                state,
                pending_video: None,
                pending_audio: VecDeque::new(),
                fragments_written: 0,
            })
        }

        fn run(&mut self, controls: &Receiver<Control>) -> Result<Recording> {
            let started = Instant::now();
            let mut timeline = RecordingTimeline::new(Duration::ZERO);
            let mut pacer = FramePacer::new(self.config.fps.get());
            let interval =
                Duration::from_nanos(1_000_000_000_u64 / u64::from(self.config.fps.get()));
            let mut next_capture = started;
            let mut last_frame: Option<Nv12Frame> = None;
            let mut failure: Option<String> = None;
            let mut user_stopped = false;

            loop {
                if self.state.state() == RecorderState::Paused {
                    match controls.recv_timeout(Duration::from_millis(50)) {
                        Ok(control) => {
                            if self.handle_control(control, started, &mut timeline) {
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
                            user_stopped = true;
                            let _ = self.state.apply(RecorderCommand::Stop);
                            break;
                        }
                    }
                    continue;
                }

                loop {
                    match controls.try_recv() {
                        Ok(control) => {
                            if self.handle_control(control, started, &mut timeline) {
                                user_stopped = true;
                                break;
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            user_stopped = true;
                            let _ = self.state.apply(RecorderCommand::Stop);
                            break;
                        }
                    }
                }
                if user_stopped {
                    break;
                }
                if self.state.state() == RecorderState::Paused {
                    continue;
                }

                let now = Instant::now();
                if now < next_capture {
                    match controls.recv_timeout(next_capture - now) {
                        Ok(control) => {
                            if self.handle_control(control, started, &mut timeline) {
                                user_stopped = true;
                                break;
                            }
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            user_stopped = true;
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
            self.finalise(duration, user_stopped, failure)
        }

        fn handle_control(
            &mut self,
            control: Control,
            started: Instant,
            timeline: &mut RecordingTimeline,
        ) -> bool {
            let now = started.elapsed();
            match control {
                Control::Pause(reply) => {
                    let result = self
                        .state
                        .apply(RecorderCommand::Pause)
                        .and_then(|_| timeline.pause(now))
                        .map(|_| ());
                    let _ = reply.send(result);
                    false
                }
                Control::Resume(reply) => {
                    let result = self
                        .state
                        .apply(RecorderCommand::Resume)
                        .and_then(|_| timeline.resume(now))
                        .map(|_| ());
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

            let completion = match failure {
                None if user_stopped => {
                    let _ = self.state.apply(RecorderCommand::Finish);
                    RecordingCompletion::Complete
                }
                None => RecordingCompletion::Partial {
                    salvageability,
                    reason: "the capture source ended without a stop request".into(),
                },
                Some(reason) => RecordingCompletion::Partial {
                    salvageability,
                    reason,
                },
            };
            Ok(Recording {
                path: self.config.destination.clone(),
                duration_secs,
                completion,
            })
        }
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
}

#[cfg(feature = "linux-native")]
pub(super) fn start(
    request: &crate::RecordingRequest,
) -> scrozz_core::Result<Box<dyn crate::RecordingSession>> {
    native::start(request)
}
