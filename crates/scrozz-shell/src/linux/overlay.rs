//! The Linux overlay window, and the single place the three backends meet.
//!
//! [`LinuxOverlay`] implements [`crate::OverlayWindow`] the way the rest of
//! Scrozz expects, and routes each call to whichever backend the session
//! actually has. Its one rule is the one this whole module exists for:
//!
//! > A method returns `Ok` only when the thing it names happened.
//!
//! On GNOME/Wayland `set_frame` cannot work, and it says so with
//! [`scrozz_core::Error::Unsupported`] and a sentence naming the compositor,
//! rather than returning `Ok(())` and leaving the caller to wonder why the
//! capture stack is in the middle of the screen.

use scrozz_core::{Error, LogicalPoint, LogicalRect, LogicalSize, Result};

use super::capability::{OverlayBackend, OverlayPlan, Placement};
use super::region::InputRegion;
use super::wayland;
use super::x11::X11Backend;
use crate::hotkey::Session;
use crate::overlay::{OverlayBehavior, OverlayReport};

/// A native window handle, in the form Linux actually provides one.
///
/// Not a `*mut c_void`, because Linux does not have one. X11 gives a numeric
/// window ID that is meaningful from any connection to the same server, and
/// Wayland gives a client-local object with no cross-library identity at all —
/// which is exactly why the Wayland variant carries nothing. There is nothing
/// useful to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxWindowHandle {
    /// An X11 window ID, from either the Xlib or the XCB window handle.
    X11 {
        /// The server-side window ID.
        window: u32,
    },
    /// A Wayland surface owned by winit.
    ///
    /// Deliberately opaque: the surface already holds the `xdg_toplevel` role,
    /// so there is nothing this crate may legally do with it beyond declining to
    /// promote it. See [`super::wayland::refusal`].
    Wayland,
}

/// An overlay window on Linux.
pub struct LinuxOverlay {
    handle: LinuxWindowHandle,
    plan: OverlayPlan,
    x11: Option<X11Backend>,
    scale: f64,
    behavior: Option<OverlayBehavior>,
}

impl std::fmt::Debug for LinuxOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxOverlay")
            .field("handle", &self.handle)
            .field("backend", &self.plan.backend)
            .field("placement", &self.plan.placement)
            .finish_non_exhaustive()
    }
}

impl LinuxOverlay {
    /// Adopts a window winit has already created.
    ///
    /// The session is passed in rather than detected here so that the app and
    /// the overlay agree about which compositor they are on — a diagnostics pane
    /// saying "layer-shell" while the overlay quietly used something else would
    /// be worse than either answer alone.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when this session has no backend that can
    /// host an overlay at all, and propagates a connection failure from X11.
    pub fn adopt(handle: LinuxWindowHandle, session: &Session) -> Result<Self> {
        let plan = super::overlay_plan(session);

        let x11 = match (plan.backend, handle) {
            (OverlayBackend::X11Retrofit, LinuxWindowHandle::X11 { .. }) => {
                Some(X11Backend::connect()?)
            }
            // A plan that says X11 with a Wayland handle means the session
            // detection and the window disagree. Trusting the window is right:
            // it is the thing that exists.
            (OverlayBackend::X11Retrofit, LinuxWindowHandle::Wayland) => None,
            _ => None,
        };

        Ok(Self {
            handle,
            plan,
            x11,
            scale: 1.0,
            behavior: None,
        })
    }

    /// What this overlay can actually do, for diagnostics and for the UI.
    #[must_use]
    pub const fn plan(&self) -> &OverlayPlan {
        &self.plan
    }

    /// Sets the scale factor used to convert logical coordinates to pixels.
    ///
    /// X11 has no per-window scale of its own, so the caller supplies winit's.
    /// Left at 1.0 the overlay is placed in logical units, which is correct on
    /// an unscaled display and wrong by exactly the scale factor everywhere
    /// else.
    pub const fn set_scale_factor(&mut self, scale: f64) {
        self.scale = scale;
    }

