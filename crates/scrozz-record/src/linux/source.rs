//! X11 still-frame pacing and Wayland portal/PipeWire media acquisition.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::ResponseError;
use ashpd::desktop::screencast::{
    CursorMode as PortalCursorMode, Screencast, SelectSourcesOptions, SourceType,
};
use pipewire as pw;
use pw::properties::{PropertiesBox, properties};
use pw::spa;
use pw::spa::pod::Pod;
use scrozz_capture::{
    LinuxSessionEnv, LinuxSessionKind, PortalSessionPlan, PortalTokenStore, detect_compositor,
    detect_session, portal_capabilities, portal_token_path,
};
use scrozz_core::{
    CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, Error, PixelFormat, Result,
};

use crate::audio::AudioBuffer;
use crate::format::{PackedFrame, PackedPixelFormat, PlacedFrame, compose_placed_frames, crop};

pub(super) trait VideoSource {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<PackedFrame>>;
    fn name(&self) -> &str;
    fn backing_scale(&self) -> Option<f64>;
}

pub(super) fn open_video_source(
    target: &CaptureTarget,
    show_cursor: bool,
) -> Result<Box<dyn VideoSource>> {
    let env = LinuxSessionEnv::from_env();
    match detect_session(&env) {
        LinuxSessionKind::X11 => Ok(Box::new(X11Source::new(target, show_cursor)?)),
        LinuxSessionKind::Wayland | LinuxSessionKind::XWayland => {
            Ok(Box::new(PortalVideoSource::new(target, show_cursor, &env)?))
        }
        LinuxSessionKind::Headless => Err(Error::Unsupported {
            what: "screen recording".into(),
            why: "neither WAYLAND_DISPLAY nor DISPLAY identifies a desktop session".into(),
        }),
    }
}

struct X11Source {
    backend: Box<dyn CaptureBackend>,
    request: CaptureRequest,
    backing_scale: Option<f64>,
}

impl X11Source {
    fn new(target: &CaptureTarget, show_cursor: bool) -> Result<Self> {
        let backend = scrozz_capture::backend()?;
        let backing_scale = x11_backing_scale(backend.as_ref(), target)?;
        let mut request = CaptureRequest::new(target.clone());
        request.cursor = if show_cursor {
            CursorMode::Visible
        } else {
            CursorMode::Hidden
        };
        Ok(Self {
            backend,
            request,
            backing_scale,
        })
    }
}

impl VideoSource for X11Source {
    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<PackedFrame>> {
        let frame = self.backend.capture(&self.request)?.frame;
        let format = match frame.format {
            PixelFormat::Rgba8 | PixelFormat::RgbaPremultiplied8 => PackedPixelFormat::Rgba,
            PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8 => PackedPixelFormat::Bgra,
        };
        Ok(Some(PackedFrame {
            width: frame.width(),
            height: frame.height(),
            stride: frame.stride,
            format,
            data: frame.data,
        }))
    }

    fn name(&self) -> &str {
        self.backend.name()
    }

    fn backing_scale(&self) -> Option<f64> {
        self.backing_scale
    }
}

fn x11_backing_scale(backend: &dyn CaptureBackend, target: &CaptureTarget) -> Result<Option<f64>> {
    let displays = backend.displays()?;
    let scales: Vec<f64> = match target {
        CaptureTarget::Display(id) => displays
            .iter()
            .filter(|display| &display.id == id)
            .map(|display| display.scale.get())
            .collect(),
        CaptureTarget::Window(id) => {
            let display_id = backend
                .windows()?
                .into_iter()
                .find(|window| &window.id == id)
                .map(|window| window.display);
            displays
                .iter()
                .filter(|display| Some(&display.id) == display_id.as_ref())
                .map(|display| display.scale.get())
                .collect()
        }
        CaptureTarget::Region(region) => displays
            .iter()
            .filter(|display| logical_rects_overlap(display.bounds, *region))
            .map(|display| display.scale.get())
            .collect(),
        CaptureTarget::AllDisplays => displays.iter().map(|display| display.scale.get()).collect(),
    };
    Ok(common_backing_scale(&scales))
}

fn logical_rects_overlap(left: scrozz_core::LogicalRect, right: scrozz_core::LogicalRect) -> bool {
    left.origin.x < right.origin.x + right.size.width
        && right.origin.x < left.origin.x + left.size.width
        && left.origin.y < right.origin.y + right.size.height
        && right.origin.y < left.origin.y + left.size.height
}

