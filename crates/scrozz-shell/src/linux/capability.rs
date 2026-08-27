//! Deciding which Linux overlay mechanism is actually available.
//!
//! Linux is not one platform for this purpose, it is three, and the difference
//! between them is not a matter of polish — it changes what the overlay *is*:
//!
//! - **X11** — a client may set absolute window coordinates and may shape its
//!   own input region. Everything Scrozz's design assumes is achievable.
//! - **Wayland with `wlr-layer-shell`** — Scrozz owns a rendered surface and asks
//!   the compositor to anchor it to a screen edge with margins. KDE/KWin and the
//!   wlroots family implement this. eframe's `xdg_toplevel` remains a separate
//!   fallback and is never promoted.
//! - **Wayland without `wlr-layer-shell`** — a client can neither position nor
//!   anchor. GNOME/Mutter is here *by choice*, not by omission (decision D31),
//!   so there is no version to wait for and no flag to set.
//!
//! # Why this is a module and not three `if`s at the call site
//!
//! Because the interesting question is not "which one is it" but "what is the
//! user actually going to get, and does the code know that it is lying". A
//! backend that quietly does nothing and returns success is the single worst
//! outcome available here: the overlay is invisible, no error is logged, and the
//! bug reads as "Scrozz doesn't work on my machine". Every plan below therefore
//! carries an explicit [`Placement`], explicit capability flags, and a sentence
//! that can be shown to a human.
//!
//! # Static knowledge is a prior, never an answer
//!
//! [`layer_shell_expectation`] encodes what is known about each compositor, and
//! it is deliberately *not* what [`plan`] trusts when a real answer is
//! available. Compositors gain protocols, users run forks, and a table baked
//! into a binary in 2025 is wrong by 2027. A live registry probe
//! ([`LayerShellProbe::Present`] / [`LayerShellProbe::Absent`]) always wins; the
//! table is consulted only when no probe has been run, and when it is consulted
//! for an unrecognised compositor the answer is the conservative one.
//!
//! Everything here is arithmetic over enums: no X connection, no Wayland
//! socket, no `cfg(target_os)`. It compiles and is tested on every host.

use crate::hotkey::{Compositor, DisplayServer};

/// What a live Wayland registry said about `zwlr_layer_shell_v1`.
///
/// The three states are genuinely distinct and collapsing any two of them loses
/// the information that makes the diagnostics useful. "Absent" means the
/// compositor was asked and said no; "not probed" means nobody asked. Reporting
/// the second as the first would blame the compositor for Scrozz's own omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerShellProbe {
    /// The global was advertised, at this interface version.
    Present {
        /// Version the compositor advertised. Version 4 is where
        /// `keyboard_interactivity: on_demand` and `set_exclusive_edge` arrive.
        version: u32,
    },
    /// The registry was enumerated to completion and the global was not there.
    Absent,
    /// No probe was attempted — not a Wayland session, or the connection failed.
    NotProbed,
}

/// What is known, statically, about a compositor's layer-shell support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// Implements `wlr-layer-shell`.
    Implements,
    /// Deliberately does not implement it, and has said so.
    Refuses,
    /// Not known. Must be probed; never assumed either way.
    Unknown,
}

/// What Scrozz knows about a compositor before asking it anything.
///
/// KDE/KWin implements `wlr-layer-shell-unstable-v1` and the wlroots family
/// defines it. GNOME/Mutter is the one entry that is [`Expectation::Refuses`]
/// rather than [`Expectation::Unknown`]: the position is longstanding and
/// deliberate, which is why decision D31 plans around it instead of waiting for
/// it.
///
/// [`Compositor::Other`] is [`Expectation::Unknown`] and must stay that way. It
/// is the bucket every compositor written after this table falls into.
#[must_use]
pub const fn layer_shell_expectation(compositor: Compositor) -> Expectation {
    match compositor {
        Compositor::Kde
        | Compositor::Sway
        | Compositor::Hyprland
        | Compositor::River
        | Compositor::Niri
        | Compositor::Wayfire => Expectation::Implements,
        Compositor::Gnome => Expectation::Refuses,
        Compositor::Other => Expectation::Unknown,
    }
}

/// The mechanism an overlay window is driven through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayBackend {
    /// X11: retrofit an existing window with EWMH state, stacking and an input
    /// shape. Includes XWayland, where it works exactly as it does on X11 but
    /// only relative to the X server's own coordinate space.
    X11Retrofit,
    /// Wayland `zwlr_layer_shell_v1`: reserved for a Scrozz-owned rendered
    /// surface anchored by the compositor.
    ///
    /// Selected only for Scrozz's own surface. A winit window already has the
    /// `xdg_toplevel` role and cannot be promoted.
    LayerShell,
    /// Wayland `xdg_shell` only: an ordinary toplevel the compositor places
    /// wherever it likes. The D31 fallback.
    CompositorPlaced,
    /// No display server at all — CLI, CI, a bare TTY.
    Headless,
}

