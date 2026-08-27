//! Scrozz-owned `wlr-layer-shell` transport.
//!
//! A windowing toolkit cannot promote an existing `xdg_toplevel` to
//! layer-shell: a `wl_surface` may have only one role. This module therefore
//! owns the complete Wayland object graph and a reusable SHM swapchain. Frames
//! arrive as premultiplied RGBA8 and are converted to native
//! `WL_SHM_FORMAT_ARGB8888` representation before attachment.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::io::ErrorKind;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::ptr::{NonNull, null_mut};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, ftruncate, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use scrozz_core::{Error, LogicalSize, Result};
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

use super::capability::LayerShellProbe;
use super::layer::{
    KeyboardInteractivity, Layer, LayerSurfaceConfig, clamp_to_available, extent_probe_config,
};
use super::region::InputRegion;
use super::surface::{
    BufferLayout, FrameCommit, LayerSurfaceEvent, OutputInfo, OutputSelector, PointerAxis,
    PointerAxisSource, PointerButtonState, RemapState, SurfaceCloseReason, SurfacePoint,
    SurfacePointerEvent, SurfaceScale, SurfaceSize, TransportState, first_reusable_buffer,
    submitted_frame_is_stale, write_argb8888,
};
use wayland_client::backend::WaylandError;

const OUTPUT_VERSION: u32 = 4;
const SEAT_VERSION: u32 = 9;
const BUFFER_COUNT: usize = 2;

/// Asks the running compositor whether it advertises `zwlr_layer_shell_v1`.
///
/// A failed connection is reported as [`LayerShellProbe::NotProbed`], not as an
/// absent protocol: only a compositor that answered can establish absence. If a
/// launcher supplies only `WAYLAND_SOCKET`, probing is deliberately skipped;
/// a second client on a duplicated descriptor would corrupt the inherited
/// Wayland byte stream needed by the real window host.
#[must_use]
pub fn probe() -> LayerShellProbe {
    let Ok(conn) = connect_for_probe() else {
        return LayerShellProbe::NotProbed;
    };
    probe_connection(&conn)
}

fn probe_connection(conn: &Connection) -> LayerShellProbe {
    let Ok((globals, _queue)) = registry_queue_init::<ProbeState>(conn) else {
        return LayerShellProbe::NotProbed;
    };
    let wanted = ZwlrLayerShellV1::interface().name;
    let found = globals.contents().with_list(|globals| {
        globals
            .iter()
            .find(|global| global.interface == wanted)
            .map(|global| global.version)
    });
    match found {
        Some(version) => LayerShellProbe::Present { version },
        None => LayerShellProbe::Absent,
    }
}

struct ProbeState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ProbeState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Explains why an existing toolkit window cannot become a layer surface.
#[must_use]
pub fn refusal() -> Error {
    Error::Unsupported {
        what: "promoting the existing window to a layer surface".into(),
        why: "a wl_surface can hold only one role for its lifetime, and winit has already \
              given this one the xdg_toplevel role; calling get_layer_surface on it would \
              raise a protocol error, which disconnects the whole client rather than \
              failing gracefully. Layer-shell anchoring needs a surface Scrozz creates \
              itself."
            .into(),
    }
}

/// Enumerates the outputs currently advertised by the Wayland compositor.
///
/// Names and descriptions are `None` when the compositor exposes an older
/// `wl_output` version. Integer scale remains available where advertised.
///
/// # Errors
///
/// Returns an error if no Wayland connection can be opened or the registry
/// round trip fails. A `WAYLAND_SOCKET`-only launch cannot be enumerated without
/// consuming the one connection reserved for the real surface.
pub fn enumerate_outputs() -> Result<Vec<OutputInfo>> {
    let conn = connect_for_probe()?;
    let (globals, mut queue) = registry_queue_init::<SessionState>(&conn)
        .map_err(|error| platform("Wayland registry did not initialise", error))?;
    let qh = queue.handle();
    let mut state = SessionState::new(SurfaceSize::new(0, 0), false, true);
    bind_initial_outputs(&globals, &qh, &mut state);
    queue
        .roundtrip(&mut state)
        .map_err(|error| platform("Wayland output enumeration failed", error))?;
    let outputs = state.output_infos();
    state.release_bindings();
    if globals.registry().is_alive() {
        globals.destroy();
    }
    let _ = conn.flush();
    Ok(outputs)
}

/// A Scrozz-owned layer surface with a double-buffered SHM transport.
///
/// The object is intended to be pumped once per host frame. Construction and
/// rare extent/remap reconfiguration perform bounded registry round trips;
/// ordinary frame pumping blocks only for the timeout the caller supplies.
pub struct LayerShellSession {
    conn: Connection,
    globals: Option<GlobalList>,
    state: SessionState,
    queue: wayland_client::EventQueue<SessionState>,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    shell: ZwlrLayerShellV1,
    surface: wl_surface::WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    fractional_scale: Option<WpFractionalScaleV1>,
    viewporter: Option<WpViewporter>,
    viewport: Option<WpViewport>,
    buffers: Vec<ShmBuffer>,
    next_buffer_id: u64,
    selector: OutputSelector,
    desired_config: LayerSurfaceConfig,
    config: LayerSurfaceConfig,
    remap: RemapState,
    version: u32,
    torn_down: bool,
}

impl LayerShellSession {
    /// Creates a surface on the compositor-selected output.
    ///
    /// This preserves the original constructor contract. Use
    /// [`Self::with_output`] to request a named output.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid layer configuration, missing mandatory
    /// protocol, failed connection, or missing initial configure.
    pub fn new(config: &LayerSurfaceConfig) -> Result<Self> {
        Self::with_output(config, OutputSelector::CompositorDefault)
    }

    /// Creates a surface using an explicit output-selection policy.
    ///
    /// [`OutputSelector::CompositorDefault`] passes `None` to layer-shell.
    /// [`OutputSelector::Named`] resolves an exact `wl_output.name` and fails
    /// rather than silently choosing a different display. A concrete
    /// bottom-left size is first measured against the output's available extent
    /// with an unmapped all-edge configure, then clamped and restored to its
    /// final bottom-left anchor before the first pixel buffer is attached.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid configuration, absent protocol, absent
    /// named output, failed connection, or missing initial configure.
    pub fn with_output(config: &LayerSurfaceConfig, selector: OutputSelector) -> Result<Self> {
        validate_config(config)?;
        if matches!(&selector, OutputSelector::Named(name) if name.is_empty()) {
            return Err(Error::InvalidRequest(
                "an explicit Wayland output name cannot be empty".into(),
            ));
        }

        let conn = connect()?;
        let (globals, queue) = registry_queue_init::<SessionState>(&conn)
            .map_err(|error| platform("Wayland registry did not initialise", error))?;
        let probe_config = extent_probe_config(config);
        let initial_config = probe_config.as_ref().unwrap_or(config);
        let mut session = Self::from_registry(conn, globals, queue, initial_config, selector)?;
        session.desired_config = config.clone();

        if probe_config.is_some() {
            session.finish_extent_probe()?;
        }
        session.state.extent_changed = false;

        Ok(session)
    }

