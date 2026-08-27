//! Runtime planning for the surface that owns interactive selection.

use scrozz_core::{SelectionCapabilities, SelectionHost, SessionFacts, host_for as core_host_for};

use crate::hotkey::{Compositor, DisplayServer, Session};

/// Implemented adapters layered on top of measured session facts.
///
/// A compositor advertising a protocol is not the same as this process owning a
/// surface for it, and a portal showing a selector is not sufficient when its
/// response cannot be represented as a [`scrozz_core::SelectionOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionIntegration {
    /// Display-server and protocol facts measured for this session.
    pub session: SessionFacts,
    /// Scrozz owns a mapped layer-shell rendering surface.
    pub layer_shell_renderer: bool,
    /// The compositor-owned selector returns a concrete capture target.
    pub compositor_returns_target: bool,
}

impl SelectionIntegration {
    /// Native macOS, Windows, or X11 client-overlay integration.
    pub const NATIVE: Self = Self {
        session: SessionFacts::NATIVE,
        layer_shell_renderer: false,
        compositor_returns_target: false,
    };

    /// A headless process.
    pub const HEADLESS: Self = Self {
        session: SessionFacts::HEADLESS,
        layer_shell_renderer: false,
        compositor_returns_target: false,
    };

    /// Starts with no rendering or portal-result adapter assumed.
    #[must_use]
    pub const fn new(session: SessionFacts) -> Self {
        Self {
            session,
            layer_shell_renderer: false,
            compositor_returns_target: false,
        }
    }

    /// Builds the measured facts for a shell session without claiming that an
    /// advertised protocol already has a renderer or result adapter.
    #[must_use]
    pub fn for_session(session: &Session) -> Self {
        let facts = match session.server {
            DisplayServer::Quartz | DisplayServer::Windows | DisplayServer::X11 => {
                SessionFacts::NATIVE
            }
            DisplayServer::Headless => SessionFacts::HEADLESS,
            DisplayServer::Wayland => {
                let (has_layer_shell, has_interactive_portal) = match session.compositor {
                    Compositor::Gnome => (false, true),
                    Compositor::Kde
                    | Compositor::Sway
                    | Compositor::Hyprland
                    | Compositor::River
                    | Compositor::Niri
                    | Compositor::Wayfire => (true, true),
                    Compositor::Other => (false, false),
                };
                SessionFacts {
                    has_display: true,
                    is_wayland: true,
                    has_layer_shell,
                    has_interactive_portal,
                }
            }
        };
        Self::new(facts)
    }
}

/// The selected host plus whether Scrozz can execute it in this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionPlan {
    /// The only host type valid for the measured session.
    pub host: SelectionHost,
    /// Capabilities that are executable, not merely advertised by a protocol.
    pub capabilities: SelectionCapabilities,
    /// A diagnostic that names both the route and any missing adapter.
    pub detail: String,
}

impl SelectionPlan {
    /// Whether calling a selector can produce a concrete outcome.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.capabilities != SelectionCapabilities::NONE
    }
}

