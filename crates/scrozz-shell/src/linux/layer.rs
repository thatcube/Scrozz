//! `wlr-layer-shell` surface configuration, as pure data.
//!
//! Everything in this file is a description of a request Scrozz will make, not
//! the request itself. That split exists because the interesting part of
//! layer-shell is not the plumbing — it is a handful of small decisions with
//! large and non-obvious consequences, and those decisions deserve to be
//! readable, reviewable and testable without a compositor in the room.
//!
//! # The three decisions that matter
//!
//! **The layer.** `Top` is above ordinary windows and below `Overlay`.
//! `Overlay` is above everything, including lock screens on some compositors.
//! Capture cards belong on `Top`: they are auxiliary, and a thumbnail that
//! floats above a screen locker is a security bug rather than a feature. The
//! fullscreen selection overlay belongs on `Overlay`, because it must be able to
//! cover the panel it is asking the user to select over.
//!
//! **The exclusive zone.** This is the layer-shell spelling of "anchor to the
//! work area, not the screen", and the protocol's own wording is exact:
//!
//! > If set to zero, the surface indicates that it would like to be moved to
//! > avoid occluding surfaces with a positive exclusive zone. If set to -1, the
//! > surface indicates that it would not like to be moved [...] and the
//! > compositor should extend it all the way to the edges it is anchored to.
//!
//! So `0` — not `-1` — is what keeps the capture stack above a KDE panel
//! instead of underneath it. `-1` is right only for the fullscreen selection
//! overlay, which genuinely does want to stretch over the panel. Getting this
//! backwards produces a stack that is invisible on exactly the machines that
//! have a bottom panel, which is most of them, and it looks like the overlay
//! failing to open at all.
//!
//! **The size.** `set_size(0, _)` means "compositor, you choose", and the
//! protocol makes it a *protocol error* — fatal to the connection — to pass 0
//! for a dimension whose opposite edges are not both anchored. The capture stack
//! is anchored bottom+left only, so both of its dimensions must be concrete.
//!
//! No `cfg(target_os)`, no Wayland types: this compiles and is tested
//! everywhere, and [`super::wayland`] is the thin part that turns it into
//! requests.

use crate::overlay::{OverlayBehavior, OverlayLevel};
use scrozz_core::LogicalSize;

/// `zwlr_layer_shell_v1.layer`.
///
/// Discriminants match the protocol's enum, so the conversion in
/// [`super::wayland`] is a cast rather than a table that can drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// Wallpaper. Below everything.
    Background = 0,
    /// Below ordinary windows.
    Bottom = 1,
    /// Above ordinary windows. Where auxiliary overlays belong.
    Top = 2,
    /// Above everything, including — on some compositors — the lock screen.
    Overlay = 3,
}

/// `zwlr_layer_surface_v1.anchor`, as a bitmask.
///
/// Hand-rolled rather than pulled from `bitflags` because it is four bits and
/// three operations, and a dependency for that is not worth the supply chain.
/// The bit values are the protocol's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Anchor(u32);

impl Anchor {
    /// Anchored to the top edge.
    pub const TOP: Self = Self(1);
    /// Anchored to the bottom edge.
    pub const BOTTOM: Self = Self(2);
    /// Anchored to the left edge.
    pub const LEFT: Self = Self(4);
    /// Anchored to the right edge.
    pub const RIGHT: Self = Self(8);

    /// No anchor: the compositor centres the surface.
    pub const NONE: Self = Self(0);
    /// All four edges — the fullscreen idiom.
    pub const ALL: Self = Self(1 | 2 | 4 | 8);

    /// The D28 anchor: bottom-left corner of the output.
    pub const BOTTOM_LEFT: Self = Self(2 | 4);

    /// The raw bitmask, for the protocol call.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether every bit in `other` is set here.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether both horizontal edges are anchored.
    ///
    /// The protocol permits `set_size(0, _)` only in this case; see the module
    /// docs on why passing 0 otherwise is fatal rather than merely wrong.
    #[must_use]
    pub const fn spans_horizontally(self) -> bool {
        self.contains(Self::LEFT) && self.contains(Self::RIGHT)
    }

