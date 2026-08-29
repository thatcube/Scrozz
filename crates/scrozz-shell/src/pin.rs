//! Platform capability query for pinned-capture windows.
//!
//! The query is deliberately separate from the renderer. Callers can explain a
//! platform gap before attempting a native operation, and every environment
//! branch is testable without mutating process environment or compiling on that
//! operating system.

pub use scrozz_core::{
    DisplaySet, LockEscape, LockEscapeRequired, NudgeStep, Opacity, PinBorder, PinChrome,
    PinChromePolicy, PinDirection as Direction, PinId, PinScale, PinState, PinnedSurface,
};

use crate::hotkey::{Compositor, DisplayServer, Session};

/// Native strategy selected for pin windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinBackend {
    /// AppKit `NSPanel`, implemented by Scrozz.
    MacPanel,
    /// Win32 child viewport using winit's portable tool-window properties.
    WindowsToolWindow,
    /// Window-manager-managed X11 dock viewport.
    X11ManagedDock,
    /// Ordinary xdg-toplevel where compositor positioning is unavailable.
    WaylandOrdinaryWindow,
    /// No display server.
    Unsupported,
}

/// Truthful support level for one capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    /// Fully implemented.
    Yes,
    /// Works through a portable fallback, with named limitations.
    Emulated {
        /// Fallback mechanism.
        via: &'static str,
    },
    /// Not available; the message includes both cause and remedy.
    No {
        /// Why support is unavailable.
        why: &'static str,
        /// What the user can do instead.
        remedy: &'static str,
    },
}

impl Support {
    /// Whether callers may safely attempt the operation.
    #[must_use]
    pub const fn available(&self) -> bool {
        !matches!(self, Self::No { .. })
    }
}

/// Capabilities of pinned windows in one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinCapabilities {
    /// Selected backend.
    pub backend: PinBackend,
    /// Whether a pin window can be created at all.
    pub pin_window: Support,
    /// Whether the client controls global screen position.
    pub positioning: Support,
    /// Whether the window can remain over other apps.
    pub always_on_top: Support,
    /// Whether locking can make the window pointer-transparent.
    pub click_through: Support,
    /// Whether opacity is applied to the native window.
    pub native_opacity: Support,
    /// Whether clicking the pin avoids activating Scrozz.
    pub non_activating: Support,
}

impl PinCapabilities {
    /// Detects the current session.
    #[must_use]
    pub fn detect() -> Self {
        let session = Session::detect();
        Self::for_session(&session)
    }

    /// Classifies explicit session facts.
    ///
    /// Like [`Session::from_env`], this is cfg-free so tests can cover macOS,
    /// Windows, X11, every Wayland compositor family, and headless behavior on
    /// any build host. A Wayland session remains Wayland here: an application
    /// preference cannot prove which backend winit actually selected, so Scrozz
    /// never claims X11 positioning or click-through from an unverified opt-in.
    #[must_use]
    pub fn for_session(session: &Session) -> Self {
        match session.server {
            DisplayServer::Quartz => Self::mac(),
            DisplayServer::Windows => Self::windows(),
            DisplayServer::X11 => Self::x11(),
            DisplayServer::Wayland => Self::wayland(session.compositor),
            DisplayServer::Headless => Self::headless(),
        }
    }

    fn mac() -> Self {
        Self {
            backend: PinBackend::MacPanel,
            pin_window: Support::Yes,
            positioning: Support::Yes,
            always_on_top: Support::Yes,
            click_through: Support::Yes,
            native_opacity: Support::Yes,
            non_activating: Support::Yes,
        }
    }

    fn windows() -> Self {
        Self {
            backend: PinBackend::WindowsToolWindow,
            pin_window: Support::Yes,
            positioning: Support::Yes,
            always_on_top: Support::Yes,
            click_through: Support::Yes,
            native_opacity: Support::Yes,
            non_activating: Support::Yes,
        }
    }

    fn x11() -> Self {
        Self {
            backend: PinBackend::X11ManagedDock,
            pin_window: Support::Emulated {
                via: "window-manager-managed X11 dock viewport",
            },
            positioning: Support::Yes,
            always_on_top: Support::Yes,
            click_through: lock_adapter_missing(),
            native_opacity: native_adapter_missing(),
            non_activating: native_adapter_missing(),
        }
    }