    fn from_registry(
        conn: Connection,
        globals: GlobalList,
        mut queue: wayland_client::EventQueue<SessionState>,
        config: &LayerSurfaceConfig,
        selector: OutputSelector,
    ) -> Result<Self> {
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals
            .bind(&qh, 1..=6, ())
            .map_err(|error| platform("compositor does not offer wl_compositor", error))?;
        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=2, ())
            .map_err(|error| platform("compositor does not offer wl_shm", error))?;
        let shell: ZwlrLayerShellV1 =
            globals
                .bind(&qh, 1..=4, ())
                .map_err(|_| Error::Unsupported {
                    what: "layer-shell overlay".into(),
                    why: "this compositor does not implement zwlr_layer_shell_v1, so a \
                          client cannot anchor a surface to a screen edge"
                        .into(),
                })?;
        let fractional_manager: Option<WpFractionalScaleManagerV1> =
            globals.bind(&qh, 1..=1, ()).ok();
        let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
        let fractional_enabled = fractional_manager.is_some() && viewporter.is_some();

        let mut state = SessionState::new(
            SurfaceSize::new(config.width, config.height),
            fractional_enabled,
            compositor.version() >= 3,
        );
        bind_initial_outputs(&globals, &qh, &mut state);
        bind_initial_seats(&globals, &qh, &mut state);
        queue
            .roundtrip(&mut state)
            .map_err(|error| platform("Wayland output discovery failed", error))?;

        let selected = state.select_output(&selector)?;
        state.selected_output_global = selected.as_ref().map(|(global, _)| *global);
        state.recompute_integer_scale();
        let selected_proxy = selected.as_ref().map(|(_, output)| output);

        let surface = compositor.create_surface(&qh, ());
        state.target_surface = Some(surface.clone());

        let viewport = if fractional_enabled {
            viewporter
                .as_ref()
                .map(|manager| manager.get_viewport(&surface, &qh, ()))
        } else {
            None
        };
        let fractional_scale = if fractional_enabled {
            fractional_manager
                .as_ref()
                .map(|manager| manager.get_fractional_scale(&surface, &qh, ()))
        } else {
            None
        };

        let version = shell.version();
        let layer_surface = shell.get_layer_surface(
            &surface,
            selected_proxy,
            wire_layer(config.layer),
            config.namespace.to_string(),
            &qh,
            (),
        );
        apply(&layer_surface, config, version);
        if fractional_enabled && surface.version() >= 3 {
            surface.set_buffer_scale(1);
        }
        surface.commit();