/// Resolves the sole truthful selector route for a session.
///
/// Protocol availability and implementation readiness remain separate. In
/// particular, eframe/winit's Wayland window is an `xdg_toplevel`, never a
/// layer-shell surface, and the interactive Screenshot portal returns an image
/// URI rather than selection coordinates.
#[must_use]
pub fn resolve_selection(integration: SelectionIntegration) -> SelectionPlan {
    let host = core_host_for(integration.session);
    match host {
        SelectionHost::ClientOverlay => SelectionPlan {
            host,
            capabilities: SelectionCapabilities::CLIENT_OVERLAY,
            detail: "Scrozz owns a positionable client overlay in this session".to_owned(),
        },
        SelectionHost::LayerShell if integration.layer_shell_renderer => SelectionPlan {
            host,
            capabilities: SelectionCapabilities::CLIENT_OVERLAY,
            detail: "Scrozz owns a mapped layer-shell rendering surface".to_owned(),
        },
        SelectionHost::LayerShell => SelectionPlan {
            host,
            capabilities: SelectionCapabilities::NONE,
            detail: "the compositor advertises layer-shell, but Scrozz has no mapped \
                     layer-shell rendering surface; its eframe window is an xdg_toplevel \
                     and cannot be treated as an overlay"
                .to_owned(),
        },
        SelectionHost::CompositorOwned if integration.compositor_returns_target => SelectionPlan {
            host,
            capabilities: SelectionCapabilities::COMPOSITOR_OWNED,
            detail: "the compositor-owned selector returns a concrete capture target".to_owned(),
        },
        SelectionHost::CompositorOwned => SelectionPlan {
            host,
            capabilities: SelectionCapabilities::NONE,
            detail: "the interactive Screenshot portal returns an image URI, not selection \
                     coordinates or a capture target, so it cannot satisfy Scrozz's selector \
                     contract without fabricating geometry"
                .to_owned(),
        },
        SelectionHost::Headless => SelectionPlan {
            host,
            capabilities: SelectionCapabilities::NONE,
            detail: if integration.session.has_display {
                "this Wayland session exposes neither a usable layer-shell renderer nor a \
                 compatible compositor-owned selector"
                    .to_owned()
            } else {
                "no display server is available for interactive selection".to_owned()
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gnome() -> SessionFacts {
        SessionFacts {
            has_display: true,
            is_wayland: true,
            has_layer_shell: false,
            has_interactive_portal: true,
        }
    }

    fn layer_shell() -> SessionFacts {
        SessionFacts {
            has_display: true,
            is_wayland: true,
            has_layer_shell: true,
            has_interactive_portal: true,
        }
    }

    #[test]
    fn native_sessions_have_the_complete_client_overlay() {
        let plan = resolve_selection(SelectionIntegration::NATIVE);
        assert_eq!(plan.host, SelectionHost::ClientOverlay);
        assert_eq!(plan.capabilities, SelectionCapabilities::CLIENT_OVERLAY);
        assert!(plan.is_available());
    }

    #[test]
    fn a_portal_ui_without_target_geometry_is_not_called_success() {
        let plan = resolve_selection(SelectionIntegration::new(gnome()));
        assert_eq!(plan.host, SelectionHost::CompositorOwned);
        assert_eq!(plan.capabilities, SelectionCapabilities::NONE);
        assert!(plan.detail.contains("image URI"), "{}", plan.detail);
        assert!(plan.detail.contains("geometry"), "{}", plan.detail);
    }

    #[test]
    fn a_compatible_compositor_result_unlocks_only_portal_capabilities() {
        let mut integration = SelectionIntegration::new(gnome());
        integration.compositor_returns_target = true;
        let plan = resolve_selection(integration);
        assert_eq!(plan.host, SelectionHost::CompositorOwned);
        assert_eq!(plan.capabilities, SelectionCapabilities::COMPOSITOR_OWNED);
    }

    #[test]
    fn advertised_layer_shell_is_not_an_eframe_renderer() {
        let plan = resolve_selection(SelectionIntegration::new(layer_shell()));
        assert_eq!(plan.host, SelectionHost::LayerShell);
        assert!(!plan.is_available());
        assert!(plan.detail.contains("xdg_toplevel"), "{}", plan.detail);
    }

    #[test]
    fn a_real_layer_shell_renderer_unlocks_the_route() {
        let mut integration = SelectionIntegration::new(layer_shell());
        integration.layer_shell_renderer = true;
        let plan = resolve_selection(integration);
        assert_eq!(plan.host, SelectionHost::LayerShell);
        assert_eq!(plan.capabilities, SelectionCapabilities::CLIENT_OVERLAY);
    }

    #[test]
    fn headless_is_an_explained_absence() {
        let plan = resolve_selection(SelectionIntegration::HEADLESS);
        assert_eq!(plan.host, SelectionHost::Headless);
        assert!(!plan.is_available());
        assert!(plan.detail.contains("no display server"), "{}", plan.detail);
    }

    #[test]
    fn a_wayland_protocol_does_not_claim_an_eframe_renderer() {
        let session = Session {
            server: DisplayServer::Wayland,
            compositor: Compositor::Kde,
            desktop: "KDE".to_owned(),
        };
        let integration = SelectionIntegration::for_session(&session);
        assert!(integration.session.has_layer_shell);
        assert!(!integration.layer_shell_renderer);
        assert!(!resolve_selection(integration).is_available());
    }
}
