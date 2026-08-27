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
//! - An OS-id-bearing [`CaptureTarget::Window`] is likewise refused before
//!   prompting. The one reserved id
//!   [`crate::WAYLAND_PORTAL_PICKER_WINDOW_ID`] explicitly means "the window the
//!   portal user selects"; it makes no OS identity, geometry, crop, or alpha
//!   association claim and exists for manual scrolling capture.
//! - Display identity, logical geometry, current physical mode, and fractional
//!   scale come from the compositor's native `wl_output` plus `xdg-output`
//!   protocols. XWayland is used only for the optional pointer-based
//!   [`TargetEnumerator::active_display`] hint.
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

mod output;
pub mod pipewire;
pub mod portal;
pub mod region;
pub mod restore;
pub mod screencast;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::Duration;

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, Display, Error, Frame,
    Provenance, Result, ScaleFactor, TargetEnumerator, Window,
};

use self::portal::{PortalFailure, SessionPlan, StreamInfo};
use self::restore::{TokenFileLock, TokenKey, TokenStore};
use super::session::{self, Compositor, PortalCapabilities, SessionEnv, SessionKind};
use crate::{CaptureCancellation, FrameSession};

/// Process-wide restore-token state and negotiation gate.
///
/// Portal restore tokens can be single-use and rotated after every successful
/// `Start`. Keeping the store behind the same lock that serialises negotiation
/// prevents two independently constructed backends from presenting the same
/// token or overwriting each other's rotation. [`TokenFileLock`] extends that
/// transaction across processes. Both locks are released before the long-lived
/// PipeWire stream is opened, so one scrolling session cannot block unrelated
/// captures for its entire lifetime.
#[derive(Debug, Default)]
struct NegotiationState {
    stores: HashMap<Option<PathBuf>, TokenStore>,
}

impl NegotiationState {
    fn tokens(&mut self, path: &Option<PathBuf>) -> Result<&mut TokenStore> {
        let Some(path) = path else {
            return Ok(self.stores.entry(None).or_default());
        };
        let store = match restore::load(path) {
            Ok(store) => store,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => TokenStore::new(),
            Err(err) => {
                return Err(Error::Platform(format!(
                    "could not read the Wayland portal restore-token store at {}: {err}",
                    path.display()
                )));
            }
        };
        self.stores.insert(Some(path.clone()), store);
        self.stores.get_mut(&Some(path.clone())).ok_or_else(|| {
            Error::Platform("the process lost its in-memory portal token store".into())
        })
    }
}

static NEGOTIATION_STATE: OnceLock<Mutex<NegotiationState>> = OnceLock::new();

fn negotiation_state() -> &'static Mutex<NegotiationState> {
    NEGOTIATION_STATE.get_or_init(|| Mutex::new(NegotiationState::default()))
}

fn lock_negotiation_state(
    cancellation: Option<&CaptureCancellation>,
) -> Result<MutexGuard<'static, NegotiationState>> {
    let state = negotiation_state();
    let Some(cancellation) = cancellation else {
        return state.lock().map_err(|_| {
            Error::Platform(
                "the Wayland portal negotiation gate was poisoned by an earlier failed capture"
                    .into(),
            )
        });
    };

    loop {
        cancellation.check()?;
        match state.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
            Err(TryLockError::Poisoned(_)) => {
                return Err(Error::Platform(
                    "the Wayland portal negotiation gate was poisoned by an earlier failed capture"
                        .into(),
                ));
            }
        }
    }
}

