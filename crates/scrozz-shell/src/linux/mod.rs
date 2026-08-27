//! Linux overlay backends: X11, `wlr-layer-shell`, and an honest fallback.
//!
//! Linux is not one platform here; it is three, and they disagree about whether
//! a client may place a window at all.
//!
//! - **X11** lets a client do anything. Scrozz gets real anchoring, real
//!   stacking and real click-through, via [`x11`].
//! - **Wayland with `wlr-layer-shell`** (KDE, Sway, Hyprland, River, Niri,
//!   Wayfire) uses a Scrozz-owned layer surface, SHM swapchain and software
//!   rendering of the real capture-card scene. eframe's `xdg_toplevel` remains
//!   only as the explicit compositor-positioned fallback.
//! - **Wayland without it** (GNOME/Mutter) has no mechanism at all. `xdg-shell`
//!   deliberately omits absolute positioning, and Mutter has declined
//!   `layer-shell` as a matter of policy, not backlog. Per decision D31 Scrozz
//!   falls back to an ordinary toplevel the compositor places, and *says so*.
//!
//! The point of this module is that the fallback is visible rather than silent.
//! Every path produces an [`capability::OverlayPlan`] describing what will
//! actually happen, and advertising a protocol selects layer-shell only when the
//! owned rendering host is the surface that will be run.
//!
//! # Layout
//!
//! - [`capability`] — which backend applies, and what it can and cannot do.
//! - [`layer`] — `wlr-layer-shell` configuration: layer, anchors, exclusive
//!   zone, margins.
//! - [`region`] — input-region arithmetic shared by X11 `SHAPE` and Wayland
//!   `wl_surface.set_input_region`.
//! - [`ewmh`] — X11 window properties, and the override-redirect branch.
//!
//! All four are pure and compile on every host, so the decisions they encode are
//! tested on macOS and Windows CI too. The modules that open a connection —
//! `x11` and `wayland` — are `cfg(target_os = "linux")`.

pub mod capability;
pub mod ewmh;
pub mod layer;
pub mod region;
pub mod surface;

#[cfg(target_os = "linux")]
mod overlay;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod smoke;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(target_os = "linux")]
pub use overlay::{LinuxOverlay, LinuxWindowHandle, work_area};
pub use surface::{
    FrameCommit, LayerSurfaceEvent, OutputInfo, OutputSelector, PointerAxis, PointerAxisSource,
    PointerButtonState, SurfaceCloseReason, SurfacePoint, SurfacePointerEvent, SurfaceScale,
    SurfaceSize, scaled_buffer_size,
};
#[cfg(target_os = "linux")]
pub use wayland::{LayerShellSession, enumerate_outputs};

use crate::hotkey::Session;
use capability::{LayerShellProbe, OverlayPlan};

/// Decides what Scrozz can do on this session, probing the compositor if it can.
///
/// Static knowledge of a compositor is only ever a prior. GNOME is expected to
/// refuse `layer-shell` and KDE is expected to offer it, but "expected" is not
/// "does": a compositor can be patched, a version can add the protocol, and
/// `Compositor::Other` is by definition unknown. So on Linux this asks the
/// running compositor and lets the answer overrule the table, and everywhere
/// else it reports what the table says without pretending to have looked.
#[must_use]
pub fn overlay_plan(session: &Session) -> OverlayPlan {
    capability::plan(session.server, session.compositor, probe_layer_shell())
}

/// Asks the running compositor whether it advertises `zwlr_layer_shell_v1`.
///
/// Returns [`LayerShellProbe::NotProbed`] off Linux, and on Linux whenever there
/// is no Wayland connection to ask — which is the honest answer, and distinct
/// from [`LayerShellProbe::Absent`], which means a compositor was asked and did
/// not offer it.
#[must_use]
pub fn probe_layer_shell() -> LayerShellProbe {
    #[cfg(target_os = "linux")]
    {
        wayland::probe()
    }
    #[cfg(not(target_os = "linux"))]
    {
        LayerShellProbe::NotProbed
    }
}
