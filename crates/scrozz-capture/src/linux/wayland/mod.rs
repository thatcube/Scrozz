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
//! - All-display capture is refused before the portal picker opens. ScreenCast
//!   does not guarantee positions for every returned stream, so a correct virtual
//!   desktop cannot be composed on every compositor and capturing only the first
//!   display would misrepresent the result.
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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, Display, Error, Frame,
    LogicalPoint, LogicalRect, LogicalSize, Provenance, Result, ScaleFactor, TargetEnumerator,
    Window,
};

use self::portal::{PortalFailure, SessionPlan, StreamInfo};
use self::restore::TokenStore;
use super::session::{self, Compositor, PortalCapabilities, SessionEnv, SessionKind};
use crate::FrameSession;

/// Process-wide restore-token state and negotiation gate.
///
/// Portal restore tokens can be single-use and rotated after every successful
/// `Start`. Keeping the store behind the same lock that serialises negotiation
/// prevents two independently constructed backends from presenting the same
/// token or overwriting each other's rotation. The lock is released before the
/// long-lived PipeWire stream is opened, so one scrolling session cannot block
/// unrelated captures for its entire lifetime.
#[derive(Debug, Default)]
struct NegotiationState {
    stores: HashMap<Option<PathBuf>, TokenStore>,
}

impl NegotiationState {
    fn tokens(&mut self, path: &Option<PathBuf>) -> &mut TokenStore {
        self.stores.entry(path.clone()).or_insert_with(|| {
            path.as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .map_or_else(TokenStore::new, |text| TokenStore::parse(&text))
        })
    }
}

static NEGOTIATION_STATE: OnceLock<Mutex<NegotiationState>> = OnceLock::new();

fn negotiation_state() -> &'static Mutex<NegotiationState> {
    NEGOTIATION_STATE.get_or_init(|| Mutex::new(NegotiationState::default()))
}

