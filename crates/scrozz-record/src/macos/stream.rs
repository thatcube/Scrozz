//! ScreenCaptureKit stream lifecycle and callbacks.

#![allow(non_snake_case)]

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_av_foundation::{
    AVCaptureAudioDataOutputSampleBufferDelegate, AVCaptureConnection, AVCaptureOutput,
};
use objc2_core_foundation::{CFRetained, Type};
use objc2_core_graphics::kCGColorSpaceSRGB;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSDictionary, NSError, NSNumber, NSObject, NSString};
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCFrameStatus, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamErrorCode, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};
use scrozz_core::{CaptureTarget, Error, PhysicalSize, Result};

use crate::{
    OverlaySource, Quality, Recording, RecordingMetadata, RecordingRequest, RecordingResolution,
    RecordingSession, RecordingSettings, RecordingState, VideoCodec,
    interaction::{InteractionMapper, InteractionRecording, PrivateRecordingSource},
    interaction_render::InteractionOverlaySource,
};

use super::audio::MicrophoneCapture;
use super::compositor::Compositor;
use super::content::{CaptureContent, CaptureSource};
use super::plan::{CapturePixelFormat, RecordingPlan};
use super::timeline::SessionTimeline;
use super::writer::{AudioTrack, Writer};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const PIXEL_FORMAT_420_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"420v");
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

