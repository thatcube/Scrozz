//! Camera devices, bounded frame delivery, and deterministic composition.
//!
//! Native adapters own permissions and hardware handles. This module owns the
//! values that cross those boundaries and the renderer shared by preview and
//! recording, so the image shown to the user is the image written to disk.

use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use scrozz_core::{
    ColorSpace, Error, Frame, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize, Result,
};

use crate::{
    overlay::{CameraCrop, CameraLayout, CameraLayoutMode, layout_camera},
    settings::{CameraSettings, CameraShape},
};

/// Maximum camera frames retained between native capture and composition.
pub const MAX_QUEUED_CAMERA_FRAMES: usize = 3;
/// A camera frame older than this is not reused for a recording frame.
pub const MAX_CAMERA_FRAME_AGE: Duration = Duration::from_millis(500);
/// Default safe-area inset, as a fraction of the shorter output edge.
pub const CAMERA_SAFE_AREA_FRACTION: f64 = 0.025;

/// Bounded producer/consumer slot that always retains the newest native frames.
pub(crate) struct LatestFrameQueue<T> {
    frames: Mutex<VecDeque<T>>,
}

impl<T> LatestFrameQueue<T> {
    pub(crate) fn new() -> Self {
        Self {
            frames: Mutex::new(VecDeque::with_capacity(MAX_QUEUED_CAMERA_FRAMES)),
        }
    }

    /// Adds a frame and reports whether the oldest queued frame was evicted.
    pub(crate) fn push(&self, frame: T) -> bool {
        let mut frames = lock(&self.frames);
        let evicted = frames.len() == MAX_QUEUED_CAMERA_FRAMES;
        if evicted {
            frames.pop_front();
        }
        frames.push_back(frame);
        evicted
    }

    /// Takes the newest frame and reports older queued frames it superseded.
    pub(crate) fn take_latest(&self) -> (Option<T>, usize) {
        let mut frames = lock(&self.frames);
        let latest = frames.pop_back();
        let superseded = frames.len();
        frames.clear();
        (latest, superseded)
    }

    /// Drops every queued native frame and returns the number discarded.
    pub(crate) fn clear(&self) -> usize {
        let mut frames = lock(&self.frames);
        let discarded = frames.len();
        frames.clear();
        discarded
    }
}

/// Persistent platform camera identity.
///
/// This is the stable identifier reported by AVFoundation, Media Foundation, or
/// V4L2. Native device handles never leave their adapter and are never persisted.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CameraDeviceId(String);

impl CameraDeviceId {
    /// Creates a validated stable device identifier.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an empty, oversized, or NUL-bearing
    /// identifier.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.trim().is_empty() || value.len() > 1_024 || value.contains('\0') {
            return Err(Error::InvalidRequest(
                "camera device id must be non-empty, at most 1024 bytes, and contain no NUL"
                    .to_owned(),
            ));
        }
        Ok(Self(value))
    }

    /// String form suitable for persisted preferences.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CameraDeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CameraDeviceId(<redacted>)")
    }
}

/// Current availability of an enumerated camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraDeviceState {
    /// Connected and not known to be busy.
    Available,
    /// Connected but currently used by another application.
    Busy,
    /// The operating system denied access to the device.
    PermissionDenied,
    /// The remembered device is not currently connected.
    Disconnected,
}

/// A camera choice exposed to settings UI.
#[derive(Clone, PartialEq, Eq)]
pub struct CameraDevice {
    /// Stable identifier safe to persist as a preference.
    pub id: CameraDeviceId,
    /// Human-readable platform name.
    pub name: String,
    /// Live availability observed during enumeration.
    pub state: CameraDeviceState,
    /// Whether the platform currently chooses this device by default.
    pub is_default: bool,
}

impl fmt::Debug for CameraDevice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CameraDevice")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("state", &self.state)
            .field("is_default", &self.is_default)
            .finish()
    }
}

/// Camera authorization state without prompting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraPermission {
    /// The first explicit camera action may ask the operating system.
    NotDetermined,
    /// Camera access is currently granted.
    Authorized,
    /// The user denied camera access.
    Denied,
    /// Device policy prevents camera access.
    Restricted,
    /// This build has no native camera adapter.
    Unsupported,
}

/// Sensor orientation applied before crop and mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CameraOrientation {
    /// No rotation.
    #[default]
    Upright,
    /// Rotate 90 degrees clockwise.
    Clockwise90,
    /// Rotate 180 degrees.
    UpsideDown,
    /// Rotate 270 degrees clockwise.
    Clockwise270,
}

impl CameraOrientation {
    /// Converts native clockwise rotation metadata into a compositor orientation.
    #[must_use]
    pub const fn from_clockwise_degrees(degrees: u32) -> Option<Self> {
        match degrees % 360 {
            0 => Some(Self::Upright),
            90 => Some(Self::Clockwise90),
            180 => Some(Self::UpsideDown),
            270 => Some(Self::Clockwise270),
            _ => None,
        }
    }
}

/// Camera selection and initial composition carried by a recording request.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraRequest {
    /// Stable preferred device, or `None` for the platform default.
    pub device_id: Option<CameraDeviceId>,
    /// Composition settings. `enabled` must be true while this request exists.
    pub settings: CameraSettings,
}

impl CameraRequest {
    /// Uses the platform default camera with the supplied composition.
    #[must_use]
    pub const fn new(settings: CameraSettings) -> Self {
        Self {
            device_id: None,
            settings,
        }
    }

    /// Selects a specific persistent device identifier.
    #[must_use]
    pub fn with_device(mut self, device_id: CameraDeviceId) -> Self {
        self.device_id = Some(device_id);
        self
    }

    /// Validates the camera request.
    pub fn validate(&self) -> Result<()> {
        if !self.settings.enabled {
            return Err(Error::InvalidRequest(
                "a camera request must have camera composition enabled".to_owned(),
            ));
        }
        self.settings.validate().map(|_| ())
    }
}

/// One camera frame on the pause-free recording clock.
#[derive(Clone)]
pub struct CameraFrame {
    /// Pixel data. Native adapters publish packed RGBA/BGRA frames only.
    pub pixels: Arc<Frame>,
    /// Timestamp normalized to active recording time.
    pub captured_at: Duration,
    /// Sensor orientation to apply before crop and mirror.
    pub orientation: CameraOrientation,
}

