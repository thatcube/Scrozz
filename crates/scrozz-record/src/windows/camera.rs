//! Media Foundation webcam capture with bounded latest-frame delivery.

use std::{
    ffi::c_void,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use scrozz_core::{ColorSpace, Error, Frame, PhysicalSize, PixelFormat, Result, ScaleFactor};
use windows::{
    Win32::{
        Foundation::E_ACCESSDENIED,
        Media::MediaFoundation::{
            IMF2DBuffer, IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSourceReader,
            MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_FRAME_RATE,
            MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
            MF_MT_VIDEO_ROTATION, MF_SOURCE_READER_ALL_STREAMS,
            MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
            MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
            MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED, MF_SOURCE_READERF_STREAMTICK,
            MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromMediaSource,
            MFEnumDeviceSources, MFMediaType_Video, MFVideoFormat_RGB32,
            MFVideoInterlace_Progressive,
        },
        System::Com::CoTaskMemFree,
    },
    core::{Interface, PWSTR},
};

use crate::{
    CameraDevice, CameraDeviceId, CameraDeviceState, CameraFrame, CameraOrientation, CameraPreview,
    CameraPreviewSession, CameraRequest, CameraRuntimeStatus,
    camera::{CameraFeed, LatestFrameQueue},
};

use super::{
    com::{Apartment, MediaFoundation},
    timing::qpc_to_hns,
};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);

pub struct CameraPacket {
    pub pixels: Frame,
    pub raw_hns: i64,
    pub orientation: CameraOrientation,
}

pub struct CameraCapture {
    frames: Arc<LatestFrameQueue<CameraPacket>>,
    warnings: Receiver<String>,
    stop: Arc<AtomicBool>,
    active: Arc<Mutex<Option<ActiveCapture>>>,
    worker: Option<JoinHandle<()>>,
}

unsafe impl Send for CameraCapture {}

struct ActiveCapture {
    source: IMFMediaSource,
    reader: IMFSourceReader,
}

// SAFETY: Media Foundation's source reader is documented as thread-safe. The
// only cross-thread calls are Flush/Shutdown used to unblock synchronous read
// during teardown; capture reads otherwise remain on the camera MTA thread.
unsafe impl Send for ActiveCapture {}
unsafe impl Sync for ActiveCapture {}

impl CameraCapture {
    pub fn start(request: CameraRequest, feed: CameraFeed) -> Result<Self> {
        Self::start_with_pause(request, feed, Arc::new(AtomicBool::new(false)))
    }