pub(crate) fn start(
    request: &RecordingRequest,
    settings: Option<&RecordingSettings>,
    mut overlays: Option<Box<dyn OverlaySource>>,
) -> Result<Box<dyn RecordingSession>> {
    if request.system_audio && !system_audio_available() {
        return Err(Error::Unsupported {
            what: "macOS system-audio recording".to_owned(),
            why: "ScreenCaptureKit system audio requires macOS 13 or newer; video-only recording remains available on macOS 12.3"
                .to_owned(),
        });
    }
    let content = super::content::resolve(&request.target)?;
    if request.microphone {
        super::permission::ensure_microphone()?;
    }

    if overlays.is_some() && settings.is_some() {
        return Err(Error::InvalidRequest(
            "custom and settings-driven recording overlays cannot be combined".to_owned(),
        ));
    }
    if let Some(settings) = settings
        && (settings.cursor_smoothing || settings.needs_input_monitoring())
    {
        let monitor = crate::platform_input::start(settings)?;
        let mapper = InteractionMapper::new(content.interaction_canvas())?;
        overlays = Some(Box::new(InteractionOverlaySource::new(
            monitor,
            mapper,
            InteractionRecording::new(
                settings.clicks,
                settings.keystrokes,
                settings.shows_cursor(),
                settings.cursor_smoothing,
            ),
            settings.after_capture.open_editor,
        )));
    }
    let has_overlays = overlays.is_some();
    let retains_interactions = overlays
        .as_ref()
        .is_some_and(|source| source.retains_interactions());
    let composites_cursor = overlays
        .as_ref()
        .is_some_and(|source| source.composites_cursor());
    let plan = RecordingPlan::new(
        request,
        content.native_width,
        content.native_height,
        content.scale,
        has_overlays || content.requires_composition(),
    );
    if plan.size.width < 2.0 || plan.size.height < 2.0 {
        return Err(Error::InvalidRequest(
            "recording resolution must be at least 2 by 2 pixels".to_owned(),
        ));
    }

    let writer = Writer::new(
        request.destination.as_deref(),
        &plan,
        request.fps,
        request.system_audio,
        request.microphone,
    )?;
    let raw_source = retains_interactions
        .then(PrivateRecordingSource::create)
        .transpose()?;
    let raw_writer = raw_source
        .as_ref()
        .map(|source| {
            Writer::new(
                Some(source.path()),
                &plan,
                request.fps,
                request.system_audio,
                request.microphone,
            )
        })
        .transpose()?;
    let output_width = plan.size.width.round() as u32;
    let output_height = plan.size.height.round() as u32;
    let compositor = if content.requires_composition() || has_overlays {
        Some(Compositor::new(
            output_width,
            output_height,
            (0..content.sources.len())
                .map(|index| content.output_rect(index, output_width, output_height))
                .collect(),
            request.fps,
        )?)
    } else {
        None
    };
    let queue = DispatchQueue::new("com.thatcube.scrozz.recording", None);
    let shared = Arc::new(Shared {
        writer: Mutex::new(writer),
        raw_writer: Mutex::new(raw_writer),
        raw_source: Mutex::new(raw_source),
        compositor: Mutex::new(compositor),
        direct_frame: Mutex::new(None),
        clock: Mutex::new(SessionTimeline::new(Duration::ZERO)),
        overlays: Mutex::new(overlays),
        failure: Mutex::new(None),
        warnings: Mutex::new(VecDeque::new()),
        raw_complete: AtomicBool::new(true),
        accepting: AtomicBool::new(false),
        stop_requested: AtomicBool::new(false),
        first_frame: AtomicBool::new(false),
        epoch: std::time::Instant::now(),
        size: plan.size,
    });
    let window_id = match &request.target {
        CaptureTarget::Window(id) => id.0.parse::<u32>().ok(),
        _ => None,
    };
    let mut streams = Vec::with_capacity(content.sources.len());
    let mut delegates = Vec::with_capacity(content.sources.len());
    let mut native_microphone = false;
    for (source_index, source) in content.sources.iter().enumerate() {
        let configuration = configure(
            &content,
            source,
            source_index,
            &plan,
            request,
            composites_cursor,
        );
        let source_microphone = source_index == 0
            && request.microphone
            && configuration.respondsToSelector(sel!(setCaptureMicrophone:));
        if source_microphone {
            // SAFETY: selector availability was checked on this exact object.
            unsafe {
                configuration.setCaptureMicrophone(true);
            }
        }
        native_microphone |= source_microphone;

        let delegate = StreamDelegate::new(
            Arc::clone(&shared),
            source_index,
            source.label.clone(),
            source.terminal_inactivity,
            queue.clone(),
        );
        let delegate_protocol: &ProtocolObject<dyn SCStreamDelegate> =
            ProtocolObject::from_ref(&*delegate);
        // SAFETY: designated initializer with live filter, configuration and
        // delegate; retained stream owns its configuration.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &source.filter,
                &configuration,
                Some(delegate_protocol),
            )
        };
        let output_protocol: &ProtocolObject<dyn SCStreamOutput> =
            ProtocolObject::from_ref(&*delegate);
        add_output(&stream, output_protocol, SCStreamOutputType::Screen, &queue)?;
        if source_index == 0 && request.system_audio {
            add_output(&stream, output_protocol, SCStreamOutputType::Audio, &queue)?;
        }
        if source_microphone {
            add_output(
                &stream,
                output_protocol,
                SCStreamOutputType::Microphone,
                &queue,
            )?;
        }
        streams.push(stream);
        delegates.push(delegate);
    }

    *lock(&shared.clock) = SessionTimeline::new(shared.now());
    shared.accepting.store(true, Ordering::Release);
    for (started_streams, stream) in streams.iter().enumerate() {
        if let Err(failure) = wait_operation("starting screen capture", |handler| {
            // SAFETY: the completion block is retained by ScreenCaptureKit for
            // the asynchronous operation.
            unsafe {
                stream.startCaptureWithCompletionHandler(Some(handler));
            }
        }) {
            shared.accepting.store(false, Ordering::Release);
            stop_started(&streams[..started_streams]);
            return Err(failure);
        }
    }
    let startup_failure = lock(&shared.failure).clone();
    if let Some(reason) = startup_failure {
        shared.accepting.store(false, Ordering::Release);
        stop_started(&streams);
        return Err(Error::Platform(reason));
    }

    let microphone = if request.microphone && !native_microphone {
        let audio_delegate: &ProtocolObject<dyn AVCaptureAudioDataOutputSampleBufferDelegate> =
            ProtocolObject::from_ref(&*delegates[0]);
        match MicrophoneCapture::start(audio_delegate, &queue) {
            Ok(capture) => Some(capture),
            Err(failure) => {
                shared.accepting.store(false, Ordering::Release);
                stop_started(&streams);
                return Err(failure);
            }
        }
    } else {
        None
    };

    Ok(Box::new(MacRecordingSession {
        streams,
        microphone,
        shared,
        target: request.target.clone(),
        quality: request.quality,
        resolution: request.resolution,
        video_codec: plan.codec,
        _delegates: delegates,
        queue,
        window_id,
        next_window_check: Cell::new(std::time::Instant::now()),
        first_frame_emitted: false,
        finalized: false,
    }))
}

