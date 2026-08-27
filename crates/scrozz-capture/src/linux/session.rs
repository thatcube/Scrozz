//! Which display server and which compositor we are actually talking to.
//!
//! Every function here is pure: it takes an environment snapshot and returns a
//! decision. That is deliberate. Backend selection is the single most
//! consequential branch in the Linux code — pick X11 inside a Wayland session
//! and you silently capture only XWayland clients, which looks like a
//! *rendering* bug rather than a *routing* one — and it is also the part that is
//! impossible to exercise on the development machine. Keeping it free of
//! `x11rb`, `ashpd` and `std::env` means it compiles and runs in the test suite
//! on macOS, Windows and Linux alike, so the decision table is genuinely
//! covered rather than merely written down.
//!
//! See `tests/linux.rs`, which includes this file directly by path for exactly
//! that reason.

use std::fmt;

/// A snapshot of the environment variables that identify a Linux session.
///
/// Taken once and passed by value so the decision functions stay pure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionEnv {
    /// `WAYLAND_DISPLAY` — the Wayland socket name, set by the compositor.
    pub wayland_display: Option<String>,
    /// `XDG_SESSION_TYPE` — `wayland`, `x11` or `tty`, set by the login manager.
    pub xdg_session_type: Option<String>,
    /// `DISPLAY` — the X11 display. Also set inside a Wayland session whenever
    /// XWayland is running, which is nearly always.
    pub display: Option<String>,
    /// `XDG_CURRENT_DESKTOP` — colon-separated desktop names, e.g. `ubuntu:GNOME`.
    pub xdg_current_desktop: Option<String>,
    /// `XDG_SESSION_DESKTOP` — a single desktop name, a weaker hint.
    pub xdg_session_desktop: Option<String>,
    /// `GDK_BACKEND` / `QT_QPA_PLATFORM` style forcing, honoured as an override.
    pub forced_backend: Option<String>,
}

impl SessionEnv {
    /// Reads the environment.
    ///
    /// The only impure function in this module, kept to one place so everything
    /// downstream of it can be tested.
    #[must_use]
    pub fn from_env() -> Self {
        fn var(key: &str) -> Option<String> {
            std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        }
        Self {
            wayland_display: var("WAYLAND_DISPLAY"),
            xdg_session_type: var("XDG_SESSION_TYPE"),
            display: var("DISPLAY"),
            xdg_current_desktop: var("XDG_CURRENT_DESKTOP"),
            xdg_session_desktop: var("XDG_SESSION_DESKTOP"),
            forced_backend: var("SCROZZ_BACKEND"),
        }
    }
}

/// The display server a capture backend must speak to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A native Wayland session. Capture goes through `xdg-desktop-portal`.
    Wayland,
    /// A native X11 session. Capture goes through the X server directly.
    X11,
    /// A Wayland session in which an X server (XWayland) is also reachable.
    ///
    /// Distinct from [`Self::X11`] because the X connection *works* — it simply
    /// cannot see the whole desktop. `_NET_CLIENT_LIST` lists XWayland clients
    /// only, and RandR reports XWayland's view of the outputs. Treating this as
    /// plain X11 is the classic Wayland screenshot bug: native GTK4 and Qt6
    /// windows are missing from the capture and nothing reports an error.
    XWayland,
    /// Neither server could be identified — a TTY, a container with no sockets
    /// forwarded, or a CI runner.
    Headless,
}

impl SessionKind {
    /// Whether captures on this session must go through the portal.
    #[must_use]
    pub const fn requires_portal(self) -> bool {
        matches!(self, Self::Wayland | Self::XWayland)
    }
}

impl fmt::Display for SessionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Wayland => "Wayland",
            Self::X11 => "X11",
            Self::XWayland => "XWayland",
            Self::Headless => "headless",
        })
    }
}