        let mut session = Self {
            conn,
            globals: Some(globals),
            state,
            queue,
            compositor,
            shm,
            shell,
            surface,
            layer_surface,
            fractional_manager,
            fractional_scale,
            viewporter,
            viewport,
            buffers: Vec::new(),
            next_buffer_id: 1,
            selector,
            desired_config: config.clone(),
            config: config.clone(),
            remap: RemapState::Ready,
            version,
            torn_down: false,
        };
        if let Err(error) = session.queue.roundtrip(&mut session.state) {
            return Err(session.connection_error("initial layer-shell configure failed", error));
        }
        session.finish_dispatch();
        if session.is_closed() {
            return Err(Error::TargetGone(
                "the compositor closed the layer surface during creation".into(),
            ));
        }
        if session.state.transport.granted().is_none() {
            return Err(Error::Platform(
                "layer-shell surface received no initial configure".into(),
            ));
        }
        session.prepare_surface_state()?;
        Ok(session)
    }

    /// Re-applies the layer, anchors, logical size, margins and input policy.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration is invalid, the surface has
    /// closed, or the compositor cannot be reached.
    pub fn reconfigure(&mut self, config: &LayerSurfaceConfig) -> Result<()> {
        self.ensure_open()?;
        validate_config(config)?;
        if config.namespace != self.config.namespace {
            return Err(Error::InvalidRequest(
                "a layer-surface namespace is immutable after creation".into(),
            ));
        }
        if self.version < 2 && config.layer != self.config.layer {
            return Err(Error::InvalidRequest(
                "this version of layer-shell cannot change an existing surface's layer".into(),
            ));
        }
        self.desired_config = config.clone();
        if extent_probe_config(config).is_some() {
            return self.refresh_available_extent();
        }
        self.apply_configuration(config)?;
        self.state.extent_changed = false;
        Ok(())
    }

    fn apply_configuration(&mut self, config: &LayerSurfaceConfig) -> Result<()> {
        self.ensure_open()?;
        validate_config(config)?;
        if config.namespace != self.config.namespace {
            return Err(Error::InvalidRequest(
                "a layer-surface namespace is immutable after creation".into(),
            ));
        }
        if self.version < 2 && config.layer != self.config.layer {
            return Err(Error::InvalidRequest(
                "this version of layer-shell cannot change an existing surface's layer".into(),
            ));
        }
        self.config = config.clone();
        self.state
            .transport
            .begin_reconfigure(SurfaceSize::new(config.width, config.height));
        apply(&self.layer_surface, config, self.version);
        if !self.is_mapped() {
            self.remap = RemapState::ConfigurePending;
        }
        self.surface.commit();
        if let Err(error) = self.queue.roundtrip(&mut self.state) {
            return Err(self.connection_error("Wayland reconfigure roundtrip failed", error));
        }
        self.finish_dispatch();
        self.ensure_open()?;
        if self.state.transport.granted().is_none() {
            return Err(Error::Platform(
                "layer-shell surface received no configure after resizing".into(),
            ));
        }
        self.remap.configured();
        self.prepare_surface_state()
    }

    /// Repeats the bufferless available-extent negotiation after an output,
    /// panel, or work-area change.
    ///
    /// Returns `true` when a new final bottom-left configuration was applied.
    ///
    /// # Errors
    ///
    /// Returns an error if either configure round trip fails or the compositor
    /// no longer grants a concrete usable extent.
    pub fn refresh_extent_if_needed(&mut self) -> Result<bool> {
        if !self.state.extent_changed {
            return Ok(false);
        }
        if extent_probe_config(&self.desired_config).is_none() {
            self.state.extent_changed = false;
            return Ok(false);
        }
        self.refresh_available_extent()?;
        Ok(true)
    }

    fn refresh_available_extent(&mut self) -> Result<()> {
        let Some(probe) = extent_probe_config(&self.desired_config) else {
            self.state.extent_changed = false;
            return Ok(());
        };
        if self.is_mapped() {
            self.unmap()?;
        }
        self.apply_configuration(&probe)?;
        self.finish_extent_probe()
    }

    fn finish_extent_probe(&mut self) -> Result<()> {
        let available = self.configured_logical_size().ok_or_else(|| {
            Error::Platform("layer-shell extent probe returned no concrete output size".into())
        })?;
        let final_config = clamp_to_available(
            &self.desired_config,
            LogicalSize::new(f64::from(available.width), f64::from(available.height)),
        );
        self.apply_configuration(&final_config)?;
        self.state.extent_changed = false;
        Ok(())
    }

    /// Submits one premultiplied RGBA8 frame.
    ///
    /// `frame_size` must match [`Self::current_buffer_size`]. `stride` is the
    /// byte distance between source rows and must hold four bytes per pixel; the
    /// slice must contain exactly `stride × frame_size.height` bytes in R, G, B,
    /// A order. RGB channels are expected to be premultiplied by alpha. The
    /// transport converts them to native-endian Wayland ARGB8888, attaches,
    /// damages and commits.
    ///
    /// When both SHM buffers remain busy, no buffer is overwritten and
    /// [`FrameCommit::BuffersBusy`] is returned so a 60 Hz host can drop the
    /// frame and try again after pumping events. A configure or scale update
    /// received after the caller chose its frame size returns
    /// [`FrameCommit::SurfaceChanged`] so the caller can rerender instead of
    /// treating a normal output transition as fatal. The first submission after
    /// [`Self::unmap`] starts layer-shell's mandatory configure handshake and
    /// returns [`FrameCommit::AwaitingConfigure`] without consuming the frame.
    ///
    /// # Errors
    ///
    /// Returns an error for a closed or unconfigured surface, invalid dimensions
    /// or byte length, allocation failure, or Wayland I/O failure.
    pub fn commit_pixels(
        &mut self,
        frame_size: SurfaceSize,
        stride: usize,
        premultiplied_rgba: &[u8],
    ) -> Result<FrameCommit> {
        self.commit_frame(frame_size, stride, premultiplied_rgba, None)
    }

    /// Submits pixels and their matching input region in one surface commit.
    ///
    /// Unlike calling [`Self::set_input_region`] before [`Self::commit_pixels`],
    /// a dropped frame cannot expose hit targets for pixels the compositor never
    /// received. The region remains unchanged when the frame is rejected as
    /// busy, awaiting configure, or stale.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::commit_pixels`].
    pub fn commit_pixels_with_input_region(
        &mut self,
        frame_size: SurfaceSize,
        stride: usize,
        premultiplied_rgba: &[u8],
        input_region: &InputRegion,
    ) -> Result<FrameCommit> {
        self.commit_frame(frame_size, stride, premultiplied_rgba, Some(input_region))
    }

    fn commit_frame(
        &mut self,
        frame_size: SurfaceSize,
        stride: usize,
        premultiplied_rgba: &[u8],
        input_region: Option<&InputRegion>,
    ) -> Result<FrameCommit> {
        let before = self.state.transport.frame_signature();
        self.poll_events()?;
        self.ensure_open()?;
        let remapping = !self.remap.is_ready();
        if self.state.extent_changed
            || (remapping && extent_probe_config(&self.desired_config).is_some())
        {
            self.refresh_available_extent()?;
            return Ok(if remapping {
                FrameCommit::AwaitingConfigure
            } else {
                FrameCommit::SurfaceChanged
            });
        }
        if !self.remap.is_ready() {
            self.request_remap_configure()?;
            return Ok(FrameCommit::AwaitingConfigure);
        }

        let size = self.current_buffer_size().ok_or_else(|| {
            Error::InvalidRequest(
                "cannot submit pixels before the layer surface has a concrete logical size".into(),
            )
        })?;
        if submitted_frame_is_stale(before, self.state.transport.frame_signature(), frame_size) {
            return Ok(FrameCommit::SurfaceChanged);
        }
        let layout = BufferLayout::new(size)
            .map_err(|message| Error::InvalidRequest(message.to_string()))?;
        layout
            .validate_pixels(frame_size, stride, premultiplied_rgba)
            .map_err(Error::InvalidRequest)?;

        self.retire_stale_buffers(layout);
        self.ensure_buffer_pair(layout)?;
        let Some(index) = first_reusable_buffer(
            self.buffers
                .iter()
                .map(|buffer| (buffer.layout, buffer.busy)),
            layout,
        ) else {
            return Ok(FrameCommit::BuffersBusy);
        };

        let buffer_damage = if self.surface.version() >= 4 {
            Some((
                i32::try_from(size.width).map_err(|_| {
                    Error::InvalidRequest("buffer width does not fit Wayland damage".into())
                })?,
                i32::try_from(size.height).map_err(|_| {
                    Error::InvalidRequest("buffer height does not fit Wayland damage".into())
                })?,
            ))
        } else {
            None
        };
        let surface_damage = if buffer_damage.is_none() {
            let logical = self.configured_logical_size().ok_or_else(|| {
                Error::InvalidRequest("surface logical size is not configured".into())
            })?;
            Some((
                i32::try_from(logical.width).map_err(|_| {
                    Error::InvalidRequest("logical width does not fit Wayland damage".into())
                })?,
                i32::try_from(logical.height).map_err(|_| {
                    Error::InvalidRequest("logical height does not fit Wayland damage".into())
                })?,
            ))
        } else {
            None
        };
        write_argb8888(
            premultiplied_rgba,
            stride,
            size,
            self.buffers[index].memory.as_mut_slice(),
        );
        self.prepare_surface_state()?;
        if let Some(region) = input_region {
            self.stage_input_region(region);
        }
        let buffer = self.buffers[index].proxy.clone();
        self.surface.attach(Some(&buffer), 0, 0);
        if let Some((width, height)) = buffer_damage {
            self.surface.damage_buffer(0, 0, width, height);
        } else if let Some((width, height)) = surface_damage {
            self.surface.damage(0, 0, width, height);
        }
        self.surface.commit();
        self.buffers[index].busy = true;
        self.state.transport.set_mapped(true);
        self.flush("Wayland frame commit failed")?;
        Ok(FrameCommit::Committed)
    }

    /// Attaches a null buffer and commits, unmapping the surface.
    ///
    /// Existing SHM storage remains alive until each compositor release arrives.
    /// Layer-shell resets its role state on unmap, so the first later
    /// [`Self::commit_pixels`] requests a fresh configure and returns
    /// [`FrameCommit::AwaitingConfigure`]; a submission after that configure
    /// maps the same surface again.
    ///
    /// # Errors
    ///
    /// Returns an error if the surface closed or the connection cannot flush.
    pub fn unmap(&mut self) -> Result<()> {
        self.ensure_open()?;
        if !self.is_mapped() {
            return Ok(());
        }
        self.surface.attach(None, 0, 0);
        self.surface.commit();
        self.state.transport.set_mapped(false);
        self.remap.after_unmap();
        self.flush("Wayland unmap failed")
    }

    /// Sets which logical rectangles accept pointer and touch input.
    ///
    /// The update is committed immediately while the surface is mapped. Before
    /// the first map, or while waiting for the configure required after an
    /// unmap, it is included in the next legal surface commit. An empty region
    /// means complete click-through; no region means the entire surface accepts
    /// input.
    ///
    /// # Errors
    ///
    /// Returns an error if the surface closed or the connection cannot flush.
    pub fn set_input_region(&mut self, region: &InputRegion) -> Result<()> {
        self.ensure_open()?;
        self.stage_input_region(region);
        if self.is_mapped() {
            self.surface.commit();
            self.flush("Wayland input-region update failed")
        } else {
            Ok(())
        }
    }

    fn stage_input_region(&self, region: &InputRegion) {
        let qh = self.queue.handle();
        match region {
            InputRegion::Everything => self.surface.set_input_region(None),
            InputRegion::Nothing => {
                let empty = self.compositor.create_region(&qh, ());
                self.surface.set_input_region(Some(&empty));
                empty.destroy();
            }
            InputRegion::Rects(rects) => {
                let shape = self.compositor.create_region(&qh, ());
                for rect in rects {
                    if rect.width == 0 || rect.height == 0 {
                        continue;
                    }
                    shape.add(
                        rect.x,
                        rect.y,
                        i32::try_from(rect.width).unwrap_or(i32::MAX),
                        i32::try_from(rect.height).unwrap_or(i32::MAX),
                    );
                }
                self.surface.set_input_region(Some(&shape));
                shape.destroy();
            }
        }
    }

    /// Reads and dispatches events, waiting no longer than `timeout`.
    ///
    /// A zero timeout is nonblocking. If events were already queued they are
    /// dispatched without polling. The return value is the number of protocol
    /// events dispatched; use [`Self::drain_events`] for typed host events.
    ///
    /// # Errors
    ///
    /// Returns an error if polling, reading, dispatching or flushing fails.
    pub fn pump_events(&mut self, timeout: Duration) -> Result<usize> {
        let mut dispatched = match self.queue.dispatch_pending(&mut self.state) {
            Ok(count) => count,
            Err(error) => {
                return Err(self.connection_error("Wayland event dispatch failed", error));
            }
        };
        self.finish_dispatch();
        if dispatched > 0 || self.is_closed() {
            return Ok(dispatched);
        }

        self.flush("Wayland event flush failed")?;
        let read_result = if let Some(guard) = self.queue.prepare_read() {
            let started = Instant::now();
            let mut ready = false;
            let poll_result: std::result::Result<(), String> = loop {
                let remaining = timeout.saturating_sub(started.elapsed());
                let timespec = match Timespec::try_from(remaining) {
                    Ok(timespec) => timespec,
                    Err(_) => {
                        break Err("Wayland event-pump timeout is too large".to_string());
                    }
                };
                let result = {
                    let fd = guard.connection_fd();
                    let mut fds = [PollFd::new(
                        &fd,
                        PollFlags::IN | PollFlags::ERR | PollFlags::HUP,
                    )];
                    match poll(&mut fds, Some(&timespec)) {
                        Ok(count) => {
                            let revents = fds[0].revents();
                            if revents.contains(PollFlags::NVAL) {
                                Err("Wayland socket poll returned an invalid descriptor".into())
                            } else {
                                ready = count > 0
                                    && revents.intersects(
                                        PollFlags::IN | PollFlags::ERR | PollFlags::HUP,
                                    );
                                Ok(())
                            }
                        }
                        Err(rustix::io::Errno::INTR) if started.elapsed() < timeout => continue,
                        Err(rustix::io::Errno::INTR) => Ok(()),
                        Err(error) => Err(format!("Wayland socket poll failed: {error}")),
                    }
                };
                break result;
            };

            match poll_result {
                Ok(()) if ready => match guard.read() {
                    Ok(_) => Ok(()),
                    Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => Ok(()),
                    Err(error) => Err(format!("Wayland socket read failed: {error}")),
                },
                Ok(()) => {
                    drop(guard);
                    Ok(())
                }
                Err(error) => {
                    drop(guard);
                    Err(error)
                }
            }
        } else {
            Ok(())
        };

        if let Err(message) = read_result {
            self.state
                .transport
                .close(SurfaceCloseReason::ConnectionLost(message.clone()));
            return Err(Error::Platform(message));
        }

        match self.queue.dispatch_pending(&mut self.state) {
            Ok(count) => dispatched += count,
            Err(error) => {
                return Err(self.connection_error("Wayland event dispatch failed", error));
            }
        }
        self.finish_dispatch();
        Ok(dispatched)
    }

    /// Dispatches all immediately available events without waiting.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be pumped.
    pub fn poll_events(&mut self) -> Result<usize> {
        self.pump_events(Duration::ZERO)
    }

    /// Removes and returns all typed events accumulated so far.
    #[must_use]
    pub fn drain_events(&mut self) -> Vec<LayerSurfaceEvent> {
        self.state.transport.take_events()
    }

    /// The compositor-resolved logical surface size.
    #[must_use]
    pub fn configured_logical_size(&self) -> Option<SurfaceSize> {
        self.state.transport.logical_size()
    }

    /// The exact preferred scale in 120ths.
    #[must_use]
    pub fn current_scale(&self) -> SurfaceScale {
        self.state.transport.scale()
    }

    /// The outward-rounded pixel dimensions required for the next frame.
    #[must_use]
    pub fn current_buffer_size(&self) -> Option<SurfaceSize> {
        self.state.transport.buffer_size()
    }

    /// Whether fractional-scale and viewporter are both active.
    #[must_use]
    pub fn uses_fractional_scale(&self) -> bool {
        self.fractional_scale.is_some() && self.viewport.is_some()
    }

    /// The raw layer-shell configure size retained for older callers.
    #[must_use]
    pub fn granted_size(&self) -> Option<(u32, u32)> {
        self.state
            .transport
            .granted()
            .map(|size| (size.width, size.height))
    }

    /// Whether a non-null pixel buffer is currently mapped.
    #[must_use]
    pub fn is_mapped(&self) -> bool {
        self.state.transport.is_mapped()
    }

    /// Whether layer-shell closed the surface or its selected output vanished.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.transport.is_closed()
    }

    /// Why the surface closed, if it has closed.
    #[must_use]
    pub fn close_reason(&self) -> Option<&SurfaceCloseReason> {
        self.state.transport.close_reason()
    }

    /// The output-selection policy used to create this surface.
    #[must_use]
    pub const fn output_selector(&self) -> &OutputSelector {
        &self.selector
    }

    /// A current snapshot of outputs known to this connection.
    #[must_use]
    pub fn outputs(&self) -> Vec<OutputInfo> {
        self.state.output_infos()
    }

    /// The negotiated `zwlr_layer_shell_v1` version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Explicitly tears down the surface and flushes destructor requests.
    ///
    /// Dropping the session performs the same teardown and ignores only the
    /// final flush error.
    ///
    /// # Errors
    ///
    /// Returns an error if the final Wayland flush fails.
    pub fn shutdown(mut self) -> Result<()> {
        self.teardown()
    }

    fn ensure_open(&self) -> Result<()> {
        if self.torn_down {
            return Err(Error::TargetGone(
                "the layer-shell transport was shut down".into(),
            ));
        }
        if self.is_closed() {
            return Err(Error::TargetGone(
                "the layer-shell surface or its selected output no longer exists".into(),
            ));
        }
        Ok(())
    }

    fn request_remap_configure(&mut self) -> Result<()> {
        if !self.remap.request_configure() {
            return self.flush("Wayland remap configure request failed");
        }
        self.state
            .transport
            .begin_reconfigure(SurfaceSize::new(self.config.width, self.config.height));
        apply(&self.layer_surface, &self.config, self.version);
        if let Some(viewport) = &self.viewport {
            if self.surface.version() >= 3 {
                self.surface.set_buffer_scale(1);
            }
            if self.config.width == 0 || self.config.height == 0 {
                viewport.set_destination(-1, -1);
            } else {
                viewport.set_destination(
                    i32::try_from(self.config.width)
                        .map_err(|_| Error::InvalidRequest("logical width exceeds i32".into()))?,
                    i32::try_from(self.config.height)
                        .map_err(|_| Error::InvalidRequest("logical height exceeds i32".into()))?,
                );
            }
        } else if self.surface.version() >= 3 {
            let integer = self.current_scale().units_120() / 120;
            self.surface.set_buffer_scale(
                i32::try_from(integer)
                    .map_err(|_| Error::InvalidRequest("buffer scale exceeds i32".into()))?,
            );
        }
        self.surface.commit();
        if let Err(error) = self.queue.roundtrip(&mut self.state) {
            return Err(self.connection_error("Wayland remap configure failed", error));
        }
        self.finish_dispatch();
        self.ensure_open()?;
        if self.state.transport.granted().is_none() {
            return Err(Error::Platform(
                "layer-shell surface received no configure while remapping".into(),
            ));
        }
        self.remap.configured();
        self.prepare_surface_state()?;
        self.state.extent_changed = false;
        self.flush("Wayland remap configure request failed")
    }

    fn prepare_surface_state(&self) -> Result<()> {
        let logical = self.configured_logical_size().ok_or_else(|| {
            Error::InvalidRequest("surface has no concrete configured logical size".into())
        })?;
        let logical_width = i32::try_from(logical.width)
            .map_err(|_| Error::InvalidRequest("logical width exceeds i32".into()))?;
        let logical_height = i32::try_from(logical.height)
            .map_err(|_| Error::InvalidRequest("logical height exceeds i32".into()))?;

        if let Some(viewport) = &self.viewport {
            if self.surface.version() >= 3 {
                self.surface.set_buffer_scale(1);
            }
            viewport.set_destination(logical_width, logical_height);
        } else if self.surface.version() >= 3 {
            let integer = self.current_scale().units_120() / 120;
            self.surface.set_buffer_scale(
                i32::try_from(integer)
                    .map_err(|_| Error::InvalidRequest("buffer scale exceeds i32".into()))?,
            );
        }
        Ok(())
    }

    fn ensure_buffer_pair(&mut self, layout: BufferLayout) -> Result<()> {
        let existing = self
            .buffers
            .iter()
            .filter(|buffer| buffer.layout == layout)
            .count();
        if existing >= BUFFER_COUNT {
            return Ok(());
        }

        let mut created = Vec::with_capacity(BUFFER_COUNT - existing);
        for _ in existing..BUFFER_COUNT {
            let id = self.next_buffer_id;
            self.next_buffer_id = self
                .next_buffer_id
                .checked_add(1)
                .ok_or_else(|| Error::Platform("Wayland SHM buffer identifier overflow".into()))?;
            created.push(ShmBuffer::new(id, layout, &self.shm, &self.queue.handle())?);
        }
        self.buffers.extend(created);
        Ok(())
    }

    fn retire_stale_buffers(&mut self, current: BufferLayout) {
        self.buffers
            .retain(|buffer| buffer.busy || buffer.layout == current);
    }

    fn finish_dispatch(&mut self) {
        for released in self.state.released_buffers.drain(..) {
            if let Some(buffer) = self.buffers.iter_mut().find(|buffer| buffer.id == released) {
                buffer.busy = false;
            }
        }
        if self.state.transport.granted().is_some() && self.remap.is_configure_pending() {
            self.remap.configured();
        }
        if let Some(size) = self.current_buffer_size()
            && let Ok(layout) = BufferLayout::new(size)
        {
            self.retire_stale_buffers(layout);
        }
    }

    fn flush(&mut self, context: &str) -> Result<()> {
        match self.conn.flush() {
            Ok(()) => Ok(()),
            Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(self.connection_error(context, error)),
        }
    }

    fn connection_error(&mut self, context: &str, error: impl std::fmt::Display) -> Error {
        let message = format!("{context}: {error}");
        self.state
            .transport
            .close(SurfaceCloseReason::ConnectionLost(message.clone()));
        Error::Platform(message)
    }

    fn teardown(&mut self) -> Result<()> {
        if self.torn_down {
            return Ok(());
        }
        self.torn_down = true;

        if self.is_mapped() && !self.is_closed() {
            self.surface.attach(None, 0, 0);
            self.surface.commit();
            self.state.transport.set_mapped(false);
        }
        self.buffers.clear();

        if let Some(fractional_scale) = self.fractional_scale.take() {
            fractional_scale.destroy();
        }
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        self.layer_surface.destroy();
        self.surface.destroy();
        self.state.target_surface = None;
        self.state.release_bindings();
        if let Some(manager) = self.fractional_manager.take() {
            manager.destroy();
        }
        if let Some(viewporter) = self.viewporter.take() {
            viewporter.destroy();
        }
        self.shell.destroy();
        if self.shm.version() >= 2 {
            self.shm.release();
        }
        if let Some(globals) = self.globals.take()
            && globals.registry().is_alive()
        {
            globals.destroy();
        }
        match self.conn.flush() {
            Ok(()) => Ok(()),
            Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(platform("Wayland teardown flush failed", error)),
        }
    }
}

