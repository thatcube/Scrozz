//! ScreenCaptureKit stream lifecycle and callbacks.

#![allow(non_snake_case)]

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
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSError, NSObject};
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
    SCStreamOutputType,
};
use scrozz_core::{CaptureTarget, Error, PhysicalSize, Result};

use crate::{
    OverlaySource, Quality, Recording, RecordingMetadata, RecordingRequest, RecordingResolution,
    RecordingSession, RecordingState, VideoCodec,
};

use super::audio::MicrophoneCapture;
use super::content::CaptureContent;
use super::plan::{CapturePixelFormat, RecordingPlan};
use super::timeline::SessionTimeline;
use super::writer::{AudioTrack, Writer};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const PIXEL_FORMAT_420_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"420v");
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

pub(crate) fn start(
    request: &RecordingRequest,
    overlays: Option<Box<dyn OverlaySource>>,
) -> Result<Box<dyn RecordingSession>> {
    let content = super::content::resolve(&request.target)?;
    if request.microphone {
        super::permission::ensure_microphone()?;
    }

    let has_overlays = overlays.is_some();
    let plan = RecordingPlan::new(
        request,
        content.native_width,
        content.native_height,
        content.scale,
        has_overlays,
    );
    if plan.size.width < 2.0 || plan.size.height < 2.0 {
        return Err(Error::InvalidRequest(
            "recording resolution must be at least 2 by 2 pixels".to_owned(),
        ));
    }

    let configuration = configure(&content, &plan, request);
    let native_microphone =
        request.microphone && configuration.respondsToSelector(sel!(setCaptureMicrophone:));
    if native_microphone {
        // SAFETY: selector availability was checked on this exact object.
        unsafe {
            configuration.setCaptureMicrophone(true);
        }
    }

    let writer = Writer::new(
        request.destination.as_deref(),
        &plan,
        request.fps,
        request.system_audio,
        request.microphone,
    )?;
    let shared = Arc::new(Shared {
        writer: Mutex::new(writer),
        clock: Mutex::new(SessionTimeline::new(Duration::ZERO)),
        overlays: Mutex::new(overlays),
        failure: Mutex::new(None),
        accepting: AtomicBool::new(false),
        epoch: std::time::Instant::now(),
        size: plan.size,
        window_target: matches!(request.target, CaptureTarget::Window(_)),
    });
    let delegate = StreamDelegate::new(Arc::clone(&shared));
    let delegate_protocol: &ProtocolObject<dyn SCStreamDelegate> =
        ProtocolObject::from_ref(&*delegate);
    // SAFETY: designated initializer with live filter, configuration and delegate.
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &content.filter,
            &configuration,
            Some(delegate_protocol),
        )
    };
    let queue = DispatchQueue::new("com.thatcube.scrozz.recording", None);
    let output_protocol: &ProtocolObject<dyn SCStreamOutput> = ProtocolObject::from_ref(&*delegate);

    add_output(&stream, output_protocol, SCStreamOutputType::Screen, &queue)?;
    if request.system_audio {
        add_output(&stream, output_protocol, SCStreamOutputType::Audio, &queue)?;
    }
    if native_microphone {
        add_output(
            &stream,
            output_protocol,
            SCStreamOutputType::Microphone,
            &queue,
        )?;
    }

    wait_operation("starting screen capture", |handler| {
        // SAFETY: the completion block is retained by ScreenCaptureKit for the
        // asynchronous operation.
        unsafe {
            stream.startCaptureWithCompletionHandler(Some(handler));
        }
    })?;
    {
        let failure = lock(&shared.failure);
        if let Some(reason) = failure.as_ref() {
            return Err(Error::Platform(reason.clone()));
        }
        *lock(&shared.clock) = SessionTimeline::new(shared.now());
    }
    shared.accepting.store(true, Ordering::Release);

    let microphone = if request.microphone && !native_microphone {
        let audio_delegate: &ProtocolObject<dyn AVCaptureAudioDataOutputSampleBufferDelegate> =
            ProtocolObject::from_ref(&*delegate);
        match MicrophoneCapture::start(audio_delegate, &queue) {
            Ok(capture) => Some(capture),
            Err(failure) => {
                let _ = wait_operation("stopping screen capture", |handler| {
                    // SAFETY: valid stream and completion block.
                    unsafe {
                        stream.stopCaptureWithCompletionHandler(Some(handler));
                    }
                });
                return Err(failure);
            }
        }
    } else {
        None
    };

    Ok(Box::new(MacRecordingSession {
        stream,
        microphone,
        shared,
        target: request.target.clone(),
        quality: request.quality,
        resolution: request.resolution,
        video_codec: plan.codec,
        _delegate: delegate,
        _queue: queue,
        finalized: false,
    }))
}