fn common_backing_scale(scales: &[f64]) -> Option<f64> {
    let first = *scales.first()?;
    if scales
        .iter()
        .all(|scale| (*scale - first).abs() <= f64::EPSILON)
    {
        Some(first)
    } else {
        tracing::warn!(
            "the X11 target spans mixed backing scales; LogicalPoints recording assumes 1:1"
        );
        None
    }
}

#[derive(Debug, Clone, Copy)]
struct PortalStream {
    node_id: u32,
    position: Option<(i32, i32)>,
}

struct PortalResponse {
    streams: Vec<PortalStream>,
    fd: OwnedFd,
    restore_token: Option<String>,
}

struct VideoUserData {
    format: spa::param::video::VideoInfoRaw,
    frames: Rc<RefCell<VecDeque<PackedFrame>>>,
    error: Rc<RefCell<Option<String>>>,
}

struct PortalVideoSource {
    listeners: Vec<pw::stream::StreamListener<VideoUserData>>,
    _streams: Vec<pw::stream::StreamRc>,
    mainloop: pw::main_loop::MainLoopRc,
    frames: Vec<Rc<RefCell<VecDeque<PackedFrame>>>>,
    errors: Vec<Rc<RefCell<Option<String>>>>,
    positions: Vec<(i32, i32)>,
    latest: Vec<Option<PackedFrame>>,
    target: CaptureTarget,
    name: String,
}

impl PortalVideoSource {
    fn new(target: &CaptureTarget, show_cursor: bool, env: &LinuxSessionEnv) -> Result<Self> {
        let compositor = detect_compositor(env);
        let capabilities = portal_capabilities(&compositor);
        let plan = PortalSessionPlan::for_target(target, show_cursor);
        let token_path = portal_token_path(
            std::env::var("XDG_STATE_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        );
        let mut tokens = token_path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map_or_else(PortalTokenStore::new, |text| PortalTokenStore::parse(&text));
        let stored_token = capabilities
            .restore_tokens
            .then(|| tokens.get(plan.restore_key).map(str::to_owned))
            .flatten();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                Error::Platform(format!("could not create the portal runtime: {error}"))
            })?;
        let response = match runtime.block_on(open_portal(
            target,
            show_cursor,
            capabilities.restore_tokens,
            stored_token.as_deref(),
        )) {
            Ok(response) => response,
            Err(error) if stored_token.is_some() && !error.is_cancellation() => {
                tokens.invalidate(plan.restore_key);
                persist_tokens(token_path.as_deref(), &tokens);
                runtime.block_on(open_portal(
                    target,
                    show_cursor,
                    capabilities.restore_tokens,
                    None,
                ))?
            }
            Err(error) => return Err(error),
        };
        if let Some(token) = response.restore_token.as_deref() {
            tokens.set(plan.restore_key, token);
            persist_tokens(token_path.as_deref(), &tokens);
        }
        if response.streams.is_empty() {
            return Err(Error::Platform(
                "the screen-cast portal returned no PipeWire streams".into(),
            ));
        }
        if response.streams.len() > 1
            && response
                .streams
                .iter()
                .any(|stream| stream.position.is_none())
        {
            return Err(Error::Unsupported {
                what: "recording multiple portal streams".into(),
                why: "the compositor omitted stream positions, so combining monitors would guess \
                      their layout"
                    .into(),
            });
        }

        let mainloop = pw::main_loop::MainLoopRc::new(None)
            .map_err(|error| pipewire_error("create its video event loop", error))?;
        let context = pw::context::ContextRc::new(&mainloop, None)
            .map_err(|error| pipewire_error("create its video context", error))?;
        let core = context
            .connect_fd_rc(response.fd, None)
            .map_err(|error| pipewire_error("connect to the portal remote", error))?;

        let mut listeners = Vec::new();
        let mut streams = Vec::new();
        let mut frames = Vec::new();
        let mut errors = Vec::new();
        let mut positions = Vec::new();
        for descriptor in response.streams {
            let queue = Rc::new(RefCell::new(VecDeque::new()));
            let error = Rc::new(RefCell::new(None));
            let stream = pw::stream::StreamRc::new(
                core.clone(),
                "scrozz-portal-video",
                properties! {
                    *pw::keys::MEDIA_TYPE => "Video",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_ROLE => "Screen",
                },
            )
            .map_err(|cause| pipewire_error("create a portal video stream", cause))?;
            let listener = stream
                .add_local_listener_with_user_data(VideoUserData {
                    format: Default::default(),
                    frames: Rc::clone(&queue),
                    error: Rc::clone(&error),
                })
                .state_changed(|_, user_data, _, new| {
                    if let pw::stream::StreamState::Error(message) = new {
                        *user_data.error.borrow_mut() = Some(message);
                    }
                })
                .param_changed(|_, user_data, id, param| {
                    let Some(param) = param else {
                        return;
                    };
                    if id != spa::param::ParamType::Format.as_raw() {
                        return;
                    }
                    let parsed = spa::param::format_utils::parse_format(param);
                    if parsed
                        != Ok((
                            spa::param::format::MediaType::Video,
                            spa::param::format::MediaSubtype::Raw,
                        ))
                    {
                        *user_data.error.borrow_mut() =
                            Some("portal selected a non-raw video format".into());
                        return;
                    }
                    if let Err(cause) = user_data.format.parse(param) {
                        *user_data.error.borrow_mut() =
                            Some(format!("could not parse portal video format: {cause:?}"));
                    }
                })
                .process(copy_video_buffer)
                .register()
                .map_err(|cause| pipewire_error("register portal video callbacks", cause))?;

            let values = video_format_parameter()?;
            let mut params = [Pod::from_bytes(&values).ok_or_else(|| {
                Error::Platform("PipeWire rejected Scrozz's video format pod".into())
            })?];
            stream
                .connect(
                    spa::utils::Direction::Input,
                    Some(descriptor.node_id),
                    pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                    &mut params,
                )
                .map_err(|cause| pipewire_error("connect a portal video node", cause))?;

            positions.push(descriptor.position.unwrap_or((0, 0)));
            frames.push(queue);
            errors.push(error);
            listeners.push(listener);
            streams.push(stream);
        }

        Ok(Self {
            latest: vec![None; frames.len()],
            listeners,
            _streams: streams,
            mainloop,
            frames,
            errors,
            positions,
            target: target.clone(),
            name: format!("Wayland portal/PipeWire ({compositor})"),
        })
    }