    /// Whether both vertical edges are anchored.
    #[must_use]
    pub const fn spans_vertically(self) -> bool {
        self.contains(Self::TOP) && self.contains(Self::BOTTOM)
    }

    /// Whether the surface is pinned to all four edges.
    #[must_use]
    pub const fn is_fullscreen(self) -> bool {
        self.spans_horizontally() && self.spans_vertically()
    }
}

impl std::ops::BitOr for Anchor {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// `zwlr_layer_surface_v1.keyboard_interactivity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardInteractivity {
    /// Never receives keyboard events. The default, and what a capture card
    /// wants: D27 requires that clicking a card does not take the user's
    /// keystrokes away from whatever they were typing in.
    None = 0,
    /// Owns the keyboard while mapped. Right for the selection overlay, which
    /// must read Escape, and wrong for everything else.
    Exclusive = 1,
    /// Focus follows clicks, like an ordinary window. Added in interface
    /// version 4; requesting it against an older compositor is a protocol
    /// error, so nothing here emits it. It is the closest analogue to macOS's
    /// `becomesKeyOnlyIfNeeded` and is the natural upgrade for cards once a
    /// version floor of 4 is acceptable.
    OnDemand = 2,
}

/// Distances from each anchored edge, in surface-local (logical) pixels.
///
/// Margins on edges the surface is not anchored to are ignored by the
/// compositor, so the unused fields are harmless rather than misleading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Margins {
    /// Distance from the top edge.
    pub top: i32,
    /// Distance from the right edge.
    pub right: i32,
    /// Distance from the bottom edge.
    pub bottom: i32,
    /// Distance from the left edge.
    pub left: i32,
}

impl Margins {
    /// The same distance on every edge.
    #[must_use]
    pub const fn uniform(value: i32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}

/// Everything needed to configure one `zwlr_layer_surface_v1`.
///
/// Produced by [`Self::for_behavior`] and consumed by [`super::wayland`], which
/// does nothing but turn each field into the matching request in the order the
/// protocol requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerSurfaceConfig {
    /// Stacking layer.
    pub layer: Layer,
    /// Anchored edges.
    pub anchor: Anchor,
    /// Surface width in logical pixels; `0` means "compositor decides" and is
    /// legal only when [`Anchor::spans_horizontally`].
    pub width: u32,
    /// Surface height in logical pixels; `0` means "compositor decides" and is
    /// legal only when [`Anchor::spans_vertically`].
    pub height: u32,
    /// Distance from each anchored edge.
    pub margins: Margins,
    /// See the module docs: `0` respects panels, `-1` covers them.
    pub exclusive_zone: i32,
    /// Keyboard policy.
    pub keyboard_interactivity: KeyboardInteractivity,
    /// The `namespace` string passed to `get_layer_surface`. Compositors expose
    /// it in window rules, so it is part of Scrozz's user-visible surface.
    pub namespace: &'static str,
}

/// The namespace every Scrozz layer surface is created with.
///
/// Stable on purpose: a user writing a KWin rule against Scrozz's overlay needs
/// something that does not change between releases.
pub const NAMESPACE: &str = "scrozz-overlay";

