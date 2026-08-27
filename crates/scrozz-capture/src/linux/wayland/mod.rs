//! The Wayland capture backend, via `xdg-desktop-portal`.
//!
//! # What Wayland actually permits, and what that costs
//!
//! Wayland's security model denies a client any view of other clients. There is
//! no protocol — in any compositor — by which an application can list windows,
//! read their titles, or read their pixels. This is not an omission waiting to be
//! filled; it is the design. Every Wayland screenshot tool therefore works the
//! same way: it asks `xdg-desktop-portal`, the portal shows *its own* picker in
//! a separate trusted process, and the application receives only what the user
//! chose.
//!
//! Three consequences run through this module:
//!
//! - [`TargetEnumerator::windows`] cannot work and returns
//!   [`Error::Unsupported`] saying so. Decision D8 requires the gap to be
//!   documented rather than hidden, and a fabricated window list would be worse
//!   than useless — every entry would fail at capture time.
//! - Display geometry is recovered from XWayland where it exists, because
//!   XWayland mirrors the compositor's outputs into RandR. That is a real answer
//!   on the overwhelming majority of desktops, and an honest refusal on the rest.
//! - Window stills use the Screenshot portal's window-restricted trusted picker.
//!   The compositor returns an encoded image only after the user chooses a
//!   surface, so Scrozz never gains ambient window-enumeration access.

pub mod portal;
pub mod restore;

use std::sync::Mutex;

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, Display, Error, Result, TargetEnumerator, Window,
    WindowPicking, WindowPickingCapability,
};

use self::restore::{TokenKey, TokenStore};
use super::session::{self, Compositor, PortalCapabilities, SessionEnv, SessionKind};

/// Still capture through `xdg-desktop-portal`.
pub struct WaylandBackend {
    compositor: Compositor,
    capabilities: PortalCapabilities,
    kind: SessionKind,
    tokens: Mutex<TokenStore>,
    token_path: Option<std::path::PathBuf>,
    /// XWayland, used only to read output geometry — never to capture.
    ///
    /// Capturing through it would silently omit every native Wayland window,
    /// which is the single most common Wayland screenshot bug. Reading geometry
    /// through it is safe, because XWayland is configured by the compositor to
    /// mirror the real output layout.
    geometry: Option<super::x11::X11Backend>,
    window_picking: WindowPickingCapability,
    name: String,
}

impl std::fmt::Debug for WaylandBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandBackend")
            .field("compositor", &self.compositor)
            .field("kind", &self.kind)
            .field("capabilities", &self.capabilities)
            .field("xwayland_geometry", &self.geometry.is_some())
            .field("window_picking", &self.window_picking)
            .finish_non_exhaustive()
    }
}