/// The compositor running the session.
///
/// Named rather than boolean because the three families genuinely differ in
/// what they implement, and decision D8 requires those differences to be stated
/// rather than discovered by a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compositor {
    /// GNOME's Mutter.
    Mutter,
    /// KDE Plasma's KWin.
    KWin,
    /// A wlroots-based compositor: sway, Hyprland, river, Wayfire.
    Wlroots,
    /// A recognised desktop that is neither of the above, e.g. XFCE or Cinnamon.
    Other(String),
    /// Nothing identifiable in the environment.
    Unknown,
}

impl fmt::Display for Compositor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mutter => f.write_str("GNOME/Mutter"),
            Self::KWin => f.write_str("KDE/KWin"),
            Self::Wlroots => f.write_str("wlroots"),
            Self::Other(name) => write!(f, "{name}"),
            Self::Unknown => f.write_str("unknown compositor"),
        }
    }
}

/// What a compositor's portal implementation can actually do.
///
/// Per decision D8 this is a *query*, never an assumption: Wayland's
/// restrictions must not leak into the core API as implicit expectations. A
/// caller asks before it offers a feature, and a `false` here becomes a
/// truthful [`scrozz_core::Error::Unsupported`] rather than a mysterious
/// failure at the point of use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalCapabilities {
    /// Whether individual windows can be enumerated by the application.
    ///
    /// Always `false` on Wayland. There is no protocol for it in any
    /// compositor; the portal's own picker performs selection out-of-process.
    pub window_enumeration: bool,
    /// Whether a ScreenCast restore token suppresses the permission prompt on
    /// subsequent captures.
    pub restore_tokens: bool,
    /// Whether the `GlobalShortcuts` portal exists.
    pub global_shortcuts: bool,
    /// Whether the `RemoteDesktop` portal exists, which scrolling capture needs
    /// in order to synthesise the scroll.
    pub remote_desktop: bool,
    /// Whether `wlr-layer-shell` is available for absolutely-positioned overlays.
    ///
    /// A Wayland client cannot set its own absolute position, so a region-select
    /// overlay needs either `zwlr_layer_shell_v1` or a compositor-side selector.
    /// This is `false` on GNOME — see [`capabilities`] for the sourcing — which
    /// is a constraint on the whole UI, not just this crate.
    pub layer_shell: bool,
}

/// The capability matrix for a compositor.
///
/// The values encode decision D8's promise — GNOME and KDE fully, wlroots
/// best-effort and documented — and the sourcing for each row is recorded in
/// `docs/platforms.md`. Treat unknown compositors pessimistically: promising a
/// capability that is absent is worse than declining one that is present.
///
/// On `layer_shell` specifically there is no portable escape hatch to wait for:
/// `ext-layer-shell-v1` (wayland-protocols MR !28) is still an open draft, and
/// neither `xdg-pip` (!132) nor `ext-toplevel-placement-v1` (!389) is merged. A
/// caller that needs an overlay must branch on this flag, not plan around it.
#[must_use]
pub const fn capabilities(compositor: &Compositor) -> PortalCapabilities {
    match compositor {
        // Mutter implements ScreenCast with persistence, GlobalShortcuts (since
        // GNOME 45) and RemoteDesktop. It does NOT implement wlr-layer-shell and
        // has explicitly declined to: mutter!973 was closed as a duplicate of
        // gnome-shell!1141, where Jonas Ådahl wrote "we don't intend to support
        // third party panels, lock screens, notification UI's etc." Verified
        // against mutter main 82ad6279: no layer-shell XML in
        // src/wayland/protocol/, and src/meson.build's wayland_protocols list
        // does not generate it. gtk-layer-shell's README independently lists
        // "Gnome-on-Wayland" as unsupported.
        Compositor::Mutter => PortalCapabilities {
            window_enumeration: false,
            restore_tokens: true,
            global_shortcuts: true,
            remote_desktop: true,
            layer_shell: false,
        },
        // KWin implements ScreenCast with persistence, RemoteDesktop, and
        // wlr-layer-shell since Plasma 5.20.0 — commit d3cca65d "Implement the
        // layer-shell v1 protocol", reachable from the v5.20.0 tag; today it
        // lives in src/wayland/layershell_v1.cpp.
        Compositor::KWin => PortalCapabilities {
            window_enumeration: false,
            restore_tokens: true,
            global_shortcuts: true,
            remote_desktop: true,
            layer_shell: true,
        },
        // xdg-desktop-portal-wlr implements ScreenCast (with persistence since
        // 0.6) but has no GlobalShortcuts implementation at all — D8's stated
        // gap. wlr-layer-shell is native to wlroots.
        Compositor::Wlroots => PortalCapabilities {
            window_enumeration: false,
            restore_tokens: true,
            global_shortcuts: false,
            remote_desktop: false,
            layer_shell: true,
        },
        Compositor::Other(_) | Compositor::Unknown => PortalCapabilities {
            window_enumeration: false,
            restore_tokens: false,
            global_shortcuts: false,
            remote_desktop: false,
            layer_shell: false,
        },
    }
}