impl LayerSurfaceConfig {
    /// Derives a configuration from an overlay's behaviour and size.
    ///
    /// `size` is the surface's own logical size — for the capture stack, the
    /// size [`crate::overlay::StackLayout`] computed from D28's slot geometry,
    /// not the size of the screen. `margin` is D28's gap from the work-area
    /// edge, expressed here as a bottom-left margin because layer-shell has no
    /// coordinates to add it to.
    ///
    /// The fullscreen case is detected from the anchor rather than requested
    /// separately, so the "size 0 needs opposite edges" rule cannot be violated
    /// by a caller that anchors one way and sizes another.
    #[must_use]
    pub fn for_behavior(behavior: &OverlayBehavior, size: LogicalSize, margin: f64) -> Self {
        let anchor = anchor_for(behavior);
        let fullscreen = anchor.is_fullscreen();

        Self {
            layer: layer_for(behavior.level),
            anchor,
            // A fullscreen surface hands both dimensions to the compositor,
            // which is both legal (opposite edges are anchored) and correct: it
            // is the only way to cover an output whose size Scrozz has not been
            // told.
            width: if anchor.spans_horizontally() {
                0
            } else {
                round_extent(size.width)
            },
            height: if anchor.spans_vertically() {
                0
            } else {
                round_extent(size.height)
            },
            margins: if fullscreen {
                Margins::default()
            } else {
                Margins {
                    bottom: round_margin(margin),
                    left: round_margin(margin),
                    ..Margins::default()
                }
            },
            exclusive_zone: exclusive_zone_for(anchor),
            keyboard_interactivity: if behavior.accepts_key {
                KeyboardInteractivity::Exclusive
            } else {
                KeyboardInteractivity::None
            },
            namespace: NAMESPACE,
        }
    }

    /// Whether this configuration would be rejected by the protocol.
    ///
    /// A layer-shell protocol error is fatal — it destroys the whole client
    /// connection, taking the application's other windows with it — so the one
    /// rule that is easy to get wrong is checked before the request is sent
    /// rather than discovered afterwards.
    ///
    /// Returns the reason as a sentence, or `None` when the configuration is
    /// valid.
    #[must_use]
    pub fn rejection_reason(&self) -> Option<String> {
        if self.width == 0 && !self.anchor.spans_horizontally() {
            return Some("width 0 requires anchoring to both the left and right edges".to_string());
        }
        if self.height == 0 && !self.anchor.spans_vertically() {
            return Some(
                "height 0 requires anchoring to both the top and bottom edges".to_string(),
            );
        }
        None
    }
}

/// Maps an overlay level onto a layer-shell layer.
///
/// [`OverlayLevel::Normal`] has no layer-shell equivalent — ordinary windows sit
/// *between* `Bottom` and `Top`, in a band no layer surface can occupy — so it
/// maps to `Bottom` and should not be made a layer surface at all. Nothing in
/// Scrozz asks for it: D27 makes `Normal` the default precisely so that a
/// surface which forgot to opt in is harmless.
#[must_use]
pub const fn layer_for(level: OverlayLevel) -> Layer {
    match level {
        OverlayLevel::Normal => Layer::Bottom,
        OverlayLevel::Floating | OverlayLevel::Status => Layer::Top,
        OverlayLevel::AboveMenuBar | OverlayLevel::Shielding => Layer::Overlay,
    }
}

/// Chooses the anchor set for a behaviour.
///
/// The rule is the one D28 states: anything that is not a full-screen shield is
/// the bottom-left capture stack. `Shielding` and `AboveMenuBar` are the two
/// levels a fullscreen selection uses.
#[must_use]
const fn anchor_for(behavior: &OverlayBehavior) -> Anchor {
    match behavior.level {
        OverlayLevel::Shielding | OverlayLevel::AboveMenuBar => Anchor::ALL,
        _ => Anchor::BOTTOM_LEFT,
    }
}

/// Chooses the exclusive zone from the anchor set.
///
/// See the module docs. `-1` for a surface that covers the output, `0` for
/// everything else so that panels push it out of the way instead of covering
/// it.
#[must_use]
const fn exclusive_zone_for(anchor: Anchor) -> i32 {
    if anchor.is_fullscreen() { -1 } else { 0 }
}

/// Converts a logical extent to the protocol's `uint`.
///
/// Rounds up, because a surface half a pixel too small clips its own content,
/// and clamps at zero so a nonsensical size becomes "compositor decides" rather
/// than a panic or a wrapped `u32`.
#[must_use]
fn round_extent(value: f64) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    value.ceil() as u32
}

/// Converts a logical margin to the protocol's `int`.
///
/// Rounds to nearest: a margin is a gap, and half a pixel either way is
/// invisible, whereas rounding up systematically would drift the stack away
/// from the corner as the margin is reapplied.
#[must_use]
fn round_margin(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    value
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}
