//! X11 still-frame pacing and Wayland portal/PipeWire media acquisition.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ashpd::desktop::ResponseError;
use ashpd::desktop::screencast::{
    CursorMode as PortalCursorMode, Screencast, SelectSourcesOptions, SourceType,
};
use ashpd::desktop::{PersistMode, Session};
use futures_util::StreamExt as _;
use pipewire as pw;
use pw::properties::{PropertiesBox, properties};
use pw::spa;
use pw::spa::pod::Pod;
use scrozz_capture::{
    LinuxSessionEnv, LinuxSessionKind, PortalTokenKey, PortalTokenStore, detect_linux_compositor,
    detect_linux_session, linux_portal_capabilities, portal_cursor_mode, portal_source_type,
    portal_token_path,
};
use scrozz_core::{
    CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, DisplayId, Error, PixelFormat,
    Result, WindowId,
};

use crate::audio::AudioBuffer;
use crate::format::{PackedFrame, PackedPixelFormat, PlacedFrame, compose_placed_frames, crop};

#[derive(Debug, Clone)]
struct RecordingPortalPlan {
    types: u32,
    cursor: u32,
    multiple: bool,
    restore_key: PortalTokenKey,
}

impl RecordingPortalPlan {
    fn for_target(target: &CaptureTarget, show_cursor: bool) -> Self {
        let (types, restore_key) = match target {
            CaptureTarget::Window(_) => (portal_source_type::WINDOW, PortalTokenKey::Window),
            CaptureTarget::AllDisplays => (
                portal_source_type::MONITOR | portal_source_type::VIRTUAL,
                PortalTokenKey::AllDisplays,
            ),
            CaptureTarget::Display(id) => {
                (portal_source_type::MONITOR, PortalTokenKey::display(id))
            }
            CaptureTarget::Region(_) => (portal_source_type::MONITOR, PortalTokenKey::Monitor),
        };
        Self {
            types,
            cursor: if show_cursor {
                portal_cursor_mode::EMBEDDED
            } else {
                portal_cursor_mode::HIDDEN
            },
            multiple: matches!(target, CaptureTarget::AllDisplays),
            restore_key,
        }
    }

    fn narrow(mut self, available_types: u32, available_cursors: u32) -> Option<Self> {
        self.types &= available_types;
        if self.types == 0 {
            return None;
        }
        if self.cursor & available_cursors == 0 {
            self.cursor = portal_cursor_mode::HIDDEN;
        }
        Some(self)
    }
}

pub(super) trait VideoSource {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<PackedFrame>>;
    fn name(&self) -> &str;
    fn backing_scale(&self) -> Option<f64>;
    fn resolved_target(&self) -> Option<CaptureTarget>;
}

