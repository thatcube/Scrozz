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
//! # Native feature status
//!
//! The workspace enables `x11rb`'s `randr`, `shm`, `libc` and `xfixes` features,
//! and `ashpd`'s `screencast` and `screenshot` features. RandR geometry and
//! XFIXES cursor composition are used here. Static X11 captures retain the
//! universally-correct `GetImage` baseline; MIT-SHM is an unimplemented
//! performance optimisation, not a missing Cargo capability. Portal negotiation
//! is available, while continuous PipeWire media acquisition lives in
//! `scrozz-record` behind its optional native-library feature.

pub mod session;
pub mod wayland;
pub mod x11;

use scrozz_core::{CaptureBackend, Error, Result};

use self::session::{SessionEnv, SessionKind};

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
