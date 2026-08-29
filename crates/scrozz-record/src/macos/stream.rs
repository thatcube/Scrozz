//! ScreenCaptureKit stream lifecycle and callbacks.

#![allow(non_snake_case)]

use std::cell::Cell;
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
    AVCaptureVideoDataOutputSampleBufferDelegate,
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
    CameraFeed, CameraRecordingMetadata, CameraRequest, CameraRuntimeStatus, OverlaySource,
    Quality, Recording, RecordingMetadata, RecordingRequest, RecordingResolution, RecordingSession,
    RecordingState, VideoCodec, camera::CameraCompositor, settings::CameraSettings,
};

use super::audio::MicrophoneCapture;
use super::camera::CameraCapture;
use super::compositor::Compositor;
use super::content::{CaptureContent, CaptureSource};
use super::plan::{CapturePixelFormat, RecordingPlan};
use super::timeline::SessionTimeline;
use super::writer::{AudioTrack, Writer};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const CAMERA_RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
const PIXEL_FORMAT_420_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"420v");
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

pub(crate) fn start(
    request: &RecordingRequest,
    overlays: Option<Box<dyn OverlaySource>>,
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

    let camera_feed = request.camera.as_ref().map(CameraFeed::new).transpose()?;
    let has_overlays = overlays.is_some() || camera_feed.is_some();
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
    let output_width = plan.size.width.round() as u32;
    let output_height = plan.size.height.round() as u32;
    if let Some(camera) = &camera_feed {
        camera.set_output_size(output_width, output_height)?;
    }
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
        compositor: Mutex::new(compositor),
        direct_frame: Mutex::new(None),
        clock: Mutex::new(SessionTimeline::new(Duration::ZERO)),
        overlays: Mutex::new(overlays),
        camera: camera_feed.clone(),
        camera_compositor: Mutex::new(CameraCompositor::default()),
        camera_warning: Mutex::new(None),
        failure: Mutex::new(None),
        accepting: AtomicBool::new(false),
        stop_requested: AtomicBool::new(false),
        first_frame: AtomicBool::new(false),
        epoch: std::time::Instant::now(),
        size: plan.size,
    });
    let camera_delegate = request
        .camera
        .as_ref()
        .map(|_| CameraDelegate::new(Arc::clone(&shared)));
    let window_id = match &request.target {
        CaptureTarget::Window(id) => id.0.parse::<u32>().ok(),
        _ => None,
    };
    let mut streams = Vec::with_capacity(content.sources.len());
    let mut delegates = Vec::with_capacity(content.sources.len());
    let mut native_microphone = false;
    for (source_index, source) in content.sources.iter().enumerate() {
        let configuration = configure(&content, source, source_index, &plan, request);
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

    let camera = match (&request.camera, camera_delegate.as_ref()) {
        (Some(camera_request), Some(delegate)) => {
            let delegate_protocol: &ProtocolObject<
                dyn AVCaptureVideoDataOutputSampleBufferDelegate,
            > = ProtocolObject::from_ref(&**delegate);
            let capture = CameraCapture::start(camera_request, delegate_protocol, &queue)?;
            if let Some(feed) = &camera_feed {
                feed.activate();
            }
            Some(capture)
        }
        _ => None,
    };

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
            if let Some(camera) = &camera {
                camera.stop();
            }
            if let Some(feed) = &camera_feed {
                feed.stop();
            }
            return Err(failure);
        }
    }
    let startup_failure = lock(&shared.failure).clone();
    if let Some(reason) = startup_failure {
        shared.accepting.store(false, Ordering::Release);
        stop_started(&streams);
        if let Some(camera) = &camera {
            camera.stop();
        }
        if let Some(feed) = &camera_feed {
            feed.stop();
        }
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
        camera,
        camera_request: request.camera.clone(),
        camera_delegate,
        shared,
        target: request.target.clone(),
        quality: request.quality,
        resolution: request.resolution,
        video_codec: plan.codec,
        _delegates: delegates,
        queue,
        window_id,
        next_window_check: Cell::new(std::time::Instant::now()),
        next_camera_check: std::time::Instant::now() + CAMERA_RECONNECT_INTERVAL,
        first_frame_emitted: false,
        finalized: false,
    }))
}

