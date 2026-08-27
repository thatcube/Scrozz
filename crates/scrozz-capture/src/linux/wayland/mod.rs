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
//! - The permission prompt must be suppressed with a restore token or the tool
//!   is unusable. [`restore`] handles that, and it is treated as a requirement
//!   rather than a nicety.
//!
//! # What this backend does and does not compile
//!
//! The portal conversation is real: [`screencast`] makes the D-Bus calls through
//! `ashpd`, and [`pipewire`] turns the node it returns into pixels by opening
//! `libpipewire-0.3.so.0` at run time. Everything that can be decided without a
//! compositor — the session plan, the format offer, stride handling, the crop
//! arithmetic, error classification — lives in modules with no platform
//! dependencies and is tested on every host by `tests/linux.rs`.
//!
//! What still needs a real Wayland session is the part that cannot be faked: the
//! FFI struct layouts, the picker, and the compositor's actual frame delivery.
//! `tools/wayland-smoke.sh` covers those and skips loudly when there is no
//! session to run against.

pub mod pipewire;
pub mod portal;
pub mod region;
pub mod restore;
pub mod screencast;

use std::sync::Mutex;

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, Display, Error, Frame,
    Provenance, Result, ScaleFactor, TargetEnumerator, Window,
};

use self::portal::{PortalFailure, SessionPlan, StreamInfo};
use self::restore::{TokenKey, TokenStore};
use super::session::{self, Compositor, PortalCapabilities, SessionEnv, SessionKind};

/// Still capture through `xdg-desktop-portal`.
pub struct WaylandBackend {
    compositor: Compositor,
    capabilities: PortalCapabilities,
    kind: SessionKind,
    /// Serialises captures so a single-use restore token is never presented by
    /// two concurrent portal sessions.
    capture_gate: Mutex<()>,
    tokens: Mutex<TokenStore>,
    token_path: Option<std::path::PathBuf>,
    /// XWayland, used only to read output geometry — never to capture.
    ///
    /// Capturing through it would silently omit every native Wayland window,
    /// which is the single most common Wayland screenshot bug. Reading geometry
    /// through it is safe, because XWayland is configured by the compositor to
    /// mirror the real output layout.
    geometry: Option<super::x11::X11Backend>,
    name: String,
}