    /// Applies overlay behaviour and reports what was actually done.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] on a Wayland session, where none of these
    /// properties can be set by a client.
    pub fn apply(&mut self, behavior: &OverlayBehavior) -> Result<OverlayReport> {
        self.behavior = Some(*behavior);
        match (&self.x11, self.handle) {
            (Some(backend), LinuxWindowHandle::X11 { window }) => backend.apply(window, behavior),
            _ => Ok(OverlayReport {
                // Not a claim of success. On Wayland an ordinary toplevel does
                // not activate the app on click the way an X11 window can, so
                // "non-activating" is true by default rather than by request —
                // and the detail says which it is.
                non_activating: true,
                detail: self.plan.detail.clone(),
            }),
        }
    }

    /// The current display's work area, in pixels, if the platform reports one.
    ///
    /// Only X11 can answer. Wayland has no protocol by which a client learns
    /// where the panels are — `layer-shell`'s exclusive zone exists precisely so
    /// that it does not need to.
    #[must_use]
    pub fn work_area(&self) -> Option<LogicalRect> {
        let backend = self.x11.as_ref()?;
        let rect = backend
            .work_area()
            .unwrap_or_else(|| backend.screen_bounds());
        Some(LogicalRect::new(
            LogicalPoint::new(
                f64::from(rect.x) / self.scale,
                f64::from(rect.y) / self.scale,
            ),
            LogicalSize::new(
                f64::from(rect.width) / self.scale,
                f64::from(rect.height) / self.scale,
            ),
        ))
    }

    /// Restricts input to a set of rectangles, in window-local logical
    /// coordinates.
    ///
    /// This is the per-card click-through that [`crate::OverlayWindow::set_click_through`]
    /// cannot express: the capture stack is mostly empty, and only the cards
    /// should catch a click.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] where the platform cannot shape input.
    pub fn set_input_region(&mut self, region: &InputRegion) -> Result<()> {
        match (&self.x11, self.handle) {
            (Some(backend), LinuxWindowHandle::X11 { window }) => {
                backend.set_input_region(window, region)
            }
            _ => Err(self.no_input_shaping()),
        }
    }

    /// The error for a platform that cannot shape input from this window.
    fn no_input_shaping(&self) -> Error {
        Error::Unsupported {
            what: "click-through overlay".into(),
            why: format!(
                "{} — input shaping needs a surface Scrozz owns, and this window belongs \
                 to winit",
                self.plan.detail
            ),
        }
    }
}

impl crate::OverlayWindow for LinuxOverlay {
    fn set_frame(&mut self, frame: LogicalRect) -> Result<()> {
        match (&self.x11, self.handle) {
            (Some(backend), LinuxWindowHandle::X11 { window }) => {
                backend.set_frame(window, frame, self.scale)
            }
            _ => Err(self.cannot_place()),
        }
    }

    fn set_click_through(&mut self, passthrough: bool) -> Result<()> {
        let region = if passthrough {
            InputRegion::Nothing
        } else {
            InputRegion::Everything
        };
        self.set_input_region(&region)
    }
}

impl LinuxOverlay {
    /// The error for a platform where a client cannot position a window.
    ///
    /// Written out at length because it is the sentence a GNOME user will see,
    /// and "unsupported" on its own reads as a Scrozz bug when it is a
    /// deliberate compositor policy.
    fn cannot_place(&self) -> Error {
        let why = match self.plan.placement {
            Placement::CompositorChosen => format!(
                "{}. The window is still shown; the compositor decides where.",
                self.plan.detail
            ),
            Placement::Anchored => format!(
                "{}. Anchoring is available through layer-shell, but this window is \
                 winit's and cannot be promoted: {}",
                self.plan.detail,
                wayland::refusal()
            ),
            Placement::Nowhere => format!("{}. Nothing is drawn.", self.plan.detail),
            Placement::Absolute => format!(
                "{}. This is a bug: the plan claims absolute placement but no backend \
                 is attached.",
                self.plan.detail
            ),
        };
        Error::Unsupported {
            what: "placing the overlay at an absolute position".into(),
            why,
        }
    }
}

/// The current work area, without needing an overlay window first.
///
/// The app asks this while laying out the capture stack, before any overlay
/// exists. Returns `None` on Wayland, where there is no such protocol, so the
/// caller falls back to the display bounds the portal reported.
#[must_use]
pub fn work_area() -> Option<LogicalRect> {
    let backend = X11Backend::connect().ok()?;
    let rect = backend.work_area()?;
    Some(LogicalRect::new(
        LogicalPoint::new(f64::from(rect.x), f64::from(rect.y)),
        LogicalSize::new(f64::from(rect.width), f64::from(rect.height)),
    ))
}
