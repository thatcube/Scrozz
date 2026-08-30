//! AVFoundation camera enumeration and capture-session ownership.

#![allow(non_snake_case)]

use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVCaptureDevice, AVCaptureDeviceInput, AVCaptureSession, AVCaptureSessionPreset1280x720,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSMutableDictionary, NSNumber, NSObject, NSString};
use scrozz_core::{ColorSpace, Error, Frame, PhysicalSize, Result, ScaleFactor};

use crate::{
    CameraDevice, CameraDeviceId, CameraDeviceState, CameraFeed, CameraFrame, CameraOrientation,
    CameraPermission, CameraPreview, CameraPreviewSession, CameraRequest, camera::CameraCompositor,
    settings::CameraSettings,
};

/// One running camera. Dropping it synchronously releases the camera.
pub(crate) struct CameraCapture {
    session: Retained<AVCaptureSession>,
    output: Retained<AVCaptureVideoDataOutput>,
    device: Retained<AVCaptureDevice>,
    device_id: CameraDeviceId,
}

// SAFETY: the capture objects are configured before crossing threads. Runtime
// access is limited to AVFoundation's synchronized status and stop methods.
unsafe impl Send for CameraCapture {}

impl CameraCapture {
    pub(crate) fn start(
        request: &CameraRequest,
        delegate: &ProtocolObject<dyn AVCaptureVideoDataOutputSampleBufferDelegate>,
        queue: &DispatchQueue,
    ) -> Result<Self> {
        super::permission::ensure_camera()?;
        let device = selected_device(request.device_id.as_ref())?;
        let device_id = CameraDeviceId::new(unsafe { device.uniqueID() }.to_string())?;
        if !unsafe { device.isConnected() } {
            return Err(Error::TargetGone(
                "the selected camera is no longer connected".to_owned(),
            ));
        }
        if unsafe { device.isInUseByAnotherApplication() } {
            return Err(Error::Unsupported {
                what: "camera capture".to_owned(),
                why: "the selected camera is busy in another application".to_owned(),
            });
        }
        let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }.map_err(
            |failure| {
                Error::Platform(super::error::describe(
                    &failure,
                    "opening the selected camera",
                ))
            },
        )?;
        let session = unsafe { AVCaptureSession::new() };
        let output = unsafe { AVCaptureVideoDataOutput::new() };
        let settings = NSMutableDictionary::<NSString, AnyObject>::new();
        let key = NSString::from_str("PixelFormatType");
        let pixel_format = NSNumber::numberWithUnsignedInt(kCVPixelFormatType_32BGRA);
        settings.insert(&*key, super::settings::any(&*pixel_format));

        unsafe {
            session.beginConfiguration();
            if !session.canAddInput(&input) {
                session.commitConfiguration();
                return Err(Error::Platform(
                    "AVCaptureSession refused the selected camera input".to_owned(),
                ));
            }
            session.addInput(&input);
            if !session.canAddOutput(&output) {
                session.commitConfiguration();
                return Err(Error::Platform(
                    "AVCaptureSession refused the camera video output".to_owned(),
                ));
            }
            output.setVideoSettings(Some(&settings));
            output.setAlwaysDiscardsLateVideoFrames(true);
            session.addOutput(&output);
            if session.canSetSessionPreset(AVCaptureSessionPreset1280x720) {
                session.setSessionPreset(AVCaptureSessionPreset1280x720);
            }
            output.setSampleBufferDelegate_queue(Some(delegate), Some(queue));
            session.commitConfiguration();
            session.startRunning();
        }
        if !unsafe { session.isRunning() } {
            unsafe {
                output.setSampleBufferDelegate_queue(None, None);
                session.stopRunning();
            }
            return Err(Error::Platform(
                "AVCaptureSession did not start the selected camera".to_owned(),
            ));
        }