    pub fn start_with_pause(
        request: CameraRequest,
        feed: CameraFeed,
        paused: Arc<AtomicBool>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(Mutex::new(None));
        let frames = Arc::new(LatestFrameQueue::new());
        let (warnings_tx, warnings) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let thread_stop = Arc::clone(&stop);
        let thread_active = Arc::clone(&active);
        let thread_paused = Arc::clone(&paused);
        let thread_frames = Arc::clone(&frames);
        let worker = thread::Builder::new()
            .name("scrozz-camera-mf".to_owned())
            .spawn(move || {
                let _apartment = match Apartment::enter() {
                    Ok(apartment) => apartment,
                    Err(error) => {
                        let _ = started_tx.try_send(Err(error));
                        return;
                    }
                };
                let _media_foundation = match MediaFoundation::start() {
                    Ok(media_foundation) => media_foundation,
                    Err(error) => {
                        let _ = started_tx.try_send(Err(error));
                        return;
                    }
                };
                let mut first = true;
                while !thread_stop.load(Ordering::Acquire) {
                    match open_reader(request.device_id.as_ref(), request.settings.size) {
                        Ok((source, reader, dimensions, orientation)) => {
                            *lock(&thread_active) = Some(ActiveCapture {
                                source: source.clone(),
                                reader: reader.clone(),
                            });
                            if first {
                                let _ = started_tx.try_send(Ok(()));
                                first = false;
                            } else {
                                feed.reconnected();
                                let _ = warnings_tx.send("camera reconnected".to_owned());
                            }
                            if let Err(error) = read_frames(
                                &reader,
                                dimensions,
                                orientation,
                                &thread_stop,
                                &thread_paused,
                                &thread_frames,
                                &feed,
                            ) {
                                feed.disconnected(format!("camera unavailable: {error}"));
                                let _ = warnings_tx.send(format!(
                                    "camera disconnected; reconnecting to the selected device: {error}"
                                ));
                            }
                            lock(&thread_active).take();
                            let _ = unsafe { source.Shutdown() };
                        }
                        Err(error) if first => {
                            let _ = started_tx.try_send(Err(error));
                            return;
                        }
                        Err(error) => {
                            let message = format!("camera unavailable: {error}");
                            if matches!(error, Error::PermissionDenied { .. }) {
                                feed.permission_denied(message);
                            } else {
                                feed.disconnected(message);
                            }
                        }
                    }
                    if !thread_stop.load(Ordering::Acquire) {
                        thread::sleep(RECONNECT_DELAY);
                    }
                }
            })
            .map_err(|error| Error::Platform(format!("could not start camera worker: {error}")))?;
        match started_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self {
                frames,
                warnings,
                stop,
                active,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                if let Some(active) = lock(&active).as_ref() {
                    let _ = unsafe {
                        active
                            .reader
                            .Flush(MF_SOURCE_READER_ALL_STREAMS.0.cast_unsigned())
                    };
                    let _ = unsafe { active.source.Shutdown() };
                }
                reap_worker(worker);
                Err(Error::Platform(
                    "Media Foundation camera startup did not complete in time".into(),
                ))
            }
        }
    }

    pub fn take_latest_frame(&self) -> (Option<CameraPacket>, usize) {
        self.frames.take_latest()
    }

    pub fn try_warning(&self) -> Option<String> {
        self.warnings.try_recv().ok()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.frames.clear();
        if let Some(active) = lock(&self.active).as_ref() {
            let _ = unsafe {
                active
                    .reader
                    .Flush(MF_SOURCE_READER_ALL_STREAMS.0.cast_unsigned())
            };
            let _ = unsafe { active.source.Shutdown() };
        }

        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn reap_worker(worker: JoinHandle<()>) {
    let _ = thread::Builder::new()
        .name("scrozz-camera-mf-reaper".to_owned())
        .spawn(move || {
            let _ = worker.join();
        });
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

struct WindowsCameraPreview {
    capture: CameraCapture,
    feed: CameraFeed,
    started: Instant,
}

impl CameraPreviewSession for WindowsCameraPreview {
    fn status(&self) -> crate::CameraRuntimeStatus {
        self.feed.status()
    }

    fn poll(&mut self) -> Option<CameraPreview> {
        let (latest, superseded) = self.capture.take_latest_frame();
        self.feed.note_drops(superseded);
        if let Some(frame) = latest
            && let Ok(frame) =
                CameraFrame::new(frame.pixels, self.started.elapsed(), frame.orientation)
        {
            let _ = self.feed.push(frame);
        }
        if let Some(warning) = self.capture.try_warning() {
            self.feed.warn(warning);
        }
        self.feed.preview(self.started.elapsed())
    }

    fn update_settings(&mut self, settings: crate::settings::CameraSettings) -> Result<()> {
        self.feed.update_settings(settings)
    }

    fn stop(mut self: Box<Self>) {
        self.capture.stop();
        self.feed.stop();
    }
}

impl Drop for WindowsCameraPreview {
    fn drop(&mut self) {
        self.capture.stop();
        self.feed.stop();
    }
}

pub fn start_preview(request: &CameraRequest) -> Result<Box<dyn CameraPreviewSession>> {
    let feed = CameraFeed::new(request)?;
    let capture = CameraCapture::start(request.clone(), feed.clone())?;
    feed.activate();
    Ok(Box::new(WindowsCameraPreview {
        capture,
        feed,
        started: Instant::now(),
    }))
}

pub fn devices() -> Result<Vec<CameraDevice>> {
    let _apartment = Apartment::enter()?;
    let _media_foundation = MediaFoundation::start()?;
    enumerate_activations()?
        .into_iter()
        .enumerate()
        .map(|(index, activation)| {
            Ok(CameraDevice {
                id: CameraDeviceId::new(attribute_string(
                    &activation,
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                )?)?,
                name: attribute_string(&activation, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)?,
                state: CameraDeviceState::Available,
                is_default: index == 0,
            })
        })
        .collect()
}

fn open_reader(
    selected: Option<&CameraDeviceId>,
    _requested_size: f32,
) -> Result<(
    IMFMediaSource,
    IMFSourceReader,
    (u32, u32),
    CameraOrientation,
)> {
    let activations = enumerate_activations()?;
    let activation = if let Some(selected) = selected {
        activations
            .into_iter()
            .find(|activation| {
                attribute_string(
                    activation,
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                )
                .is_ok_and(|id| id == selected.as_str())
            })
            .ok_or_else(|| Error::TargetGone("the selected camera is not connected".into()))?
    } else {
        activations
            .into_iter()
            .next()
            .ok_or_else(|| Error::Unsupported {
                what: "camera capture".into(),
                why: "Media Foundation reported no video capture device".into(),
            })?
    };
    let source: IMFMediaSource =
        unsafe { activation.ActivateObject() }.map_err(|error| match error.code() {
            E_ACCESSDENIED => Error::PermissionDenied {
                capability: "camera".into(),
                remedy:
                    "allow desktop apps to access the camera in Windows Privacy & security settings"
                        .into(),
            },
            _ => Error::Unsupported {
                what: "camera capture".into(),
                why: format!(
                    "the selected camera is busy or unavailable to Media Foundation ({error})"
                ),
            },
        })?;
    let attributes = attributes(2)?;
    unsafe {
        attributes
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .map_err(|error| {
                Error::Platform(format!("could not enable camera color conversion: {error}"))
            })?;
    }
    let reader =
        unsafe { MFCreateSourceReaderFromMediaSource(&source, &attributes) }.map_err(|error| {
            Error::Platform(format!("could not create camera source reader: {error}"))
        })?;
    let media_type = camera_media_type()?;
    unsafe {
        reader.SetCurrentMediaType(
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0.cast_unsigned(),
            None,
            &media_type,
        )
    }
    .map_err(|error| Error::Unsupported {
        what: "camera BGRA conversion".into(),
        why: format!("Media Foundation rejected RGB32 output: {error}"),
    })?;
    let current = unsafe {
        reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0.cast_unsigned())
    }
    .map_err(|error| Error::Platform(format!("could not inspect camera media type: {error}")))?;
    let dimensions = unpack_pair(
        unsafe { current.GetUINT64(&MF_MT_FRAME_SIZE) }
            .map_err(|error| Error::Platform(format!("camera has no frame size: {error}")))?,
    );
    if dimensions.0 == 0 || dimensions.1 == 0 {
        return Err(Error::Platform(
            "Media Foundation selected an empty camera frame size".into(),
        ));
    }
    let orientation = unsafe { current.GetUINT32(&MF_MT_VIDEO_ROTATION) }
        .ok()
        .and_then(CameraOrientation::from_clockwise_degrees)
        .unwrap_or(CameraOrientation::Upright);
    Ok((source, reader, dimensions, orientation))
}

fn read_frames(
    reader: &IMFSourceReader,
    dimensions: (u32, u32),
    orientation: CameraOrientation,
    stop: &AtomicBool,
    paused: &AtomicBool,
    frames: &LatestFrameQueue<CameraPacket>,
    feed: &CameraFeed,
) -> Result<()> {
    while !stop.load(Ordering::Acquire) {
        let mut flags = 0;
        let mut sample = None;
        unsafe {
            reader.ReadSample(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0.cast_unsigned(),
                0,
                None,
                Some(&raw mut flags),
                None,
                Some(&raw mut sample),
            )
        }
        .map_err(|error| Error::Platform(format!("reading a camera frame failed: {error}")))?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0.cast_unsigned() != 0 {
            return Err(Error::TargetGone("camera stream ended".into()));
        }
        if flags
            & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0)
                .cast_unsigned()
            != 0
        {
            return Err(Error::Platform(format!(
                "camera media stream changed or failed (flags 0x{flags:x})"
            )));
        }
        if flags & MF_SOURCE_READERF_STREAMTICK.0.cast_unsigned() != 0 {
            continue;
        }
        let Some(sample) = sample else {
            continue;
        };
        if paused.load(Ordering::Acquire) {
            continue;
        }
        let pixels = copy_sample(&sample, dimensions)?;
        let raw_hns = qpc_now_hns()?;
        if frames.push(CameraPacket {
            pixels,
            raw_hns,
            orientation,
        }) {
            feed.note_drop();
        }
    }
    Ok(())
}