struct Shared {
    writer: Mutex<Writer>,
    compositor: Mutex<Option<Compositor>>,
    direct_frame: Mutex<Option<DirectFrame>>,
    clock: Mutex<SessionTimeline>,
    overlays: Mutex<Option<Box<dyn OverlaySource>>>,
    camera: Option<CameraFeed>,
    camera_compositor: Mutex<CameraCompositor>,
    camera_warning: Mutex<Option<String>>,
    failure: Mutex<Option<String>>,
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
            lock(&self.writer).append_audio(AudioTrack::System, sample)
        } else if output_type == SCStreamOutputType::Microphone {
            lock(&self.writer).append_audio(AudioTrack::Microphone, sample)
        } else {
            Ok(())
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
            overlays
                .as_mut()
                .map_or_else(Vec::new, |source| source.layers(elapsed, self.size))
        };
        if !layers.is_empty() {
            super::overlay::composite(sample, &layers)?;
        }
        if let Some(camera) = &self.camera {
            let frame = camera.frame_for(elapsed);
            super::camera::composite(
                sample,
                frame.as_ref(),
                camera.settings(),
                &mut lock(&self.camera_compositor),
            )?;
        }
        if lock(&self.writer).append_video(sample)? {
            self.first_frame.store(true, Ordering::Release);
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

    fn append_camera(&self, sample: &CMSampleBuffer) -> Result<()> {
        if !self.accepting.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.state() != RecordingState::Recording {
            return Ok(());
        }
        let Some(camera) = &self.camera else {
            return Ok(());
        };
        let frame = super::camera::copy_frame(sample, self.elapsed())?;
        camera.push(frame).map(|_| ())
    }

    fn warn_camera(&self, warning: impl Into<String>) {
        let warning = warning.into();
        if let Some(camera) = &self.camera {
            camera.warn(warning.clone());
        }
        *lock(&self.camera_warning) = Some(warning);
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

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = CameraDelegateIvars]
    struct CameraDelegate;

    unsafe impl NSObjectProtocol for CameraDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CameraDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn captureOutput_didOutputSampleBuffer_fromConnection(
            &self,
            _output: &AVCaptureOutput,
            sample_buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            if let Err(failure) = self.ivars().shared.append_camera(sample_buffer) {
                self.ivars().shared.warn_camera(failure.to_string());
            }
        }

        #[unsafe(method(captureOutput:didDropSampleBuffer:fromConnection:))]
        unsafe fn captureOutput_didDropSampleBuffer_fromConnection(
            &self,
            _output: &AVCaptureOutput,
            _sample_buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            if let Some(camera) = &self.ivars().shared.camera {
                camera.note_drop();
            }
        }
    }
);

struct CameraDelegateIvars {
    shared: Arc<Shared>,
}