impl Drop for LayerShellSession {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}

fn connect() -> Result<Connection> {
    Connection::connect_to_env().map_err(|error| Error::Unsupported {
        what: "layer-shell overlay".into(),
        why: format!("no Wayland connection: {error}"),
    })
}

fn connect_for_probe() -> Result<Connection> {
    if std::env::var_os("WAYLAND_SOCKET").is_none() {
        return connect();
    }

    let socket_name = std::env::var_os("WAYLAND_DISPLAY").ok_or_else(|| Error::Unsupported {
        what: "probing layer-shell without consuming WAYLAND_SOCKET".into(),
        why: "the launcher supplied only an inherited Wayland socket, so Scrozz cannot open an \
              independent capability-probe connection; the compositor-positioned fallback \
              remains available"
            .into(),
    })?;
    let socket_path = if PathBuf::from(&socket_name).is_absolute() {
        PathBuf::from(socket_name)
    } else {
        let mut runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| Error::Unsupported {
                what: "probing layer-shell without consuming WAYLAND_SOCKET".into(),
                why: "WAYLAND_DISPLAY is relative but XDG_RUNTIME_DIR is absent or not absolute"
                    .into(),
            })?;
        runtime.push(socket_name);
        runtime
    };
    let socket = UnixStream::connect(&socket_path).map_err(|error| Error::Unsupported {
        what: "layer-shell overlay".into(),
        why: format!(
            "the independent Wayland probe connection at {} failed: {error}",
            socket_path.display()
        ),
    })?;
    Connection::from_socket(socket).map_err(|error| Error::Unsupported {
        what: "layer-shell overlay".into(),
        why: format!("no Wayland probe connection: {error}"),
    })
}

