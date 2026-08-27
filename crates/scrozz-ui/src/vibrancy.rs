//! Native window materials — the real frosted backdrop, where the OS has one.
//!
//! Scrozz's surfaces float over whatever the user is doing. Drawing a
//! translucent grey rectangle is not the same thing as the desktop actually
//! being *frosted* behind them: a real material samples and blurs live content,
//! reacts to what is underneath, and is composited by the window server rather
//! than by us. It is the single largest difference between a UI that looks
//! native and one that looks like a web page in a borderless window.
//!
//! # What the spike established
//!
//! Two macOS materials were tried, and the distinction matters enough to encode
//! in the type:
//!
//! * **`NSVisualEffectView`** (classic vibrancy) sits *behind* the view's
//!   content. The desktop frosts; the controls stay crisp. This is what an
//!   overlay wants.
//! * **`NSGlassEffectView`** (Liquid Glass, macOS 26) styles the view's
//!   *content* as glass, which blurs our own foreground UI along with
//!   everything else. It is beautiful for a control that *is* the glass, and
//!   unusable as a plain window backdrop.
//!
//! Both are represented so the choice stays deliberate rather than being
//! rediscovered later.
//!
//! # Current status: no material is applied on any platform
//!
//! [`apply`] is real, total and honest, but it always reports
//! [`Applied::Unavailable`]. Nothing here silently no-ops, and nothing here
//! panics; a caller can always ask what it got and adjust. This is deliberate:
//! the dependency that implements the macOS path is not in the workspace.
//!
//! Surfaces must therefore **look correct with no material at all** — the
//! translucent [`crate::theme::Palette::card_fill`] plus
//! [`crate::paint::glass_panel`] carry the whole effect today. A material is a
//! *refinement* that lets those be dialled back (see
//! [`crate::theme::Palette::over_material`]), never a prerequisite.
//!
//! # Restoring the macOS material
//!
//! The spike used [`window-vibrancy`](https://github.com/tauri-apps/window-vibrancy).
//! Released 0.8.0 exposes classic vibrancy but **not** Liquid Glass, whose
//! `apply_liquid_glass` / `NSGlassEffectViewStyle` API lives only on the `dev`
//! branch — the spike pinned commit `e9f765a4c5a291d8eb636ffabf638b39d9783ebe`
//! for reproducibility. To bring it back:
//!
//! 1. Add to the workspace manifest, and to `scrozz-ui`:
//!
//!    ```toml
//!    [target.'cfg(target_os = "macos")'.dependencies]
//!    window-vibrancy = "0.8"                       # classic vibrancy only
//!    # ...or, for Liquid Glass, an exact git revision:
//!    # window-vibrancy = { git = "https://github.com/tauri-apps/window-vibrancy", rev = "e9f765a…" }
//!    ```
//!
//! 2. Fill in the one clearly-marked branch in [`apply`]. Everything else here
//!    — the vocabulary, the fallback order, the reporting — is already correct
//!    and needs no change.
//!
//! Weigh it first. A git dependency on an unreleased branch of a third-party
//! Objective-C shim is a real supply-chain and maintenance cost for an effect
//! the spike found actively *wrong* for a backdrop. Classic vibrancy from the
//! released crate delivers most of the value at a fraction of the risk, and is
//! the recommended step if this is picked up.
//!
//! # Other platforms
//!
//! Windows 11 has Mica and Acrylic (`DwmSetWindowAttribute`), and the same
//! crate wraps them. Linux has no equivalent at all: blur behind a surface is a
//! compositor extension, absent on wlroots, and per decision D8 that gap is
//! documented and degraded rather than papered over. [`Material::supported`]
//! encodes which of these could ever work.

/// A native backdrop material.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Material {
    /// Draw the backdrop ourselves. Always available, always correct.
    #[default]
    None,
    /// The platform's standard behind-content blur: `NSVisualEffectView`
    /// `HudWindow` on macOS, Acrylic on Windows.
    ///
    /// Content stays crisp. This is the right choice for an overlay.
    Vibrancy,
    /// macOS 26 Liquid Glass (`NSGlassEffectView`).
    ///
    /// Styles the view's *content* as glass, blurring our own foreground.
    /// Correct only for a surface that is meant to read as a glass object in
    /// its entirety.
    Glass,
}