impl fmt::Debug for CameraFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CameraFrame")
            .field("width", &self.pixels.width())
            .field("height", &self.pixels.height())
            .field("captured_at", &self.captured_at)
            .field("orientation", &self.orientation)
            .finish_non_exhaustive()
    }
}

impl CameraFrame {
    /// Creates a validated frame without exposing its bytes through `Debug`.
    pub fn new(
        pixels: Frame,
        captured_at: Duration,
        orientation: CameraOrientation,
    ) -> Result<Self> {
        if !pixels.is_well_formed() {
            return Err(Error::InvalidRequest(
                "camera frame storage does not match its geometry".to_owned(),
            ));
        }
        Ok(Self {
            pixels: Arc::new(pixels),
            captured_at,
            orientation,
        })
    }

    /// Display aspect after applying orientation.
    #[must_use]
    pub fn oriented_aspect(&self) -> f64 {
        match self.orientation {
            CameraOrientation::Upright | CameraOrientation::UpsideDown => {
                f64::from(self.pixels.width()) / f64::from(self.pixels.height())
            }
            CameraOrientation::Clockwise90 | CameraOrientation::Clockwise270 => {
                f64::from(self.pixels.height()) / f64::from(self.pixels.width())
            }
        }
    }
}

/// Pixel-free camera diagnostics safe for UI and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraRuntimeStatus {
    /// Camera session owns a native device.
    pub active: bool,
    /// Scrozz's in-app privacy indicator must be visible.
    pub privacy_indicator_visible: bool,
    /// Current selected-device state.
    pub device_state: CameraDeviceState,
    /// Number of valid native frames observed.
    pub frames_received: u64,
    /// Frames evicted or rejected under bounded backpressure.
    pub dropped_frames: u64,
    /// Frames currently retained.
    pub queued_frames: usize,
    /// Most recent recoverable device message.
    pub warning: Option<String>,
}

/// Latest camera frame and the exact composition settings applied to it.
#[derive(Clone)]
pub struct CameraPreview {
    /// Latest bounded camera frame.
    pub frame: CameraFrame,
    /// Live composition settings.
    pub settings: CameraSettings,
    /// Pixel-free capture status.
    pub status: CameraRuntimeStatus,
    /// Encoded output aspect for presenter-crop parity during recording.
    pub output_aspect: Option<f64>,
}

/// Explicit camera-only preview session used by settings.
pub trait CameraPreviewSession: Send {
    /// Pixel-free runtime state.
    fn status(&self) -> CameraRuntimeStatus;
    /// Latest preview frame, if one has arrived.
    fn poll(&mut self) -> Option<CameraPreview>;
    /// Updates preview composition without reopening the device.
    fn update_settings(&mut self, settings: CameraSettings) -> Result<()>;
    /// Stops capture and releases the native device.
    fn stop(self: Box<Self>);
}

impl fmt::Debug for CameraPreview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CameraPreview")
            .field("frame", &self.frame)
            .field("settings", &self.settings)
            .field("status", &self.status)
            .field("output_aspect", &self.output_aspect)
            .finish()
    }
}

/// Camera composition retained with recording metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraRecordingMetadata {
    /// Whether presenter mode was active at finalization.
    pub presenter: bool,
    /// Whether presenter mode retained a screen inset.
    pub presenter_screen: bool,
    /// Final camera mask.
    pub shape: CameraShape,
    /// Whether final camera pixels were mirrored.
    pub mirrored: bool,
    /// Camera frames dropped under bounded backpressure.
    pub dropped_frames: u64,
}

impl CameraRecordingMetadata {
    /// Captures pixel-free composition metadata.
    #[must_use]
    pub fn from_runtime(settings: CameraSettings, status: &CameraRuntimeStatus) -> Self {
        Self {
            presenter: settings.presenter,
            presenter_screen: settings.presenter_screen,
            shape: if settings.presenter {
                CameraShape::Rectangle
            } else {
                settings.shape
            },
            mirrored: settings.mirror,
            dropped_frames: status.dropped_frames,
        }
    }
}

impl Default for CameraRuntimeStatus {
    fn default() -> Self {
        Self {
            active: false,
            privacy_indicator_visible: false,
            device_state: CameraDeviceState::Disconnected,
            frames_received: 0,
            dropped_frames: 0,
            queued_frames: 0,
            warning: None,
        }
    }
}

#[derive(Debug)]
struct CameraRuntime {
    settings: CameraSettings,
    frames: VecDeque<CameraFrame>,
    status: CameraRuntimeStatus,
    last_timestamp: Option<Duration>,
    output_aspect: Option<f64>,
}

/// Thread-safe camera state shared by a native callback and video compositor.
#[derive(Clone, Debug)]
pub struct CameraFeed {
    inner: Arc<Mutex<CameraRuntime>>,
}

impl CameraFeed {
    /// Creates an inactive feed for a validated camera request.
    pub fn new(request: &CameraRequest) -> Result<Self> {
        request.validate()?;
        Ok(Self {
            inner: Arc::new(Mutex::new(CameraRuntime {
                settings: request.settings,
                frames: VecDeque::with_capacity(MAX_QUEUED_CAMERA_FRAMES),
                status: CameraRuntimeStatus::default(),
                last_timestamp: None,
                output_aspect: None,
            })),
        })
    }

    /// Marks the native device live. Call only after capture actually starts.
    pub fn activate(&self) {
        let mut inner = lock(&self.inner);
        inner.status.active = true;
        inner.status.privacy_indicator_visible = true;
        inner.status.device_state = CameraDeviceState::Available;
        inner.status.warning = None;
    }

