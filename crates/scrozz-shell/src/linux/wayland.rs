//! The Wayland side: probing for `wlr-layer-shell`, and using it.
//!
//! # Why a probe exists at all
//!
//! Scrozz knows, statically, that KDE and the wlroots family implement
//! `zwlr_layer_shell_v1` and that GNOME/Mutter has declined to. That knowledge is
//! a prior, not an answer — compositors get patched, versions add protocols, and
//! `Compositor::Other` is by definition an unknown. So [`probe`] asks the running
//! compositor and lets the answer overrule the table.
//!
//! # Why this module does not promote winit's window
//!
//! The obvious implementation would be to take the `wl_surface` eframe already
//! owns and call `zwlr_layer_shell_v1.get_layer_surface` on it. That is not a
//! degraded path or a fragile one; it is a fatal error:
//!
//! > A `wl_surface` may hold exactly one role for its entire lifetime. winit has
//! > already given its surface the `xdg_toplevel` role, and `get_layer_surface`
//! > "raises a protocol error if another role is already assigned". A Wayland
//! > protocol error is not recoverable — the compositor disconnects the client,
//! > and every window Scrozz owns disappears at once.
//!
//! So the promotion is refused rather than attempted, and [`refusal`] says
//! exactly why. Real layer-shell needs a surface Scrozz creates itself, which
//! needs a renderer outside eframe's window; [`LayerShellSession`] is that
//! surface's protocol half, complete and exercisable today, waiting on the
//! rendering half.
//!
//! This is the distinction the whole capability model turns on: "your compositor
//! supports this and Scrozz cannot use it yet" is a different sentence from
//! "your compositor refuses this and there is nothing to wait for", and a user
//! deserves to be told which one they are in.

use scrozz_core::{Error, Result};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_output, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

use super::capability::LayerShellProbe;
use super::layer::{KeyboardInteractivity, Layer, LayerSurfaceConfig, NAMESPACE};
use super::region::InputRegion;

/// Asks the running compositor whether it advertises `zwlr_layer_shell_v1`.
///
/// Every failure — no `WAYLAND_DISPLAY`, a refused connection, a registry that
/// never completes — returns [`LayerShellProbe::NotProbed`] rather than
/// [`LayerShellProbe::Absent`]. The distinction matters: `Absent` is a statement
/// about a compositor that answered, and reporting it for a compositor that was
/// never reached would be a guess wearing a fact's clothes.
#[must_use]
pub fn probe() -> LayerShellProbe {
    let Ok(conn) = Connection::connect_to_env() else {
        return LayerShellProbe::NotProbed;
    };
    probe_connection(&conn)
}

/// The probe, against a connection the caller already has.
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

/// State for the registry-only probe, which handles no events.
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
        // The global list maintains itself; the probe reads it after
        // `registry_queue_init` has done its round trip.
    }
}

/// The error returned when asked to turn an existing window into a layer
/// surface.
///
/// Phrased for a human reading a diagnostics pane, because this is the sentence
/// that stops the next person re-attempting the promotion and taking down the
/// client.
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

// ---------------------------------------------------------------------------
// A real layer surface
// ---------------------------------------------------------------------------

/// A Scrozz-owned `zwlr_layer_surface_v1`, configured and committed.
///
/// This is the protocol half of the layer-shell overlay: it binds the shell,
/// creates its own `wl_surface`, applies a [`LayerSurfaceConfig`], and waits for
/// the compositor's `configure`. What it does not do is paint — attaching a
/// buffer needs a renderer, and until one is wired in the surface stays
/// unmapped.
///
/// That still makes it useful rather than decorative, and deliberately so: the
/// `configure` round trip is the compositor *accepting* the anchors, layer and
/// exclusive zone, and a mistake in any of them shows up as a protocol error or
/// a surprising granted size. The smoke tests drive exactly this to prove the
/// configuration is right on a real KDE or wlroots session, long before there is
/// anything to draw.
pub struct LayerShellSession {
    conn: Connection,
    state: SessionState,
    queue: wayland_client::EventQueue<SessionState>,
    compositor: wl_compositor::WlCompositor,
    surface: wl_surface::WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    version: u32,
}