fn persist_tokens(path: Option<&Path>, store: &TokenStore) {
    let Some(path) = path else {
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

const PORTAL_GEOMETRY_TOLERANCE: f64 = 1.0;

fn rect_approximately_equals(left: LogicalRect, right: LogicalRect) -> bool {
    (left.origin.x - right.origin.x).abs() <= PORTAL_GEOMETRY_TOLERANCE
        && (left.origin.y - right.origin.y).abs() <= PORTAL_GEOMETRY_TOLERANCE
        && (left.size.width - right.size.width).abs() <= PORTAL_GEOMETRY_TOLERANCE
        && (left.size.height - right.size.height).abs() <= PORTAL_GEOMETRY_TOLERANCE
}

fn rect_contains(outer: LogicalRect, inner: LogicalRect) -> bool {
    inner.origin.x + PORTAL_GEOMETRY_TOLERANCE >= outer.origin.x
        && inner.origin.y + PORTAL_GEOMETRY_TOLERANCE >= outer.origin.y
        && inner.origin.x + inner.size.width
            <= outer.origin.x + outer.size.width + PORTAL_GEOMETRY_TOLERANCE
        && inner.origin.y + inner.size.height
            <= outer.origin.y + outer.size.height + PORTAL_GEOMETRY_TOLERANCE
}

fn describe_rect(rect: LogicalRect) -> String {
    format!(
        "({:.0}, {:.0}) {:.0}x{:.0}",
        rect.origin.x, rect.origin.y, rect.size.width, rect.size.height
    )
}

/// Still capture through `xdg-desktop-portal`.
pub struct WaylandBackend {
    compositor: Compositor,
    capabilities: PortalCapabilities,
    kind: SessionKind,
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

/// A portal grant and PipeWire stream retained across viewport captures.
///
/// Field order is part of teardown: the PipeWire stream drops first, then the
/// portal session closes.
pub struct WaylandFrameSession {
    pipewire: pipewire::FrameStream,
    _negotiation: screencast::Negotiation,
    stream: StreamInfo,
    target: CaptureTarget,
    fallback_scale: ScaleFactor,
    compositor: String,
    name: String,
}

impl std::fmt::Debug for WaylandFrameSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandFrameSession")
            .field("stream", &self.stream)
            .field("target", &self.target)
            .field("compositor", &self.compositor)
            .finish_non_exhaustive()
    }
}

impl FrameSession for WaylandFrameSession {
    fn capture_frame(&mut self) -> Result<Frame> {
        let mut frame = self.pipewire.capture_frame()?;
        frame.scale = region::resolve_scale(
            &self.stream,
            (frame.width(), frame.height()),
            self.fallback_scale,
        );

        if let CaptureTarget::Region(rect) = &self.target {
            let crop = region::plan_crop(*rect, &self.stream, (frame.width(), frame.height()))
                .map_err(|err| err.into_error(&self.compositor))?;
            frame = region::crop(&frame, crop).map_err(|err| err.into_error(&self.compositor))?;
        }

        debug_assert!(frame.is_well_formed());
        Ok(frame)
    }

    fn name(&self) -> &str {
        &self.name
    }
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

    fn validate_negotiated_target(
        &self,
        target: &CaptureTarget,
        negotiation: &screencast::Negotiation,
    ) -> Result<()> {
        if negotiation.streams.len() != 1 {
            return Err(Error::Platform(format!(
                "the desktop portal returned {} streams after Scrozz explicitly requested one",
                negotiation.streams.len()
            )));
        }
        let stream = negotiation
            .streams
            .first()
            .ok_or_else(|| PortalFailure::NoStreams.into_error(&self.compositor.to_string()))?;
        let stream_bounds = match (stream.position, stream.size) {
            (Some((x, y)), Some((width, height))) if width > 0 && height > 0 => {
                Some(LogicalRect::new(
                    LogicalPoint::new(f64::from(x), f64::from(y)),
                    LogicalSize::new(f64::from(width), f64::from(height)),
                ))
            }
            _ => None,
        };

        match target {
            CaptureTarget::Display(id) => {
                let expected = self
                    .geometry_backend()?
                    .displays()?
                    .into_iter()
                    .find(|display| display.id == *id)
                    .ok_or_else(|| Error::TargetGone(format!("display `{}`", id.0)))?;
                let actual = stream_bounds.ok_or_else(|| Error::Unsupported {
                    what: "binding the portal stream to the requested display".into(),
                    why: "the portal omitted the stream's compositor-space position or size, so \
                          Scrozz cannot prove that its picker returned the requested display"
                        .into(),
                })?;
                if !rect_approximately_equals(actual, expected.bounds) {
                    return Err(Error::InvalidRequest(format!(
                        "the desktop portal selected display bounds {} instead of the requested \
                         display `{}` at {}; choose the requested display in the portal picker",
                        describe_rect(actual),
                        id.0,
                        describe_rect(expected.bounds)
                    )));
                }
            }
            CaptureTarget::Region(region) => {
                let actual = stream_bounds.ok_or_else(|| Error::Unsupported {
                    what: "binding the portal stream to the requested region".into(),
                    why: "the portal omitted the selected monitor's compositor-space position or \
                          size, so Scrozz cannot prove that it contains the requested region"
                        .into(),
                })?;
                if !rect_contains(actual, *region) {
                    return Err(Error::InvalidRequest(format!(
                        "the desktop portal selected monitor bounds {}, which do not fully contain \
                         the requested region {}; choose the region's monitor in the portal picker",
                        describe_rect(actual),
                        describe_rect(*region)
                    )));
                }
            }
            CaptureTarget::Window(_) => {
                // Wayland exposes no stable global window identity. A fresh
                // portal picker is therefore authoritative for every session.
            }
            CaptureTarget::AllDisplays => unreachable!("the session plan rejects this target"),
        }
        Ok(())
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
    fn open_session(
        &self,
        plan: &SessionPlan,
        target: &CaptureTarget,
    ) -> Result<screencast::Negotiation> {
        tracing::debug!("waiting for the process-wide portal negotiation gate");
        let mut state = negotiation_state().lock().map_err(|_| {
            Error::Platform(
                "the Wayland portal negotiation gate was poisoned by an earlier failed capture"
                    .into(),
            )
        })?;
        tracing::debug!("acquired the process-wide portal negotiation gate");
        let tokens = state.tokens(&self.token_path);
        let stored = plan.restore_key.and_then(|key| {
            self.capabilities
                .restore_tokens
                .then(|| tokens.get(key).map(ToOwned::to_owned))
                .flatten()
        });
        tracing::debug!(
            restore_key = plan
                .restore_key
                .map(restore::TokenKey::storage_key)
                .as_deref()
                .unwrap_or("none"),
            stored_token = stored.is_some(),
            "opening a desktop-portal ScreenCast session"
        );

        let outcome = screencast::negotiate(plan, stored.as_deref());

        let mut negotiation = match outcome {
            Ok(negotiation) => negotiation,
            Err(PortalFailure::Cancelled) => return Err(Error::Cancelled),
            Err(failure) if stored.is_some() && failure.should_retry_without_restore() => {
                tracing::info!(
                    ?failure,
                    "the stored portal restore token was not accepted; asking again without it"
                );
                if let Some(key) = plan.restore_key {
                    tokens.invalidate(key);
                    persist_tokens(self.token_path.as_deref(), tokens);
                }
                screencast::negotiate(plan, None)
                    .map_err(|err| err.into_error(&self.compositor.to_string()))?
            }
            Err(failure) => return Err(failure.into_error(&self.compositor.to_string())),
        };

        if let Err(error) = self.validate_negotiated_target(target, &negotiation) {
            if let (Some(key), Some(_)) = (plan.restore_key, stored.as_ref()) {
                tracing::info!(
                    restore_key = key.storage_key(),
                    %error,
                    "the restored portal stream no longer matches its target; asking once without it"
                );
                tokens.invalidate(key);
                persist_tokens(self.token_path.as_deref(), tokens);
                drop(negotiation);
                negotiation = screencast::negotiate(plan, None)
                    .map_err(|err| err.into_error(&self.compositor.to_string()))?;
                self.validate_negotiated_target(target, &negotiation)?;
            } else {
                return Err(error);
            }
        }

        // Store on every success, not only when the token changed: the portal is
        // entitled to rotate a token it accepted, and dropping the rotation is a
        // silent regression to a prompt on the capture after next.
        match (&negotiation.restore_token, plan.restore_key) {
            (Some(token), Some(key)) => {
                tracing::debug!(
                    restore_key = key.storage_key(),
                    replaced = stored.as_deref() != Some(token.as_str()),
                    "the desktop portal issued a restore token"
                );
                tokens.set(key, token);
                persist_tokens(self.token_path.as_deref(), tokens);
            }
            // A successful restore consumes the old token. If the portal did
            // not issue a replacement, retaining the old value guarantees a
            // stale restore attempt on the next capture.
            (None, Some(key)) if stored.is_some() => {
                tracing::debug!(
                    restore_key = key.storage_key(),
                    "the desktop portal consumed the restore token without replacing it"
                );
                tokens.invalidate(key);
                persist_tokens(self.token_path.as_deref(), tokens);
            }
            _ => {}
        }

        Ok(negotiation)
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

    /// Opens one portal and PipeWire stream for repeated viewport captures.
    ///
    /// # Errors
    ///
    /// Rejects unsupported targets before opening the portal, then propagates
    /// portal, token, remote, and PipeWire connection failures.
    pub fn open_frame_session(&self, request: &CaptureRequest) -> Result<WaylandFrameSession> {
        let plan = SessionPlan::for_target(&request.target, request.cursor == CursorMode::Visible)
            .map_err(portal::PlanFailure::into_error)?;

        if request.target.is_window() && self.kind == SessionKind::Wayland {
            tracing::info!(
                "window capture on Wayland is chosen in the portal's picker, not by window id; \
                 the requested id is advisory only"
            );
        }

        let negotiation = self.open_session(&plan, &request.target)?;
        let stream = *negotiation
            .streams
            .first()
            .ok_or_else(|| PortalFailure::NoStreams.into_error(&self.compositor.to_string()))?;

        let fd = negotiation.remote.try_clone().map_err(|err| {
            Error::Platform(format!(
                "could not duplicate the PipeWire socket the portal returned: {err}"
            ))
        })?;
        let fallback_scale = self.fallback_scale();
        let pipewire = pipewire::FrameStream::connect(fd, stream.node_id, fallback_scale)?;

        Ok(WaylandFrameSession {
            pipewire,
            _negotiation: negotiation,
            stream,
            target: request.target.clone(),
            fallback_scale,
            compositor: self.compositor.to_string(),
            name: self.name.clone(),
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
        let mut session = self.open_frame_session(request)?;
        let frame = session.capture_frame()?;

        Ok(Capture {
            frame,
            provenance: match &request.target {
                CaptureTarget::Display(_) => Provenance::Display,
                // D9: the portal's own picker decides which window this is, and
                // the compositor supplies its true shape — the same guarantee
                // that makes window pixels sacred everywhere else.
                CaptureTarget::Window(_) => Provenance::Window,
                CaptureTarget::Region(_) => Provenance::Region,
                CaptureTarget::AllDisplays => unreachable!("the session plan rejects this target"),
            },
            target: request.target.clone(),
        })
    }

    fn name(&self) -> &str {
        &self.name
    }
}