    /// Publishes one frame, evicting the oldest at the strict memory bound.
    ///
    /// # Errors
    ///
    /// Rejects malformed or backwards timestamps. A disconnected/inactive feed
    /// never accepts pixels.
    pub fn push(&self, frame: CameraFrame) -> Result<bool> {
        let mut inner = lock(&self.inner);
        if !inner.status.active || inner.status.device_state != CameraDeviceState::Available {
            inner.status.dropped_frames = inner.status.dropped_frames.saturating_add(1);
            return Ok(false);
        }
        if inner
            .last_timestamp
            .is_some_and(|timestamp| frame.captured_at < timestamp)
        {
            inner.status.dropped_frames = inner.status.dropped_frames.saturating_add(1);
            return Err(Error::InvalidRequest(
                "camera timestamps must not move backwards on the recording clock".to_owned(),
            ));
        }
        inner.last_timestamp = Some(frame.captured_at);
        inner.status.frames_received = inner.status.frames_received.saturating_add(1);
        if inner.frames.len() == MAX_QUEUED_CAMERA_FRAMES {
            inner.frames.pop_front();
            inner.status.dropped_frames = inner.status.dropped_frames.saturating_add(1);
        }
        inner.frames.push_back(frame);
        inner.status.queued_frames = inner.frames.len();
        Ok(true)
    }

    /// Returns the newest non-future frame close enough to the requested time.
    #[must_use]
    pub fn frame_for(&self, at: Duration) -> Option<CameraFrame> {
        let mut inner = lock(&self.inner);
        let frame = inner
            .frames
            .iter()
            .rev()
            .find(|frame| {
                frame.captured_at <= at
                    && at.saturating_sub(frame.captured_at) <= MAX_CAMERA_FRAME_AGE
            })
            .cloned()?;
        while inner
            .frames
            .front()
            .is_some_and(|queued| queued.captured_at < frame.captured_at)
        {
            inner.frames.pop_front();
        }
        inner.status.queued_frames = inner.frames.len();
        Some(frame)
    }

    /// Current composition settings.
    #[must_use]
    pub fn settings(&self) -> CameraSettings {
        lock(&self.inner).settings
    }

    /// Supplies the encoded canvas aspect used by presenter-mode previews.
    pub fn set_output_size(&self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidRequest(
                "camera output dimensions must be non-zero".to_owned(),
            ));
        }
        lock(&self.inner).output_aspect = Some(f64::from(width) / f64::from(height));
        Ok(())
    }

    /// Replaces composition geometry without restarting camera capture.
    pub fn update_settings(&self, settings: CameraSettings) -> Result<()> {
        settings.validate()?;
        if !settings.enabled {
            return Err(Error::InvalidRequest(
                "disable camera capture by stopping its native session, not by hiding its indicator"
                    .to_owned(),
            ));
        }
        lock(&self.inner).settings = settings;
        Ok(())
    }

    /// Marks a temporary disconnect and releases every retained frame.
    pub fn disconnected(&self, message: impl Into<String>) {
        self.unavailable(CameraDeviceState::Disconnected, message);
    }

    /// Marks a runtime permission revocation and releases retained frames.
    pub fn permission_denied(&self, message: impl Into<String>) {
        self.unavailable(CameraDeviceState::PermissionDenied, message);
    }

    fn unavailable(&self, state: CameraDeviceState, message: impl Into<String>) {
        let mut inner = lock(&self.inner);
        inner.frames.clear();
        inner.last_timestamp = None;
        inner.status.active = false;
        inner.status.privacy_indicator_visible = false;
        inner.status.device_state = state;
        inner.status.queued_frames = 0;
        inner.status.warning = Some(message.into());
    }

    /// Marks a successful reconnect using the same stable preference.
    pub fn reconnected(&self) {
        self.activate();
    }

    /// Accounts for a native frame dropped before pixels reached the queue.
    pub fn note_drop(&self) {
        self.note_drops(1);
    }

    /// Accounts for several native frames superseded before composition.
    pub fn note_drops(&self, count: usize) {
        let mut inner = lock(&self.inner);
        inner.status.dropped_frames = inner.status.dropped_frames.saturating_add(count as u64);
    }

    /// Releases queued pixels while keeping the native session and indicator live.
    pub fn clear_frames(&self) {
        let mut inner = lock(&self.inner);
        inner.frames.clear();
        inner.last_timestamp = None;
        inner.status.queued_frames = 0;
    }

    /// Publishes a recoverable camera warning without exposing pixels or handles.
    pub fn warn(&self, message: impl Into<String>) {
        lock(&self.inner).status.warning = Some(message.into());
    }

    /// Stops capture and immediately clears the privacy indicator and pixels.
    pub fn stop(&self) {
        let mut inner = lock(&self.inner);
        inner.frames.clear();
        inner.last_timestamp = None;
        inner.status.active = false;
        inner.status.privacy_indicator_visible = false;
        inner.status.device_state = CameraDeviceState::Disconnected;
        inner.status.queued_frames = 0;
    }

    /// Pixel-free runtime snapshot.
    #[must_use]
    pub fn status(&self) -> CameraRuntimeStatus {
        lock(&self.inner).status.clone()
    }

    /// Returns the latest usable frame with its live composition state.
    #[must_use]
    pub fn preview(&self, at: Duration) -> Option<CameraPreview> {
        let mut inner = lock(&self.inner);
        let frame = inner
            .frames
            .iter()
            .rev()
            .find(|frame| {
                frame.captured_at <= at
                    && at.saturating_sub(frame.captured_at) <= MAX_CAMERA_FRAME_AGE
            })?
            .clone();
        while inner
            .frames
            .front()
            .is_some_and(|queued| queued.captured_at < frame.captured_at)
        {
            inner.frames.pop_front();
        }
        inner.status.queued_frames = inner.frames.len();
        Some(CameraPreview {
            frame,
            settings: inner.settings,
            status: inner.status.clone(),
            output_aspect: inner.output_aspect,
        })
    }
}