/// Everything the event queue writes back into.
#[derive(Default)]
struct SessionState {
    /// The size the compositor granted, once it has told us.
    granted: Option<(u32, u32)>,
    /// Set when the compositor closes the surface, e.g. its output went away.
    closed: bool,
}

impl LayerShellSession {
    /// Creates a layer surface with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when the compositor does not advertise
    /// `zwlr_layer_shell_v1` — the GNOME case — and [`Error::InvalidRequest`]
    /// when the configuration would itself raise a protocol error, which is
    /// caught here rather than sent, since sending it would kill the client.
    pub fn new(config: &LayerSurfaceConfig) -> Result<Self> {
        if let Some(reason) = config.rejection_reason() {
            return Err(Error::InvalidRequest(format!(
                "layer surface configuration would raise a protocol error: {reason}"
            )));
        }

        let conn = Connection::connect_to_env().map_err(|e| Error::Unsupported {
            what: "layer-shell overlay".into(),
            why: format!("no Wayland connection: {e}"),
        })?;
        let (globals, mut queue) = registry_queue_init::<SessionState>(&conn)
            .map_err(|e| Error::Platform(format!("Wayland registry did not initialise: {e}")))?;
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).map_err(|e| {
                Error::Platform(format!("compositor does not offer wl_compositor: {e}"))
            })?;

        // Version 4 is the floor for `on_demand` keyboard interactivity. Asking
        // for 1..=4 means an older compositor still binds, and
        // `KeyboardInteractivity::OnDemand` is downgraded below rather than sent
        // to a compositor that would reject it.
        let shell: ZwlrLayerShellV1 =
            globals
                .bind(&qh, 1..=4, ())
                .map_err(|_| Error::Unsupported {
                    what: "layer-shell overlay".into(),
                    why: "this compositor does not implement zwlr_layer_shell_v1, so a \
                          client cannot anchor a surface to a screen edge"
                        .into(),
                })?;
        let version = shell.version();

        let surface = compositor.create_surface(&qh, ());
        let layer_surface = shell.get_layer_surface(
            &surface,
            // No output: let the compositor choose, which is what it does for
            // the focused output and matches "wherever the user is working".
            None::<&wl_output::WlOutput>,
            wire_layer(config.layer),
            config.namespace.to_string(),
            &qh,
            (),
        );

        apply(&layer_surface, config, version);
        surface.commit();

        let mut state = SessionState::default();
        // The first configure is the compositor's answer. Round-tripping here
        // means `new` returns a surface whose granted size is already known,
        // rather than one that silently has no size for a frame or two.
        queue
            .roundtrip(&mut state)
            .map_err(|e| Error::Platform(format!("Wayland roundtrip failed: {e}")))?;

        Ok(Self {
            conn,
            state,
            queue,
            compositor,
            surface,
            layer_surface,
            version,
        })
    }

    /// The size the compositor granted, if it has configured the surface.
    #[must_use]
    pub const fn granted_size(&self) -> Option<(u32, u32)> {
        self.state.granted
    }

    /// Whether the compositor has closed this surface.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.state.closed
    }

    /// The layer-shell interface version the compositor bound.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Re-applies a configuration to an existing surface.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a configuration that would raise a
    /// protocol error, and [`Error::Platform`] if the round trip failed.
    pub fn reconfigure(&mut self, config: &LayerSurfaceConfig) -> Result<()> {
        if let Some(reason) = config.rejection_reason() {
            return Err(Error::InvalidRequest(format!(
                "layer surface configuration would raise a protocol error: {reason}"
            )));
        }
        apply(&self.layer_surface, config, self.version);
        self.surface.commit();
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|e| Error::Platform(format!("Wayland roundtrip failed: {e}")))?;
        Ok(())
    }

    /// Sets which parts of the surface accept pointer and touch input.
    ///
    /// Wayland's core protocol handles this, not layer-shell, so it works
    /// identically on a compositor-placed fallback toplevel. The two special
    /// cases are opposites and easy to confuse: an empty region rejects
    /// everything, while *no* region accepts everything.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if the compositor could not be reached.
    pub fn set_input_region(&mut self, region: &InputRegion) -> Result<()> {
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
        self.surface.commit();
        self.conn
            .flush()
            .map_err(|e| Error::Platform(format!("Wayland flush failed: {e}")))?;
        Ok(())
    }
}