struct Shared {
    writer: Mutex<Writer>,
    raw_writer: Mutex<Option<Writer>>,
    raw_source: Mutex<Option<PrivateRecordingSource>>,
    compositor: Mutex<Option<Compositor>>,
    direct_frame: Mutex<Option<DirectFrame>>,
    clock: Mutex<SessionTimeline>,
    overlays: Mutex<Option<Box<dyn OverlaySource>>>,
    failure: Mutex<Option<String>>,
    warnings: Mutex<VecDeque<String>>,
    raw_complete: AtomicBool,
    accepting: AtomicBool,
    stop_requested: AtomicBool,
    first_frame: AtomicBool,
    epoch: std::time::Instant,
    size: PhysicalSize,
}

struct DirectFrame(CFRetained<CMSampleBuffer>);

// SAFETY: the retained sample is immutable after ScreenCaptureKit publishes it.
// Every later read/append is serialized through Shared's mutexes.
unsafe impl Send for DirectFrame {}

impl Shared {
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    fn state(&self) -> RecordingState {
        lock(&self.clock).state()
    }

    fn elapsed(&self) -> Duration {
        lock(&self.clock).elapsed(self.now())
    }

    fn append(
        &self,
        source_index: usize,
        sample: &CMSampleBuffer,
        output_type: SCStreamOutputType,
    ) -> Result<()> {
        if !self.accepting.load(Ordering::Acquire) {
            return Ok(());
        }
        let state = self.state();
        if state == RecordingState::Stopped {
            return Ok(());
        }
        // SAFETY: immutable readiness read on a live sample from the callback.
        if !unsafe { sample.data_is_ready() } {
            return Ok(());
        }

        if output_type == SCStreamOutputType::Screen {
            match screen_frame_status(sample) {
                Some(SCFrameStatus::Complete | SCFrameStatus::Started) => self.append_screen(
                    source_index,
                    sample,
                    true,
                    state == RecordingState::Recording,
                ),
                Some(SCFrameStatus::Idle) => self.append_screen(
                    source_index,
                    sample,
                    false,
                    state == RecordingState::Recording,
                ),
                _ => Ok(()),
            }
        } else if state != RecordingState::Recording {
            Ok(())
        } else if output_type == SCStreamOutputType::Audio {
            self.append_audio(AudioTrack::System, sample)
        } else if output_type == SCStreamOutputType::Microphone {
            self.append_audio(AudioTrack::Microphone, sample)
        } else {
            Ok(())
        }
    }

    fn append_audio(&self, track: AudioTrack, sample: &CMSampleBuffer) -> Result<()> {
        lock(&self.writer).append_audio(track, sample)?;
        if !self.raw_complete.load(Ordering::Acquire) {
            return Ok(());
        }
        let raw_result = lock(&self.raw_writer)
            .as_mut()
            .map(|writer| writer.append_audio(track, sample));
        if let Some(Err(error)) = raw_result {
            self.abandon_raw_source(format!(
                "private editable audio source stopped accepting samples: {error}"
            ));
        }
        Ok(())
    }

    fn abandon_raw_source(&self, warning: String) {
        if self.raw_complete.swap(false, Ordering::AcqRel) {
            if let Some(mut writer) = lock(&self.raw_writer).take() {
                let _ = writer.discard();
            }
            lock(&self.raw_source).take();
            lock(&self.warnings).push_back(warning);
        }
    }

