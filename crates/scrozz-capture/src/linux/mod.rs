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
//! # Transport choices in this build
//!
//! The workspace enables the relevant `x11rb` and `ashpd` protocol features.
//! Not every enabled protocol has a completed transport path yet, so those
//! implementation gaps remain explicit:
//!
//! | Need | Implementation today |
//! |---|---|
//! | RandR monitor geometry | The narrow request/reply surface remains encoded in [`x11::wire`] |
//! | MIT-SHM fast path | Not wired; portable `GetImage` is used |
//! | XFIXES cursor image | Not wired; the cursor is omitted from X11 captures |
//! | Portal ScreenCast | Protocol enabled; display/region streaming is not wired |
//! | Portal Screenshot | Window-restricted still picker implemented for portal v3+ |
//!
//! RandR was worth hand-rolling because without it a multi-monitor desktop is
//! indistinguishable from one enormous screen. MIT-SHM was not, because the
//! feature alone is insufficient — the shared segment still needs a safe,
//! owned mapping lifecycle. That path has not been implemented.

pub mod session;
pub mod wayland;
pub mod x11;

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, Display, Error, Result, TargetEnumerator, Window,
    WindowPicking, WindowPickingCapability,
};

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

    let backend =
        match kind {
            SessionKind::X11 => LinuxBackend::X11(x11::X11Backend::connect()?),

            // XWayland is reachable here, and using it would appear to work — which
            // is exactly why it is refused. An X11 capture inside a Wayland session
            // sees only XWayland clients, so native GTK4 and Qt6 windows are absent
            // from the image with nothing to indicate it. The portal is the only
            // correct route, so the portal backend is what is returned, gaps and all.
            SessionKind::Wayland | SessionKind::XWayland => {
                LinuxBackend::Wayland(wayland::WaylandBackend::new(&env)?)
            }

            SessionKind::Headless => return Err(Error::Unsupported {
                what: "screen capture".into(),
                why: "no display server was found: neither WAYLAND_DISPLAY nor DISPLAY is set, so \
                  this is a text console, a container without a forwarded socket, or a CI \
                  runner. Set one of them, or run under Xvfb, to capture"
                    .into(),
            }),
        };
    Ok(Box::new(backend))
}

/// Runtime dispatch between Linux's two unrelated capture protocols.
enum LinuxBackend {
    X11(x11::X11Backend),
    Wayland(wayland::WaylandBackend),
}

impl TargetEnumerator for LinuxBackend {
    fn displays(&self) -> Result<Vec<Display>> {
        match self {
            Self::X11(backend) => backend.displays(),
            Self::Wayland(backend) => backend.displays(),
        }
    }

    fn windows(&self) -> Result<Vec<Window>> {
        match self {
            Self::X11(backend) => backend.windows(),
            Self::Wayland(backend) => backend.windows(),
        }
    }

    fn active_display(&self) -> Result<Display> {
        match self {
            Self::X11(backend) => backend.active_display(),
            Self::Wayland(backend) => backend.active_display(),
        }
    }
}

impl CaptureBackend for LinuxBackend {
    fn capture(&self, request: &CaptureRequest) -> Result<Capture> {
        match self {
            Self::X11(backend) => backend.capture(request),
            Self::Wayland(backend) => backend.capture(request),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::X11(backend) => backend.name(),
            Self::Wayland(backend) => backend.name(),
        }
    }
}

impl WindowPicking for LinuxBackend {
    fn window_picking(&self) -> WindowPickingCapability {
        match self {
            Self::X11(backend) => backend.window_picking(),
            Self::Wayland(backend) => backend.window_picking(),
        }
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
            Ok(wayland::scroll::driver_for_session(&env))
        }
        SessionKind::Headless => Err(Error::Unsupported {
            what: "scroll input".into(),
            why: "no display server was found: neither WAYLAND_DISPLAY nor DISPLAY is set".into(),
        }),
    }
}