    fn take_composite(&mut self) -> Result<Option<PackedFrame>> {
        for (latest, queue) in self.latest.iter_mut().zip(&self.frames) {
            let newest = {
                let mut queue = queue.borrow_mut();
                let newest = queue.pop_back();
                queue.clear();
                newest
            };
            if let Some(newest) = newest {
                *latest = Some(newest);
            }
        }
        if self.latest.iter().any(Option::is_none) {
            return Ok(None);
        }
        let placed: Vec<_> = self
            .latest
            .iter()
            .zip(&self.positions)
            .map(|(frame, position)| PlacedFrame {
                x: position.0,
                y: position.1,
                frame: frame.as_ref().expect("checked above"),
            })
            .collect();
        let (frame, origin) = compose_placed_frames(&placed)?;
        if let CaptureTarget::Region(region) = &self.target {
            let x = (region.origin.x.round() as i64 - i64::from(origin.0))
                .try_into()
                .map_err(|_| Error::InvalidRequest("portal region begins off-screen".into()))?;
            let y = (region.origin.y.round() as i64 - i64::from(origin.1))
                .try_into()
                .map_err(|_| Error::InvalidRequest("portal region begins off-screen".into()))?;
            let width = region.size.width.round().max(0.0) as u32;
            let height = region.size.height.round().max(0.0) as u32;
            return crop(&frame, x, y, width, height).map(Some);
        }
        Ok(Some(frame))
    }
}