/// Renders the camera portion of a composition for a live UI preview.
pub fn render_camera_preview(
    camera: &CameraFrame,
    width: u32,
    height: u32,
    settings: CameraSettings,
) -> Result<Frame> {
    if width == 0 || height == 0 {
        return Err(Error::InvalidRequest(
            "camera preview dimensions must be non-zero".to_owned(),
        ));
    }
    let stride = width as usize * 4;
    let mut data = vec![0_u8; stride * height as usize];
    let rect = LogicalRect::new(
        LogicalPoint::new(0.0, 0.0),
        LogicalSize::new(f64::from(width), f64::from(height)),
    );
    let shape = if settings.presenter {
        CameraShape::Rectangle
    } else {
        settings.shape
    };
    let crop = if shape.is_square() {
        CameraCrop::CenterSquare
    } else {
        CameraCrop::FillOutput
    };
    let settings = CameraSettings { shape, ..settings };
    draw_camera(
        &mut data, width, height, stride, camera, rect, crop, settings,
    )?;
    Ok(Frame {
        data,
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride,
        format: scrozz_core::PixelFormat::Bgra8,
        color_space: ColorSpace::Srgb,
        scale: scrozz_core::ScaleFactor::IDENTITY,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reusable CPU compositor for native BGRA output buffers.
#[derive(Default)]
pub(crate) struct CameraCompositor {
    screen_scratch: Vec<u8>,
}

impl fmt::Debug for CameraCompositor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CameraCompositor")
            .field("screen_scratch", &"<redacted pixels>")
            .finish()
    }
}

impl CameraCompositor {
    /// Draws a camera frame when available, or protects a camera-only presenter
    /// canvas from exposing the captured screen while the camera is unavailable.
    pub(crate) fn compose_optional(
        &mut self,
        destination: &mut [u8],
        width: u32,
        height: u32,
        stride: usize,
        camera: Option<&CameraFrame>,
        settings: CameraSettings,
    ) -> Result<Option<CameraLayout>> {
        if let Some(camera) = camera {
            return self
                .compose(destination, width, height, stride, camera, settings)
                .map(Some);
        }
        if settings.presenter {
            let required = validate_destination(destination, width, height, stride)?;
            self.screen_scratch.clear();
            if settings.presenter_screen {
                self.screen_scratch
                    .extend_from_slice(&destination[..required]);
            }
            for row in destination.chunks_exact_mut(stride).take(height as usize) {
                for pixel in row[..width as usize * 4].as_chunks_mut::<4>().0 {
                    pixel.copy_from_slice(&[0, 0, 0, 255]);
                }
            }
            let output = LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(f64::from(width), f64::from(height)),
            );
            let margin = f64::from(width.min(height)) * CAMERA_SAFE_AREA_FRACTION;
            let layout = layout_camera(output, 1.0, margin, settings)?
                .expect("presenter camera settings are enabled");
            if let Some(screen) = layout.screen {
                draw_shadow(destination, width, height, stride, screen, settings.shadow);
                draw_scaled_bgra(
                    destination,
                    width,
                    height,
                    stride,
                    &self.screen_scratch,
                    width,
                    height,
                    stride,
                    screen,
                    false,
                    CameraShape::Rounded,
                    settings.border,
                );
            }
            return Ok(Some(layout));
        }
        Ok(None)
    }

    /// Draws the camera composition into one BGRA screen buffer.
    pub(crate) fn compose(
        &mut self,
        destination: &mut [u8],
        width: u32,
        height: u32,
        stride: usize,
        camera: &CameraFrame,
        settings: CameraSettings,
    ) -> Result<CameraLayout> {
        let required = validate_destination(destination, width, height, stride)?;
        let output = LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(f64::from(width), f64::from(height)),
        );
        let margin = f64::from(width.min(height)) * CAMERA_SAFE_AREA_FRACTION;
        let layout = layout_camera(output, camera.oriented_aspect(), margin, settings)?
            .ok_or_else(|| Error::InvalidRequest("camera composition is disabled".to_owned()))?;

        if layout.mode == CameraLayoutMode::Presenter {
            self.screen_scratch.clear();
            if layout.screen.is_some() {
                self.screen_scratch
                    .extend_from_slice(&destination[..required]);
            }
            for row in destination.chunks_exact_mut(stride).take(height as usize) {
                for pixel in row[..width as usize * 4].as_chunks_mut::<4>().0 {
                    pixel.copy_from_slice(&[0, 0, 0, 255]);
                }
            }
            draw_camera(
                destination,
                width,
                height,
                stride,
                camera,
                layout.camera,
                layout.crop,
                CameraSettings {
                    shape: layout.shape,
                    ..settings
                },
            )?;
            if let Some(screen) = layout.screen {
                draw_shadow(destination, width, height, stride, screen, settings.shadow);
                draw_scaled_bgra(
                    destination,
                    width,
                    height,
                    stride,
                    &self.screen_scratch,
                    width,
                    height,
                    stride,
                    screen,
                    false,
                    CameraShape::Rounded,
                    settings.border,
                );
            }
        } else {
            draw_shadow(
                destination,
                width,
                height,
                stride,
                layout.camera,
                settings.shadow,
            );
            draw_camera(
                destination,
                width,
                height,
                stride,
                camera,
                layout.camera,
                layout.crop,
                CameraSettings {
                    shape: layout.shape,
                    ..settings
                },
            )?;
        }
        Ok(layout)
    }
}

fn validate_destination(
    destination: &[u8],
    width: u32,
    height: u32,
    stride: usize,
) -> Result<usize> {
    let row_bytes = width as usize * 4;
    let required = stride.checked_mul(height as usize).ok_or_else(|| {
        Error::InvalidRequest("camera destination buffer size overflowed".to_owned())
    })?;
    if width == 0 || height == 0 || stride < row_bytes || destination.len() < required {
        return Err(Error::InvalidRequest(
            "camera destination buffer does not match its geometry".to_owned(),
        ));
    }
    Ok(required)
}