fn lock_token_file(
    path: Option<&Path>,
    cancellation: Option<&CaptureCancellation>,
) -> Result<Option<TokenFileLock>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let Some(cancellation) = cancellation else {
        return TokenFileLock::acquire(path)
            .map(Some)
            .map_err(token_lock_error);
    };

    loop {
        cancellation.check()?;
        match TokenFileLock::try_acquire(path).map_err(token_lock_error)? {
            Some(lock) => return Ok(Some(lock)),
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn token_lock_error(err: std::io::Error) -> Error {
    Error::Platform(format!(
        "could not lock the Wayland portal restore-token store: {err}"
    ))
}

fn persist_tokens(path: Option<&Path>, store: &TokenStore) -> Result<()> {
    restore::persist_fail_closed(path, store).map_err(|err| {
        Error::Platform(format!(
            "could not persist the rotated Wayland portal restore token{}: {err}. Capture stopped \
             before opening PipeWire, and Scrozz removed the previous store where possible so \
             another process cannot reuse consumed authorization state",
            path.map_or_else(String::new, |path| format!(" at {}", path.display()))
        ))
    })
}

/// Still capture through `xdg-desktop-portal`.
pub struct WaylandBackend {
    compositor: Compositor,
    capabilities: PortalCapabilities,
    kind: SessionKind,
    token_path: Option<std::path::PathBuf>,
    /// XWayland, used only to locate the pointer's native output — never to
    /// enumerate outputs or capture.
    ///
    /// Capturing through it would silently omit every native Wayland window,
    /// which is the single most common Wayland screenshot bug. Reading geometry
    /// through it is safe, because XWayland is configured by the compositor to
    /// mirror the real output layout. Exact target facts always come from the
    /// native Wayland output registry.
    pointer_geometry: Option<Arc<super::x11::X11Backend>>,
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
    outputs: Option<output::OutputMonitor>,
    verified_display: Option<VerifiedDisplay>,
    fallback_scale: ScaleFactor,
    compositor: String,
    name: String,
}

#[derive(Debug, Clone)]
struct VerifiedDisplay {
    display: Display,
    /// The compositor-global `wl_output` proven to back this display.
    output_identity: output::OutputIdentity,
}

fn expected_display_from_outputs(
    outputs: Option<&mut output::OutputMonitor>,
    compositor: &str,
    target: &CaptureTarget,
) -> Result<Option<VerifiedDisplay>> {
    let needs_geometry = matches!(target, CaptureTarget::Display(_) | CaptureTarget::Region(_));
    if !needs_geometry {
        return Ok(None);
    }

    let outputs = outputs.ok_or_else(|| Error::Unsupported {
        what: "capturing an exact display or region on Wayland".into(),
        why: format!(
            "ScreenCast asks the user to choose a monitor and cannot target one by Scrozz display \
             id. Scrozz could not retain {compositor}'s native wl_output/xdg-output registry, so \
             the returned stream cannot be checked against a compositor output. Scrozz refuses \
             before opening the portal picker instead of guessing"
        ),
    })?;
    let snapshots = outputs.snapshots(compositor)?;
    let displays: Vec<Display> = snapshots
        .iter()
        .map(|snapshot| snapshot.display.clone())
        .collect();

    let display = match target {
        CaptureTarget::Display(id) => displays
            .iter()
            .find(|display| &display.id == id)
            .cloned()
            .ok_or_else(|| Error::TargetGone(format!("display {} is no longer connected", id.0)))?,
        CaptureTarget::Region(rect) => region::display_for_region(*rect, &displays)
            .cloned()
            .map_err(region::RegionDisplayError::into_error)?,
        CaptureTarget::Window(_) | CaptureTarget::AllDisplays => return Ok(None),
    };
    region::verify_display_identity(&display, &displays)
        .map_err(|failure| failure.into_error(compositor, &display))?;
    let output_identity = snapshots
        .iter()
        .find(|snapshot| snapshot.display.id == display.id)
        .map(|snapshot| snapshot.identity.clone())
        .ok_or_else(|| {
            Error::Platform(
                "the verified Wayland display lost its native wl_output identity".into(),
            )
        })?;
    Ok(Some(VerifiedDisplay {
        display,
        output_identity,
    }))
}

impl std::fmt::Debug for WaylandFrameSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandFrameSession")
            .field("stream", &self.stream)
            .field("target", &self.target)
            .field("verified_display", &self.verified_display)
            .field("compositor", &self.compositor)
            .finish_non_exhaustive()
    }
}

impl WaylandFrameSession {
    fn verify_target(&mut self) -> Result<()> {
        let Some(initial) = self.verified_display.as_ref() else {
            return Ok(());
        };
        let outputs = self.outputs.as_mut().ok_or_else(|| {
            Error::Platform(
                "the reusable Wayland session lost its native output identity source".into(),
            )
        })?;
        let current = expected_display_from_outputs(Some(outputs), &self.compositor, &self.target)?
            .ok_or_else(|| {
                Error::Platform(
                    "the reusable Wayland session lost its verified target display".into(),
                )
            })?;

        if !region::display_identity_unchanged(
            &initial.display,
            &initial.output_identity,
            &current.display,
            &current.output_identity,
        ) {
            return Err(Error::TargetGone(format!(
                "the display backing this Wayland capture session changed from {} at {:?} on \
                 wl_output {} to {} at {:?} on wl_output {}; reopen capture after the display \
                 layout settles",
                initial.display.id.0,
                initial.display.bounds,
                initial.output_identity,
                current.display.id.0,
                current.display.bounds,
                current.output_identity
            )));
        }
        region::verify_stream_matches_display(&self.stream, &current.display)
            .map_err(|failure| failure.into_error(&self.compositor, &current.display, &self.stream))
    }
}