impl VideoSource for PortalVideoSource {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<PackedFrame>> {
        let deadline = Instant::now() + timeout;
        loop {
            for error in &self.errors {
                if let Some(message) = error.borrow_mut().take() {
                    return Err(Error::Platform(format!(
                        "PipeWire portal stream failed: {message}"
                    )));
                }
            }
            if let Some(frame) = self.take_composite()? {
                return Ok(Some(frame));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            self.mainloop.loop_().iterate(pw::loop_::Timeout::Finite(
                (deadline - now).min(Duration::from_millis(20)),
            ));
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn backing_scale(&self) -> Option<f64> {
        // ScreenCast exposes physical buffers and stream positions, but no
        // compositor-independent logical-to-physical scale.
        None
    }
}

impl Drop for PortalVideoSource {
    fn drop(&mut self) {
        // Keeping the listeners as a named field makes the required lifetime
        // obvious; reading its length also prevents an accidental "unused field"
        // cleanup from dropping callbacks immediately after registration.
        tracing::trace!(
            streams = self.listeners.len(),
            "closing portal recording streams"
        );
    }
}

async fn open_portal(
    target: &CaptureTarget,
    show_cursor: bool,
    persist: bool,
    restore_token: Option<&str>,
) -> Result<PortalResponse> {
    let proxy = Screencast::new().await.map_err(map_portal_error)?;
    let available_sources = proxy
        .available_source_types()
        .await
        .map_err(map_portal_error)?;
    let available_cursors = proxy
        .available_cursor_modes()
        .await
        .map_err(map_portal_error)?;
    let plan = PortalSessionPlan::for_target(target, show_cursor)
        .narrow(available_sources.bits(), available_cursors.bits())
        .ok_or_else(|| Error::Unsupported {
            what: "the requested portal recording source".into(),
            why: "the desktop portal reports no compatible source or cursor mode".into(),
        })?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(map_portal_error)?;
    let sources = match plan.types {
        1 => SourceType::Monitor.into(),
        2 => SourceType::Window.into(),
        5 => SourceType::Monitor | SourceType::Virtual,
        _ => {
            return Err(Error::Unsupported {
                what: "the requested portal source combination".into(),
                why: format!("unsupported ScreenCast source mask {}", plan.types),
            });
        }
    };
    let mut options = SelectSourcesOptions::default().set_sources(sources);
    options = options
        .set_cursor_mode(if plan.cursor == 2 {
            PortalCursorMode::Embedded
        } else {
            PortalCursorMode::Hidden
        })
        .set_multiple(plan.multiple)
        .set_persist_mode(if persist {
            PersistMode::ExplicitlyRevoked
        } else {
            PersistMode::DoNot
        })
        .set_restore_token(restore_token);
    proxy
        .select_sources(&session, options)
        .await
        .map_err(map_portal_error)?;
    let response = proxy
        .start(&session, None, Default::default())
        .await
        .map_err(map_portal_error)?
        .response()
        .map_err(map_portal_error)?;
    let streams = response
        .streams()
        .iter()
        .map(|stream| PortalStream {
            node_id: stream.pipe_wire_node_id(),
            position: stream.position(),
        })
        .collect();
    let restore_token = response.restore_token().map(str::to_owned);
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(map_portal_error)?;
    Ok(PortalResponse {
        streams,
        fd,
        restore_token,
    })
}

fn map_portal_error(error: ashpd::Error) -> Error {
    match error {
        ashpd::Error::Response(ResponseError::Cancelled)
        | ashpd::Error::Portal(ashpd::PortalError::Cancelled(_)) => Error::Cancelled,
        ashpd::Error::Portal(ashpd::PortalError::NotAllowed(message)) => Error::PermissionDenied {
            capability: "screen recording".into(),
            remedy: if message.is_empty() {
                "allow screen sharing in the desktop portal prompt".into()
            } else {
                message
            },
        },
        ashpd::Error::PortalNotFound(_) => Error::Unsupported {
            what: "Wayland screen recording".into(),
            why: "no xdg-desktop-portal ScreenCast implementation is installed".into(),
        },
        other => Error::Platform(format!("screen-cast portal failed: {other}")),
    }
}

fn persist_tokens(path: Option<&std::path::Path>, tokens: &PortalTokenStore) {
    let Some(path) = path else {
        return;
    };
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, tokens.serialise()));
    if let Err(error) = result {
        tracing::warn!(
            %error,
            path = %path.display(),
            "could not persist the portal restore token"
        );
    }
}

fn video_format_parameter() -> Result<Vec<u8>> {
    let object = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::RGBA,
        ),
    );
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(object),
    )
    .map(|serialized| serialized.0.into_inner())
    .map_err(|error| Error::Platform(format!("could not build PipeWire video format: {error:?}")))
}