impl Material {
    /// Parse a material from a configuration or command-line string.
    ///
    /// Unknown values fall back to [`Material::None`] rather than failing: a
    /// stale config should not stop the app from opening.
    #[must_use]
    pub fn parse(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "vibrancy" | "blur" | "acrylic" | "hud" => Self::Vibrancy,
            "glass" | "liquid" | "liquid-glass" => Self::Glass,
            _ => Self::None,
        }
    }

    /// The name [`Material::parse`] round-trips.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Vibrancy => "vibrancy",
            Self::Glass => "glass",
        }
    }

    /// Whether this material could ever be applied on the current platform,
    /// ignoring whether the code to do so is compiled in.
    ///
    /// Distinct from "will work today": use it to decide whether to *offer* the
    /// option in settings at all. Offering a control that cannot do anything on
    /// the user's system is worse than not offering it.
    #[must_use]
    pub const fn supported(self) -> bool {
        match self {
            Self::None => true,
            Self::Vibrancy => cfg!(any(target_os = "macos", target_os = "windows")),
            Self::Glass => cfg!(target_os = "macos"),
        }
    }

    /// Every material, for settings UI and tests.
    pub const ALL: &'static [Self] = &[Self::None, Self::Vibrancy, Self::Glass];
}

impl std::fmt::Display for Material {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What [`apply`] actually managed to do.
///
/// Returned rather than logged-and-forgotten because the answer changes how the
/// surface should paint: over a real material the card fill is dialled back and
/// its shadow suppressed, because the material is already doing that work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// No material was requested. Not a failure.
    NotRequested,
    /// The requested material is in place.
    Material(Material),
    /// The requested material was unavailable and a lesser one was used.
    FellBack {
        /// What was asked for.
        wanted: Material,
        /// What is actually in place.
        got: Material,
        /// Why, in terms a log reader can act on.
        why: String,
    },
    /// Nothing was applied.
    Unavailable {
        /// What was asked for.
        wanted: Material,
        /// Why, in terms a log reader can act on.
        why: String,
    },
}

impl Applied {
    /// The material that ended up in place.
    #[must_use]
    pub fn material(&self) -> Material {
        match self {
            Self::NotRequested | Self::Unavailable { .. } => Material::None,
            Self::Material(m) => *m,
            Self::FellBack { got, .. } => *got,
        }
    }

    /// Whether a real OS material is compositing behind the window.
    ///
    /// Feed this to [`crate::theme::Palette::over_material`].
    #[must_use]
    pub fn has_material(&self) -> bool {
        self.material() != Material::None
    }
}

impl std::fmt::Display for Applied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRequested => f.write_str("none (not requested)"),
            Self::Material(m) => write!(f, "{m}"),
            Self::FellBack { wanted, got, why } => write!(f, "{got} (wanted {wanted}: {why})"),
            Self::Unavailable { wanted, why } => write!(f, "none (wanted {wanted}: {why})"),
        }
    }
}

/// The corner radius a window material is masked to, in physical pixels.
///
/// Matches [`crate::theme::Radius::CARD`]; a material with square corners
/// behind a rounded card produces a visible grey collar.
pub const CORNER_RADIUS: f64 = 20.0;

/// Apply a native backdrop material to the window, reporting what happened.
///
/// Call once, during `eframe` app construction — the material attaches to the
/// window's view, and re-applying per frame would leak view layers.
///
/// Never fails and never panics: an unavailable material is a degraded
/// appearance, not an error, and per decision D8 the app must open regardless.
///
/// # Restoring the real implementation
///
/// The `Material::Vibrancy | Material::Glass` branch below is the *only* thing
/// that needs to change; see the module documentation for the dependency and
/// the rationale.
#[allow(unused_variables)]
pub fn apply(cc: &eframe::CreationContext<'_>, material: Material, radius: f64) -> Applied {
    match material {
        Material::None => Applied::NotRequested,

        // ── The one branch to fill in ────────────────────────────────────────
        //
        // With `window-vibrancy` available this becomes, on macOS:
        //
        //     Material::Vibrancy => match apply_vibrancy(
        //         cc,
        //         NSVisualEffectMaterial::HudWindow,
        //         Some(NSVisualEffectState::Active),
        //         Some(radius),
        //     ) {
        //         Ok(()) => Applied::Material(Material::Vibrancy),
        //         Err(e) => Applied::Unavailable { wanted: material, why: e.to_string() },
        //     },
        //
        // and for Glass, `apply_liquid_glass` with a fall back to the vibrancy
        // arm above — Liquid Glass needs macOS 26 and fails cleanly below it,
        // which is exactly what `Applied::FellBack` is for.
        wanted => {
            let why = if wanted.supported() {
                "no window-material backend is compiled in; see the `vibrancy` module docs"
            } else if cfg!(target_os = "linux") {
                "no compositor-independent blur protocol exists on Linux (D8)"
            } else {
                "not available on this platform"
            };
            tracing::debug!(%wanted, why, "window material not applied");
            Applied::Unavailable {
                wanted,
                why: why.to_owned(),
            }
        }
    }
}