impl OverlayBackend {
    /// A short stable token for logs, diagnostics and tests.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::X11Retrofit => "x11",
            Self::LayerShell => "layer-shell",
            Self::CompositorPlaced => "compositor-placed",
            Self::Headless => "headless",
        }
    }
}

/// How much control Scrozz has over where the overlay ends up.
///
/// This is the field the capture stack cares about, because D28's geometry is
/// only meaningful if something honours it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Scrozz sets coordinates and they are used verbatim. X11 only.
    Absolute,
    /// Scrozz names an edge and margins; the compositor computes coordinates
    /// from them. Layer-shell. D28's bottom-left anchor survives this intact,
    /// which is the whole reason the stack was specified as an anchor rather
    /// than a position.
    Anchored,
    /// The compositor decides, and Scrozz is not consulted. Requested positions
    /// are discarded by the protocol, not by the compositor's whim.
    CompositorChosen,
    /// Nothing is placed because nothing is shown.
    Nowhere,
}

impl Placement {
    /// Whether D28's bottom-left stack anchor will actually be honoured.
    #[must_use]
    pub const fn honours_anchor(self) -> bool {
        matches!(self, Self::Absolute | Self::Anchored)
    }
}

/// The complete, honest account of what the overlay layer can do here.
///
/// Constructed by [`plan`] and carried into diagnostics unchanged. Every
/// boolean is a promise: if it is `true`, the corresponding backend call is
/// expected to do something real, and a backend that cannot must report a
/// failure rather than return `Ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayPlan {
    /// Mechanism in use.
    pub backend: OverlayBackend,
    /// Positional control.
    pub placement: Placement,
    /// Whether clicks can be made to fall through the transparent parts of the
    /// overlay. X11 `SHAPE`, Wayland `wl_surface.set_input_region`.
    ///
    /// Without this the overlay is a rectangle that eats every click inside its
    /// bounds, which for a mostly-empty capture stack means a large dead zone in
    /// the corner of the user's screen. D27's invisibility-at-rest depends on
    /// it.
    pub input_shaping: bool,
    /// Whether the overlay is reliably stacked above ordinary windows.
    pub stays_above: bool,
    /// Whether Scrozz controls whether the surface takes keyboard focus.
    pub controls_focus: bool,
    /// One sentence, addressed to a human, saying what is happening and why.
    pub detail: String,
}

impl OverlayPlan {
    /// Whether this plan describes a surface Scrozz genuinely controls.
    ///
    /// Used to decide whether to say "overlay ready" or to say what is missing.
    #[must_use]
    pub const fn is_fully_controlled(&self) -> bool {
        self.placement.honours_anchor() && self.input_shaping
    }

    /// Whether anything is drawn at all.
    #[must_use]
    pub const fn draws_anything(&self) -> bool {
        !matches!(self.backend, OverlayBackend::Headless)
    }
}

/// Chooses the overlay backend for a session.
///
/// `probe` is the result of a live registry enumeration. It is ignored on X11,
/// where layer-shell is irrelevant. On Wayland an advertised global selects
/// Scrozz's owned layer surface except on GNOME, where D31 deliberately retains
/// the ordinary compositor-positioned window. The eframe/winit fallback is
/// already an `xdg_toplevel` and is never promoted.
///
/// # The GNOME case is a decision, not a defect
///
/// When the answer is [`OverlayBackend::CompositorPlaced`] the returned plan
/// says `placement: CompositorChosen` and the detail sentence names the reason.
/// That plan is then *used* — Scrozz shows an ordinary window rather than
/// nothing — but it never claims the window is anchored. Decision D31 calls this
/// "visibly intentional rather than pretending to be anchored", and the
/// difference between the two is exactly this struct.
#[must_use]
pub fn plan(server: DisplayServer, compositor: Compositor, probe: LayerShellProbe) -> OverlayPlan {
    match server {
        DisplayServer::X11 => OverlayPlan {
            backend: OverlayBackend::X11Retrofit,
            placement: Placement::Absolute,
            input_shaping: true,
            stays_above: true,
            controls_focus: true,
            detail: "X11: overlays are positioned absolutely, kept above other \
                     windows, and shaped so clicks fall through empty space."
                .into(),
        },
        DisplayServer::Wayland => wayland_plan(compositor, probe),
        DisplayServer::Quartz | DisplayServer::Windows => OverlayPlan {
            backend: OverlayBackend::Headless,
            placement: Placement::Nowhere,
            input_shaping: false,
            stays_above: false,
            controls_focus: false,
            detail: "Not a Linux session; the Linux overlay backend is not used here.".into(),
        },
        DisplayServer::Headless => OverlayPlan {
            backend: OverlayBackend::Headless,
            placement: Placement::Nowhere,
            input_shaping: false,
            stays_above: false,
            controls_focus: false,
            detail: "No display server detected: no overlay can be shown. \
                     Scrozz's CLI is unaffected."
                .into(),
        },
    }
}