        Ok(Self {
            session,
            output,
            device,
            device_id,
        })
    }

    pub(crate) fn is_connected(&self) -> bool {
        unsafe { self.device.isConnected() }
    }

    pub(crate) fn is_running(&self) -> bool {
        unsafe { self.session.isRunning() }
    }

    pub(crate) fn device_id(&self) -> &CameraDeviceId {
        &self.device_id
    }

    pub(crate) fn stop(&self) {
        unsafe {
            self.output.setSampleBufferDelegate_queue(None, None);
            self.session.stopRunning();
        }
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

struct MacCameraPreview {
    capture: Option<CameraCapture>,
    feed: CameraFeed,
    started: Instant,
    request: CameraRequest,
    next_reconnect: Instant,
    _delegate: Retained<PreviewDelegate>,
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: CameraCapture serializes AVFoundation shutdown, PreviewDelegate owns
// only Arc state, and preview values contain owned Rust pixels.
unsafe impl Send for MacCameraPreview {}

impl CameraPreviewSession for MacCameraPreview {
    fn status(&self) -> crate::CameraRuntimeStatus {
        self.feed.status()
    }

    fn poll(&mut self) -> Option<CameraPreview> {
        if Instant::now() >= self.next_reconnect {
            self.next_reconnect = Instant::now() + Duration::from_secs(1);
            let disconnected = self
                .capture
                .as_ref()
                .is_some_and(|capture| !capture.is_connected() || !capture.is_running());
            if disconnected {
                self.capture.take();
                self.feed
                    .disconnected("camera disconnected; waiting to reconnect");
            }
            if self.capture.is_none() {
                let protocol: &ProtocolObject<dyn AVCaptureVideoDataOutputSampleBufferDelegate> =
                    ProtocolObject::from_ref(&*self._delegate);
                match CameraCapture::start(&self.request, protocol, &self._queue) {
                    Ok(capture) => {
                        self.feed.reconnected();
                        self.capture = Some(capture);
                    }
                    Err(error) => {
                        let message = format!("camera unavailable: {error}");
                        if matches!(error, Error::PermissionDenied { .. }) {
                            self.feed.permission_denied(message);
                        } else {
                            self.feed.disconnected(message);
                        }
                    }
                }
            }
        }
        self.feed.preview(self.started.elapsed())
    }

    fn update_settings(&mut self, settings: CameraSettings) -> Result<()> {
        self.feed.update_settings(settings)
    }

    fn stop(mut self: Box<Self>) {
        if let Some(capture) = self.capture.take() {
            capture.stop();
        }
        self.feed.stop();
    }
}

impl Drop for MacCameraPreview {
    fn drop(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.stop();
        }
        self.feed.stop();
    }
}

pub(crate) fn start_preview(request: &CameraRequest) -> Result<Box<dyn CameraPreviewSession>> {
    let feed = CameraFeed::new(request)?;
    let started = Instant::now();
    let delegate = PreviewDelegate::new(feed.clone(), started);
    let protocol: &ProtocolObject<dyn AVCaptureVideoDataOutputSampleBufferDelegate> =
        ProtocolObject::from_ref(&*delegate);
    let queue = DispatchQueue::new("com.thatcube.scrozz.camera-preview", None);
    let capture = CameraCapture::start(request, protocol, &queue)?;
    feed.activate();
    Ok(Box::new(MacCameraPreview {
        capture: Some(capture),
        feed,
        started,
        request: request.clone(),
        next_reconnect: Instant::now() + Duration::from_secs(1),
        _delegate: delegate,
        _queue: queue,
    }))
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = PreviewDelegateIvars]
    struct PreviewDelegate;

    unsafe impl NSObjectProtocol for PreviewDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for PreviewDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn captureOutput_didOutputSampleBuffer_fromConnection(
            &self,
            _output: &objc2_av_foundation::AVCaptureOutput,
            sample_buffer: &objc2_core_media::CMSampleBuffer,
            _connection: &objc2_av_foundation::AVCaptureConnection,
        ) {
            match copy_frame(sample_buffer, self.ivars().started.elapsed()) {
                Ok(frame) => {
                    if let Err(error) = self.ivars().feed.push(frame) {
                        self.ivars().feed.warn(error.to_string());
                    }
                }
                Err(error) => self.ivars().feed.warn(error.to_string()),
            }
        }

        #[unsafe(method(captureOutput:didDropSampleBuffer:fromConnection:))]
        unsafe fn captureOutput_didDropSampleBuffer_fromConnection(
            &self,
            _output: &objc2_av_foundation::AVCaptureOutput,
            _sample_buffer: &objc2_core_media::CMSampleBuffer,
            _connection: &objc2_av_foundation::AVCaptureConnection,
        ) {
            self.ivars().feed.note_drop();
        }
    }
);