impl FrameSession for WaylandFrameSession {
    fn capture_frame(&mut self) -> Result<Frame> {
        self.verify_target()?;
        let mut frame = self.pipewire.capture_frame()?;
        self.verify_target()?;
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
            .field("xwayland_pointer_hint", &self.pointer_geometry.is_some())
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

        // Pointer position is privileged on Wayland. XWayland can provide that
        // optional hint on GNOME and KDE, while native output enumeration remains
        // available in a pure-Wayland session without it.
        let pointer_geometry = (env.display.is_some())
            .then(super::x11::X11Backend::connect)
            .and_then(|result| match result {
                Ok(backend) => Some(Arc::new(backend)),
                Err(err) => {
                    tracing::debug!(%err, "XWayland unreachable; active-display pointer hint unavailable");
                    None
                }
            });

        let name = format!("xdg-desktop-portal ScreenCast on {compositor}");

        Ok(Self {
            compositor,
            capabilities,
            kind,
            token_path,
            pointer_geometry,
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

    fn output_monitor(&self) -> Result<output::OutputMonitor> {
        output::OutputMonitor::connect(&self.compositor.to_string())
    }

    fn output_snapshots(&self) -> Result<Vec<output::OutputSnapshot>> {
        self.output_monitor()?
            .snapshots(&self.compositor.to_string())
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
        initial_expected_display: Option<&VerifiedDisplay>,
        outputs: Option<&mut output::OutputMonitor>,
        cancellation: Option<&CaptureCancellation>,
    ) -> Result<(screencast::Negotiation, Option<VerifiedDisplay>)> {
        tracing::debug!("waiting for the process-wide portal negotiation gate");
        let mut state = lock_negotiation_state(cancellation)?;
        tracing::debug!("acquired the process-wide portal negotiation gate");
        let _file_lock = lock_token_file(self.token_path.as_deref(), cancellation)?;
        let tokens = state.tokens(&self.token_path)?;
        let mut outputs = outputs;
        let mut candidate = self
            .capabilities
            .restore_tokens
            .then(|| {
                tokens
                    .candidate(&plan.restore_key)
                    .map(|(key, token)| (key, token.to_owned()))
            })
            .flatten();

        loop {
            let stored_key = candidate.as_ref().map(|(key, _)| key);
            let stored = candidate.as_ref().map(|(_, token)| token.as_str());
            tracing::debug!(
                restore_key = plan.restore_key.as_str(),
                stored_key = stored_key.map(TokenKey::as_str),
                stored_token = stored.is_some(),
                "opening a desktop-portal ScreenCast session"
            );

            let negotiation = match screencast::negotiate(plan, stored, cancellation) {
                Ok(negotiation) => negotiation,
                Err(PortalFailure::Cancelled) => return Err(Error::Cancelled),
                Err(failure) if stored.is_some() && failure.should_retry_without_restore() => {
                    tracing::info!(
                        ?failure,
                        "the stored portal restore token was not accepted; asking again without it"
                    );
                    if let Some(stored_key) = stored_key {
                        tokens.invalidate(stored_key);
                    }
                    persist_tokens(self.token_path.as_deref(), tokens)?;
                    candidate = None;
                    continue;
                }
                Err(failure) => return Err(failure.into_error(&self.compositor.to_string())),
            };

            // Start consumes a single-use restore token and may rotate it. Commit
            // that transaction before any fallible stream/topology validation or
            // PipeWire work. If validation below proves this grant names the
            // wrong target, it is invalidated and persisted again before retrying.
            let token_store_changed = match &negotiation.restore_token {
                Some(token) => {
                    let changed = tokens.get(&plan.restore_key) != Some(token.as_str())
                        || (plan.restore_key.is_display()
                            && tokens.get(&TokenKey::Monitor).is_some());
                    tracing::debug!(
                        restore_key = plan.restore_key.as_str(),
                        replaced = stored != Some(token.as_str()),
                        "the desktop portal issued a restore token"
                    );
                    tokens.set(&plan.restore_key, token);
                    if plan.restore_key.is_display() {
                        tokens.invalidate(&TokenKey::Monitor);
                    }
                    changed
                }
                // A successful restore consumes the old token. If the portal did
                // not issue a replacement, retaining the old value guarantees a
                // stale restore attempt on the next capture.
                None if stored.is_some() => {
                    tracing::debug!(
                        restore_key = plan.restore_key.as_str(),
                        "the desktop portal consumed the restore token without replacing it"
                    );
                    if let Some(stored_key) = stored_key {
                        tokens.invalidate(stored_key);
                    }
                    if plan.restore_key.is_display() {
                        tokens.invalidate(&TokenKey::Monitor);
                    }
                    true
                }
                None if plan.restore_key.is_display()
                    && tokens.get(&TokenKey::Monitor).is_some() =>
                {
                    tokens.invalidate(&TokenKey::Monitor);
                    true
                }
                None => false,
            };
            if token_store_changed {
                persist_tokens(self.token_path.as_deref(), tokens)?;
            }

            if let Some(error) = negotiation.start_error.as_deref() {
                if let Some(stored_key) = stored_key {
                    tokens.invalidate(stored_key);
                }
                tokens.invalidate(&plan.restore_key);
                persist_tokens(self.token_path.as_deref(), tokens).map_err(|persist_error| {
                    Error::Platform(format!(
                        "the portal returned a malformed successful Start response ({error}), and \
                         Scrozz could not invalidate its consumed restore token: {persist_error}"
                    ))
                })?;
                return Err(
                    PortalFailure::Bus(error.to_owned()).into_error(&self.compositor.to_string())
                );
            }

            if let Some(error) = negotiation.streams.iter().find_map(|stream| {
                stream
                    .validate_contract(negotiation.portal_version, plan.types)
                    .err()
            }) {
                if let Some(stored_key) = stored_key {
                    tokens.invalidate(stored_key);
                }
                tokens.invalidate(&plan.restore_key);
                persist_tokens(self.token_path.as_deref(), tokens).map_err(|persist_error| {
                    Error::Platform(format!(
                        "the portal returned an unverifiable stream ({error}), and Scrozz could \
                         not invalidate its consumed restore token: {persist_error}"
                    ))
                })?;
                return Err(PortalFailure::Bus(error).into_error(&self.compositor.to_string()));
            }

            let stream = match negotiation.streams.as_slice() {
                [stream] => *stream,
                streams if stored.is_some() => {
                    tracing::info!(
                        streams = streams.len(),
                        "the restored portal session did not resolve to exactly one source; \
                         asking again without it"
                    );
                    if let Some(stored_key) = stored_key {
                        tokens.invalidate(stored_key);
                    }
                    tokens.invalidate(&plan.restore_key);
                    persist_tokens(self.token_path.as_deref(), tokens)?;
                    candidate = None;
                    drop(negotiation);
                    continue;
                }
                streams => {
                    tokens.invalidate(&plan.restore_key);
                    persist_tokens(self.token_path.as_deref(), tokens)?;
                    return Err(Error::Platform(format!(
                        "the desktop portal returned {} streams after Scrozz explicitly requested \
                         one",
                        streams.len()
                    )));
                }
            };

            let expected_display = if let Some(initial) = initial_expected_display {
                let current = match self.expected_display(target, outputs.as_deref_mut()) {
                    Ok(Some(current)) => current,
                    Ok(None) => {
                        tokens.invalidate(&plan.restore_key);
                        persist_tokens(self.token_path.as_deref(), tokens)?;
                        return Err(Error::Platform(
                            "Wayland exact-target verification lost its display after portal Start"
                                .into(),
                        ));
                    }
                    Err(error) => {
                        if let Some(stored_key) = stored_key {
                            tokens.invalidate(stored_key);
                        }
                        tokens.invalidate(&plan.restore_key);
                        persist_tokens(self.token_path.as_deref(), tokens)?;
                        return Err(error);
                    }
                };
                if !region::display_identity_unchanged(
                    &initial.display,
                    &initial.output_identity,
                    &current.display,
                    &current.output_identity,
                ) {
                    if let Some(stored_key) = stored_key {
                        tokens.invalidate(stored_key);
                    }
                    tokens.invalidate(&plan.restore_key);
                    persist_tokens(self.token_path.as_deref(), tokens)?;
                    return Err(Error::TargetGone(format!(
                        "the display containing the requested Wayland target changed from {} on \
                         wl_output {} to {} on wl_output {} while the portal picker was open; retry \
                         after the display layout settles",
                        initial.display.id.0,
                        initial.output_identity,
                        current.display.id.0,
                        current.output_identity
                    )));
                }
                Some(current)
            } else {
                None
            };

            if let Some(expected_display) = expected_display.as_ref() {
                if let Err(failure) =
                    region::verify_stream_matches_display(&stream, &expected_display.display)
                {
                    if stored.is_some() {
                        tracing::info!(
                            ?failure,
                            requested_display = %expected_display.display.id.0,
                            "the restored portal session resolved to a different display; asking \
                             again without it"
                        );
                        if let Some(stored_key) = stored_key {
                            tokens.invalidate(stored_key);
                        }
                        tokens.invalidate(&plan.restore_key);
                        persist_tokens(self.token_path.as_deref(), tokens)?;
                        candidate = None;
                        drop(negotiation);
                        continue;
                    }
                    tokens.invalidate(&plan.restore_key);
                    tokens.invalidate(&TokenKey::Monitor);
                    persist_tokens(self.token_path.as_deref(), tokens)?;
                    return Err(failure.into_error(
                        &self.compositor.to_string(),
                        &expected_display.display,
                        &stream,
                    ));
                }
                tracing::debug!(
                    display_id = %expected_display.display.id.0,
                    display_name = %expected_display.display.name,
                    output_identity = %expected_display.output_identity,
                    position = ?stream.position,
                    size = ?stream.size,
                    "portal stream geometry matched the requested display"
                );
            }

            return Ok((negotiation, expected_display));
        }
    }

    /// The scale to assume when the stream itself does not reveal one.
    ///
    /// Only reached when the portal reported no stream size, which is rare; the
    /// selected display's native scale is the best available fallback. A portal
    /// window has no independently disclosed output, so it falls back to 1:1
    /// only when its stream also omits logical size.
    fn fallback_scale(&self) -> ScaleFactor {
        ScaleFactor::IDENTITY
    }

    /// Resolves the exact display a monitor stream must represent.
    ///
    /// ScreenCast cannot target a portal picker by display id. Native compositor
    /// output geometry is therefore the independent fact used to verify the
    /// stream the portal granted.
    fn expected_display(
        &self,
        target: &CaptureTarget,
        outputs: Option<&mut output::OutputMonitor>,
    ) -> Result<Option<VerifiedDisplay>> {
        expected_display_from_outputs(outputs, &self.compositor.to_string(), target)
    }

    fn fallback_scale_for(&self, expected_display: Option<&VerifiedDisplay>) -> ScaleFactor {
        expected_display.map_or_else(|| self.fallback_scale(), |display| display.display.scale)
    }

    /// Opens one portal and PipeWire stream for repeated viewport captures.
    ///
    /// # Errors
    ///
    /// Rejects unsupported targets before opening the portal, then propagates
    /// portal, token, remote, and PipeWire connection failures.
    pub fn open_frame_session(&self, request: &CaptureRequest) -> Result<WaylandFrameSession> {
        self.open_frame_session_inner(request, None)
    }

    pub(crate) fn open_frame_session_inner(
        &self,
        request: &CaptureRequest,
        cancellation: Option<&CaptureCancellation>,
    ) -> Result<WaylandFrameSession> {
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        let mut plan =
            SessionPlan::for_target(&request.target, request.cursor == CursorMode::Visible)
                .map_err(portal::PlanFailure::into_error)?;
        let needs_outputs = matches!(
            request.target,
            CaptureTarget::Display(_) | CaptureTarget::Region(_)
        );
        let mut outputs = needs_outputs.then(|| self.output_monitor()).transpose()?;
        let expected_display = self.expected_display(&request.target, outputs.as_mut())?;
        if matches!(request.target, CaptureTarget::Region(_)) {
            let display = expected_display.as_ref().ok_or_else(|| {
                Error::Platform(
                    "Wayland region capture lost its verified containing display".into(),
                )
            })?;
            plan = plan.bind_monitor(&display.display.id);
        }
        // Loading PipeWire after Start would prompt the user even when capture is
        // guaranteed to fail. Resolve every symbol and validate the handwritten
        // ABI before opening any portal picker.
        let pipewire_runtime = pipewire::preflight()?;

        let (mut negotiation, verified_display) = self.open_session(
            &plan,
            &request.target,
            expected_display.as_ref(),
            outputs.as_mut(),
            cancellation,
        )?;
        let stream = *negotiation
            .streams
            .first()
            .ok_or_else(|| PortalFailure::NoStreams.into_error(&self.compositor.to_string()))?;

        let fd = negotiation
            .open_remote(cancellation)
            .map_err(|failure| failure.into_error(&self.compositor.to_string()))?;
        let fd = fd.try_clone().map_err(|err| {
            Error::Platform(format!(
                "could not duplicate the PipeWire socket the portal returned: {err}"
            ))
        })?;
        let fallback_scale = self.fallback_scale_for(verified_display.as_ref());
        let pipewire = pipewire::FrameStream::connect(
            pipewire_runtime,
            fd,
            stream.node_id,
            stream.pipewire_serial,
            fallback_scale,
        )?;

        Ok(WaylandFrameSession {
            pipewire,
            _negotiation: negotiation,
            stream,
            target: request.target.clone(),
            outputs: verified_display.as_ref().and(outputs),
            verified_display,
            fallback_scale,
            compositor: self.compositor.to_string(),
            name: self.name.clone(),
        })
    }

    pub(crate) fn capture_with_cancellation(
        &self,
        request: &CaptureRequest,
        cancellation: &CaptureCancellation,
    ) -> Result<Capture> {
        let mut session = self.open_frame_session_inner(request, Some(cancellation))?;
        cancellation.check()?;
        let frame = session.capture_frame()?;
        Ok(capture_from_frame(request, frame))
    }
}

impl TargetEnumerator for WaylandBackend {
    fn displays(&self) -> Result<Vec<Display>> {
        Ok(self
            .output_snapshots()?
            .into_iter()
            .map(|snapshot| snapshot.display)
            .collect())
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
        let displays = self.displays()?;
        if displays.len() == 1 {
            return displays.into_iter().next().ok_or_else(|| {
                Error::TargetGone("the sole Wayland output disappeared during enumeration".into())
            });
        }
        let xwayland =
            self.pointer_geometry.as_deref().ok_or_else(|| {
                Error::Unsupported {
            what: "choosing the active display on Wayland".into(),
            why: "Wayland deliberately does not expose global pointer position or a primary-output \
                  concept. Scrozz can identify the active output through XWayland when it exists, \
                  or unambiguously in a single-output session, but this pure-Wayland session has \
                  several outputs. Choose a display explicitly."
                .into(),
        }
            })?;
        let active = xwayland.active_display()?;
        let mut matching = displays
            .into_iter()
            .filter(|display| display.bounds == active.bounds);
        let selected = matching.next().ok_or_else(|| Error::Unsupported {
            what: "choosing the active display on Wayland".into(),
            why: "XWayland's pointer-containing monitor could not be matched exactly to one native \
                  wl_output. Scrozz refuses to apply a global X11 scale to compositor-native output \
                  geometry; choose a display explicitly."
                .into(),
        })?;
        if matching.next().is_some() {
            return Err(Error::Unsupported {
                what: "choosing the active display on mirrored Wayland outputs".into(),
                why: "Several native outputs share the pointer-containing geometry, so Wayland \
                      does not disclose which physical output is active. Choose a display \
                      explicitly."
                    .into(),
            });
        }
        Ok(selected)
    }
}

impl CaptureBackend for WaylandBackend {
    fn capture(&self, request: &CaptureRequest) -> Result<Capture> {
        let mut session = self.open_frame_session(request)?;
        let frame = session.capture_frame()?;
        Ok(capture_from_frame(request, frame))
    }

    fn name(&self) -> &str {
        &self.name
    }
}

fn capture_from_frame(request: &CaptureRequest, frame: Frame) -> Capture {
    Capture {
        frame,
        provenance: match &request.target {
            CaptureTarget::Display(_) => Provenance::Display,
            CaptureTarget::Region(_) => Provenance::Region,
            CaptureTarget::AllDisplays => {
                unreachable!("the session plan rejects this target")
            }
            CaptureTarget::Window(_) => Provenance::Window,
        },
        target: request.target.clone(),
    }
}