    fn append_screen(
        &self,
        source_index: usize,
        sample: &CMSampleBuffer,
        has_new_pixels: bool,
        emit: bool,
    ) -> Result<()> {
        let composite = {
            let mut compositor = lock(&self.compositor);
            let Some(compositor) = compositor.as_mut() else {
                if source_index != 0 {
                    return Err(Error::Codec(format!(
                        "single-source recording received unexpected source {source_index}"
                    )));
                }
                let mut direct_frame = lock(&self.direct_frame);
                if !emit {
                    if has_new_pixels {
                        *direct_frame = Some(DirectFrame(sample.retain()));
                    }
                    return Ok(());
                }
                if has_new_pixels {
                    if self.append_video_with_overlays(sample)? {
                        *direct_frame = None;
                    } else {
                        *direct_frame = Some(DirectFrame(sample.retain()));
                    }
                    return Ok(());
                }
                return self.emit_direct_frame_locked(&mut direct_frame);
            };
            let all_sources_ready = if has_new_pixels {
                compositor.update(source_index, sample)?
            } else {
                compositor.sources_ready()
            };
            if !all_sources_ready
                || !emit
                || !lock(&self.writer).video_ready()
                || !compositor.ready_to_emit(sample)?
            {
                return Ok(());
            }
            compositor.compose(sample)?
        };
        self.append_video_with_overlays(&composite).map(|_| ())
    }

    fn emit_direct_frame(&self) -> Result<()> {
        self.emit_direct_frame_locked(&mut lock(&self.direct_frame))
    }

    fn emit_direct_frame_locked(&self, pending: &mut Option<DirectFrame>) -> Result<()> {
        let Some(sample) = pending.take() else {
            return Ok(());
        };
        if !self.append_video_with_overlays(&sample.0)? && pending.is_none() {
            *pending = Some(sample);
        }
        Ok(())
    }

