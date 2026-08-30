//! Linux still capture.
//!
//! Linux is two platforms wearing one name. X11 lets any client read any pixel
//! and enumerate every window; Wayland forbids both by design and routes
//! everything through `xdg-desktop-portal`. They share no API, so this module
//! chooses between two complete backends at runtime rather than abstracting over
//! them.
//!
//! [`session`] makes that choice, and the distinction it draws between a native
//! X11 session and an X connection that merely happens to exist inside a Wayland
//! session is the important one: preferring `DISPLAY` when `WAYLAND_DISPLAY` is
//! also set produces a screenshot missing every native Wayland window, with no
//! error to explain it.
//!
//! # X11 feature boundaries
//!
//! The manifest enables the protocol features needed across both Linux capture
//! paths, but enabling a wire protocol is not the same as implementing every
//! capture path that can use it:
//!
//! | Need | Feature required | Consequence today |
//! |---|---|---|
//! | RandR monitor geometry / identity | `x11rb/randr` | Enabled; X11 keeps its tested wire parser and Wayland uses typed identity queries |
//! | MIT-SHM fast path | `x11rb/shm` plus shared-memory lifecycle | Protocol enabled; allocation/mapping is not implemented, so `GetImage` is used |
//! | XFIXES cursor image | `x11rb/xfixes` | Protocol enabled; cursor compositing remains unimplemented |
//! | Synthetic wheel input | `x11rb/xtest` | Enabled and used by the X11 scrolling driver |
//!
//! RandR was originally hand-rolled because without it a multi-monitor desktop
//! is indistinguishable from one enormous screen; that parser remains the
//! well-tested X11 path. MIT-SHM still needs ownership and cleanup code beyond
//! the generated requests, so merely enabling the feature does not make it safe.
//!
//! The Wayland side has no such gap: `ashpd` is built with `screencast` and
//! `screenshot`, and PipeWire is opened at run time rather than linked, so a
//! machine without it still builds, still starts, and still captures over X11.
//! See [`wayland::pipewire`] for why that indirection was chosen.

pub mod session;
pub mod wayland;
pub mod x11;

use scrozz_core::{Capture, CaptureBackend, CaptureRequest, Error, Result};

use self::session::{SessionEnv, SessionKind};
use crate::{CaptureCancellation, FrameSession};

/// Chooses and constructs the backend for this session.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] when neither display server is reachable, and
/// propagates connection failures from the chosen backend.
pub fn backend() -> Result<Box<dyn CaptureBackend>> {
    let env = SessionEnv::from_env();
    let kind = session::detect_session(&env);
    tracing::debug!(session = %session::describe(&env), "selecting a Linux capture backend");

    match kind {
        SessionKind::X11 => Ok(Box::new(x11::X11Backend::connect()?)),

        // XWayland is reachable here, and using it would appear to work — which
        // is exactly why it is refused. An X11 capture inside a Wayland session
        // sees only XWayland clients, so native GTK4 and Qt6 windows are absent
        // from the image with nothing to indicate it. The portal is the only
        // correct route, so the portal backend is what is returned, gaps and all.
        SessionKind::Wayland | SessionKind::XWayland => {
            Ok(Box::new(wayland::WaylandBackend::new(&env)?))
        }

        SessionKind::Headless => Err(Error::Unsupported {
            what: "screen capture".into(),
            why: "no display server was found: neither WAYLAND_DISPLAY nor DISPLAY is set, so \
                  this is a text console, a container without a forwarded socket, or a CI \
                  runner. Set one of them, or run under Xvfb, to capture"
                .into(),
        }),
    }
}

/// Captures once with cancellable Wayland portal acquisition.
pub fn capture_with_cancellation(
    request: &CaptureRequest,
    cancellation: &CaptureCancellation,
) -> Result<Capture> {
    cancellation.check()?;
    let env = SessionEnv::from_env();
    let kind = session::detect_session(&env);

    match kind {
        SessionKind::Wayland | SessionKind::XWayland => {
            wayland::WaylandBackend::new(&env)?.capture_with_cancellation(request, cancellation)
        }
        SessionKind::X11 | SessionKind::Headless => backend()?.capture(request),
    }
}

/// Opens a repeated-frame source for the current Linux display server.
///
/// Wayland gets a native long-lived portal/PipeWire session. X11 uses the
/// ordinary backend adapter because its direct capture has no permission session
/// to preserve.
pub fn frame_session(request: CaptureRequest) -> Result<Box<dyn FrameSession>> {
    frame_session_inner(request, None)
}

/// Opens a repeated-frame source with cancellable Wayland portal acquisition.
pub fn frame_session_with_cancellation(
    request: CaptureRequest,
    cancellation: &CaptureCancellation,
) -> Result<Box<dyn FrameSession>> {
    cancellation.check()?;
    frame_session_inner(request, Some(cancellation))
}

fn frame_session_inner(
    request: CaptureRequest,
    cancellation: Option<&CaptureCancellation>,
) -> Result<Box<dyn FrameSession>> {
    let env = SessionEnv::from_env();
    let kind = session::detect_session(&env);

    match kind {
        SessionKind::Wayland | SessionKind::XWayland => {
            let backend = wayland::WaylandBackend::new(&env)?;
            Ok(Box::new(
                backend.open_frame_session_inner(&request, cancellation)?,
            ))
        }
        SessionKind::X11 | SessionKind::Headless => Ok(crate::backend_frame_session(
            backend()?,
            request,
            cancellation,
        )),
    }
}

/// Chooses and constructs the scroll-input driver for this Linux session.
pub fn scroll_driver() -> Result<Box<dyn scrozz_core::ScrollDriver>> {
    let env = SessionEnv::from_env();
    match session::detect_session(&env) {
        SessionKind::X11 => match x11::scroll::X11ScrollDriver::connect() {
            Ok(driver) => Ok(Box::new(driver)),
            Err(Error::Unsupported { why, .. }) => {
                Ok(Box::new(scrozz_core::ManualScrollDriver::new(why)))
            }
            Err(error) => Err(error),
        },
        SessionKind::Wayland | SessionKind::XWayland => {
            let compositor = session::detect_compositor(&env);
            Ok(Box::new(scrozz_core::ManualScrollDriver::new(format!(
                "Wayland cannot guarantee that RemoteDesktop input is bound to the same \
                 portal-selected surface captured on {compositor}; scroll that surface manually \
                 while Scrozz follows"
            ))))
        }
        SessionKind::Headless => Err(Error::Unsupported {
            what: "scroll input".into(),
            why: "no display server was found: neither WAYLAND_DISPLAY nor DISPLAY is set".into(),
        }),
    }
}