#[allow(clippy::too_many_arguments)]
fn draw_camera(
    destination: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    camera: &CameraFrame,
    rect: LogicalRect,
    crop: CameraCrop,
    settings: CameraSettings,
) -> Result<()> {
    let source = &camera.pixels;
    if !source.is_well_formed() {
        return Err(Error::InvalidRequest(
            "camera frame became malformed before composition".to_owned(),
        ));
    }
    draw_source(
        destination,
        width,
        height,
        stride,
        source,
        camera.orientation,
        rect,
        crop,
        settings.mirror,
        settings.shape,
        settings.border,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn draw_source(
    destination: &mut [u8],
    destination_width: u32,
    destination_height: u32,
    destination_stride: usize,
    source: &Frame,
    orientation: CameraOrientation,
    rect: LogicalRect,
    crop: CameraCrop,
    mirror: bool,
    shape: CameraShape,
    border: bool,
) {
    let left = rect.origin.x.round().max(0.0) as u32;
    let top = rect.origin.y.round().max(0.0) as u32;
    let right = (rect.origin.x + rect.size.width)
        .round()
        .clamp(0.0, f64::from(destination_width)) as u32;
    let bottom = (rect.origin.y + rect.size.height)
        .round()
        .clamp(0.0, f64::from(destination_height)) as u32;
    let target_width = right.saturating_sub(left);
    let target_height = bottom.saturating_sub(top);
    if target_width == 0 || target_height == 0 {
        return;
    }

    for target_y in 0..target_height {
        for target_x in 0..target_width {
            let normalized_x = (f64::from(target_x) + 0.5) / f64::from(target_width);
            let normalized_y = (f64::from(target_y) + 0.5) / f64::from(target_height);
            let edge_distance = mask_edge_distance(normalized_x, normalized_y, shape);
            if edge_distance < 0.0 {
                continue;
            }
            let destination_offset =
                (top + target_y) as usize * destination_stride + (left + target_x) as usize * 4;
            if border && edge_distance < 0.018 {
                blend_bgra(
                    &mut destination[destination_offset..destination_offset + 4],
                    [245, 245, 245, 230],
                );
                continue;
            }
            let (sample_x, sample_y) = source_coordinate(
                normalized_x,
                normalized_y,
                source.width(),
                source.height(),
                orientation,
                crop,
                mirror,
                f64::from(target_width) / f64::from(target_height),
            );
            let rgba = source_pixel(source, sample_x, sample_y);
            blend_bgra(
                &mut destination[destination_offset..destination_offset + 4],
                rgba,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_scaled_bgra(
    destination: &mut [u8],
    destination_width: u32,
    destination_height: u32,
    destination_stride: usize,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    source_stride: usize,
    rect: LogicalRect,
    mirror: bool,
    shape: CameraShape,
    border: bool,
) {
    let left = rect.origin.x.round().max(0.0) as u32;
    let top = rect.origin.y.round().max(0.0) as u32;
    let right = (rect.origin.x + rect.size.width)
        .round()
        .clamp(0.0, f64::from(destination_width)) as u32;
    let bottom = (rect.origin.y + rect.size.height)
        .round()
        .clamp(0.0, f64::from(destination_height)) as u32;
    let target_width = right.saturating_sub(left);
    let target_height = bottom.saturating_sub(top);
    if target_width == 0 || target_height == 0 {
        return;
    }
    let source_aspect = f64::from(source_width) / f64::from(source_height);
    let destination_aspect = f64::from(target_width) / f64::from(target_height);
    for target_y in 0..target_height {
        for target_x in 0..target_width {
            let normalized_x = (f64::from(target_x) + 0.5) / f64::from(target_width);
            let normalized_y = (f64::from(target_y) + 0.5) / f64::from(target_height);
            let edge_distance = mask_edge_distance(normalized_x, normalized_y, shape);
            if edge_distance < 0.0 {
                continue;
            }
            let destination_offset =
                (top + target_y) as usize * destination_stride + (left + target_x) as usize * 4;
            if border && edge_distance < 0.018 {
                blend_bgra(
                    &mut destination[destination_offset..destination_offset + 4],
                    [245, 245, 245, 230],
                );
                continue;
            }
            let (mut source_x, source_y) = if source_aspect > destination_aspect {
                let visible = destination_aspect / source_aspect;
                ((1.0 - visible) * 0.5 + normalized_x * visible, normalized_y)
            } else {
                let visible = source_aspect / destination_aspect;
                (normalized_x, (1.0 - visible) * 0.5 + normalized_y * visible)
            };
            if mirror {
                source_x = 1.0 - source_x;
            }
            let source_x = (source_x * f64::from(source_width))
                .floor()
                .clamp(0.0, f64::from(source_width.saturating_sub(1)))
                as usize;
            let source_y = (source_y * f64::from(source_height))
                .floor()
                .clamp(0.0, f64::from(source_height.saturating_sub(1)))
                as usize;
            let source_offset = source_y * source_stride + source_x * 4;
            blend_bgra(
                &mut destination[destination_offset..destination_offset + 4],
                [
                    source[source_offset],
                    source[source_offset + 1],
                    source[source_offset + 2],
                    source[source_offset + 3],
                ],
            );
        }
    }
}

fn draw_shadow(
    destination: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    rect: LogicalRect,
    enabled: bool,
) {
    if !enabled {
        return;
    }
    let radius = (f64::from(width.min(height)) * 0.012).clamp(4.0, 24.0) as i32;
    let left = rect.origin.x.round() as i32;
    let top = rect.origin.y.round() as i32;
    let right = (rect.origin.x + rect.size.width).round() as i32;
    let bottom = (rect.origin.y + rect.size.height).round() as i32;
    for y in (top - radius).max(0)..(bottom + radius).min(height as i32) {
        for x in (left - radius).max(0)..(right + radius).min(width as i32) {
            if x >= left && x < right && y >= top && y < bottom {
                continue;
            }
            let dx = if x < left {
                left - x
            } else if x >= right {
                x - right + 1
            } else {
                0
            };
            let dy = if y < top {
                top - y
            } else if y >= bottom {
                y - bottom + 1
            } else {
                0
            };
            let distance = ((dx * dx + dy * dy) as f32).sqrt();
            if distance >= radius as f32 {
                continue;
            }
            let alpha = ((1.0 - distance / radius as f32) * 0.28 * 255.0) as u8;
            let offset = y as usize * stride + x as usize * 4;
            blend_bgra(&mut destination[offset..offset + 4], [0, 0, 0, alpha]);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn source_coordinate(
    normalized_x: f64,
    normalized_y: f64,
    source_width: u32,
    source_height: u32,
    orientation: CameraOrientation,
    crop: CameraCrop,
    mirror: bool,
    destination_aspect: f64,
) -> (u32, u32) {
    let (oriented_width, oriented_height) = match orientation {
        CameraOrientation::Upright | CameraOrientation::UpsideDown => (source_width, source_height),
        CameraOrientation::Clockwise90 | CameraOrientation::Clockwise270 => {
            (source_height, source_width)
        }
    };
    let source_aspect = f64::from(oriented_width) / f64::from(oriented_height);
    let (mut x, y) = match crop {
        CameraCrop::PreserveSourceAspect => (normalized_x, normalized_y),
        CameraCrop::CenterSquare => {
            if oriented_width > oriented_height {
                let visible = f64::from(oriented_height) / f64::from(oriented_width);
                ((1.0 - visible) * 0.5 + normalized_x * visible, normalized_y)
            } else {
                let visible = f64::from(oriented_width) / f64::from(oriented_height);
                (normalized_x, (1.0 - visible) * 0.5 + normalized_y * visible)
            }
        }
        CameraCrop::FillOutput if source_aspect > destination_aspect => {
            let visible = destination_aspect / source_aspect;
            ((1.0 - visible) * 0.5 + normalized_x * visible, normalized_y)
        }
        CameraCrop::FillOutput => {
            let visible = source_aspect / destination_aspect;
            (normalized_x, (1.0 - visible) * 0.5 + normalized_y * visible)
        }
    };
    if mirror {
        x = 1.0 - x;
    }
    let oriented_x = (x * f64::from(oriented_width))
        .floor()
        .clamp(0.0, f64::from(oriented_width.saturating_sub(1))) as u32;
    let oriented_y = (y * f64::from(oriented_height))
        .floor()
        .clamp(0.0, f64::from(oriented_height.saturating_sub(1))) as u32;
    match orientation {
        CameraOrientation::Upright => (oriented_x, oriented_y),
        CameraOrientation::Clockwise90 => (
            oriented_y,
            source_height.saturating_sub(oriented_x).saturating_sub(1),
        ),
        CameraOrientation::UpsideDown => (
            source_width.saturating_sub(oriented_x).saturating_sub(1),
            source_height.saturating_sub(oriented_y).saturating_sub(1),
        ),
        CameraOrientation::Clockwise270 => (
            source_width.saturating_sub(oriented_y).saturating_sub(1),
            oriented_x,
        ),
    }
}

fn mask_edge_distance(x: f64, y: f64, shape: CameraShape) -> f64 {
    match shape {
        CameraShape::Circle => 0.5 - ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt(),
        CameraShape::Rounded => {
            let radius = 0.10;
            let dx = (x - 0.5).abs() - (0.5 - radius);
            let dy = (y - 0.5).abs() - (0.5 - radius);
            radius - (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt()
        }
        CameraShape::Square | CameraShape::Rectangle => x.min(1.0 - x).min(y.min(1.0 - y)),
    }
}

fn source_pixel(source: &Frame, x: u32, y: u32) -> [u8; 4] {
    let offset = y as usize * source.stride + x as usize * 4;
    let pixel = &source.data[offset..offset + 4];
    let (red, green, blue, alpha) = match source.format {
        scrozz_core::PixelFormat::Rgba8 | scrozz_core::PixelFormat::RgbaPremultiplied8 => {
            (pixel[0], pixel[1], pixel[2], pixel[3])
        }
        scrozz_core::PixelFormat::Bgra8 | scrozz_core::PixelFormat::BgraPremultiplied8 => {
            (pixel[2], pixel[1], pixel[0], pixel[3])
        }
    };
    let (red, green, blue) = convert_to_srgb(red, green, blue, source.color_space);
    [blue, green, red, alpha]
}

fn convert_to_srgb(red: u8, green: u8, blue: u8, color_space: ColorSpace) -> (u8, u8, u8) {
    if matches!(color_space, ColorSpace::Srgb | ColorSpace::Unknown) {
        return (red, green, blue);
    }
    let linear = [
        srgb_to_linear(red),
        srgb_to_linear(green),
        srgb_to_linear(blue),
    ];
    let converted = match color_space {
        ColorSpace::DisplayP3 => [
            1.224_745 * linear[0] - 0.224_904 * linear[1],
            -0.042_058 * linear[0] + 1.042_081 * linear[1],
            -0.019_642 * linear[1] + 1.019_882 * linear[2],
        ],
        ColorSpace::Rec2020 => [
            1.660_491 * linear[0] - 0.587_641 * linear[1] - 0.072_850 * linear[2],
            -0.124_550 * linear[0] + 1.132_900 * linear[1] - 0.008_349 * linear[2],
            -0.018_151 * linear[0] - 0.100_579 * linear[1] + 1.118_730 * linear[2],
        ],
        ColorSpace::Srgb | ColorSpace::Unknown => linear,
    };
    (
        linear_to_srgb(converted[0]),
        linear_to_srgb(converted[1]),
        linear_to_srgb(converted[2]),
    )
}

fn srgb_to_linear(value: u8) -> f64 {
    let value = f64::from(value) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f64) -> u8 {
    let value = value.clamp(0.0, 1.0);
    let encoded = if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round() as u8
}

fn blend_bgra(destination: &mut [u8], source: [u8; 4]) {
    let alpha = f32::from(source[3]) / 255.0;
    let inverse = 1.0 - alpha;
    for channel in 0..3 {
        destination[channel] = (f32::from(source[channel]) * alpha
            + f32::from(destination[channel]) * inverse)
            .round() as u8;
    }
    destination[3] = 255;
}

#[cfg(test)]
mod tests {
    use scrozz_core::{PixelFormat, ScaleFactor};

    use super::*;
    use crate::settings::{CameraPlacement, OverlayAnchor};

    fn frame(width: u32, height: u32, pixels: Vec<u8>) -> Frame {
        Frame {
            data: pixels,
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride: width as usize * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::IDENTITY,
        }
    }

    #[test]
    fn queue_is_bounded_and_rejects_backwards_time() {
        let settings = CameraSettings {
            enabled: true,
            ..CameraSettings::default()
        };
        let feed = CameraFeed::new(&CameraRequest::new(settings)).unwrap();
        feed.set_output_size(1920, 1080).unwrap();
        feed.activate();
        for index in 0..8 {
            feed.push(
                CameraFrame::new(
                    frame(1, 1, vec![index, 0, 0, 255]),
                    Duration::from_millis(index as u64),
                    CameraOrientation::Upright,
                )
                .unwrap(),
            )
            .unwrap();
        }
        let status = feed.status();
        assert_eq!(status.queued_frames, MAX_QUEUED_CAMERA_FRAMES);
        assert_eq!(status.dropped_frames, 5);
        assert_eq!(
            feed.preview(Duration::from_millis(7))
                .unwrap()
                .output_aspect,
            Some(16.0 / 9.0)
        );
        assert!(
            feed.push(
                CameraFrame::new(
                    frame(1, 1, vec![0, 0, 0, 255]),
                    Duration::ZERO,
                    CameraOrientation::Upright,
                )
                .unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn native_queue_keeps_the_newest_frame_under_backpressure() {
        let queue = LatestFrameQueue::new();
        let mut evicted = 0;
        for frame in 0..6 {
            evicted += usize::from(queue.push(frame));
        }
        let (latest, superseded) = queue.take_latest();
        assert_eq!(latest, Some(5));
        assert_eq!(evicted, 3);
        assert_eq!(superseded, 2);
        assert_eq!(queue.take_latest(), (None, 0));
    }

    #[test]
    fn stop_clears_pixels_and_privacy_indicator_together() {
        let settings = CameraSettings {
            enabled: true,
            ..CameraSettings::default()
        };
        let feed = CameraFeed::new(&CameraRequest::new(settings)).unwrap();
        feed.activate();
        feed.push(
            CameraFrame::new(
                frame(1, 1, vec![255, 0, 0, 255]),
                Duration::ZERO,
                CameraOrientation::Upright,
            )
            .unwrap(),
        )
        .unwrap();
        feed.stop();
        let status = feed.status();
        assert!(!status.active);
        assert!(!status.privacy_indicator_visible);
        assert_eq!(status.queued_frames, 0);
        assert!(feed.frame_for(Duration::ZERO).is_none());
    }

    #[test]
    fn camera_only_presenter_never_falls_back_to_screen_pixels() {
        let settings = CameraSettings {
            enabled: true,
            presenter: true,
            presenter_screen: false,
            ..CameraSettings::default()
        };
        let mut output = vec![73_u8; 8 * 6 * 4];
        for alpha in output[3..].iter_mut().step_by(4) {
            *alpha = 255;
        }
        CameraCompositor::default()
            .compose_optional(&mut output, 8, 6, 32, None, settings)
            .unwrap();
        for pixel in output.as_chunks::<4>().0 {
            assert_eq!(*pixel, [0, 0, 0, 255]);
        }
    }

    #[test]
    fn presenter_metadata_records_the_effective_full_frame_shape() {
        let settings = CameraSettings {
            enabled: true,
            presenter: true,
            shape: CameraShape::Circle,
            ..CameraSettings::default()
        };
        let metadata =
            CameraRecordingMetadata::from_runtime(settings, &CameraRuntimeStatus::default());
        assert_eq!(metadata.shape, CameraShape::Rectangle);
    }

    #[test]
    fn presenter_camera_covers_screen_pixels_before_drawing_border() {
        let settings = CameraSettings {
            enabled: true,
            presenter: true,
            presenter_screen: false,
            border: true,
            ..CameraSettings::default()
        };
        let camera = CameraFrame::new(
            frame(1, 1, vec![200, 150, 100, 255]),
            Duration::ZERO,
            CameraOrientation::Upright,
        )
        .unwrap();
        let mut output = Vec::with_capacity(8 * 6 * 4);
        for _ in 0..8 * 6 {
            output.extend_from_slice(&[1, 2, 3, 255]);
        }
        CameraCompositor::default()
            .compose(&mut output, 8, 6, 32, &camera, settings)
            .unwrap();
        assert!(
            output
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[..3] != [1, 2, 3])
        );
    }

    #[test]
    fn disconnect_and_reconnect_keep_privacy_state_truthful() {
        let settings = CameraSettings {
            enabled: true,
            ..CameraSettings::default()
        };
        let feed = CameraFeed::new(&CameraRequest::new(settings)).unwrap();
        feed.activate();
        feed.disconnected("camera unplugged");
        let disconnected = feed.status();
        assert!(!disconnected.active);
        assert!(!disconnected.privacy_indicator_visible);
        assert_eq!(disconnected.device_state, CameraDeviceState::Disconnected);
        assert_eq!(disconnected.warning.as_deref(), Some("camera unplugged"));

        feed.reconnected();
        let reconnected = feed.status();
        assert!(reconnected.active);
        assert!(reconnected.privacy_indicator_visible);
        assert_eq!(reconnected.device_state, CameraDeviceState::Available);
        assert!(reconnected.warning.is_none());
    }

    #[test]
    fn permission_revocation_clears_pixels_and_reports_denial() {
        let settings = CameraSettings {
            enabled: true,
            ..CameraSettings::default()
        };
        let feed = CameraFeed::new(&CameraRequest::new(settings)).unwrap();
        feed.activate();
        feed.push(
            CameraFrame::new(
                frame(1, 1, vec![1, 2, 3, 255]),
                Duration::ZERO,
                CameraOrientation::Upright,
            )
            .unwrap(),
        )
        .unwrap();
        feed.permission_denied("camera permission was revoked");
        let status = feed.status();
        assert_eq!(status.device_state, CameraDeviceState::PermissionDenied);
        assert!(!status.active);
        assert!(!status.privacy_indicator_visible);
        assert_eq!(status.queued_frames, 0);
    }

    #[test]
    fn stale_camera_frames_are_not_reused() {
        let settings = CameraSettings {
            enabled: true,
            ..CameraSettings::default()
        };
        let feed = CameraFeed::new(&CameraRequest::new(settings)).unwrap();
        feed.activate();
        feed.push(
            CameraFrame::new(
                frame(1, 1, vec![255, 0, 0, 255]),
                Duration::ZERO,
                CameraOrientation::Upright,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            feed.frame_for(MAX_CAMERA_FRAME_AGE - Duration::from_millis(1))
                .is_some()
        );
        assert!(
            feed.frame_for(MAX_CAMERA_FRAME_AGE + Duration::from_millis(1))
                .is_none()
        );
    }

    #[test]
    fn mismatched_camera_fps_uses_latest_non_future_frame() {
        let settings = CameraSettings {
            enabled: true,
            ..CameraSettings::default()
        };
        let feed = CameraFeed::new(&CameraRequest::new(settings)).unwrap();
        feed.activate();
        for (millis, red) in [(0, 10), (16, 20), (33, 30)] {
            feed.push(
                CameraFrame::new(
                    frame(1, 1, vec![red, 0, 0, 255]),
                    Duration::from_millis(millis),
                    CameraOrientation::Upright,
                )
                .unwrap(),
            )
            .unwrap();
        }
        let at_24 = feed.frame_for(Duration::from_millis(24)).unwrap();
        assert_eq!(at_24.pixels.data[0], 20);
        let at_40 = feed.frame_for(Duration::from_millis(40)).unwrap();
        assert_eq!(at_40.pixels.data[0], 30);
        assert!(feed.status().queued_frames <= MAX_QUEUED_CAMERA_FRAMES);
    }

    #[test]
    fn composition_applies_mirror_shape_border_and_presenter_screen() {
        let pixels = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let camera = CameraFrame::new(
            frame(2, 2, pixels),
            Duration::ZERO,
            CameraOrientation::Upright,
        )
        .unwrap();
        let mut output = vec![20_u8; 64 * 64 * 4];
        for alpha in output[3..].iter_mut().step_by(4) {
            *alpha = 255;
        }
        let mut settings = CameraSettings {
            enabled: true,
            size: 0.5,
            position: OverlayAnchor::TopLeft,
            placement: Some(CameraPlacement::new(0.0, 0.0).unwrap()),
            border: true,
            shadow: true,
            ..CameraSettings::default()
        };
        let mut compositor = CameraCompositor::default();
        let pip = compositor
            .compose(&mut output, 64, 64, 256, &camera, settings)
            .unwrap();
        assert_eq!(pip.mode, CameraLayoutMode::PictureInPicture);
        assert!(output.iter().any(|byte| *byte != 20));

        settings.presenter = true;
        settings.presenter_screen = true;
        let presenter = compositor
            .compose(&mut output, 64, 64, 256, &camera, settings)
            .unwrap();
        assert_eq!(presenter.mode, CameraLayoutMode::Presenter);
        assert!(presenter.screen.is_some());
    }

    #[test]
    fn orientation_changes_the_effective_aspect() {
        let camera = CameraFrame::new(
            frame(4, 2, vec![0; 4 * 2 * 4]),
            Duration::ZERO,
            CameraOrientation::Clockwise90,
        )
        .unwrap();
        assert_eq!(camera.oriented_aspect(), 0.5);
        assert_eq!(
            CameraOrientation::from_clockwise_degrees(450),
            Some(CameraOrientation::Clockwise90)
        );
        assert_eq!(CameraOrientation::from_clockwise_degrees(45), None);
    }

    #[test]
    fn mirror_and_orientation_change_sample_mapping() {
        let camera = CameraFrame::new(
            frame(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 255]),
            Duration::ZERO,
            CameraOrientation::Upright,
        )
        .unwrap();
        let plain = CameraSettings {
            enabled: true,
            shape: CameraShape::Rectangle,
            mirror: false,
            border: false,
            shadow: false,
            ..CameraSettings::default()
        };
        let mirrored = CameraSettings {
            mirror: true,
            ..plain
        };
        let plain = render_camera_preview(&camera, 2, 1, plain).unwrap();
        let mirrored = render_camera_preview(&camera, 2, 1, mirrored).unwrap();
        assert_eq!(&plain.data[..4], &mirrored.data[4..8]);
        assert_eq!(&plain.data[4..8], &mirrored.data[..4]);
    }

    #[test]
    fn every_camera_shape_renders_with_valid_storage() {
        let camera = CameraFrame::new(
            frame(2, 2, vec![255; 16]),
            Duration::ZERO,
            CameraOrientation::Upright,
        )
        .unwrap();
        for shape in CameraShape::ALL {
            let output = render_camera_preview(
                &camera,
                32,
                32,
                CameraSettings {
                    enabled: true,
                    shape,
                    ..CameraSettings::default()
                },
            )
            .unwrap();
            assert!(output.is_well_formed(), "{shape:?}");
        }
    }

    #[test]
    fn presenter_preview_and_encoded_compositor_match() {
        let camera = CameraFrame::new(
            frame(
                2,
                2,
                vec![
                    255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
                ],
            ),
            Duration::ZERO,
            CameraOrientation::Upright,
        )
        .unwrap();
        let settings = CameraSettings {
            enabled: true,
            presenter: true,
            presenter_screen: false,
            mirror: true,
            border: false,
            shadow: false,
            ..CameraSettings::default()
        };
        let preview = render_camera_preview(&camera, 16, 10, settings).unwrap();
        let mut encoded = vec![0_u8; 16 * 10 * 4];
        for alpha in encoded[3..].iter_mut().step_by(4) {
            *alpha = 255;
        }
        CameraCompositor::default()
            .compose(&mut encoded, 16, 10, 16 * 4, &camera, settings)
            .unwrap();
        assert_eq!(preview.data, encoded);
    }

    #[test]
    fn debug_output_never_contains_camera_pixels_or_device_ids() {
        let id = CameraDeviceId::new("secret-device-id").unwrap();
        assert!(!format!("{id:?}").contains("secret-device-id"));
        let camera = CameraFrame::new(
            frame(1, 1, vec![17, 23, 42, 255]),
            Duration::ZERO,
            CameraOrientation::Upright,
        )
        .unwrap();
        let debug = format!("{camera:?}");
        assert!(!debug.contains("17, 23, 42"));
        assert!(debug.contains("width"));
        let compositor = CameraCompositor {
            screen_scratch: vec![17, 23, 42, 255],
        };
        let debug = format!("{compositor:?}");
        assert!(!debug.contains("17, 23, 42"));
        assert!(debug.contains("redacted pixels"));
    }
}