struct PreviewDelegateIvars {
    feed: CameraFeed,
    started: Instant,
}

impl PreviewDelegate {
    fn new(feed: CameraFeed, started: Instant) -> Retained<Self> {
        let allocated = Self::alloc().set_ivars(PreviewDelegateIvars { feed, started });
        unsafe { msg_send![super(allocated), init] }
    }
}

pub(crate) fn permission_status() -> CameraPermission {
    super::permission::camera_status()
}

#[allow(deprecated)]
pub(crate) fn devices() -> Result<Vec<CameraDevice>> {
    let media_type = video_media_type()?;
    let default = unsafe { AVCaptureDevice::defaultDeviceWithMediaType(media_type) }
        .map(|device| unsafe { device.uniqueID() }.to_string());
    let devices = unsafe { AVCaptureDevice::devicesWithMediaType(media_type) };
    devices
        .iter()
        .map(|device| {
            let id = unsafe { device.uniqueID() }.to_string();
            let state = if !unsafe { device.isConnected() } {
                CameraDeviceState::Disconnected
            } else if unsafe { device.isInUseByAnotherApplication() } {
                CameraDeviceState::Busy
            } else {
                CameraDeviceState::Available
            };
            Ok(CameraDevice {
                id: CameraDeviceId::new(id.clone())?,
                name: unsafe { device.localizedName() }.to_string(),
                state,
                is_default: default.as_deref() == Some(id.as_str()),
            })
        })
        .collect()
}

fn selected_device(id: Option<&CameraDeviceId>) -> Result<Retained<AVCaptureDevice>> {
    let media_type = video_media_type()?;
    if let Some(id) = id {
        let id = NSString::from_str(id.as_str());
        return unsafe { AVCaptureDevice::deviceWithUniqueID(&id) }.ok_or_else(|| {
            Error::TargetGone("the selected camera is not currently connected".to_owned())
        });
    }
    unsafe { AVCaptureDevice::defaultDeviceWithMediaType(media_type) }.ok_or_else(|| {
        Error::Unsupported {
            what: "camera capture".to_owned(),
            why: "no video capture device is available".to_owned(),
        }
    })
}

fn video_media_type() -> Result<&'static objc2_av_foundation::AVMediaType> {
    unsafe { AVMediaTypeVideo }.ok_or_else(|| Error::Unsupported {
        what: "camera capture".to_owned(),
        why: "AVFoundation did not expose the video media type".to_owned(),
    })
}