fn copy_video_buffer(stream: &pw::stream::Stream, user_data: &mut VideoUserData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };
    let chunk_offset = data.chunk().offset() as usize;
    let chunk_size = data.chunk().size() as usize;
    let chunk_stride = data.chunk().stride();
    let format = user_data.format.format();
    let size = user_data.format.size();
    let packed_format = if format == spa::param::video::VideoFormat::BGRx {
        PackedPixelFormat::Bgrx
    } else if format == spa::param::video::VideoFormat::BGRA {
        PackedPixelFormat::Bgra
    } else if format == spa::param::video::VideoFormat::RGBx {
        PackedPixelFormat::Rgbx
    } else if format == spa::param::video::VideoFormat::RGBA {
        PackedPixelFormat::Rgba
    } else {
        *user_data.error.borrow_mut() =
            Some(format!("PipeWire negotiated unsupported format {format:?}"));
        return;
    };
    if size.width == 0 || size.height == 0 {
        return;
    }
    let row_bytes = size.width as usize * 4;
    let source_stride = if chunk_stride == 0 {
        row_bytes
    } else {
        chunk_stride.unsigned_abs() as usize
    };
    let required = source_stride.saturating_mul(size.height as usize);
    let Some(raw) = data.data() else {
        *user_data.error.borrow_mut() =
            Some("PipeWire returned an unmapped DMA-BUF video frame".into());
        return;
    };
    let available_end = chunk_offset.saturating_add(chunk_size).min(raw.len());
    if source_stride < row_bytes || chunk_offset.saturating_add(required) > available_end {
        *user_data.error.borrow_mut() = Some(format!(
            "PipeWire video buffer is short: need {required} bytes at offset {chunk_offset}, \
             received {chunk_size}"
        ));
        return;
    }

    let mut pixels = Vec::with_capacity(row_bytes * size.height as usize);
    for output_row in 0..size.height as usize {
        let source_row = if chunk_stride < 0 {
            size.height as usize - output_row - 1
        } else {
            output_row
        };
        let start = chunk_offset + source_row * source_stride;
        pixels.extend_from_slice(&raw[start..start + row_bytes]);
    }
    let mut queue = user_data.frames.borrow_mut();
    if queue.len() == 4 {
        queue.pop_front();
    }
    queue.push_back(PackedFrame {
        width: size.width,
        height: size.height,
        stride: row_bytes,
        format: packed_format,
        data: pixels,
    });
}

pub(super) struct PipeWireAudio {
    listeners: Vec<pw::stream::StreamListener<AudioUserData>>,
    _streams: Vec<pw::stream::StreamRc>,
    mainloop: pw::main_loop::MainLoopRc,
    microphone: Option<Rc<RefCell<VecDeque<AudioBuffer>>>>,
    system: Option<Rc<RefCell<VecDeque<AudioBuffer>>>>,
    errors: Vec<Rc<RefCell<Option<String>>>>,
}

struct AudioUserData {
    format: spa::param::audio::AudioInfoRaw,
    frames: Rc<RefCell<VecDeque<AudioBuffer>>>,
    error: Rc<RefCell<Option<String>>>,
    next_frame: u64,
}

impl PipeWireAudio {
    pub(super) fn new(microphone: bool, system: bool) -> Result<Option<Self>> {
        if !microphone && !system {
            return Ok(None);
        }
        let mainloop = pw::main_loop::MainLoopRc::new(None)
            .map_err(|error| pipewire_error("create its audio event loop", error))?;
        let context = pw::context::ContextRc::new(&mainloop, None)
            .map_err(|error| pipewire_error("create its audio context", error))?;
        let core = context
            .connect_rc(None)
            .map_err(|error| pipewire_error("connect to the audio graph", error))?;
        let mut listeners = Vec::new();
        let mut streams = Vec::new();
        let mut errors = Vec::new();
        let mut microphone_queue = None;
        let mut system_queue = None;

        for is_system in [false, true] {
            if (is_system && !system) || (!is_system && !microphone) {
                continue;
            }
            let queue = Rc::new(RefCell::new(VecDeque::new()));
            let error = Rc::new(RefCell::new(None));
            let mut props: PropertiesBox = properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => if is_system { "Music" } else { "Communication" },
            };
            if is_system {
                props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
            }
            let stream = pw::stream::StreamRc::new(
                core.clone(),
                if is_system {
                    "scrozz-system-audio"
                } else {
                    "scrozz-microphone"
                },
                props,
            )
            .map_err(|cause| pipewire_error("create an audio capture stream", cause))?;
            let listener = stream
                .add_local_listener_with_user_data(AudioUserData {
                    format: Default::default(),
                    frames: Rc::clone(&queue),
                    error: Rc::clone(&error),
                    next_frame: 0,
                })
                .state_changed(|_, user_data, _, new| {
                    if let pw::stream::StreamState::Error(message) = new {
                        *user_data.error.borrow_mut() = Some(message);
                    }
                })
                .param_changed(|_, user_data, id, param| {
                    let Some(param) = param else {
                        return;
                    };
                    if id != spa::param::ParamType::Format.as_raw() {
                        return;
                    }
                    if let Err(cause) = user_data.format.parse(param) {
                        *user_data.error.borrow_mut() =
                            Some(format!("could not parse PipeWire audio format: {cause:?}"));
                    }
                })
                .process(copy_audio_buffer)
                .register()
                .map_err(|cause| pipewire_error("register audio callbacks", cause))?;
            let values = audio_format_parameter()?;
            let mut params = [Pod::from_bytes(&values).ok_or_else(|| {
                Error::Platform("PipeWire rejected Scrozz's audio format pod".into())
            })?];
            stream
                .connect(
                    spa::utils::Direction::Input,
                    None,
                    pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                    &mut params,
                )
                .map_err(|cause| pipewire_error("connect an audio capture stream", cause))?;
            if is_system {
                system_queue = Some(queue);
            } else {
                microphone_queue = Some(queue);
            }
            errors.push(error);
            listeners.push(listener);
            streams.push(stream);
        }