    fn wayland(_compositor: Compositor) -> Self {
        Self {
            backend: PinBackend::WaylandOrdinaryWindow,
            pin_window: Support::Emulated {
                via: "ordinary compositor-positioned xdg-toplevel",
            },
            positioning: Support::No {
                why: "xdg-shell has no client positioning, and Scrozz does not bind a discovered layer-shell protocol",
                remedy: "use compositor window rules; XWayland is an explicit fidelity trade-off, not an automatic fallback",
            },
            always_on_top: Support::No {
                why: "ordinary xdg-toplevel windows cannot request an always-on-top layer; GNOME exposes no layer-shell protocol",
                remedy: "use a compositor window rule on compositors that provide one",
            },
            click_through: Support::No {
                why: "Scrozz has no Wayland focus-release adapter, so a pointer-transparent pin could still hold keyboard focus",
                remedy: "keep the pin unlocked",
            },
            native_opacity: Support::No {
                why: "native window opacity is unavailable for ordinary Wayland viewports",
                remedy: "use the composited image-opacity fallback",
            },
            non_activating: Support::No {
                why: "ordinary xdg-toplevel windows follow compositor activation policy; compositor names do not prove protocol availability",
                remedy: "use a compositor window rule; a future adapter must discover and bind layer-shell before claiming support",
            },
        }
    }

    fn headless() -> Self {
        let unsupported = Support::No {
            why: "no display server was detected",
            remedy: "start Scrozz inside a graphical session",
        };
        Self {
            backend: PinBackend::Unsupported,
            pin_window: unsupported.clone(),
            positioning: unsupported.clone(),
            always_on_top: unsupported.clone(),
            click_through: unsupported.clone(),
            native_opacity: unsupported.clone(),
            non_activating: unsupported,
        }
    }
}

fn native_adapter_missing() -> Support {
    Support::No {
        why: "this build has no native pinned-window adapter for the platform",
        remedy: "portable pinning still works, but activation and native-alpha guarantees are unavailable",
    }
}

fn lock_adapter_missing() -> Support {
    Support::No {
        why: "this build cannot guarantee that locking releases keyboard focus as well as pointer input",
        remedy: "keep the pin unlocked until the platform focus-release adapter is implemented",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(server: DisplayServer, compositor: Compositor) -> Session {
        Session {
            server,
            compositor,
            desktop: String::new(),
        }
    }

    #[test]
    fn native_platforms_and_x11_have_distinct_truthful_backends() {
        let mac = PinCapabilities::for_session(&session(DisplayServer::Quartz, Compositor::Other));
        assert_eq!(mac.backend, PinBackend::MacPanel);
        assert_eq!(mac.non_activating, Support::Yes);
        assert_eq!(mac.click_through, Support::Yes);

        let windows =
            PinCapabilities::for_session(&session(DisplayServer::Windows, Compositor::Other));
        assert_eq!(windows.backend, PinBackend::WindowsToolWindow);
        assert_eq!(windows.non_activating, Support::Yes);
        assert_eq!(windows.click_through, Support::Yes);

        let x11 = PinCapabilities::for_session(&session(DisplayServer::X11, Compositor::Other));
        assert_eq!(x11.backend, PinBackend::X11ManagedDock);
        assert!(x11.positioning.available());
        assert!(!x11.click_through.available());
    }

    #[test]
    fn gnome_wayland_never_claims_client_positioning() {
        let caps =
            PinCapabilities::for_session(&session(DisplayServer::Wayland, Compositor::Gnome));
        assert_eq!(caps.backend, PinBackend::WaylandOrdinaryWindow);
        assert!(!caps.positioning.available());
        assert!(!caps.always_on_top.available());
    }

    #[test]
    fn compositor_names_never_fabricate_layer_shell_protocol_support() {
        for compositor in [
            Compositor::Sway,
            Compositor::Hyprland,
            Compositor::River,
            Compositor::Niri,
            Compositor::Wayfire,
            Compositor::Kde,
        ] {
            let caps = PinCapabilities::for_session(&session(DisplayServer::Wayland, compositor));
            assert_eq!(caps.backend, PinBackend::WaylandOrdinaryWindow);
            assert!(!caps.positioning.available());
            assert!(!caps.click_through.available());
            assert!(!caps.non_activating.available());
        }
    }

    #[test]
    fn a_wayland_session_never_claims_an_unverified_x11_backend() {
        let gnome = session(DisplayServer::Wayland, Compositor::Gnome);
        assert_eq!(
            PinCapabilities::for_session(&gnome).backend,
            PinBackend::WaylandOrdinaryWindow
        );
    }

    #[test]
    fn headless_is_completely_unsupported() {
        let caps =
            PinCapabilities::for_session(&session(DisplayServer::Headless, Compositor::Other));
        assert_eq!(caps.backend, PinBackend::Unsupported);
        assert!(!caps.pin_window.available());
    }
}