fn validate_config(config: &LayerSurfaceConfig) -> Result<()> {
    if let Some(reason) = config.rejection_reason() {
        return Err(Error::InvalidRequest(format!(
            "layer surface configuration would raise a protocol error: {reason}"
        )));
    }
    if config.width > i32::MAX as u32 || config.height > i32::MAX as u32 {
        return Err(Error::InvalidRequest(
            "layer surface dimensions exceed the SHM transport's signed 32-bit range".into(),
        ));
    }
    Ok(())
}

fn platform(context: &str, error: impl std::fmt::Display) -> Error {
    Error::Platform(format!("{context}: {error}"))
}

#[derive(Debug, Clone, Copy)]
struct OutputData {
    global_name: u32,
}

#[derive(Debug)]
struct OutputBinding {
    proxy: wl_output::WlOutput,
    name: Option<String>,
    description: Option<String>,
    scale: u32,
}

#[derive(Debug, Clone, Copy)]
struct SeatData {
    global_name: u32,
}

#[derive(Debug, Clone, Copy)]
struct PointerData {
    seat_global_name: u32,
}

#[derive(Debug)]
struct SeatBinding {
    proxy: wl_seat::WlSeat,
    pointer: Option<wl_pointer::WlPointer>,
    pointer_inside: bool,
    pointer_position: SurfacePoint,
}