    fn append_video_with_overlays(&self, sample: &CMSampleBuffer) -> Result<bool> {
        let elapsed = self.elapsed();
        let layers = {
            let mut overlays = lock(&self.overlays);
            let layers = overlays
                .as_mut()
                .map_or_else(Vec::new, |source| source.layers(elapsed, self.size));
            if let Some(warning) = overlays.as_mut().and_then(|source| source.take_warning()) {
                let mut warnings = lock(&self.warnings);
                if warnings.back() != Some(&warning) {
                    warnings.push_back(warning);
                }
            }
            layers
        };
        let rendered = if layers.is_empty() {
            None
        } else if self.raw_complete.load(Ordering::Acquire) && lock(&self.raw_writer).is_some() {
            let rendered = super::compositor::clone_bgra_sample(sample)?;
            super::overlay::composite(&rendered, &layers)?;
            Some(rendered)
        } else {
            super::overlay::composite(sample, &layers)?;
            None
        };
        let final_sample = rendered.as_deref().unwrap_or(sample);
        if lock(&self.writer).append_video(final_sample)? {
            self.first_frame.store(true, Ordering::Release);
            if self.raw_complete.load(Ordering::Acquire) {
                let raw_result = lock(&self.raw_writer)
                    .as_mut()
                    .map(|writer| writer.append_video(sample));
                match raw_result {
                    Some(Ok(true)) | None => {}
                    Some(Ok(false)) => self.abandon_raw_source(
                        "private editable source fell behind; the final recording is intact but interaction toggles are unavailable"
                            .to_owned(),
                    ),
                    Some(Err(error)) => self.abandon_raw_source(format!(
                        "private editable source failed; the final recording is intact: {error}"
                    )),
                }
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn fail(&self, reason: String) {
        self.accepting.store(false, Ordering::Release);
        let mut failure = lock(&self.failure);
        if failure.is_none() {
            *failure = Some(reason);
        }
        lock(&self.clock).stop(self.now());
    }

    fn add_failure(&self, reason: String) {
        let mut failure = lock(&self.failure);
        if let Some(existing) = failure.as_mut() {
            existing.push_str("; ");
            existing.push_str(&reason);
        } else {
            *failure = Some(reason);
        }
    }
}

fn screen_frame_status(sample: &CMSampleBuffer) -> Option<SCFrameStatus> {
    // SAFETY: SCK owns the immutable attachment array for the duration of this
    // callback; its first element is the documented frame-info dictionary.
    let attachments = (unsafe { sample.sample_attachments_array(false) })?;
    if attachments.count() < 1 {
        return None;
    }
    // SAFETY: the array is nonempty and SCK documents this value as an
    // NSDictionary whose status value is an NSNumber.
    let dictionary = unsafe {
        &*attachments
            .value_at_index(0)
            .cast::<NSDictionary<NSString, NSNumber>>()
    };
    // SAFETY: immutable weak-linked SCK frame-info key.
    let key = unsafe { SCStreamFrameInfoStatus };
    dictionary
        .objectForKey(key)
        .map(|status| SCFrameStatus(status.integerValue()))
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = StreamDelegateIvars]
    struct StreamDelegate;

    unsafe impl NSObjectProtocol for StreamDelegate {}

    unsafe impl SCStreamOutput for StreamDelegate {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            output_type: SCStreamOutputType,
        ) {
            let ivars = self.ivars();
            if output_type == SCStreamOutputType::Screen
                && ivars.terminal_inactivity
                && screen_frame_status(sample_buffer) == Some(SCFrameStatus::Stopped)
            {
                ivars.shared.fail(format!(
                    "recording target {} stopped producing frames",
                    ivars.source_label
                ));
                return;
            }
            if let Err(failure) =
                ivars
                    .shared
                    .append(ivars.source_index, sample_buffer, output_type)
            {
                ivars.shared.fail(failure.to_string());
                // SAFETY: called on the stream's serial callback queue; stopping
                // asynchronously is valid and avoids blocking that queue.
                unsafe {
                    stream.stopCaptureWithCompletionHandler(None);
                }
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, failure: &NSError) {
            let ivars = self.ivars();
            let expected_stop = ivars.shared.stop_requested.load(Ordering::Acquire);
            let user_stopped = SCStreamErrorCode(failure.code()) == SCStreamErrorCode::UserStopped;
            let reason = super::error::from_sck(failure, "screen capture stopped").to_string();
            let shared = Arc::clone(&ivars.shared);
            ivars.callback_queue.exec_async(move || {
                if !(expected_stop && user_stopped) {
                    shared.fail(reason);
                }
            });
        }

        #[unsafe(method(streamDidBecomeInactive:))]
        unsafe fn streamDidBecomeInactive(&self, _stream: &SCStream) {
            let ivars = self.ivars();
            if ivars.terminal_inactivity {
                ivars.shared.fail(format!(
                    "recording target {} disappeared before the session was stopped",
                    ivars.source_label
                ));
            }
        }
    }

    unsafe impl AVCaptureAudioDataOutputSampleBufferDelegate for StreamDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn captureOutput_didOutputSampleBuffer_fromConnection(
            &self,
            _output: &AVCaptureOutput,
            sample_buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            if let Err(failure) =
                self.ivars()
                    .shared
                    .append(0, sample_buffer, SCStreamOutputType::Microphone)
            {
                self.ivars().shared.fail(failure.to_string());
            }
        }
    }
);

struct StreamDelegateIvars {
    shared: Arc<Shared>,
    source_index: usize,
    source_label: String,
    terminal_inactivity: bool,
    callback_queue: DispatchRetained<DispatchQueue>,
}

impl StreamDelegate {
    fn new(
        shared: Arc<Shared>,
        source_index: usize,
        source_label: String,
        terminal_inactivity: bool,
        callback_queue: DispatchRetained<DispatchQueue>,
    ) -> Retained<Self> {
        let allocated = Self::alloc().set_ivars(StreamDelegateIvars {
            shared,
            source_index,
            source_label,
            terminal_inactivity,
            callback_queue,
        });
        // SAFETY: NSObject's initializer is valid for this direct subclass and
        // the Rust ivars were initialized above.
        unsafe { msg_send![super(allocated), init] }
    }
}

struct MacRecordingSession {
    streams: Vec<Retained<SCStream>>,
    microphone: Option<MicrophoneCapture>,
    shared: Arc<Shared>,
    target: CaptureTarget,
    quality: Quality,
    resolution: RecordingResolution,
    video_codec: VideoCodec,
    _delegates: Vec<Retained<StreamDelegate>>,
    queue: DispatchRetained<DispatchQueue>,
    window_id: Option<u32>,
    next_window_check: Cell<std::time::Instant>,
    first_frame_emitted: bool,
    finalized: bool,
}

// SAFETY: ScreenCaptureKit streams are documented for control from arbitrary
// threads. Shared mutable Rust state is behind mutexes/atomics, while the
// delegate itself is invoked only on the serial queue retained above.
unsafe impl Send for MacRecordingSession {}

impl RecordingSession for MacRecordingSession {
    fn state(&self) -> RecordingState {
        let state = self.shared.state();
        let now = std::time::Instant::now();
        if state != RecordingState::Stopped
            && let Some(window_id) = self.window_id
            && now >= self.next_window_check.get()
        {
            self.next_window_check.set(now + Duration::from_millis(250));
            if let Some(bounds) = super::content::window_frame(window_id) {
                if let Some(overlays) = lock(&self.shared.overlays).as_mut() {
                    let _ = overlays.update_source_bounds(bounds);
                }
            } else if !super::content::window_exists(window_id) {
                self.shared
                    .fail(format!("recording target window {window_id} disappeared"));
                return RecordingState::Stopped;
            }
        }
        state
    }