fn copy_sample(
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
    (width, height): (u32, u32),
) -> Result<Frame> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|error| Error::Platform(format!("camera sample has no buffer: {error}")))?;
    let buffer: IMF2DBuffer = buffer.cast().map_err(|error| Error::Unsupported {
        what: "camera RGB32 buffer".into(),
        why: format!("Media Foundation did not expose a 2D buffer: {error}"),
    })?;
    let mut scanline = core::ptr::null_mut();
    let mut pitch = 0;
    unsafe { buffer.Lock2D(&raw mut scanline, &raw mut pitch) }
        .map_err(|error| Error::Platform(format!("could not lock camera frame: {error}")))?;
    let row_bytes = width as usize * 4;
    let result = (|| {
        if scanline.is_null() || pitch.unsigned_abs() < row_bytes as u32 {
            return Err(Error::Platform(
                "camera RGB32 buffer has invalid stride".into(),
            ));
        }
        let mut data = Vec::with_capacity(row_bytes * height as usize);
        for row in 0..height as isize {
            let source = unsafe {
                std::slice::from_raw_parts(scanline.offset(row * pitch as isize), row_bytes)
            };
            data.extend_from_slice(source);
        }
        for pixel in data.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        Ok(Frame {
            data,
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride: row_bytes,
            format: PixelFormat::Bgra8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::IDENTITY,
        })
    })();
    unsafe { buffer.Unlock2D() }
        .map_err(|error| Error::Platform(format!("could not unlock camera frame: {error}")))?;
    result
}