#[derive(Debug, Clone, Copy)]
struct BufferData {
    id: u64,
}

struct SessionState {
    transport: TransportState,
    outputs: BTreeMap<u32, OutputBinding>,
    seats: BTreeMap<u32, SeatBinding>,
    entered_outputs: BTreeSet<u32>,
    selected_output_global: Option<u32>,
    target_surface: Option<wl_surface::WlSurface>,
    preferred_integer_scale: Option<u32>,
    can_buffer_scale: bool,
    released_buffers: Vec<u64>,
    extent_changed: bool,
}

impl SessionState {
    fn new(requested: SurfaceSize, fractional_enabled: bool, can_buffer_scale: bool) -> Self {
        Self {
            transport: TransportState::new(requested, fractional_enabled),
            outputs: BTreeMap::new(),
            seats: BTreeMap::new(),
            entered_outputs: BTreeSet::new(),
            selected_output_global: None,
            target_surface: None,
            preferred_integer_scale: None,
            can_buffer_scale,
            released_buffers: Vec::new(),
            extent_changed: false,
        }
    }

    fn select_output(
        &self,
        selector: &OutputSelector,
    ) -> Result<Option<(u32, wl_output::WlOutput)>> {
        match selector {
            OutputSelector::CompositorDefault => Ok(None),
            OutputSelector::Named(wanted) => self
                .outputs
                .iter()
                .find(|(_, output)| output.name.as_deref() == Some(wanted.as_str()))
                .map(|(global, output)| Some((*global, output.proxy.clone())))
                .ok_or_else(|| {
                    let available: Vec<&str> = self
                        .outputs
                        .values()
                        .filter_map(|output| output.name.as_deref())
                        .collect();
                    let detail = if available.is_empty() {
                        "the compositor exposed no wl_output names".to_string()
                    } else {
                        format!("available outputs: {}", available.join(", "))
                    };
                    Error::TargetGone(format!(
                        "requested Wayland output {wanted:?} is absent ({detail})"
                    ))
                }),
        }
    }

    fn output_infos(&self) -> Vec<OutputInfo> {
        self.outputs
            .iter()
            .map(|(global_name, output)| OutputInfo {
                global_name: *global_name,
                name: output.name.clone(),
                description: output.description.clone(),
                scale: output.scale,
            })
            .collect()
    }

    fn output_global(&self, proxy: &wl_output::WlOutput) -> Option<u32> {
        self.outputs
            .iter()
            .find_map(|(global, output)| (output.proxy == *proxy).then_some(*global))
    }

    fn recompute_integer_scale(&mut self) {
        let scale = if !self.can_buffer_scale {
            1
        } else if let Some(preferred) = self.preferred_integer_scale {
            preferred
        } else {
            self.entered_outputs
                .iter()
                .filter_map(|global| self.outputs.get(global))
                .map(|output| output.scale)
                .chain(
                    self.selected_output_global
                        .and_then(|global| self.outputs.get(&global))
                        .map(|output| output.scale),
                )
                .max()
                .unwrap_or(1)
        };
        self.transport.set_integer_scale(scale.max(1));
    }

    fn remove_output(&mut self, global_name: u32) {
        let selected = self.selected_output_global == Some(global_name);
        let removed = self.outputs.remove(&global_name);
        self.entered_outputs.remove(&global_name);
        if let Some(output) = removed {
            let name = output.name.clone();
            release_output(output.proxy);
            if selected {
                self.transport
                    .close(SurfaceCloseReason::OutputRemoved { name });
            }
        }
        self.recompute_integer_scale();
    }

    fn remove_seat(&mut self, global_name: u32) {
        if let Some(mut seat) = self.seats.remove(&global_name) {
            if let Some(pointer) = seat.pointer.take() {
                release_pointer(pointer);
            }
            release_seat(seat.proxy);
        }
    }

    fn release_bindings(&mut self) {
        for (_, output) in std::mem::take(&mut self.outputs) {
            release_output(output.proxy);
        }
        for (_, mut seat) in std::mem::take(&mut self.seats) {
            if let Some(pointer) = seat.pointer.take() {
                release_pointer(pointer);
            }
            release_seat(seat.proxy);
        }
        self.entered_outputs.clear();
        self.selected_output_global = None;
    }
}

fn initial_globals(globals: &GlobalList, interface: &str) -> Vec<(u32, u32)> {
    globals.contents().with_list(|advertised| {
        advertised
            .iter()
            .filter(|global| global.interface == interface)
            .map(|global| (global.name, global.version))
            .collect()
    })
}

fn bind_initial_outputs(
    globals: &GlobalList,
    qh: &QueueHandle<SessionState>,
    state: &mut SessionState,
) {
    for (name, version) in initial_globals(globals, wl_output::WlOutput::interface().name) {
        bind_output(globals.registry(), qh, state, name, version);
    }
}

