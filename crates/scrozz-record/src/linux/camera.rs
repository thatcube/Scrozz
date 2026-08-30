//! Direct V4L2 camera capture for the opt-in native Linux recorder.

use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read},
    mem::size_of,
    os::unix::{ffi::OsStrExt as _, fs::OpenOptionsExt as _, io::AsRawFd as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use scrozz_core::{ColorSpace, Error, Frame, PhysicalSize, PixelFormat, Result, ScaleFactor};

use crate::{
    CameraDevice, CameraDeviceId, CameraDeviceState, CameraFrame, CameraOrientation, CameraPreview,
    CameraPreviewSession, CameraRequest,
    camera::{CameraFeed, LatestFrameQueue},
};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const READ_RETRY: Duration = Duration::from_millis(5);
const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_FIELD_ANY: u32 = 0;
const V4L2_PIX_FMT_YUYV: u32 = u32::from_le_bytes(*b"YUYV");
const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const V4L2_CAP_READWRITE: u32 = 0x0100_0000;
const V4L2_CAP_STREAMING: u32 = 0x0400_0000;
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_CID_CAMERA_SENSOR_ROTATION: u32 = 0x009a_0923;
const VIDIOC_QUERYCAP: libc::c_ulong = ioctl_read(b'V', 0, size_of::<V4l2Capability>());
const VIDIOC_S_FMT: libc::c_ulong = ioctl_read_write(b'V', 5, size_of::<V4l2Format>());
const VIDIOC_G_CTRL: libc::c_ulong = ioctl_read_write(b'V', 27, size_of::<V4l2Control>());
const VIDIOC_REQBUFS: libc::c_ulong = ioctl_read_write(b'V', 8, size_of::<V4l2RequestBuffers>());
const VIDIOC_QUERYBUF: libc::c_ulong = ioctl_read_write(b'V', 9, size_of::<V4l2Buffer>());
const VIDIOC_QBUF: libc::c_ulong = ioctl_read_write(b'V', 15, size_of::<V4l2Buffer>());
const VIDIOC_DQBUF: libc::c_ulong = ioctl_read_write(b'V', 17, size_of::<V4l2Buffer>());
const VIDIOC_STREAMON: libc::c_ulong = ioctl_write(b'V', 18, size_of::<u32>());
const VIDIOC_STREAMOFF: libc::c_ulong = ioctl_write(b'V', 19, size_of::<u32>());

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

impl Default for V4l2Capability {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

#[repr(C)]
union V4l2FormatValue {
    pix: V4l2PixFormat,
    raw: [u8; 200],
    align: u64,
}

#[repr(C)]
struct V4l2Format {
    type_: u32,
    value: V4l2FormatValue,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
union V4l2BufferMemory {
    offset: u32,
    userptr: libc::c_ulong,
    planes: *mut libc::c_void,
    fd: i32,
}

impl Default for V4l2BufferMemory {
    fn default() -> Self {
        Self { userptr: 0 }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: V4l2Timecode,
    sequence: u32,
    memory: u32,
    value: V4l2BufferMemory,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Control {
    id: u32,
    value: i32,
}

struct MappedBuffer {
    address: *mut u8,
    length: usize,
}

impl Drop for MappedBuffer {
    fn drop(&mut self) {
        if !self.address.is_null() && self.length != 0 {
            unsafe {
                libc::munmap(self.address.cast(), self.length);
            }
        }
    }
}

enum CaptureMethod {
    Read,
    Streaming(Vec<MappedBuffer>),
}

struct OpenCamera {
    file: File,
    width: u32,
    height: u32,
    bytes_per_line: usize,
    frame_bytes: usize,
    orientation: CameraOrientation,
    method: CaptureMethod,
}

impl Drop for OpenCamera {
    fn drop(&mut self) {
        if matches!(self.method, CaptureMethod::Streaming(_)) {
            let mut buffer_type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            let _ = ioctl(
                self.file.as_raw_fd(),
                VIDIOC_STREAMOFF,
                &raw mut buffer_type,
            );
        }
    }
}

pub struct CameraPacket {
    pub pixels: Frame,
    pub captured_at: Instant,
    pub orientation: CameraOrientation,
}

pub struct CameraCapture {
    frames: Arc<LatestFrameQueue<CameraPacket>>,
    warnings: Receiver<String>,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl CameraCapture {
    pub fn start(request: CameraRequest, feed: CameraFeed) -> Result<Self> {
        let selected_device = request.device_id;
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(LatestFrameQueue::new());
        let (warnings_tx, warnings) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let thread_stop = Arc::clone(&stop);
        let thread_paused = Arc::clone(&paused);
        let thread_frames = Arc::clone(&frames);
        let worker = thread::Builder::new()
            .name("scrozz-camera-v4l2".to_owned())
            .spawn(move || {
                let mut first = true;
                while !thread_stop.load(Ordering::Acquire) {
                    match selected_path(selected_device.as_ref())
                        .and_then(|path| open_camera(&path))
                    {
                        Ok(mut camera) => {
                            if first {
                                let _ = started_tx.try_send(Ok(()));
                                first = false;
                            } else {
                                feed.reconnected();
                                let _ = warnings_tx.send("camera reconnected".to_owned());
                            }
                            if let Err(error) =
                                read_frames(
                                    &mut camera,
                                    &thread_stop,
                                    &thread_paused,
                                    &thread_frames,
                                    &feed,
                                )
                            {
                                feed.disconnected(format!("camera unavailable: {error}"));
                                let _ = warnings_tx.send(format!(
                                    "camera disconnected; reconnecting to the selected device: {error}"
                                ));
                            }
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
                        thread::park_timeout(RECONNECT_DELAY);
                    }
                }
            })
            .map_err(|error| Error::Platform(format!("could not start V4L2 worker: {error}")))?;
        match started_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self {
                frames,
                warnings,
                stop,
                paused,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                worker.thread().unpark();
                reap_worker(worker);
                Err(Error::Platform(
                    "V4L2 camera startup did not complete in time".into(),
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
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }

    pub fn set_paused(&self, paused: bool) -> usize {
        if paused {
            self.paused.store(true, Ordering::Release);
            self.frames.clear()
        } else {
            let discarded = self.frames.clear();
            self.paused.store(false, Ordering::Release);
            discarded
        }
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn reap_worker(worker: JoinHandle<()>) {
    let _ = thread::Builder::new()
        .name("scrozz-camera-v4l2-reaper".to_owned())
        .spawn(move || {
            let _ = worker.join();
        });
}

struct LinuxCameraPreview {
    capture: CameraCapture,
    feed: CameraFeed,
    started: Instant,
}

impl CameraPreviewSession for LinuxCameraPreview {
    fn status(&self) -> crate::CameraRuntimeStatus {
        self.feed.status()
    }

    fn poll(&mut self) -> Option<CameraPreview> {
        let (latest, superseded) = self.capture.take_latest_frame();
        self.feed.note_drops(superseded);
        if let Some(frame) = latest
            && let Ok(frame) = CameraFrame::new(
                frame.pixels,
                frame.captured_at.saturating_duration_since(self.started),
                frame.orientation,
            )
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

impl Drop for LinuxCameraPreview {
    fn drop(&mut self) {
        self.capture.stop();
        self.feed.stop();
    }
}

pub fn start_preview(request: &CameraRequest) -> Result<Box<dyn CameraPreviewSession>> {
    let feed = CameraFeed::new(request)?;
    let capture = CameraCapture::start(request.clone(), feed.clone())?;
    feed.activate();
    Ok(Box::new(LinuxCameraPreview {
        capture,
        feed,
        started: Instant::now(),
    }))
}

pub fn devices() -> Result<Vec<CameraDevice>> {
    let paths = camera_paths()?;
    let mut devices = Vec::new();
    for path in paths {
        match open_query(&path) {
            Ok((_, capability)) => {
                let caps = effective_capabilities(capability);
                if caps & V4L2_CAP_VIDEO_CAPTURE == 0
                    || caps & (V4L2_CAP_READWRITE | V4L2_CAP_STREAMING) == 0
                {
                    continue;
                }
                let is_default = devices.is_empty();
                devices.push(CameraDevice {
                    id: CameraDeviceId::new(stable_id(&path, &capability))?,
                    name: nul_terminated(&capability.card)
                        .unwrap_or_else(|| path.to_string_lossy().into_owned()),
                    state: CameraDeviceState::Available,
                    is_default,
                });
            }
            Err(Error::PermissionDenied { .. }) => {
                let is_default = devices.is_empty();
                devices.push(CameraDevice {
                    id: CameraDeviceId::new(path.to_string_lossy())?,
                    name: path.to_string_lossy().into_owned(),
                    state: CameraDeviceState::PermissionDenied,
                    is_default,
                });
            }
            Err(Error::Unsupported { .. }) => {
                let is_default = devices.is_empty();
                devices.push(CameraDevice {
                    id: CameraDeviceId::new(path.to_string_lossy())?,
                    name: path.to_string_lossy().into_owned(),
                    state: CameraDeviceState::Busy,
                    is_default,
                });
            }
            Err(_) => {}
        }
    }
    Ok(devices)
}

fn selected_path(selected: Option<&CameraDeviceId>) -> Result<PathBuf> {
    if let Some(selected) = selected {
        let direct = PathBuf::from(selected.as_str());
        if direct.starts_with("/dev/v4l/by-id") {
            return Ok(direct);
        }
        if direct.is_absolute() {
            return Err(Error::TargetGone(
                "the remembered camera uses a transient device path; select it again to store a stable identity"
                    .into(),
            ));
        }
        for path in camera_paths()? {
            if let Ok((_, capability)) = open_query(&path)
                && stable_id(&path, &capability) == selected.as_str()
            {
                return Ok(path);
            }
        }
        return Err(Error::TargetGone(
            "the selected V4L2 camera is not connected".into(),
        ));
    }

    camera_paths()?
        .into_iter()
        .next()
        .ok_or_else(|| Error::Unsupported {
            what: "camera capture".into(),
            why: "no V4L2 video capture device is available".into(),
        })
}

fn stable_id(path: &Path, capability: &V4l2Capability) -> String {
    if path.starts_with("/dev/v4l/by-id") {
        return path.to_string_lossy().into_owned();
    }
    nul_terminated(&capability.bus_info)
        .map(|bus| format!("v4l2:{bus}"))
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn camera_paths() -> Result<Vec<PathBuf>> {
    let stable = Path::new("/dev/v4l/by-id");
    let mut paths = if stable.is_dir() {
        read_paths(stable)?
    } else {
        Vec::new()
    };
    if paths.is_empty() {
        paths = read_paths(Path::new("/dev"))?
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.as_bytes().starts_with(b"video"))
            })
            .collect();
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn read_paths(directory: &Path) -> Result<Vec<PathBuf>> {
    match fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.path()).map_err(Error::Io))
            .collect(),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(Error::Io(error)),
    }
}

fn open_query(path: &Path) -> Result<(File, V4l2Capability)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| map_open_error(path, error))?;
    let mut capability = V4l2Capability::default();
    ioctl(file.as_raw_fd(), VIDIOC_QUERYCAP, &raw mut capability)?;
    Ok((file, capability))
}

fn open_camera(path: &Path) -> Result<OpenCamera> {
    let (mut file, mut format, caps, mut orientation) = open_configured_camera(path)?;
    let method = if caps & V4L2_CAP_STREAMING != 0 {
        match start_streaming(&file) {
            Ok(buffers) => CaptureMethod::Streaming(buffers),
            Err(_) if caps & V4L2_CAP_READWRITE != 0 => {
                // Closing is the only portable way to unwind a partially
                // successful REQBUFS/QBUF sequence before changing I/O method.
                drop(file);
                (file, format, _, orientation) = open_configured_camera(path)?;
                CaptureMethod::Read
            }
            Err(error) => return Err(error),
        }
    } else {
        CaptureMethod::Read
    };
    let bytes_per_line = (format.width as usize * 2).max(format.bytesperline as usize);
    let frame_bytes = bytes_per_line
        .checked_mul(format.height as usize)
        .ok_or_else(|| Error::Platform("V4L2 camera buffer size overflowed".into()))?;
    Ok(OpenCamera {
        file,
        width: format.width,
        height: format.height,
        bytes_per_line,
        frame_bytes: frame_bytes.max(format.sizeimage as usize),
        orientation,
        method,
    })
}

fn open_configured_camera(path: &Path) -> Result<(File, V4l2PixFormat, u32, CameraOrientation)> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| map_open_error(path, error))?;
    let mut capability = V4l2Capability::default();
    ioctl(file.as_raw_fd(), VIDIOC_QUERYCAP, &raw mut capability)?;
    let caps = effective_capabilities(capability);
    if caps & V4L2_CAP_VIDEO_CAPTURE == 0 || caps & (V4L2_CAP_READWRITE | V4L2_CAP_STREAMING) == 0 {
        return Err(Error::Unsupported {
            what: "V4L2 camera capture".into(),
            why: format!(
                "{} does not provide single-plane read() or streaming video capture",
                path.display()
            ),
        });
    }
    let mut format = V4l2Format {
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
        value: V4l2FormatValue { raw: [0; 200] },
    };
    format.value.pix = V4l2PixFormat {
        width: 1280,
        height: 720,
        pixelformat: V4L2_PIX_FMT_YUYV,
        field: V4L2_FIELD_ANY,
        bytesperline: 0,
        sizeimage: 0,
        colorspace: 0,
        private: 0,
        flags: 0,
        ycbcr_enc: 0,
        quantization: 0,
        xfer_func: 0,
    };
    ioctl(file.as_raw_fd(), VIDIOC_S_FMT, &raw mut format)?;
    let format = unsafe { format.value.pix };
    // MJPEG-only cameras land here on purpose. Decoding a JPEG per frame inside
    // the recorder would add a codec dependency and per-frame latency the
    // pipeline has not agreed to, so the refusal names the device instead.
    if format.pixelformat != V4L2_PIX_FMT_YUYV || format.width == 0 || format.height == 0 {
        return Err(Error::Unsupported {
            what: "V4L2 camera pixel format".into(),
            why: format!(
                "{} cannot provide packed YUYV frames; MJPEG-only cameras are not supported",
                path.display()
            ),
        });
    }
    let orientation = camera_orientation(&file);
    Ok((file, format, caps, orientation))
}

fn camera_orientation(file: &File) -> CameraOrientation {
    let mut control = V4l2Control {
        id: V4L2_CID_CAMERA_SENSOR_ROTATION,
        value: 0,
    };
    if ioctl(file.as_raw_fd(), VIDIOC_G_CTRL, &raw mut control).is_ok() {
        CameraOrientation::from_clockwise_degrees(control.value.max(0) as u32)
            .unwrap_or(CameraOrientation::Upright)
    } else {
        CameraOrientation::Upright
    }
}

fn start_streaming(file: &File) -> Result<Vec<MappedBuffer>> {
    let mut request = V4l2RequestBuffers {
        count: 4,
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
        memory: V4L2_MEMORY_MMAP,
        ..V4l2RequestBuffers::default()
    };
    ioctl(file.as_raw_fd(), VIDIOC_REQBUFS, &raw mut request)?;
    if request.count < 2 {
        return Err(Error::Unsupported {
            what: "V4L2 streaming camera capture".into(),
            why: "the camera did not provide at least two streaming buffers".into(),
        });
    }

    let mut buffers = Vec::with_capacity(request.count as usize);
    for index in 0..request.count {
        let mut buffer = V4l2Buffer {
            index,
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            memory: V4L2_MEMORY_MMAP,
            ..V4l2Buffer::default()
        };
        ioctl(file.as_raw_fd(), VIDIOC_QUERYBUF, &raw mut buffer)?;
        let length = buffer.length as usize;
        if length == 0 {
            return Err(Error::Platform(
                "V4L2 returned an empty streaming buffer".into(),
            ));
        }
        let offset = unsafe { buffer.value.offset };
        let address = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                libc::off_t::from(offset),
            )
        };
        if address == libc::MAP_FAILED {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        buffers.push(MappedBuffer {
            address: address.cast(),
            length,
        });
        ioctl(file.as_raw_fd(), VIDIOC_QBUF, &raw mut buffer)?;
    }

    let mut buffer_type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    ioctl(file.as_raw_fd(), VIDIOC_STREAMON, &raw mut buffer_type)?;
    Ok(buffers)
}

fn read_frames(
    camera: &mut OpenCamera,
    stop: &AtomicBool,
    paused: &AtomicBool,
    frames: &LatestFrameQueue<CameraPacket>,
    feed: &CameraFeed,
) -> Result<()> {
    let fd = camera.file.as_raw_fd();
    let width = camera.width;
    let height = camera.height;
    let bytes_per_line = camera.bytes_per_line;
    let frame_bytes = camera.frame_bytes;
    let orientation = camera.orientation;
    match &mut camera.method {
        CaptureMethod::Read => read_frames_with_read(
            &mut camera.file,
            width,
            height,
            bytes_per_line,
            frame_bytes,
            orientation,
            stop,
            paused,
            frames,
            feed,
        ),
        CaptureMethod::Streaming(buffers) => read_frames_with_mmap(
            fd,
            buffers,
            width,
            height,
            bytes_per_line,
            orientation,
            stop,
            paused,
            frames,
            feed,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn read_frames_with_read(
    file: &mut File,
    width: u32,
    height: u32,
    bytes_per_line: usize,
    frame_bytes: usize,
    orientation: CameraOrientation,
    stop: &AtomicBool,
    paused: &AtomicBool,
    frames: &LatestFrameQueue<CameraPacket>,
    feed: &CameraFeed,
) -> Result<()> {
    let mut buffer = vec![0_u8; frame_bytes];
    while !stop.load(Ordering::Acquire) {
        match file.read(&mut buffer) {
            Ok(read) if read >= bytes_per_line * height as usize => {
                if paused.load(Ordering::Acquire) {
                    continue;
                }
                let pixels = yuyv_to_bgra(&buffer[..read], width, height, bytes_per_line)?;
                if frames.push(CameraPacket {
                    pixels,
                    captured_at: Instant::now(),
                    orientation,
                }) {
                    feed.note_drop();
                }
            }
            Ok(_) => feed.note_drop(),
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(READ_RETRY),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn read_frames_with_mmap(
    fd: std::os::fd::RawFd,
    buffers: &[MappedBuffer],
    width: u32,
    height: u32,
    bytes_per_line: usize,
    orientation: CameraOrientation,
    stop: &AtomicBool,
    paused: &AtomicBool,
    frames: &LatestFrameQueue<CameraPacket>,
    feed: &CameraFeed,
) -> Result<()> {
    while !stop.load(Ordering::Acquire) {
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&raw mut descriptor, 1, 100) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io(error));
        }
        if ready == 0 {
            continue;
        }

        let mut buffer = V4l2Buffer {
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            memory: V4L2_MEMORY_MMAP,
            ..V4l2Buffer::default()
        };
        if let Err(error) = ioctl(fd, VIDIOC_DQBUF, &raw mut buffer) {
            if matches!(&error, Error::Io(io_error) if io_error.kind() == ErrorKind::WouldBlock) {
                continue;
            }
            return Err(error);
        }
        let mapped = buffers.get(buffer.index as usize).ok_or_else(|| {
            Error::Platform(format!(
                "V4L2 dequeued unknown streaming buffer {}",
                buffer.index
            ))
        })?;
        let used = (buffer.bytesused as usize).min(mapped.length);
        let pixels = if paused.load(Ordering::Acquire) {
            None
        } else {
            let source = unsafe { std::slice::from_raw_parts(mapped.address, used) };
            Some(yuyv_to_bgra(source, width, height, bytes_per_line))
        };
        ioctl(fd, VIDIOC_QBUF, &raw mut buffer)?;
        if let Some(pixels) = pixels {
            let pixels = pixels?;
            if frames.push(CameraPacket {
                pixels,
                captured_at: Instant::now(),
                orientation,
            }) {
                feed.note_drop();
            }
        }
    }
    Ok(())
}

fn yuyv_to_bgra(source: &[u8], width: u32, height: u32, stride: usize) -> Result<Frame> {
    let row_bytes = width as usize * 2;
    if !width.is_multiple_of(2)
        || stride < row_bytes
        || source.len() < stride.saturating_mul(height as usize)
    {
        return Err(Error::Platform(
            "V4L2 returned malformed packed YUYV storage".into(),
        ));
    }
    let output_stride = width as usize * 4;
    let mut data = Vec::with_capacity(output_stride * height as usize);
    for row in 0..height as usize {
        let row = &source[row * stride..row * stride + row_bytes];
        for pair in row.as_chunks::<4>().0 {
            let y0 = i32::from(pair[0]);
            let u = i32::from(pair[1]) - 128;
            let y1 = i32::from(pair[2]);
            let v = i32::from(pair[3]) - 128;
            data.extend_from_slice(&yuv_pixel(y0, u, v));
            data.extend_from_slice(&yuv_pixel(y1, u, v));
        }
    }
    Ok(Frame {
        data,
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride: output_stride,
        format: PixelFormat::Bgra8,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::IDENTITY,
    })
}

fn yuv_pixel(y: i32, u: i32, v: i32) -> [u8; 4] {
    let y = (y - 16).max(0);
    let red = (298 * y + 409 * v + 128) >> 8;
    let green = (298 * y - 100 * u - 208 * v + 128) >> 8;
    let blue = (298 * y + 516 * u + 128) >> 8;
    [
        blue.clamp(0, 255) as u8,
        green.clamp(0, 255) as u8,
        red.clamp(0, 255) as u8,
        255,
    ]
}

fn effective_capabilities(capability: V4l2Capability) -> u32 {
    if capability.capabilities & V4L2_CAP_DEVICE_CAPS != 0 {
        capability.device_caps
    } else {
        capability.capabilities
    }
}

fn nul_terminated(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let value = String::from_utf8_lossy(&bytes[..end]).trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn map_open_error(path: &Path, error: std::io::Error) -> Error {
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => Error::PermissionDenied {
            capability: "camera".into(),
            remedy: format!(
                "grant this user read/write access to the V4L2 device {}",
                path.display()
            ),
        },
        Some(libc::EBUSY) => Error::Unsupported {
            what: "camera capture".into(),
            why: format!("{} is busy in another application", path.display()),
        },
        _ => Error::Io(error),
    }
}

fn ioctl<T>(fd: std::os::fd::RawFd, request: libc::c_ulong, value: *mut T) -> Result<()> {
    let result = unsafe { libc::ioctl(fd, request, value) };
    if result == -1 {
        Err(Error::Io(std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

const fn ioctl_read(kind: u8, number: u8, size: usize) -> libc::c_ulong {
    ioctl_code(2, kind, number, size)
}

const fn ioctl_write(kind: u8, number: u8, size: usize) -> libc::c_ulong {
    ioctl_code(1, kind, number, size)
}

const fn ioctl_read_write(kind: u8, number: u8, size: usize) -> libc::c_ulong {
    ioctl_code(3, kind, number, size)
}

const fn ioctl_code(direction: u8, kind: u8, number: u8, size: usize) -> libc::c_ulong {
    ((direction as libc::c_ulong) << 30)
        | ((size as libc::c_ulong) << 16)
        | ((kind as libc::c_ulong) << 8)
        | number as libc::c_ulong
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuyv_conversion_preserves_black_and_white_levels() {
        let frame = yuyv_to_bgra(&[16, 128, 235, 128], 2, 1, 4).unwrap();
        assert_eq!(&frame.data[..4], &[0, 0, 0, 255]);
        assert_eq!(&frame.data[4..], &[255, 255, 255, 255]);
    }

    #[test]
    fn ioctl_numbers_match_the_linux_uapi_on_sixty_four_bit_hosts() {
        assert_eq!(size_of::<V4l2Capability>(), 104);
        assert_eq!(size_of::<V4l2Format>(), 208);
        assert_eq!(size_of::<V4l2RequestBuffers>(), 20);
        assert_eq!(size_of::<V4l2Buffer>(), 88);
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_G_CTRL, 0xc008_561b);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
    }

    #[test]
    fn fallback_device_identity_uses_bus_path_not_video_number() {
        let mut capability = V4l2Capability::default();
        let bus = b"usb-0000:00:14.0-4\0";
        capability.bus_info[..bus.len()].copy_from_slice(bus);
        assert_eq!(
            stable_id(Path::new("/dev/video7"), &capability),
            "v4l2:usb-0000:00:14.0-4"
        );
    }

    #[test]
    fn transient_video_node_is_never_reopened_as_a_persistent_preference() {
        let selected = CameraDeviceId::new("/dev/video7").unwrap();
        assert!(matches!(
            selected_path(Some(&selected)),
            Err(Error::TargetGone(message)) if message.contains("transient")
        ));
    }
}