fn camera_media_type() -> Result<IMFMediaType> {
    let media = unsafe { MFCreateMediaType() }
        .map_err(|error| Error::Platform(format!("could not create camera media type: {error}")))?;
    unsafe {
        media
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .and_then(|()| media.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32))
            .and_then(|()| {
                media.SetUINT32(
                    &MF_MT_INTERLACE_MODE,
                    MFVideoInterlace_Progressive.0.cast_unsigned(),
                )
            })
            .and_then(|()| media.SetUINT64(&MF_MT_FRAME_RATE, pack_pair(30, 1)))
    }
    .map_err(|error| Error::Platform(format!("could not configure camera media type: {error}")))?;
    Ok(media)
}

fn enumerate_activations() -> Result<Vec<IMFActivate>> {
    let attributes = attributes(1)?;
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }
    .map_err(|error| Error::Platform(format!("could not configure camera enumeration: {error}")))?;
    let mut found: *mut Option<IMFActivate> = core::ptr::null_mut();
    let mut count = 0;
    unsafe { MFEnumDeviceSources(&attributes, &raw mut found, &raw mut count) }
        .map_err(|error| Error::Platform(format!("camera enumeration failed: {error}")))?;
    if found.is_null() {
        return Ok(Vec::new());
    }
    let values = unsafe { core::slice::from_raw_parts_mut(found, count as usize) }
        .iter_mut()
        .filter_map(Option::take)
        .collect();
    unsafe { CoTaskMemFree(Some(found.cast::<c_void>())) };
    Ok(values)
}

fn attribute_string(attributes: &IMFAttributes, key: &windows::core::GUID) -> Result<String> {
    let mut value = PWSTR::null();
    let mut length = 0;
    unsafe { attributes.GetAllocatedString(key, &raw mut value, &raw mut length) }
        .map_err(|error| Error::Platform(format!("camera attribute read failed: {error}")))?;
    if value.is_null() {
        return Err(Error::Platform(
            "Media Foundation returned a null camera attribute".into(),
        ));
    }
    let string =
        String::from_utf16_lossy(unsafe { core::slice::from_raw_parts(value.0, length as usize) });
    unsafe { CoTaskMemFree(Some(value.0.cast::<c_void>())) };
    Ok(string)
}

fn attributes(capacity: u32) -> Result<IMFAttributes> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&raw mut attributes, capacity) }
        .map_err(|error| Error::Platform(format!("MFCreateAttributes failed: {error}")))?;
    attributes.ok_or_else(|| Error::Platform("Media Foundation returned no attributes".into()))
}

const fn pack_pair(first: u32, second: u32) -> u64 {
    (first as u64) << 32 | second as u64
}

const fn unpack_pair(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

fn qpc_now_hns() -> Result<i64> {
    let mut counter = 0;
    let mut frequency = 0;
    unsafe {
        windows::Win32::System::Performance::QueryPerformanceCounter(&raw mut counter)
            .map_err(|error| Error::Platform(format!("camera QPC read failed: {error}")))?;
        windows::Win32::System::Performance::QueryPerformanceFrequency(&raw mut frequency)
            .map_err(|error| Error::Platform(format!("camera QPC frequency failed: {error}")))?;
    }

    qpc_to_hns(counter, frequency)
        .ok_or_else(|| Error::Platform("camera QPC conversion overflowed".into()))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[allow(dead_code)]
fn _status_contract(status: CameraRuntimeStatus) -> bool {
    status.privacy_indicator_visible == status.active
}