pub(super) fn open_video_source(
    target: &CaptureTarget,
    show_cursor: bool,
) -> Result<Box<dyn VideoSource>> {
    let env = LinuxSessionEnv::from_env();
    match detect_linux_session(&env) {
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
    target: CaptureTarget,
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
            target: target.clone(),
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

    fn resolved_target(&self) -> Option<CaptureTarget> {
        Some(self.target.clone())
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

#[derive(Debug, Clone)]
struct PortalStream {
    node_id: u32,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    source_type: Option<SourceType>,
    id: Option<String>,
    mapping_id: Option<String>,
}

struct PortalResponse {
    streams: Vec<PortalStream>,
    fd: OwnedFd,
    restore_token: Option<String>,
    session: Session<Screencast>,
}

struct PortalLifetime {
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    terminal: std::sync::mpsc::Receiver<Error>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl PortalLifetime {
    fn new(runtime: tokio::runtime::Runtime, session: Session<Screencast>) -> Result<Self> {
        let (shutdown, shutdown_rx) = std::sync::mpsc::channel();
        let (terminal_tx, terminal) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("scrozz-portal-session".into())
            .spawn(move || {
                runtime.block_on(async move {
                    {
                        let closed = match session.receive_closed().await {
                            Ok(closed) => closed,
                            Err(error) => {
                                let _ = terminal_tx.send(Error::Platform(format!(
                                    "could not monitor the ScreenCast portal session: {error}"
                                )));
                                let _ = session.close().await;
                                return;
                            }
                        };
                        futures_util::pin_mut!(closed);
                        loop {
                            if shutdown_rx.try_recv().is_ok() {
                                break;
                            }
                            match tokio::time::timeout(Duration::from_millis(20), closed.next()).await
                            {
                                Ok(Some(_)) => {
                                    let _ = terminal_tx.send(Error::TargetGone(
                                        "the ScreenCast portal closed the recording session".into(),
                                    ));
                                    break;
                                }
                                Ok(None) => {
                                    let _ = terminal_tx.send(Error::Platform(
                                        "the ScreenCast portal close monitor ended unexpectedly"
                                            .into(),
                                    ));
                                    break;
                                }
                                Err(_) => {}
                            }
                        }
                    }
                    if let Err(error) = session.close().await {
                        tracing::warn!(%error, "could not explicitly close the ScreenCast portal session");
                    }
                });
            })
            .map_err(Error::Io)?;
        Ok(Self {
            shutdown: Some(shutdown),
            terminal,
            worker: Some(worker),
        })
    }

    fn close(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("the ScreenCast portal monitor thread panicked");
        }
    }

    fn take_terminal(&self) -> Option<Error> {
        self.terminal.try_recv().ok()
    }
}

impl Drop for PortalLifetime {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Debug, Clone, Copy)]
struct PortalGeometry {
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
}

struct VideoUserData {
    format: spa::param::video::VideoInfoRaw,
    frames: Rc<RefCell<VecDeque<PackedFrame>>>,
    error: Rc<RefCell<Option<Error>>>,
    was_streaming: bool,
}

struct PortalVideoSource {
    listeners: Vec<pw::stream::StreamListener<VideoUserData>>,
    _streams: Vec<pw::stream::StreamRc>,
    mainloop: pw::main_loop::MainLoopRc,
    frames: Vec<Rc<RefCell<VecDeque<PackedFrame>>>>,
    errors: Vec<Rc<RefCell<Option<Error>>>>,
    geometries: Vec<PortalGeometry>,
    latest: Vec<Option<PackedFrame>>,
    target: CaptureTarget,
    resolved_target: Option<CaptureTarget>,
    name: String,
    portal: PortalLifetime,
}

impl PortalVideoSource {
    fn new(target: &CaptureTarget, show_cursor: bool, env: &LinuxSessionEnv) -> Result<Self> {
        let compositor = detect_linux_compositor(env);
        let capabilities = linux_portal_capabilities(&compositor);
        let plan = RecordingPortalPlan::for_target(target, show_cursor);
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
            .then(|| tokens.get(&plan.restore_key).map(str::to_owned))
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
                tokens.invalidate(&plan.restore_key);
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
        let PortalResponse {
            streams: descriptors,
            fd,
            restore_token,
            session,
        } = response;
        let portal = PortalLifetime::new(runtime, session)?;
        if let Some(token) = restore_token.as_deref() {
            tokens.set(&plan.restore_key, token);
            persist_tokens(token_path.as_deref(), &tokens);
        }
        if descriptors.is_empty() {
            return Err(Error::Platform(
                "the screen-cast portal returned no PipeWire streams".into(),
            ));
        }
        let resolved_target = validate_portal_selection(target, &descriptors)?;
        if descriptors.len() > 1
            && descriptors
                .iter()
                .any(|stream| stream.position.is_none() || stream.size.is_none())
        {
            return Err(Error::Unsupported {
                what: "recording multiple portal streams".into(),
                why: "the compositor omitted stream positions or displayed sizes, so combining \
                      monitors in buffer-pixel coordinates would guess their layout"
                    .into(),
            });
        }
        if matches!(target, CaptureTarget::Region(_))
            && descriptors
                .iter()
                .any(|stream| stream.position.is_none() || stream.size.is_none())
        {
            return Err(Error::Unsupported {
                what: "recording a region through the desktop portal".into(),
                why: "the compositor omitted monitor position or displayed size, so the logical \
                      region cannot be transformed into buffer pixels"
                    .into(),
            });
        }
        tracing::debug!(
            selection = ?descriptors
                .iter()
                .map(portal_stream_identity)
                .collect::<Vec<_>>(),
            "desktop portal selected recording sources"
        );

        let mainloop = pw::main_loop::MainLoopRc::new(None)
            .map_err(|error| pipewire_error("create its video event loop", error))?;
        let context = pw::context::ContextRc::new(&mainloop, None)
            .map_err(|error| pipewire_error("create its video context", error))?;
        let core = context
            .connect_fd_rc(fd, None)
            .map_err(|error| pipewire_error("connect to the portal remote", error))?;

        let mut listeners = Vec::new();
        let mut streams = Vec::new();
        let mut frames = Vec::new();
        let mut errors = Vec::new();
        let mut geometries = Vec::new();
        for descriptor in descriptors {
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
                    was_streaming: false,
                })
                .state_changed(|_, user_data, _, new| match new {
                    pw::stream::StreamState::Streaming => user_data.was_streaming = true,
                    pw::stream::StreamState::Unconnected if user_data.was_streaming => {
                        *user_data.error.borrow_mut() = Some(Error::TargetGone(
                            "a PipeWire portal video stream disconnected".into(),
                        ));
                    }
                    pw::stream::StreamState::Error(message) => {
                        *user_data.error.borrow_mut() = Some(Error::Platform(message));
                    }
                    _ => {}
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
                        *user_data.error.borrow_mut() = Some(Error::Platform(
                            "portal selected a non-raw video format".into(),
                        ));
                        return;
                    }
                    if let Err(cause) = user_data.format.parse(param) {
                        *user_data.error.borrow_mut() = Some(Error::Platform(format!(
                            "could not parse portal video format: {cause:?}"
                        )));
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

            geometries.push(PortalGeometry {
                position: descriptor.position,
                size: descriptor.size,
            });
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
            geometries,
            target: target.clone(),
            resolved_target,
            name: format!("Wayland portal/PipeWire ({compositor})"),
            portal,
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
        let frames: Vec<_> = self
            .latest
            .iter()
            .map(|frame| frame.as_ref().expect("checked above"))
            .collect();
        let (positions, scale) = portal_pixel_layout(&frames, &self.geometries, &self.target)?;
        let placed: Vec<_> = frames
            .iter()
            .zip(&positions)
            .map(|(frame, position)| PlacedFrame {
                x: position.0,
                y: position.1,
                frame,
            })
            .collect();
        let (frame, origin) = compose_placed_frames(&placed)?;
        if let CaptureTarget::Region(region) = &self.target {
            let scale = scale.expect("region layout requires a portal scale");
            let region_x = scale_portal_coordinate(region.origin.x, scale, "region x")?;
            let region_y = scale_portal_coordinate(region.origin.y, scale, "region y")?;
            let x = (i64::from(region_x) - i64::from(origin.0))
                .try_into()
                .map_err(|_| Error::InvalidRequest("portal region begins off-screen".into()))?;
            let y = (i64::from(region_y) - i64::from(origin.1))
                .try_into()
                .map_err(|_| Error::InvalidRequest("portal region begins off-screen".into()))?;
            let width = scale_portal_extent(region.size.width, scale, "region width")?;
            let height = scale_portal_extent(region.size.height, scale, "region height")?;
            let cropped = crop(&frame, x, y, width, height)?;
            self.resolved_target = Some(self.target.clone());
            return Ok(Some(cropped));
        }
        Ok(Some(frame))
    }
}

impl VideoSource for PortalVideoSource {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<PackedFrame>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(error) = self.portal.take_terminal() {
                return Err(error);
            }
            self.mainloop.loop_().iterate(pw::loop_::Timeout::None);
            for error in &self.errors {
                if let Some(error) = error.borrow_mut().take() {
                    return Err(error);
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

    fn resolved_target(&self) -> Option<CaptureTarget> {
        self.resolved_target.clone()
    }
}

impl Drop for PortalVideoSource {
    fn drop(&mut self) {
        self.portal.close();
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
    if show_cursor && !available_cursors.contains(PortalCursorMode::Embedded) {
        return Err(Error::Unsupported {
            what: "recording the cursor on Wayland".into(),
            why: "the desktop portal does not provide embedded cursor capture for this session"
                .into(),
        });
    }
    let plan = RecordingPortalPlan::for_target(target, show_cursor)
        .narrow(available_sources.bits(), available_cursors.bits())
        .ok_or_else(|| Error::Unsupported {
            what: "the requested portal recording source".into(),
            why: "the desktop portal reports no compatible source or cursor mode".into(),
        })?;
    let sources = match plan.types {
        portal_source_type::MONITOR => SourceType::Monitor.into(),
        portal_source_type::WINDOW => SourceType::Window.into(),
        types
            if types == portal_source_type::MONITOR | portal_source_type::VIRTUAL =>
        {
            SourceType::Monitor | SourceType::Virtual
        }
        _ => {
            return Err(Error::Unsupported {
                what: "the requested portal source combination".into(),
                why: format!("unsupported ScreenCast source mask {}", plan.types),
            });
        }
    };
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(map_portal_error)?;
    let mut options = SelectSourcesOptions::default().set_sources(sources);
    options = options
        .set_cursor_mode(if plan.cursor == portal_cursor_mode::EMBEDDED {
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
    let opened = async {
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
                size: stream.size(),
                source_type: stream.source_type(),
                id: stream.id().map(str::to_owned),
                mapping_id: stream.mapping_id().map(str::to_owned),
            })
            .collect();
        let restore_token = response.restore_token().map(str::to_owned);
        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .map_err(map_portal_error)?;
        Ok::<_, Error>((streams, fd, restore_token))
    }
    .await;
    match opened {
        Ok((streams, fd, restore_token)) => Ok(PortalResponse {
            streams,
            fd,
            restore_token,
            session,
        }),
        Err(error) => {
            if let Err(close_error) = session.close().await {
                tracing::warn!(
                    %close_error,
                    "could not close a failed ScreenCast portal session"
                );
            }
            Err(error)
        }
    }
}

fn validate_portal_selection(
    requested: &CaptureTarget,
    streams: &[PortalStream],
) -> Result<Option<CaptureTarget>> {
    if !matches!(requested, CaptureTarget::AllDisplays) && streams.len() != 1 {
        return Err(Error::Platform(format!(
            "the desktop portal returned {} sources for a single-source request",
            streams.len()
        )));
    }
    for stream in streams {
        let Some(source_type) = stream.source_type else {
            continue;
        };
        let valid = match requested {
            CaptureTarget::Window(_) => source_type == SourceType::Window,
            CaptureTarget::Display(_) | CaptureTarget::Region(_) => {
                source_type == SourceType::Monitor
            }
            CaptureTarget::AllDisplays => {
                matches!(source_type, SourceType::Monitor | SourceType::Virtual)
            }
        };
        if !valid {
            return Err(Error::Platform(format!(
                "the desktop portal returned a {source_type:?} source for {requested:?}"
            )));
        }
    }
    if matches!(requested, CaptureTarget::Region(_)) || streams.len() != 1 {
        return Ok(None);
    }
    let stream = &streams[0];
    let Some(source_type) = stream.source_type else {
        return Ok(None);
    };
    let Some(id) = portal_stream_id(stream) else {
        return Ok(None);
    };
    Ok(match source_type {
        SourceType::Monitor => Some(CaptureTarget::Display(DisplayId(id))),
        SourceType::Window => Some(CaptureTarget::Window(WindowId(id))),
        SourceType::Virtual => None,
    })
}

fn portal_stream_id(stream: &PortalStream) -> Option<String> {
    stream
        .id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| format!("portal:{id}"))
        .or_else(|| {
            stream
                .mapping_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(|id| format!("portal-mapping:{id}"))
        })
}

fn portal_stream_identity(stream: &PortalStream) -> (Option<SourceType>, Option<String>) {
    (stream.source_type, portal_stream_id(stream))
}

type PortalPixelLayout = (Vec<(i32, i32)>, Option<f64>);

fn portal_pixel_layout(
    frames: &[&PackedFrame],
    geometries: &[PortalGeometry],
    target: &CaptureTarget,
) -> Result<PortalPixelLayout> {
    if frames.len() != geometries.len() {
        return Err(Error::Platform(
            "portal stream geometry does not match its buffers".into(),
        ));
    }
    let needs_global_geometry = frames.len() > 1 || matches!(target, CaptureTarget::Region(_));
    if !needs_global_geometry {
        return Ok((vec![(0, 0); frames.len()], None));
    }

    let mut common_scale: Option<f64> = None;
    let mut positions = Vec::with_capacity(frames.len());
    for (frame, geometry) in frames.iter().zip(geometries) {
        let (x, y) = geometry.position.ok_or_else(|| Error::Unsupported {
            what: "positioning desktop portal streams".into(),
            why: "the compositor omitted a source position".into(),
        })?;
        let (width, height) = geometry.size.ok_or_else(|| Error::Unsupported {
            what: "scaling desktop portal streams".into(),
            why: "the compositor omitted a displayed source size".into(),
        })?;
        if width <= 0 || height <= 0 {
            return Err(Error::Platform(format!(
                "the desktop portal returned invalid displayed size {width}x{height}"
            )));
        }
        let x_scale = f64::from(frame.width) / f64::from(width);
        let y_scale = f64::from(frame.height) / f64::from(height);
        if !scales_match(x_scale, y_scale) {
            return Err(Error::Unsupported {
                what: "non-uniformly scaled desktop portal streams".into(),
                why: format!(
                    "the compositor maps {width}x{height} coordinates to a {}x{} buffer",
                    frame.width, frame.height
                ),
            });
        }
        let scale = (x_scale + y_scale) / 2.0;
        if let Some(common) = common_scale
            && !scales_match(common, scale)
        {
            return Err(Error::Unsupported {
                what: "mixed-scale desktop portal recording".into(),
                why: format!(
                    "selected sources use incompatible buffer scales ({common:.3}x and \
                     {scale:.3}x); Scrozz will not guess their pixel-space layout"
                ),
            });
        }
        common_scale = Some(scale);
        positions.push((
            scale_portal_coordinate(f64::from(x), scale, "stream x")?,
            scale_portal_coordinate(f64::from(y), scale, "stream y")?,
        ));
    }
    Ok((positions, common_scale))
}

fn scales_match(left: f64, right: f64) -> bool {
    let tolerance = left.abs().max(right.abs()).max(1.0) * 0.01;
    left.is_finite() && right.is_finite() && (left - right).abs() <= tolerance
}

fn scale_portal_coordinate(value: f64, scale: f64, label: &str) -> Result<i32> {
    let scaled = value * scale;
    if !scaled.is_finite() || scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
        return Err(Error::InvalidRequest(format!(
            "portal {label} exceeds pixel coordinates"
        )));
    }
    Ok(scaled.round() as i32)
}

fn scale_portal_extent(value: f64, scale: f64, label: &str) -> Result<u32> {
    let scaled = value * scale;
    if !scaled.is_finite() || scaled.round() < 1.0 || scaled.round() > f64::from(u32::MAX) {
        return Err(Error::InvalidRequest(format!(
            "portal {label} exceeds pixel dimensions"
        )));
    }
    Ok(scaled.round() as u32)
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
        *user_data.error.borrow_mut() = Some(Error::Unsupported {
            what: "PipeWire video format".into(),
            why: format!("the portal negotiated unsupported format {format:?}"),
        });
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
    let storage = data.type_();
    let Some(raw) = data.data() else {
        *user_data.error.borrow_mut() = Some(if storage == spa::buffer::DataType::DmaBuf {
            Error::Unsupported {
                what: "PipeWire DMA-BUF video buffers".into(),
                why: "this recorder currently accepts mapped MemPtr and MemFd buffers".into(),
            }
        } else {
            Error::Platform(format!(
                "PipeWire returned unmapped {storage:?} video storage"
            ))
        });
        return;
    };
    let available_end = chunk_offset.saturating_add(chunk_size).min(raw.len());
    if source_stride < row_bytes || chunk_offset.saturating_add(required) > available_end {
        *user_data.error.borrow_mut() = Some(Error::Platform(format!(
            "PipeWire video buffer is short: need {required} bytes at offset {chunk_offset}, \
             received {chunk_size}"
        )));
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

pub(super) struct AudioBatch {
    pub(super) microphone: Vec<AudioBuffer>,
    pub(super) system: Vec<AudioBuffer>,
}

const MAX_QUEUED_AUDIO_FRAMES: u64 = 48_000;
const MAX_AUDIO_DISCONTINUITY_FRAMES: u64 = 48_000;

struct AudioUserData {
    format: spa::param::audio::AudioInfoRaw,
    frames: Rc<RefCell<VecDeque<AudioBuffer>>>,
    error: Rc<RefCell<Option<String>>>,
    pts_origin: Rc<RefCell<Option<i64>>>,
    end_frame: u64,
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
        let pts_origin = Rc::new(RefCell::new(None));

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
                    pts_origin: Rc::clone(&pts_origin),
                    end_frame: 0,
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

    pub(super) fn poll(&mut self) -> Result<AudioBatch> {
        self.mainloop.loop_().iterate(pw::loop_::Timeout::None);
        for error in &self.errors {
            if let Some(message) = error.borrow_mut().take() {
                return Err(Error::Platform(format!(
                    "PipeWire audio stream failed: {message}"
                )));
            }
        }
        let microphone = self
            .microphone
            .as_ref()
            .map(drain_audio_queue)
            .unwrap_or_default();
        let system = self
            .system
            .as_ref()
            .map(drain_audio_queue)
            .unwrap_or_default();
        Ok(AudioBatch { microphone, system })
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

fn drain_audio_queue(queue: &Rc<RefCell<VecDeque<AudioBuffer>>>) -> Vec<AudioBuffer> {
    let mut queue = queue.borrow_mut();
    queue.drain(..).collect()
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
    let Some(header) = buffer.find_meta::<spa::buffer::meta::MetaHeader>() else {
        *user_data.error.borrow_mut() =
            Some("PipeWire audio buffer omitted presentation timestamp metadata".into());
        return;
    };
    let pts = header.pts();
    let flags = header.flags();
    if pts < 0 {
        *user_data.error.borrow_mut() =
            Some(format!("PipeWire audio buffer returned invalid PTS {pts}"));
        return;
    }
    if flags.contains(spa::buffer::meta::MetaHeaderFlags::CORRUPTED) {
        *user_data.error.borrow_mut() = Some("PipeWire marked an audio buffer corrupted".into());
        return;
    }
    if flags.contains(spa::buffer::meta::MetaHeaderFlags::GAP) {
        return;
    }
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
    if !samples.len().is_multiple_of(channels as usize) {
        *user_data.error.borrow_mut() =
            Some("PipeWire returned an incomplete interleaved audio frame".into());
        return;
    }
    let origin = {
        let mut origin = user_data.pts_origin.borrow_mut();
        *origin.get_or_insert(pts)
    };
    let mut start_frame = nanoseconds_to_audio_frames(pts.saturating_sub(origin), rate);
    let mut samples = samples;
    if start_frame < user_data.end_frame {
        let overlap_frames = user_data.end_frame - start_frame;
        if overlap_frames > MAX_AUDIO_DISCONTINUITY_FRAMES {
            *user_data.error.borrow_mut() = Some(format!(
                "PipeWire audio timestamp moved backwards by {overlap_frames} frames"
            ));
            return;
        }
        let overlap_samples = usize::try_from(overlap_frames)
            .unwrap_or(usize::MAX)
            .saturating_mul(channels as usize);
        if overlap_samples >= samples.len() {
            return;
        }
        samples.drain(..overlap_samples);
        start_frame = user_data.end_frame;
    }
    let frames = samples.len() as u64 / u64::from(channels);
    if frames == 0 {
        return;
    }
    if start_frame.saturating_sub(user_data.end_frame) > MAX_AUDIO_DISCONTINUITY_FRAMES {
        *user_data.error.borrow_mut() = Some(format!(
            "PipeWire audio timestamp jumped forward by {} frames",
            start_frame.saturating_sub(user_data.end_frame)
        ));
        return;
    }
    let mut queue = user_data.frames.borrow_mut();
    let queued_frames = queue.iter().fold(0_u64, |total, buffer| {
        total.saturating_add(buffer.samples.len() as u64 / u64::from(buffer.channels))
    });
    if queued_frames.saturating_add(frames) > MAX_QUEUED_AUDIO_FRAMES {
        *user_data.error.borrow_mut() = Some(format!(
            "PipeWire audio queue exceeded {MAX_QUEUED_AUDIO_FRAMES} frames"
        ));
        return;
    }
    queue.push_back(AudioBuffer {
        sample_rate: rate,
        channels: channels as u8,
        start_frame,
        samples,
    });
    user_data.end_frame = start_frame.saturating_add(frames);
}

fn nanoseconds_to_audio_frames(nanoseconds: i64, sample_rate: u32) -> u64 {
    let nanoseconds = u64::try_from(nanoseconds).unwrap_or_default();
    ((u128::from(nanoseconds) * u128::from(sample_rate)) / 1_000_000_000)
        .try_into()
        .unwrap_or(u64::MAX)
}

fn pipewire_error(action: &str, error: pw::Error) -> Error {
    Error::Platform(format!("PipeWire could not {action}: {error}"))
}

#[cfg(test)]
mod tests {
    use ashpd::desktop::screencast::SourceType;
    use scrozz_core::{CaptureTarget, WindowId};

    use super::{
        PackedFrame, PackedPixelFormat, PortalGeometry, PortalStream, nanoseconds_to_audio_frames,
        portal_pixel_layout, validate_portal_selection,
    };

    fn frame(width: u32, height: u32) -> PackedFrame {
        PackedFrame {
            width,
            height,
            stride: width as usize * 4,
            format: PackedPixelFormat::Rgba,
            data: vec![0; width as usize * height as usize * 4],
        }
    }

    fn portal_stream(source_type: Option<SourceType>, id: Option<&str>) -> PortalStream {
        PortalStream {
            node_id: 7,
            position: Some((0, 0)),
            size: Some((100, 50)),
            source_type,
            id: id.map(str::to_owned),
            mapping_id: None,
        }
    }

    #[test]
    fn portal_provenance_uses_selected_identity_not_requested_identity() {
        let requested = CaptureTarget::Window(WindowId("requested-window".into()));
        let selected = portal_stream(Some(SourceType::Window), Some("selected-window"));
        assert_eq!(
            validate_portal_selection(&requested, &[selected]).unwrap(),
            Some(CaptureTarget::Window(WindowId(
                "portal:selected-window".into()
            )))
        );

        let unidentified = portal_stream(None, Some("untyped-source"));
        assert_eq!(
            validate_portal_selection(&requested, &[unidentified]).unwrap(),
            None
        );
    }

    #[test]
    fn portal_selection_rejects_an_unexpected_source_type() {
        let requested = CaptureTarget::Window(WindowId("requested-window".into()));
        let selected = portal_stream(Some(SourceType::Monitor), Some("monitor"));
        assert!(validate_portal_selection(&requested, &[selected]).is_err());
    }

    #[test]
    fn portal_layout_transforms_compositor_positions_to_buffer_pixels() {
        let left = frame(200, 100);
        let right = frame(200, 100);
        let geometries = [
            PortalGeometry {
                position: Some((-100, 0)),
                size: Some((100, 50)),
            },
            PortalGeometry {
                position: Some((0, 0)),
                size: Some((100, 50)),
            },
        ];
        let (positions, scale) =
            portal_pixel_layout(&[&left, &right], &geometries, &CaptureTarget::AllDisplays)
                .unwrap();
        assert_eq!(positions, vec![(-200, 0), (0, 0)]);
        assert_eq!(scale, Some(2.0));
    }

    #[test]
    fn portal_layout_rejects_mixed_buffer_scales() {
        let left = frame(200, 100);
        let right = frame(100, 50);
        let geometries = [
            PortalGeometry {
                position: Some((0, 0)),
                size: Some((100, 50)),
            },
            PortalGeometry {
                position: Some((100, 0)),
                size: Some((100, 50)),
            },
        ];
        let error = portal_pixel_layout(&[&left, &right], &geometries, &CaptureTarget::AllDisplays)
            .unwrap_err();
        assert!(error.to_string().contains("mixed-scale"));
    }

    #[test]
    fn pipewire_nanosecond_pts_convert_to_audio_frames() {
        assert_eq!(nanoseconds_to_audio_frames(1_000_000_000, 48_000), 48_000);
        assert_eq!(nanoseconds_to_audio_frames(10_000_000, 48_000), 480);
    }
}