/// Reports what an already-role-bearing native window can actually do.
///
/// An adopted Wayland window is an `xdg_toplevel`; advertising layer-shell on
/// the same compositor cannot promote that surface to a second, incompatible
/// role. Owned layer-shell hosts use [`plan`], while winit adoption uses this
/// function and therefore always reports the compositor-positioned fallback.
#[must_use]
pub fn adopted_plan(
    server: DisplayServer,
    compositor: Compositor,
    probe: LayerShellProbe,
) -> OverlayPlan {
    if server == DisplayServer::Wayland {
        compositor_placed(compositor, probe)
    } else {
        plan(server, compositor, probe)
    }
}

/// The Wayland half of [`plan`], split out because it is the half with rules.
fn wayland_plan(compositor: Compositor, probe: LayerShellProbe) -> OverlayPlan {
    if compositor != Compositor::Gnome
        && let LayerShellProbe::Present { version } = probe
    {
        return OverlayPlan {
            backend: OverlayBackend::LayerShell,
            placement: Placement::Anchored,
            input_shaping: true,
            stays_above: true,
            controls_focus: true,
            detail: format!(
                "Wayland: Scrozz is using its rendered wlr-layer-shell v{version} surface, \
                 anchored bottom-left above ordinary windows with per-card input regions and \
                 no keyboard focus."
            ),
        };
    }
    compositor_placed(compositor, probe)
}

/// The fallback Wayland plan, with a reason that distinguishes compositor
/// capability from a registry Scrozz could not use.
fn compositor_placed(compositor: Compositor, probe: LayerShellProbe) -> OverlayPlan {
    let detail = match (compositor, probe) {
        (Compositor::Gnome, LayerShellProbe::Present { version }) => format!(
            "GNOME/Wayland: this Mutter build advertises wlr-layer-shell v{version}, but D31 \
             deliberately keeps the capture stack on the ordinary compositor-positioned \
             window path; region selection still uses the screenshot portal."
        ),
        (_, LayerShellProbe::Present { version }) => format!(
            "Wayland: wlr-layer-shell v{version} was advertised, but the owned surface was not \
             selected. Scrozz is explicitly using an ordinary compositor-positioned window."
        ),
        (Compositor::Gnome, LayerShellProbe::Absent | LayerShellProbe::NotProbed) => {
            "GNOME/Wayland: Mutter does not implement wlr-layer-shell, so no client can \
             anchor a window to the screen. Scrozz shows the capture stack as an ordinary \
             window the compositor places; region selection still uses the screenshot portal."
                .to_string()
        }
        (_, LayerShellProbe::Absent) => {
            "Wayland: this compositor does not offer wlr-layer-shell, so Scrozz cannot \
             anchor the capture stack. It is shown as an ordinary window the compositor places."
                .to_string()
        }
        (_, LayerShellProbe::NotProbed)
            if layer_shell_expectation(compositor) == Expectation::Implements =>
        {
            "Wayland: this compositor is expected to offer wlr-layer-shell, but Scrozz has \
             not been able to verify the live registry. The capture stack is an ordinary \
             window the compositor places."
                .to_string()
        }
        (_, LayerShellProbe::NotProbed) => {
            "Wayland: Scrozz does not know whether this compositor offers wlr-layer-shell \
             and has not been able to ask. Falling back to an ordinary window the \
             compositor places."
                .to_string()
        }
    };

    OverlayPlan {
        backend: OverlayBackend::CompositorPlaced,
        placement: Placement::CompositorChosen,
        // Winit can still toggle an xdg_toplevel's core input region, so the
        // pointer-based click-through fallback survives even though stable
        // per-card regions and controlled placement do not.
        input_shaping: true,
        // `xdg_shell` has no "always on top". Some compositors offer it as a
        // user-driven window rule, but no client can ask for it.
        stays_above: false,
        // Focus follows the compositor's ordinary toplevel policy.
        controls_focus: false,
        detail,
    }
}
