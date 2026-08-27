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
//! # Manifest constraints on this build
//!
//! Some optional features of the X11 dependency are not enabled, and the limits
//! are load-bearing enough to state here rather than bury:
//!
//! | Need | Feature required | Consequence today |
//! |---|---|---|
//! | RandR monitor geometry | `x11rb/randr` | Encoded by hand in [`x11::wire`] — works |
//! | MIT-SHM fast path | `x11rb/shm` **and** `libc`/`rustix` | Unavailable; `GetImage` used |
//! | XFIXES cursor image | `x11rb/xfixes` | Cursor omitted from X11 captures |
//!
//! RandR was worth hand-rolling because without it a multi-monitor desktop is
//! indistinguishable from one enormous screen. MIT-SHM was not, because the
//! feature alone is insufficient — the shared segment needs `shmget`/`shmat` or
//! `memfd_create`, and neither `libc` nor `rustix` is a dependency of this
//! crate, so there is no syscall to make.
//!
//! The Wayland side has no such gap: `ashpd` is built with `screencast` and
//! `screenshot`, and PipeWire is opened at run time rather than linked, so a
//! machine without it still builds, still starts, and still captures over X11.
//! See [`wayland::pipewire`] for why that indirection was chosen.

pub mod session;
pub mod wayland;
pub mod x11;

use scrozz_core::{CaptureBackend, CaptureRequest, Error, Result};

use self::session::{SessionEnv, SessionKind};
use crate::FrameSession;

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

/// Opens a repeated-frame source for the current Linux display server.
///
/// Wayland gets a native long-lived portal/PipeWire session. X11 uses the
/// ordinary backend adapter because its direct capture has no permission session
/// to preserve.
pub fn frame_session(request: CaptureRequest) -> Result<Box<dyn FrameSession>> {
    let env = SessionEnv::from_env();
    let kind = session::detect_session(&env);

    match kind {
        SessionKind::Wayland | SessionKind::XWayland => {
            let backend = wayland::WaylandBackend::new(&env)?;
            Ok(Box::new(backend.open_frame_session(&request)?))
        }
        SessionKind::X11 | SessionKind::Headless => {
            Ok(crate::backend_frame_session(backend()?, request))
        }
    }
}