/// Sends one configuration to a layer surface.
///
/// Split out because `new` and `reconfigure` must send exactly the same
/// requests in exactly the same order; two copies would drift.
fn apply(surface: &ZwlrLayerSurfaceV1, config: &LayerSurfaceConfig, version: u32) {
    surface.set_size(config.width, config.height);
    surface.set_anchor(wire_anchor(config.anchor.bits()));
    surface.set_exclusive_zone(config.exclusive_zone);
    surface.set_margin(
        config.margins.top,
        config.margins.right,
        config.margins.bottom,
        config.margins.left,
    );

    // `on_demand` arrived in version 4. Sending it to an older compositor is a
    // protocol error, so it is downgraded to `none` — the conservative choice,
    // since a surface that never takes the keyboard is merely inconvenient while
    // one that takes it unexpectedly eats the user's typing.
    let interactivity = match config.keyboard_interactivity {
        KeyboardInteractivity::OnDemand if version < 4 => KeyboardInteractivity::None,
        other => other,
    };
    surface.set_keyboard_interactivity(wire_keyboard(interactivity));
}

/// Maps Scrozz's layer to the generated protocol enum.
///
/// Written out rather than converted numerically: the wire values are protocol
/// constants, and a `transmute`-by-number would silently survive a reordering
/// that changed their meaning.
const fn wire_layer(layer: Layer) -> zwlr_layer_shell_v1::Layer {
    match layer {
        Layer::Background => zwlr_layer_shell_v1::Layer::Background,
        Layer::Bottom => zwlr_layer_shell_v1::Layer::Bottom,
        Layer::Top => zwlr_layer_shell_v1::Layer::Top,
        Layer::Overlay => zwlr_layer_shell_v1::Layer::Overlay,
    }
}

/// Maps Scrozz's keyboard-interactivity choice to the generated protocol enum.
const fn wire_keyboard(
    interactivity: KeyboardInteractivity,
) -> zwlr_layer_surface_v1::KeyboardInteractivity {
    match interactivity {
        KeyboardInteractivity::None => zwlr_layer_surface_v1::KeyboardInteractivity::None,
        KeyboardInteractivity::Exclusive => zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
        KeyboardInteractivity::OnDemand => zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    }
}

/// Maps Scrozz's anchor bitmask to the generated protocol bitflags.
///
/// `from_bits` returns `None` for a bit the protocol does not define; that
/// cannot happen for a mask built by [`super::layer::Anchor`], and if it somehow
/// did, anchoring nowhere is safer than anchoring somewhere arbitrary.
fn wire_anchor(bits: u32) -> zwlr_layer_surface_v1::Anchor {
    zwlr_layer_surface_v1::Anchor::from_bits(bits).unwrap_or(zwlr_layer_surface_v1::Anchor::empty())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for SessionState {
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
                // Acknowledging is mandatory. A layer surface that never acks
                // its configure is never mapped, and the failure looks exactly
                // like the compositor ignoring the client.
                surface.ack_configure(serial);
                state.granted = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

delegate_noop!(SessionState: ZwlrLayerShellV1);
delegate_noop!(SessionState: wl_compositor::WlCompositor);
delegate_noop!(SessionState: wayland_client::protocol::wl_region::WlRegion);
delegate_noop!(SessionState: ignore wl_surface::WlSurface);
delegate_noop!(SessionState: ignore wl_output::WlOutput);