    fn elapsed(&self) -> Duration {
        self.shared.elapsed()
    }

    fn pause(&mut self) -> Result<()> {
        if let Some(overlays) = lock(&self.shared.overlays).as_mut() {
            overlays.pause();
        }
        lock(&self.shared.clock)
            .pause(self.shared.now())
            .map_err(|message| Error::InvalidRequest(message.to_owned()))?;
        if let Err(failure) = lock(&self.shared.writer).pause() {
            self.shared.fail(failure.to_string());
            return Err(failure);
        }
        let raw_pause = lock(&self.shared.raw_writer).as_mut().map(Writer::pause);
        if let Some(Err(failure)) = raw_pause {
            self.shared.abandon_raw_source(format!(
                "private editable source could not pause; the final recording is intact: {failure}"
            ));
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        {
            let mut clock = lock(&self.shared.clock);
            let paused = clock
                .resume(self.shared.now())
                .map_err(|message| Error::InvalidRequest(message.to_owned()))?;
            lock(&self.shared.writer).resume(paused);
            if let Some(writer) = lock(&self.shared.raw_writer).as_mut() {
                writer.resume(paused);
            }
        }
        if let Some(overlays) = lock(&self.shared.overlays).as_mut() {
            overlays.resume();
        }
        if let Err(failure) = self.shared.emit_direct_frame() {
            self.shared.fail(failure.to_string());
            return Err(failure);
        }
        Ok(())
    }

    fn poll(&mut self) -> Option<crate::SessionEvent> {
        if let Some(warning) = lock(&self.shared.warnings).pop_front() {
            Some(crate::SessionEvent::Warning(warning))
        } else if !self.first_frame_emitted && self.shared.first_frame.load(Ordering::Acquire) {
            self.first_frame_emitted = true;
            Some(crate::SessionEvent::FirstFrame)
        } else {
            None
        }
    }

    fn engine_elapsed_secs(&self) -> Option<f64> {
        Some(self.shared.elapsed().as_secs_f64())
    }

    fn stop(mut self: Box<Self>) -> Result<Recording> {
        self.queue.exec_sync(|| {});
        self.shared.stop_requested.store(true, Ordering::Release);
        self.shared.accepting.store(false, Ordering::Release);
        let interactions = lock(&self.shared.overlays)
            .take()
            .and_then(|mut source| source.finish());
        lock(&self.shared.clock).stop(self.shared.now());
        for stream in &self.streams {
            if let Err(failure) = wait_operation("stopping screen capture", |handler| {
                // SAFETY: valid stream and completion block.
                unsafe {
                    stream.stopCaptureWithCompletionHandler(Some(handler));
                }
            }) {
                self.shared.add_failure(failure.to_string());
            }
        }
        if let Some(microphone) = &self.microphone {
            microphone.stop();
        }
        self.queue.exec_sync(|| {});
        let elapsed = self.shared.elapsed();
        let interrupted = lock(&self.shared.failure).take();
        let summary = lock(&self.shared.writer).finish(interrupted.clone(), elapsed)?;
        let raw_summary = if self.shared.raw_complete.load(Ordering::Acquire) {
            lock(&self.shared.raw_writer)
                .as_mut()
                .map(|writer| writer.finish(interrupted, elapsed))
                .transpose()
        } else {
            if let Some(writer) = lock(&self.shared.raw_writer).as_mut() {
                let _ = writer.discard();
            }
            Ok(None)
        };
        self.finalized = true;

        let metadata = RecordingMetadata {
            size: Some(self.shared.size),
            frames: Some(summary.frames),
            audio_channels: Some(u16::from(summary.has_audio) * 2),
            file_size_bytes: std::fs::metadata(&summary.path)
                .ok()
                .map(|value| value.len()),
            video_codec: Some(self.video_codec),
            quality: Some(self.quality),
            resolution: Some(self.resolution),
            interaction_editable: None,
        };
        let mut recording = Recording::native(
            summary.path,
            summary.duration.as_secs_f64(),
            super::ENGINE_NAME,
        )?
        .with_native_details(self.target.clone(), metadata)?;
        match (interactions, raw_summary) {
            (Some(mut interactions), Ok(Some(raw))) => {
                let source = lock(&self.shared.raw_source).take().ok_or_else(|| {
                    Error::Platform(
                        "interaction recording lost its private source ownership".to_owned(),
                    )
                })?;
                if raw.path != source.path() {
                    return Err(Error::Platform(
                        "interaction recording source path changed during finalisation".to_owned(),
                    ));
                }
                interactions.attach_source(source);
                recording = recording.with_interactions(interactions);
            }
            (Some(_), Err(error)) => {
                tracing::warn!(
                    "private editable interaction source failed; the final recording is intact: {error}"
                );
                recording.metadata.interaction_editable = Some(false);
            }
            (Some(_), Ok(None)) => recording.metadata.interaction_editable = Some(false),
            (None, _) => {}
        }
        if let Some(reason) = summary.partial {
            recording = recording.into_partial_with_salvageability(
                summary
                    .salvageability
                    .expect("a partial writer summary always classifies retained output"),
                reason,
            )?;
        }
        Ok(recording)
    }
}

impl Drop for MacRecordingSession {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        self.shared.stop_requested.store(true, Ordering::Release);
        self.shared.accepting.store(false, Ordering::Release);
        lock(&self.shared.overlays).take();
        // SAFETY: best-effort asynchronous shutdown during an abandoned session.
        for stream in &self.streams {
            unsafe {
                stream.stopCaptureWithCompletionHandler(None);
            }
        }
        if let Some(microphone) = &self.microphone {
            microphone.stop();
        }
        lock(&self.shared.clock).stop(self.shared.now());
        if let Err(failure) = lock(&self.shared.writer).discard() {
            tracing::error!("abandoned macOS recording cleanup failed: {failure}");
        }
        if let Some(writer) = lock(&self.shared.raw_writer).as_mut()
            && let Err(failure) = writer.discard()
        {
            tracing::error!("abandoned private recording cleanup failed: {failure}");
        }
        lock(&self.shared.raw_source).take();
        self.finalized = true;
    }
}

fn configure(
    content: &CaptureContent,
    source: &CaptureSource,
    source_index: usize,
    plan: &RecordingPlan,
    request: &RecordingRequest,
    composites_cursor: bool,
) -> Retained<SCStreamConfiguration> {
    // SAFETY: ordinary ScreenCaptureKit object construction.
    let configuration = unsafe { SCStreamConfiguration::new() };
    // SAFETY: all values are validated and written before the configuration is
    // attached to a stream.
    unsafe {
        let output = content.output_rect(
            source_index,
            plan.size.width.round() as u32,
            plan.size.height.round() as u32,
        );
        configuration.setWidth(output.width as usize);
        configuration.setHeight(output.height as usize);
        configuration.setMinimumFrameInterval(CMTime::new(1, request.fps as i32));
        configuration.setPixelFormat(match plan.pixel_format {
            CapturePixelFormat::VideoRange420 => PIXEL_FORMAT_420_VIDEO_RANGE,
            CapturePixelFormat::Bgra => PIXEL_FORMAT_BGRA,
        });
        configuration.setShowsCursor(request.show_cursor && !composites_cursor);
        configuration.setQueueDepth(5);
        configuration.setScalesToFit(
            content.requires_composition()
                || plan.size.width.round() as u32 != content.native_width
                || plan.size.height.round() as u32 != content.native_height,
        );
        if configuration.respondsToSelector(sel!(setPreservesAspectRatio:)) {
            configuration.setPreservesAspectRatio(!content.requires_composition());
        }
        if configuration.respondsToSelector(sel!(setCaptureResolution:)) {
            configuration.setCaptureResolution(SCCaptureResolutionType::Best);
        }
        if plan.pixel_format == CapturePixelFormat::Bgra {
            configuration.setColorSpaceName(kCGColorSpaceSRGB);
        }
        if let Some(source_rect) = source.source_rect {
            configuration.setSourceRect(source_rect);
        }
        if source_index == 0 && request.system_audio {
            configuration.setCapturesAudio(true);
            configuration.setSampleRate(48_000);
            configuration.setChannelCount(2);
            configuration.setExcludesCurrentProcessAudio(true);
        }
    }
    configuration
}

pub(super) fn system_audio_available() -> bool {
    // SAFETY: ordinary configuration construction used only for selector
    // availability checks against the running ScreenCaptureKit framework.
    let configuration = unsafe { SCStreamConfiguration::new() };
    configuration_supports_system_audio(&configuration)
}

fn configuration_supports_system_audio(configuration: &SCStreamConfiguration) -> bool {
    configuration.respondsToSelector(sel!(setCapturesAudio:))
        && configuration.respondsToSelector(sel!(setSampleRate:))
        && configuration.respondsToSelector(sel!(setChannelCount:))
        && configuration.respondsToSelector(sel!(setExcludesCurrentProcessAudio:))
}

fn stop_started(streams: &[Retained<SCStream>]) {
    for stream in streams {
        let _ = wait_operation(
            "stopping screen capture after a startup failure",
            |handler| {
                // SAFETY: valid stream and completion block.
                unsafe {
                    stream.stopCaptureWithCompletionHandler(Some(handler));
                }
            },
        );
    }
}

fn add_output(
    stream: &SCStream,
    delegate: &ProtocolObject<dyn SCStreamOutput>,
    output_type: SCStreamOutputType,
    queue: &DispatchQueue,
) -> Result<()> {
    // SAFETY: delegate and serial queue outlive the stream session.
    unsafe {
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(delegate, output_type, Some(queue))
            .map_err(|failure| super::error::from_sck(&failure, "attaching a capture output"))
    }
}

struct ErrorDelivery(Option<Retained<NSError>>);

// SAFETY: this wrapper crosses one thread boundary by move under a mutex. The
// NSError is immutable and retained across the source autorelease pool.
unsafe impl Send for ErrorDelivery {}

fn wait_operation(
    context: &str,
    operation: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> Result<()> {
    let delivery = Arc::new((Mutex::new(None::<ErrorDelivery>), Condvar::new()));
    let completion = {
        let delivery = Arc::clone(&delivery);
        RcBlock::new(move |failure: *mut NSError| {
            // SAFETY: the callback supplies null or a live NSError.
            let failure = unsafe { Retained::retain(failure) };
            let (slot, ready) = &*delivery;
            *lock(slot) = Some(ErrorDelivery(failure));
            ready.notify_all();
        })
    };
    operation(&completion);

    let (slot, ready) = &*delivery;
    let (mut slot, _) = ready
        .wait_timeout_while(lock(slot), OPERATION_TIMEOUT, |value| value.is_none())
        .unwrap_or_else(PoisonError::into_inner);
    let ErrorDelivery(failure) = slot.take().ok_or_else(|| {
        Error::Platform(format!(
            "{context}: ScreenCaptureKit did not answer in time"
        ))
    })?;
    failure.map_or_else(
        || Ok(()),
        |failure| Err(super::error::from_sck(&failure, context)),
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