        Ok(Some(Self {
            listeners,
            _streams: streams,
            mainloop,
            microphone: microphone_queue,
            system: system_queue,
            errors,
        }))
    }

    pub(super) fn poll(&mut self) -> Result<(Option<AudioBuffer>, Option<AudioBuffer>)> {
        self.mainloop.loop_().iterate(pw::loop_::Timeout::None);
        for error in &self.errors {
            if let Some(message) = error.borrow_mut().take() {
                return Err(Error::Platform(format!(
                    "PipeWire audio stream failed: {message}"
                )));
            }
        }
        Ok((
            self.microphone.as_ref().and_then(drain_audio_queue),
            self.system.as_ref().and_then(drain_audio_queue),
        ))
    }
}

impl Drop for PipeWireAudio {
    fn drop(&mut self) {
        tracing::trace!(
            streams = self.listeners.len(),
            "closing PipeWire audio streams"
        );
    }
}

fn drain_audio_queue(queue: &Rc<RefCell<VecDeque<AudioBuffer>>>) -> Option<AudioBuffer> {
    let mut queue = queue.borrow_mut();
    let first = queue.pop_front()?;
    let sample_rate = first.sample_rate;
    let channels = first.channels;
    let start_frame = first.start_frame;
    let mut samples = first.samples;
    let mut expected = start_frame + samples.len() as u64 / u64::from(channels);
    while let Some(next) = queue.pop_front() {
        if next.start_frame > expected {
            let silence_frames = next.start_frame - expected;
            samples.resize(
                samples.len().saturating_add(
                    usize::try_from(silence_frames)
                        .unwrap_or(usize::MAX / 2)
                        .saturating_mul(usize::from(channels)),
                ),
                0.0,
            );
        }
        samples.extend_from_slice(&next.samples);
        expected = next.start_frame + next.samples.len() as u64 / u64::from(next.channels);
    }
    Some(AudioBuffer {
        sample_rate,
        channels,
        start_frame,
        samples,
    })
}

fn audio_format_parameter() -> Result<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(48_000);
    info.set_channels(2);
    let object = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(object),
    )
    .map(|serialized| serialized.0.into_inner())
    .map_err(|error| Error::Platform(format!("could not build PipeWire audio format: {error:?}")))
}

fn copy_audio_buffer(stream: &pw::stream::Stream, user_data: &mut AudioUserData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };
    let offset = data.chunk().offset() as usize;
    let size = data.chunk().size() as usize;
    let channels = user_data.format.channels();
    let rate = user_data.format.rate();
    if rate == 0 || !matches!(channels, 1 | 2) {
        return;
    }
    let Some(raw) = data.data() else {
        *user_data.error.borrow_mut() = Some("PipeWire returned unmapped audio storage".into());
        return;
    };
    let end = offset.saturating_add(size).min(raw.len());
    let bytes = &raw[offset..end];
    if bytes.len() % std::mem::size_of::<f32>() != 0 {
        *user_data.error.borrow_mut() = Some("PipeWire returned a partial f32 sample".into());
        return;
    }
    let samples: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|sample| f32::from_le_bytes(*sample))
        .collect();
    let frames = samples.len() as u64 / u64::from(channels);
    let mut queue = user_data.frames.borrow_mut();
    queue.push_back(AudioBuffer {
        sample_rate: rate,
        channels: channels as u8,
        start_frame: user_data.next_frame,
        samples,
    });
    user_data.next_frame = user_data.next_frame.saturating_add(frames);
}

fn pipewire_error(action: &str, error: pw::Error) -> Error {
    Error::Platform(format!("PipeWire could not {action}: {error}"))
}