impl std::fmt::Debug for WaylandBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandBackend")
            .field("compositor", &self.compositor)
            .field("kind", &self.kind)
            .field("capabilities", &self.capabilities)
            .field("xwayland_geometry", &self.geometry.is_some())
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
            capture_gate: Mutex::new(()),
            tokens: Mutex::new(tokens),
            token_path,
            geometry,
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

    /// Negotiates a ScreenCast session, reusing a stored grant where possible.
    ///
    /// A syntactically invalid token is rejected by `SelectSources` as
    /// `InvalidArgument` with a token-specific message. Only that classified
    /// failure is retried without the token. Missing portals, unsupported source
    /// types, empty stream sets and PipeWire-remote failures cannot be repaired
    /// by deleting a token, and retrying them would duplicate arbitrary failures
    /// or show a second picker.
    ///
    /// Cancellation is never retried: the user said no, and asking again
    /// immediately is precisely the behaviour that makes a tool feel hostile.
    fn open_session(&self, plan: &SessionPlan) -> Result<screencast::Negotiation> {
        let stored = self
            .capabilities
            .restore_tokens
            .then(|| self.stored_token(plan.restore_key))
            .flatten();

        let outcome = screencast::negotiate(plan, stored.as_deref());

        let negotiation = match outcome {
            Ok(negotiation) => negotiation,
            Err(PortalFailure::Cancelled) => return Err(Error::Cancelled),
            Err(failure) if stored.is_some() && failure.should_retry_without_restore() => {
                tracing::info!(
                    ?failure,
                    "the stored portal restore token was not accepted; asking again without it"
                );
                self.forget_token(plan.restore_key);
                self.persist_tokens();
                screencast::negotiate(plan, None)
                    .map_err(|err| err.into_error(&self.compositor.to_string()))?
            }
            Err(failure) => return Err(failure.into_error(&self.compositor.to_string())),
        };

        // Store on every success, not only when the token changed: the portal is
        // entitled to rotate a token it accepted, and dropping the rotation is a
        // silent regression to a prompt on the capture after next.
        match &negotiation.restore_token {
            Some(token) => self.remember_token(plan.restore_key, token),
            // A successful restore consumes the old token. If the portal did
            // not issue a replacement, retaining the old value guarantees a
            // stale restore attempt on the next capture.
            None if stored.is_some() => {
                self.forget_token(plan.restore_key);
                self.persist_tokens();
            }
            None => {}
        }

        Ok(negotiation)
    }

    /// Writes the token store out after an invalidation.
    fn persist_tokens(&self) {
        let (Ok(store), Some(path)) = (self.tokens.lock(), &self.token_path) else {
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

    /// The scale to assume when the stream itself does not reveal one.
    ///
    /// Only reached when the portal reported no stream size, which is rare; the
    /// active display's scale is the best available guess and an unreachable
    /// XWayland leaves 1:1, which is right on the majority of such machines.
    fn fallback_scale(&self) -> ScaleFactor {
        self.geometry
            .as_ref()
            .and_then(|backend| backend.active_display().ok())
            .map_or(ScaleFactor::IDENTITY, |display| display.scale)
    }

    /// Reads one frame from the first stream the portal granted.
    fn frame_from(&self, negotiation: &screencast::Negotiation) -> Result<(Frame, StreamInfo)> {
        // The portal may grant several streams for an all-displays request. Only
        // the first is read here: compositing them needs each stream's position,
        // which the specification makes optional, and stitching frames on
        // guessed geometry would produce a misaligned image rather than an
        // honest refusal.
        let stream = *negotiation
            .streams
            .first()
            .ok_or_else(|| PortalFailure::NoStreams.into_error(&self.compositor.to_string()))?;

        if negotiation.streams.len() > 1 {
            tracing::warn!(
                granted = negotiation.streams.len(),
                "the portal granted several streams; capturing the first, because compositing \
                 them needs per-stream positions the portal does not guarantee"
            );
        }

        // `OwnedFd` is moved into PipeWire, which closes it on teardown, so the
        // negotiation's fd is duplicated rather than consumed — the caller may
        // still want to report on the session afterwards.
        let fd = negotiation.remote.try_clone().map_err(|err| {
            Error::Platform(format!(
                "could not duplicate the PipeWire socket the portal returned: {err}"
            ))
        })?;

        let mut frame = pipewire::acquire_frame(fd, stream.node_id, self.fallback_scale())?;
        frame.scale = region::resolve_scale(
            &stream,
            (frame.width(), frame.height()),
            self.fallback_scale(),
        );

        Ok((frame, stream))
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
        let _capture = self.capture_gate.lock().map_err(|_| {
            Error::Platform(
                "the Wayland capture gate was poisoned by an earlier failed capture".into(),
            )
        })?;
        let plan = SessionPlan::for_target(&request.target, request.cursor == CursorMode::Visible);

        if request.target.is_window() && self.kind == SessionKind::Wayland {
            tracing::info!(
                "window capture on Wayland is chosen in the portal's picker, not by window id; \
                 the requested id is advisory only"
            );
        }

        let negotiation = self.open_session(&plan)?;
        let frame = self.frame_from(&negotiation);
        if let Err(close_failure) = negotiation.close() {
            tracing::warn!(
                ?close_failure,
                "could not close the desktop-portal ScreenCast session after capture"
            );
        }
        let (frame, stream) = frame?;

        let frame = match &request.target {
            CaptureTarget::Region(rect) => {
                let crop = region::plan_crop(*rect, &stream, (frame.width(), frame.height()))
                    .map_err(|err| err.into_error(&self.compositor.to_string()))?;
                region::crop(&frame, crop)
                    .map_err(|err| err.into_error(&self.compositor.to_string()))?
            }
            _ => frame,
        };

        debug_assert!(frame.is_well_formed());

        Ok(Capture {
            frame,
            provenance: match &request.target {
                CaptureTarget::Display(_) => Provenance::Display,
                // D9: the portal's own picker decides which window this is, and
                // the compositor supplies its true shape — the same guarantee
                // that makes window pixels sacred everywhere else.
                CaptureTarget::Window(_) => Provenance::Window,
                CaptureTarget::Region(_) => Provenance::Region,
                CaptureTarget::AllDisplays => Provenance::AllDisplays,
            },
            target: request.target.clone(),
        })
    }

    fn name(&self) -> &str {
        &self.name
    }
}