impl WaylandBackend {
    /// Prepares a portal-backed backend for the current session.
    ///
    /// # Errors
    ///
    /// Never fails: an unreachable portal is discovered at capture time, and
    /// refusing to construct the backend would leave the application with no way
    /// to report why.
    pub fn new(env: &SessionEnv) -> Result<Self> {
        let compositor = session::detect_compositor(env);
        let capabilities = session::capabilities(&compositor);
        let kind = session::detect_session(env);

        let token_path = restore::token_path(
            std::env::var("XDG_STATE_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        );

        let tokens = token_path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map_or_else(TokenStore::new, |text| TokenStore::parse(&text));

        // XWayland is present in essentially every GNOME and KDE session, but a
        // pure-Wayland sway session may genuinely lack it, and a failure here is
        // a missing capability rather than an error.
        let geometry = (env.display.is_some())
            .then(super::x11::X11Backend::connect)
            .and_then(|result| match result {
                Ok(backend) => Some(backend),
                Err(err) => {
                    tracing::debug!(%err, "XWayland unreachable; display geometry unavailable");
                    None
                }
            });
        let window_picking = if kind == SessionKind::Wayland {
            portal::window_picking_capability().unwrap_or_else(|error| {
                tracing::debug!(%error, "Screenshot portal window-target probe failed");
                portal::unchecked_window_picking_capability(format!(
                    "the Screenshot portal could not be queried: {error}"
                ))
            })
        } else {
            portal::unchecked_window_picking_capability(
                "portal window selection applies only to native Wayland sessions",
            )
        };

        let name = format!(
            "xdg-desktop-portal ScreenCast on {compositor}{}",
            if geometry.is_some() {
                " (geometry via XWayland)"
            } else {
                ""
            }
        );

        Ok(Self {
            compositor,
            capabilities,
            kind,
            tokens: Mutex::new(tokens),
            token_path,
            geometry,
            window_picking,
            name,
        })
    }

    /// What this compositor's portal can do.
    #[must_use]
    pub const fn capabilities(&self) -> PortalCapabilities {
        self.capabilities
    }

    /// The compositor this backend detected.
    #[must_use]
    pub const fn compositor(&self) -> &Compositor {
        &self.compositor
    }

    /// The restore token stored for a session kind, if any.
    #[must_use]
    pub fn stored_token(&self, key: TokenKey) -> Option<String> {
        self.tokens
            .lock()
            .ok()?
            .get(key)
            .map(std::borrow::ToOwned::to_owned)
    }

    /// Records a token the portal issued and writes it out.
    ///
    /// Write failures are logged rather than propagated: losing persistence
    /// costs a permission prompt next time, whereas failing the capture the user
    /// just took loses their work.
    pub fn remember_token(&self, key: TokenKey, token: &str) {
        let Ok(mut store) = self.tokens.lock() else {
            return;
        };
        store.set(key, token);

        let Some(path) = &self.token_path else {
            return;
        };
        let write = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(path, store.serialise()));
        if let Err(err) = write {
            tracing::warn!(%err, path = %path.display(), "could not persist the portal restore token");
        }
    }

    /// Forgets a token the portal rejected.
    pub fn forget_token(&self, key: TokenKey) {
        if let Ok(mut store) = self.tokens.lock() {
            store.invalidate(key);
        }
    }

    fn geometry_backend(&self) -> Result<&super::x11::X11Backend> {
        self.geometry.as_ref().ok_or_else(|| Error::Unsupported {
            what: "listing displays on Wayland".into(),
            why: format!(
                "Wayland exposes outputs through the `wl_output` protocol, which needs a Wayland \
                 client connection this build does not make, and no XWayland server was reachable \
                 on {compositor} to read the layout from instead",
                compositor = self.compositor
            ),
        })
    }
}

impl TargetEnumerator for WaylandBackend {
    fn displays(&self) -> Result<Vec<Display>> {
        // XWayland's RandR view is the compositor's own output layout, so the
        // bounds and primary flag are right. Under fractional scaling the
        // compositor may hand XWayland a rounded-up integer-scaled view, so the
        // scale reported here can differ from the compositor's true fraction;
        // that is a known approximation, not a silent one.
        self.geometry_backend()?.displays()
    }

    fn windows(&self) -> Result<Vec<Window>> {
        Err(Error::Unsupported {
            what: "listing windows".into(),
            why: format!(
                "Wayland has no window-enumeration protocol — not on {compositor}, and not on any \
                 other compositor. A client is not permitted to see other clients' surfaces at \
                 all. Window selection happens in xdg-desktop-portal's own picker, which runs \
                 out-of-process and returns only the window the user chose, so there is no list \
                 to show and none to fake",
                compositor = self.compositor
            ),
        })
    }

    fn active_display(&self) -> Result<Display> {
        // Pointer position is likewise privileged on Wayland; XWayland's is the
        // real pointer whenever XWayland exists.
        self.geometry_backend()?.active_display()
    }
}

impl CaptureBackend for WaylandBackend {
    fn capture(&self, request: &CaptureRequest) -> Result<Capture> {
        if request.target.is_window() {
            if self.kind == SessionKind::Wayland {
                tracing::info!(
                    "window capture on Wayland is chosen in the portal's picker, not by window id; \
                    the requested id is advisory only"
                );
            }
            return portal::capture_window(request);
        }

        Err(Error::Unsupported {
            what: "capturing this target on Wayland".into(),
            why: format!(
                "the {compositor} Screenshot portal path currently supports window stills; \
                 display and region requests require a separately placeable ScreenCast stream",
                compositor = self.compositor
            ),
        })
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl WindowPicking for WaylandBackend {
    fn window_picking(&self) -> WindowPickingCapability {
        self.window_picking.clone()
    }
}