fn bind_initial_seats(
    globals: &GlobalList,
    qh: &QueueHandle<SessionState>,
    state: &mut SessionState,
) {
    for (name, version) in initial_globals(globals, wl_seat::WlSeat::interface().name) {
        bind_seat(globals.registry(), qh, state, name, version);
    }
}

fn bind_output(
    registry: &wl_registry::WlRegistry,
    qh: &QueueHandle<SessionState>,
    state: &mut SessionState,
    name: u32,
    version: u32,
) {
    if state.outputs.contains_key(&name) {
        return;
    }
    let proxy = registry.bind::<wl_output::WlOutput, _, _>(
        name,
        version.min(OUTPUT_VERSION),
        qh,
        OutputData { global_name: name },
    );
    state.outputs.insert(
        name,
        OutputBinding {
            proxy,
            name: None,
            description: None,
            scale: 1,
        },
    );
}

fn bind_seat(
    registry: &wl_registry::WlRegistry,
    qh: &QueueHandle<SessionState>,
    state: &mut SessionState,
    name: u32,
    version: u32,
) {
    if state.seats.contains_key(&name) {
        return;
    }
    let proxy = registry.bind::<wl_seat::WlSeat, _, _>(
        name,
        version.min(SEAT_VERSION),
        qh,
        SeatData { global_name: name },
    );
    state.seats.insert(
        name,
        SeatBinding {
            proxy,
            pointer: None,
            pointer_inside: false,
            pointer_position: SurfacePoint { x: 0.0, y: 0.0 },
        },
    );
}

fn release_output(output: wl_output::WlOutput) {
    if output.version() >= 3 {
        output.release();
    }
}

fn release_pointer(pointer: wl_pointer::WlPointer) {
    if pointer.version() >= 3 {
        pointer.release();
    }
}

fn release_seat(seat: wl_seat::WlSeat) {
    if seat.version() >= 5 {
        seat.release();
    }
}

struct SharedMapping {
    pointer: NonNull<u8>,
    byte_len: usize,
}

impl SharedMapping {
    fn new(fd: &OwnedFd, byte_len: usize) -> Result<Self> {
        // SAFETY: `fd` is a private memfd sized to `byte_len`, remains alive for
        // the mapping's lifetime, and no code changes its length afterwards.
        let pointer = unsafe {
            mmap(
                null_mut(),
                byte_len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                fd,
                0,
            )
        }
        .map_err(|error| Error::Io(error.into()))?;
        let Some(pointer) = NonNull::new(pointer.cast::<u8>()) else {
            // SAFETY: a successful `mmap` may legally select address zero; this
            // transport cannot represent that with `NonNull`, so release it.
            let _ = unsafe { munmap(pointer, byte_len) };
            return Err(Error::Platform("mmap returned a null SHM address".into()));
        };
        Ok(Self { pointer, byte_len })
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the mapping owns `byte_len` writable bytes at `pointer`, and
        // `&mut self` guarantees no other Rust slice aliases this one.
        unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.byte_len) }
    }
}

impl Drop for SharedMapping {
    fn drop(&mut self) {
        // SAFETY: this is the exact live mapping returned by `mmap`, and Drop
        // runs once after the Wayland buffer proxy has been destroyed.
        let _ = unsafe { munmap(self.pointer.as_ptr().cast::<c_void>(), self.byte_len) };
    }
}

struct ShmBuffer {
    id: u64,
    layout: BufferLayout,
    proxy: wl_buffer::WlBuffer,
    memory: SharedMapping,
    _fd: OwnedFd,
    busy: bool,
}