/// Decides which display server to talk to.
///
/// The ordering is the whole point:
///
/// 1. An explicit `SCROZZ_BACKEND` override wins, so a user on an unusual setup
///    can force the right answer and a bug report can be narrowed in one step.
/// 2. `WAYLAND_DISPLAY` beats `DISPLAY`. Inside a Wayland session XWayland is
///    almost always running, so `DISPLAY` is set too; preferring it is how
///    screenshot tools end up silently capturing half a desktop.
/// 3. `XDG_SESSION_TYPE` is only a tie-breaker. Login managers set it wrongly
///    often enough — notably `x11` under a nested Wayland session — that it
///    cannot be trusted over the presence of the socket itself.
#[must_use]
pub fn detect_session(env: &SessionEnv) -> SessionKind {
    if let Some(forced) = env.forced_backend.as_deref() {
        match forced.trim().to_ascii_lowercase().as_str() {
            "wayland" => return SessionKind::Wayland,
            "x11" | "xcb" => return SessionKind::X11,
            _ => {}
        }
    }

    let wayland = env.wayland_display.is_some()
        || env
            .xdg_session_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("wayland"));

    match (wayland, env.display.is_some()) {
        (true, true) => SessionKind::XWayland,
        (true, false) => SessionKind::Wayland,
        (false, true) => SessionKind::X11,
        (false, false) => SessionKind::Headless,
    }
}

/// Identifies the compositor from `XDG_CURRENT_DESKTOP`.
///
/// The variable is colon-separated and frequently vendor-prefixed —
/// `ubuntu:GNOME`, `pop:GNOME`, `KDE`, `sway`, `Hyprland` — so this matches on
/// components rather than on the whole string, and falls back to
/// `XDG_SESSION_DESKTOP`.
#[must_use]
pub fn detect_compositor(env: &SessionEnv) -> Compositor {
    let sources = [
        env.xdg_current_desktop.as_deref(),
        env.xdg_session_desktop.as_deref(),
    ];

    let mut fallback: Option<String> = None;

    for source in sources.into_iter().flatten() {
        for component in source.split(':').map(str::trim).filter(|c| !c.is_empty()) {
            let lower = component.to_ascii_lowercase();
            match lower.as_str() {
                "gnome" | "gnome-classic" | "gnome-flashback" | "unity" => {
                    return Compositor::Mutter;
                }
                "kde" | "plasma" => return Compositor::KWin,
                "sway" | "hyprland" | "river" | "wayfire" | "labwc" | "niri" => {
                    return Compositor::Wlroots;
                }
                _ => {
                    if fallback.is_none() && !lower.starts_with("x-") {
                        fallback = Some(component.to_owned());
                    }
                }
            }
        }
    }

    fallback.map_or(Compositor::Unknown, Compositor::Other)
}

/// A one-line description of the session, for logs and bug reports.
///
/// Which backend was chosen and which compositor it found is reliably the first
/// question worth answering about any Linux capture defect.
#[must_use]
pub fn describe(env: &SessionEnv) -> String {
    format!(
        "{} on {}",
        detect_session(env),
        detect_compositor(env)
    )
}