pub(crate) fn copy_frame(
    sample: &objc2_core_media::CMSampleBuffer,
    captured_at: std::time::Duration,
) -> Result<CameraFrame> {
    let image = unsafe { sample.image_buffer() }
        .ok_or_else(|| Error::Platform("camera sample contained no image buffer".to_owned()))?;
    if CVPixelBufferGetPixelFormatType(&image) != kCVPixelFormatType_32BGRA {
        return Err(Error::Unsupported {
            what: "macOS camera pixel format".to_owned(),
            why: "AVFoundation did not honor the requested BGRA camera format".to_owned(),
        });
    }
    let width = u32::try_from(CVPixelBufferGetWidth(&image))
        .map_err(|_| Error::Platform("camera width exceeds u32".to_owned()))?;
    let height = u32::try_from(CVPixelBufferGetHeight(&image))
        .map_err(|_| Error::Platform("camera height exceeds u32".to_owned()))?;
    let source_stride = CVPixelBufferGetBytesPerRow(&image);
    let row_bytes = width as usize * 4;
    if width == 0 || height == 0 || source_stride < row_bytes {
        return Err(Error::Platform(
            "camera returned invalid BGRA geometry".to_owned(),
        ));
    }
    let flags = CVPixelBufferLockFlags::ReadOnly;
    let status = unsafe { CVPixelBufferLockBaseAddress(&image, flags) };
    if status != 0 {
        return Err(Error::Platform(format!(
            "locking the camera pixel buffer failed with status {status}"
        )));
    }
    let copied = (|| {
        let base = CVPixelBufferGetBaseAddress(&image).cast::<u8>();
        if base.is_null() {
            return Err(Error::Platform(
                "camera pixel buffer had no readable base address".to_owned(),
            ));
        }
        let mut data = Vec::with_capacity(row_bytes * height as usize);
        for row in 0..height as usize {
            let source =
                unsafe { std::slice::from_raw_parts(base.add(row * source_stride), row_bytes) };
            data.extend_from_slice(source);
        }
        CameraFrame::new(
            Frame {
                data,
                size: PhysicalSize::new(f64::from(width), f64::from(height)),
                stride: row_bytes,
                format: scrozz_core::PixelFormat::Bgra8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            captured_at,
            CameraOrientation::Upright,
        )
    })();
    let unlock = unsafe { CVPixelBufferUnlockBaseAddress(&image, flags) };
    if unlock != 0 {
        return Err(Error::Platform(format!(
            "unlocking the camera pixel buffer failed with status {unlock}"
        )));
    }
    copied
}

pub(crate) fn composite(
    sample: &objc2_core_media::CMSampleBuffer,
    camera: Option<&CameraFrame>,
    settings: CameraSettings,
    compositor: &mut CameraCompositor,
) -> Result<Option<crate::overlay::CameraLayout>> {
    let image = unsafe { sample.image_buffer() }
        .ok_or_else(|| Error::Codec("camera composition source had no pixel buffer".to_owned()))?;
    if CVPixelBufferGetPixelFormatType(&image) != kCVPixelFormatType_32BGRA {
        return Err(Error::Codec(
            "camera composition requires BGRA screen frames".to_owned(),
        ));
    }
    let width = u32::try_from(CVPixelBufferGetWidth(&image))
        .map_err(|_| Error::Codec("camera composition width exceeds u32".to_owned()))?;
    let height = u32::try_from(CVPixelBufferGetHeight(&image))
        .map_err(|_| Error::Codec("camera composition height exceeds u32".to_owned()))?;
    let stride = CVPixelBufferGetBytesPerRow(&image);
    let required = stride
        .checked_mul(height as usize)
        .ok_or_else(|| Error::Codec("camera composition buffer size overflowed".to_owned()))?;
    let status = unsafe { CVPixelBufferLockBaseAddress(&image, CVPixelBufferLockFlags::empty()) };
    if status != 0 {
        return Err(Error::Codec(format!(
            "locking the camera composition buffer failed with status {status}"
        )));
    }
    let result = (|| {
        let base = CVPixelBufferGetBaseAddress(&image).cast::<u8>();
        if base.is_null() {
            return Err(Error::Codec(
                "camera composition buffer had no writable base address".to_owned(),
            ));
        }
        let destination = unsafe { std::slice::from_raw_parts_mut(base, required) };
        compositor.compose_optional(destination, width, height, stride, camera, settings)
    })();
    let unlock = unsafe { CVPixelBufferUnlockBaseAddress(&image, CVPixelBufferLockFlags::empty()) };
    if unlock != 0 {
        return Err(Error::Codec(format!(
            "unlocking the camera composition buffer failed with status {unlock}"
        )));
    }
    result
}