impl ShmBuffer {
    fn new(
        id: u64,
        layout: BufferLayout,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<SessionState>,
    ) -> Result<Self> {
        let fd = memfd_create(
            "scrozz-layer-buffer",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .map_err(|error| Error::Io(error.into()))?;
        ftruncate(&fd, layout.byte_len as u64).map_err(|error| Error::Io(error.into()))?;
        fcntl_add_seals(&fd, SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL)
            .map_err(|error| Error::Io(error.into()))?;
        let memory = SharedMapping::new(&fd, layout.byte_len)?;
        let pool = shm.create_pool(fd.as_fd(), layout.pool_len, qh, ());
        let proxy = pool.create_buffer(
            0,
            i32::try_from(layout.size.width)
                .map_err(|_| Error::InvalidRequest("SHM width exceeds i32".into()))?,
            i32::try_from(layout.size.height)
                .map_err(|_| Error::InvalidRequest("SHM height exceeds i32".into()))?,
            layout.stride,
            wl_shm::Format::Argb8888,
            qh,
            BufferData { id },
        );
        pool.destroy();
        Ok(Self {
            id,
            layout,
            proxy,
            memory,
            _fd: fd,
            busy: false,
        })
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

fn apply(surface: &ZwlrLayerSurfaceV1, config: &LayerSurfaceConfig, version: u32) {
    if version >= 2 {
        surface.set_layer(wire_layer(config.layer));
    }
    surface.set_size(config.width, config.height);
    surface.set_anchor(wire_anchor(config.anchor.bits()));
    surface.set_exclusive_zone(config.exclusive_zone);
    surface.set_margin(
        config.margins.top,
        config.margins.right,
        config.margins.bottom,
        config.margins.left,
    );

    let interactivity = match config.keyboard_interactivity {
        KeyboardInteractivity::OnDemand if version < 4 => KeyboardInteractivity::None,
        other => other,
    };
    surface.set_keyboard_interactivity(wire_keyboard(interactivity));
}

const fn wire_layer(layer: Layer) -> zwlr_layer_shell_v1::Layer {
    match layer {
        Layer::Background => zwlr_layer_shell_v1::Layer::Background,
        Layer::Bottom => zwlr_layer_shell_v1::Layer::Bottom,
        Layer::Top => zwlr_layer_shell_v1::Layer::Top,
        Layer::Overlay => zwlr_layer_shell_v1::Layer::Overlay,
    }
}

const fn wire_keyboard(
    interactivity: KeyboardInteractivity,
) -> zwlr_layer_surface_v1::KeyboardInteractivity {
    match interactivity {
        KeyboardInteractivity::None => zwlr_layer_surface_v1::KeyboardInteractivity::None,
        KeyboardInteractivity::Exclusive => zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
        KeyboardInteractivity::OnDemand => zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    }
}

fn wire_anchor(bits: u32) -> zwlr_layer_surface_v1::Anchor {
    zwlr_layer_surface_v1::Anchor::from_bits(bits).unwrap_or(zwlr_layer_surface_v1::Anchor::empty())
}

fn wire_axis(axis: WEnum<wl_pointer::Axis>) -> Option<PointerAxis> {
    match axis {
        WEnum::Value(wl_pointer::Axis::VerticalScroll) => Some(PointerAxis::Vertical),
        WEnum::Value(wl_pointer::Axis::HorizontalScroll) => Some(PointerAxis::Horizontal),
        WEnum::Value(_) | WEnum::Unknown(_) => None,
    }
}

fn wire_axis_source(source: WEnum<wl_pointer::AxisSource>) -> PointerAxisSource {
    match source {
        WEnum::Value(wl_pointer::AxisSource::Wheel) => PointerAxisSource::Wheel,
        WEnum::Value(wl_pointer::AxisSource::Finger) => PointerAxisSource::Finger,
        WEnum::Value(wl_pointer::AxisSource::Continuous) => PointerAxisSource::Continuous,
        WEnum::Value(wl_pointer::AxisSource::WheelTilt) => PointerAxisSource::WheelTilt,
        WEnum::Value(_) => PointerAxisSource::Unknown(u32::MAX),
        WEnum::Unknown(value) => PointerAxisSource::Unknown(value),
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for SessionState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == wl_output::WlOutput::interface().name => {
                bind_output(registry, qh, state, name, version);
            }
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == wl_seat::WlSeat::interface().name => {
                bind_seat(registry, qh, state, name, version);
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.remove_output(name);
                state.remove_seat(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for SessionState {
    fn event(
        state: &mut Self,
        surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                surface.ack_configure(serial);
                state.transport.configure(SurfaceSize::new(width, height));
                state.extent_changed = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.transport.close(SurfaceCloseReason::Compositor);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, OutputData> for SessionState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        data: &OutputData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let relevant = state.selected_output_global == Some(data.global_name)
            || state.entered_outputs.contains(&data.global_name);
        let Some(output) = state.outputs.get_mut(&data.global_name) else {
            return;
        };
        match event {
            wl_output::Event::Scale { factor } => {
                output.scale = u32::try_from(factor).unwrap_or(1).max(1);
                state.recompute_integer_scale();
                state.extent_changed |= relevant;
            }
            wl_output::Event::Name { name } => output.name = Some(name),
            wl_output::Event::Description { description } => {
                output.description = Some(description);
            }
            wl_output::Event::Geometry { .. } | wl_output::Event::Mode { .. } => {
                state.extent_changed |= relevant;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for SessionState {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { output } => {
                if let Some(global) = state.output_global(&output) {
                    state.extent_changed |= state.entered_outputs.insert(global);
                    state.recompute_integer_scale();
                }
            }
            wl_surface::Event::Leave { output } => {
                if let Some(global) = state.output_global(&output) {
                    state.extent_changed |= state.entered_outputs.remove(&global);
                    state.recompute_integer_scale();
                }
            }
            wl_surface::Event::PreferredBufferScale { factor } => {
                if let Ok(factor) = u32::try_from(factor)
                    && factor != 0
                {
                    state.preferred_integer_scale = Some(factor);
                    state.recompute_integer_scale();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for SessionState {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.transport.set_fractional_scale(scale);
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for SessionState {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &BufferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.released_buffers.push(data.id);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, SeatData> for SessionState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        data: &SeatData,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        else {
            return;
        };
        let Some(binding) = state.seats.get_mut(&data.global_name) else {
            return;
        };
        let has_pointer = capabilities.contains(wl_seat::Capability::Pointer);
        if has_pointer && binding.pointer.is_none() {
            binding.pointer = Some(seat.get_pointer(
                qh,
                PointerData {
                    seat_global_name: data.global_name,
                },
            ));
        } else if !has_pointer {
            if let Some(pointer) = binding.pointer.take() {
                release_pointer(pointer);
            }
            binding.pointer_inside = false;
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, PointerData> for SessionState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        data: &PointerData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let target_surface = state.target_surface.clone();
        let Some(binding) = state.seats.get_mut(&data.seat_global_name) else {
            return;
        };

        let pointer_event = match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                if target_surface.as_ref() != Some(&surface) {
                    binding.pointer_inside = false;
                    return;
                }
                let position = SurfacePoint {
                    x: surface_x,
                    y: surface_y,
                };
                binding.pointer_inside = true;
                binding.pointer_position = position;
                Some(SurfacePointerEvent::Enter { serial, position })
            }
            wl_pointer::Event::Motion {
                time,
                surface_x,
                surface_y,
            } if binding.pointer_inside => {
                let position = SurfacePoint {
                    x: surface_x,
                    y: surface_y,
                };
                binding.pointer_position = position;
                Some(SurfacePointerEvent::Motion { time, position })
            }
            wl_pointer::Event::Leave { serial, surface }
                if binding.pointer_inside || target_surface.as_ref() == Some(&surface) =>
            {
                binding.pointer_inside = false;
                Some(SurfacePointerEvent::Leave { serial })
            }
            wl_pointer::Event::Button {
                serial,
                time,
                button,
                state: WEnum::Value(button_state),
            } if binding.pointer_inside => {
                let state = match button_state {
                    wl_pointer::ButtonState::Pressed => PointerButtonState::Pressed,
                    wl_pointer::ButtonState::Released => PointerButtonState::Released,
                    _ => return,
                };
                Some(SurfacePointerEvent::Button {
                    serial,
                    time,
                    button,
                    state,
                    position: binding.pointer_position,
                })
            }
            wl_pointer::Event::Axis { time, axis, value } if binding.pointer_inside => {
                wire_axis(axis).map(|axis| SurfacePointerEvent::Axis {
                    time,
                    axis,
                    value,
                    position: binding.pointer_position,
                })
            }
            wl_pointer::Event::AxisDiscrete { axis, discrete } if binding.pointer_inside => {
                wire_axis(axis).map(|axis| SurfacePointerEvent::AxisDiscrete {
                    axis,
                    steps: discrete,
                    position: binding.pointer_position,
                })
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } if binding.pointer_inside => {
                wire_axis(axis).map(|axis| SurfacePointerEvent::AxisValue120 {
                    axis,
                    value_120: value120,
                    position: binding.pointer_position,
                })
            }
            wl_pointer::Event::AxisStop { time, axis } if binding.pointer_inside => wire_axis(axis)
                .map(|axis| SurfacePointerEvent::AxisStop {
                    time,
                    axis,
                    position: binding.pointer_position,
                }),
            wl_pointer::Event::AxisSource { axis_source } if binding.pointer_inside => Some(
                SurfacePointerEvent::AxisSource(wire_axis_source(axis_source)),
            ),
            wl_pointer::Event::Frame if binding.pointer_inside => Some(SurfacePointerEvent::Frame),
            _ => None,
        };

        if let Some(event) = pointer_event {
            state.transport.push_pointer(event);
        }
    }
}

delegate_noop!(SessionState: ZwlrLayerShellV1);
delegate_noop!(SessionState: wl_compositor::WlCompositor);
delegate_noop!(SessionState: ignore wl_shm::WlShm);
delegate_noop!(SessionState: wl_shm_pool::WlShmPool);
delegate_noop!(SessionState: wayland_client::protocol::wl_region::WlRegion);
delegate_noop!(SessionState: WpFractionalScaleManagerV1);
delegate_noop!(SessionState: WpViewporter);
delegate_noop!(SessionState: WpViewport);