struct Shared {
    writer: Mutex<Writer>,
    clock: Mutex<SessionTimeline>,
    overlays: Mutex<Option<Box<dyn OverlaySource>>>,
    failure: Mutex<Option<String>>,
    accepting: AtomicBool,
    epoch: std::time::Instant,
    size: PhysicalSize,
    window_target: bool,
}

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

    fn append(&self, sample: &CMSampleBuffer, output_type: SCStreamOutputType) -> Result<()> {
        if !self.accepting.load(Ordering::Acquire) || self.state() != RecordingState::Recording {
            return Ok(());
        }
        // SAFETY: immutable readiness read on a live sample from the callback.
        if !unsafe { sample.data_is_ready() } {
            return Ok(());
        }

        if output_type == SCStreamOutputType::Screen {
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
            lock(&self.writer).append_video(sample)
        } else if output_type == SCStreamOutputType::Audio {
            lock(&self.writer).append_audio(AudioTrack::System, sample)
        } else if output_type == SCStreamOutputType::Microphone {
            lock(&self.writer).append_audio(AudioTrack::Microphone, sample)
        } else {
            Ok(())
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

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = Arc<Shared>]
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
            if let Err(failure) = self.ivars().append(sample_buffer, output_type) {
                self.ivars().fail(failure.to_string());
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
            self.ivars()
                .fail(super::error::from_sck(failure, "screen capture stopped").to_string());
        }

        #[unsafe(method(streamDidBecomeInactive:))]
        unsafe fn streamDidBecomeInactive(&self, stream: &SCStream) {
            if self.ivars().window_target {
                self.ivars().fail(
                    "recording target disappeared before the session was stopped".to_owned(),
                );
                // SAFETY: see the sample callback's stop path.
                unsafe {
                    stream.stopCaptureWithCompletionHandler(None);
                }
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
            if let Err(failure) = self
                .ivars()
                .append(sample_buffer, SCStreamOutputType::Microphone)
            {
                self.ivars().fail(failure.to_string());
            }
        }
    }
);

impl StreamDelegate {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let allocated = Self::alloc().set_ivars(shared);
        // SAFETY: NSObject's initializer is valid for this direct subclass and
        // the Rust ivars were initialized above.
        unsafe { msg_send![super(allocated), init] }
    }
}

struct MacRecordingSession {
    stream: Retained<SCStream>,
    microphone: Option<MicrophoneCapture>,
    shared: Arc<Shared>,
    target: CaptureTarget,
    quality: Quality,
    resolution: RecordingResolution,
    video_codec: VideoCodec,
    _delegate: Retained<StreamDelegate>,
    _queue: DispatchRetained<DispatchQueue>,
    finalized: bool,
}

// SAFETY: ScreenCaptureKit streams are documented for control from arbitrary
// threads. Shared mutable Rust state is behind mutexes/atomics, while the
// delegate itself is invoked only on the serial queue retained above.
unsafe impl Send for MacRecordingSession {}

impl RecordingSession for MacRecordingSession {
    fn state(&self) -> RecordingState {
        self.shared.state()
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
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        lock(&self.shared.writer).resume();
        lock(&self.shared.clock)
            .resume(self.shared.now())
            .map_err(|message| Error::InvalidRequest(message.to_owned()))
    }

    fn engine_elapsed_secs(&self) -> Option<f64> {
        Some(self.shared.elapsed().as_secs_f64())
    }

    fn stop(mut self: Box<Self>) -> Result<Recording> {
        self.shared.accepting.store(false, Ordering::Release);
        if let Err(failure) = wait_operation("stopping screen capture", |handler| {
            // SAFETY: valid stream and completion block.
            unsafe {
                self.stream.stopCaptureWithCompletionHandler(Some(handler));
            }
        }) {
            self.shared.add_failure(failure.to_string());
        }
        if let Some(microphone) = &self.microphone {
            microphone.stop();
        }
        lock(&self.shared.clock).stop(self.shared.now());
        let interrupted = lock(&self.shared.failure).take();
        let summary = lock(&self.shared.writer).finish(interrupted)?;
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
        };
        let mut recording = Recording::native(
            summary.path,
            summary.duration.as_secs_f64(),
            super::ENGINE_NAME,
        )?
        .with_native_details(self.target.clone(), metadata)?;
        if let Some(reason) = summary.partial {
            recording = recording.into_partial(reason)?;
        }
        Ok(recording)
    }
}

impl Drop for MacRecordingSession {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        self.shared.accepting.store(false, Ordering::Release);
        // SAFETY: best-effort asynchronous shutdown during an abandoned session.
        unsafe {
            self.stream.stopCaptureWithCompletionHandler(None);
        }
        if let Some(microphone) = &self.microphone {
            microphone.stop();
        }
        lock(&self.shared.clock).stop(self.shared.now());
        let mut interrupted = lock(&self.shared.failure).take();
        if interrupted.is_none() {
            interrupted = Some("recording session was dropped without an explicit stop".to_owned());
        }
        let _ = lock(&self.shared.writer).finish(interrupted);
        self.finalized = true;
    }
}

fn configure(
    content: &CaptureContent,
    plan: &RecordingPlan,
    request: &RecordingRequest,
) -> Retained<SCStreamConfiguration> {
    // SAFETY: ordinary ScreenCaptureKit object construction.
    let configuration = unsafe { SCStreamConfiguration::new() };
    // SAFETY: all values are validated and written before the configuration is
    // attached to a stream.
    unsafe {
        configuration.setWidth(plan.size.width.round() as usize);
        configuration.setHeight(plan.size.height.round() as usize);
        configuration.setMinimumFrameInterval(CMTime::new(1, request.fps as i32));
        configuration.setPixelFormat(match plan.pixel_format {
            CapturePixelFormat::VideoRange420 => PIXEL_FORMAT_420_VIDEO_RANGE,
            CapturePixelFormat::Bgra => PIXEL_FORMAT_BGRA,
        });
        configuration.setShowsCursor(request.show_cursor);
        configuration.setQueueDepth(5);
        configuration.setScalesToFit(
            plan.size.width.round() as u32 != content.native_width
                || plan.size.height.round() as u32 != content.native_height,
        );
        configuration.setPreservesAspectRatio(true);
        configuration.setCaptureResolution(SCCaptureResolutionType::Best);
        if let Some(source_rect) = content.source_rect {
            configuration.setSourceRect(source_rect);
        }
        if request.system_audio {
            configuration.setCapturesAudio(true);
            configuration.setSampleRate(48_000);
            configuration.setChannelCount(2);
            configuration.setExcludesCurrentProcessAudio(true);
        }
    }
    configuration
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