impl CameraDelegate {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let allocated = Self::alloc().set_ivars(CameraDelegateIvars { shared });
        unsafe { msg_send![super(allocated), init] }
    }
}

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
    camera: Option<CameraCapture>,
    camera_request: Option<CameraRequest>,
    camera_delegate: Option<Retained<CameraDelegate>>,
    shared: Arc<Shared>,
    target: CaptureTarget,
    quality: Quality,
    resolution: RecordingResolution,
    video_codec: VideoCodec,
    _delegates: Vec<Retained<StreamDelegate>>,
    queue: DispatchRetained<DispatchQueue>,
    window_id: Option<u32>,
    next_window_check: Cell<std::time::Instant>,
    next_camera_check: std::time::Instant,
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
            if !super::content::window_exists(window_id) {
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
        lock(&self.shared.clock)
            .pause(self.shared.now())
            .map_err(|message| Error::InvalidRequest(message.to_owned()))?;
        if let Err(failure) = lock(&self.shared.writer).pause() {
            self.shared.fail(failure.to_string());
            return Err(failure);
        }
        if let Some(camera) = &self.shared.camera {
            camera.clear_frames();
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
        }
        if let Some(camera) = &self.shared.camera {
            camera.clear_frames();
        }
        if let Err(failure) = self.shared.emit_direct_frame() {
            self.shared.fail(failure.to_string());
            return Err(failure);
        }
        Ok(())
    }

    fn poll(&mut self) -> Option<crate::SessionEvent> {
        self.refresh_camera();
        if let Some(warning) = lock(&self.shared.camera_warning).take() {
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

    fn camera_status(&self) -> Option<CameraRuntimeStatus> {
        self.shared.camera.as_ref().map(CameraFeed::status)
    }

    fn camera_preview(&self) -> Option<crate::CameraPreview> {
        self.shared
            .camera
            .as_ref()
            .and_then(|camera| camera.preview(self.shared.elapsed()))
    }

    fn update_camera(&mut self, settings: CameraSettings) -> Result<()> {
        self.shared
            .camera
            .as_ref()
            .ok_or_else(|| Error::InvalidRequest("this recording has no active camera".to_owned()))?
            .update_settings(settings)?;
        if let Some(request) = &mut self.camera_request {
            request.settings = settings;
        }
        Ok(())
    }

    fn stop(mut self: Box<Self>) -> Result<Recording> {
        self.queue.exec_sync(|| {});
        self.shared.stop_requested.store(true, Ordering::Release);
        self.shared.accepting.store(false, Ordering::Release);
        lock(&self.shared.clock).stop(self.shared.now());
        if let Some(camera) = self.camera.take() {
            camera.stop();
            drop(camera);
        }
        if let Some(feed) = &self.shared.camera {
            feed.stop();
        }
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
        let summary = lock(&self.shared.writer).finish(interrupted, elapsed)?;
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
            camera: self.shared.camera.as_ref().map(|camera| {
                Box::new(CameraRecordingMetadata::from_runtime(
                    camera.settings(),
                    &camera.status(),
                ))
            }),
        };
        let mut recording = Recording::native(
            summary.path,
            summary.duration.as_secs_f64(),
            super::ENGINE_NAME,
        )?
        .with_native_details(self.target.clone(), metadata)?;
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
        if let Some(camera) = self.camera.take() {
            camera.stop();
            drop(camera);
        }
        if let Some(feed) = &self.shared.camera {
            feed.stop();
        }
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
        self.finalized = true;
    }
}

impl MacRecordingSession {
    fn refresh_camera(&mut self) {
        let Some(request) = self.camera_request.clone() else {
            return;
        };
        if std::time::Instant::now() < self.next_camera_check {
            return;
        }
        self.next_camera_check = std::time::Instant::now() + CAMERA_RECONNECT_INTERVAL;
        let disconnected = self
            .camera
            .as_ref()
            .is_some_and(|camera| !camera.is_connected() || !camera.is_running());
        if disconnected {
            self.camera.take();
            if let Some(feed) = &self.shared.camera {
                feed.disconnected("camera disconnected; screen recording continues");
            }
            self.shared
                .warn_camera("camera disconnected; reconnecting to the selected device");
        }
        if self.camera.is_some() {
            return;
        }
        let Some(delegate) = &self.camera_delegate else {
            return;
        };
        let protocol: &ProtocolObject<dyn AVCaptureVideoDataOutputSampleBufferDelegate> =
            ProtocolObject::from_ref(&**delegate);
        match CameraCapture::start(&request, protocol, &self.queue) {
            Ok(camera) => {
                if let Some(feed) = &self.shared.camera {
                    feed.reconnected();
                }
                self.camera = Some(camera);
                self.shared.warn_camera("camera reconnected");
            }
            Err(error) => {
                if let Some(feed) = &self.shared.camera {
                    let message = format!("camera unavailable: {error}");
                    if matches!(error, Error::PermissionDenied { .. }) {
                        feed.permission_denied(message);
                    } else {
                        feed.disconnected(message);
                    }
                }
            }
        }
    }
}

fn configure(
    content: &CaptureContent,
    source: &CaptureSource,
    source_index: usize,
    plan: &RecordingPlan,
    request: &RecordingRequest,
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
        configuration.setShowsCursor(request.show_cursor);
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
