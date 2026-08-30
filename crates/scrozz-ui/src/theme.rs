//! Design tokens — colour, space, radius, elevation and type.
//!
//! Default egui reads as a debug tool: flat fills, hard 2 px radii, one weight
//! of one font, no elevation, no optical spacing. The polish does not live in
//! egui; it lives in the token layer you bring. This module is that layer, and
//! it is the single source of truth for every hand-drawn surface in the crate.
//!
//! # Shape of the system
//!
//! Tokens are grouped by *role*, not by value, so a surface names a decision
//! rather than a number:
//!
//! * [`Space`] — a 4 pt rhythm. Never write a raw gap.
//! * [`Radius`] — one corner family, so every rounded thing agrees.
//! * [`Elevation`] — how far off the surface something sits.
//! * [`Text`] — the type ramp, resolved against the four embedded Inter cuts.
//! * [`Palette`] — semantic colours for one appearance.
//! * [`Theme`] — a palette plus the appearance it came from, which is what a
//!   surface actually receives.
//!
//! # Accessibility (D13)
//!
//! Every foreground/background pair the design uses is checkable at build time:
//! [`contrast_ratio`] implements the WCAG 2.1 relative-luminance formula and
//! [`Contrast`] names the thresholds. The point is that a contrast regression
//! becomes a failing test rather than something a human has to notice.
//!
//! Nothing here encodes meaning in colour alone, and no token pair below AA is
//! used for text.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, epaint::Shadow};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Spacing
// ---------------------------------------------------------------------------

/// The spacing rhythm: a 4 pt grid.
///
/// Coherence in a premium UI comes largely from every gap being a step on one
/// scale rather than a number someone typed. Reach for the nearest step; if
/// nothing fits, the layout is probably wrong.
pub struct Space;

impl Space {
    /// 2 pt — hairline separation, optical nudges.
    pub const HAIR: f32 = 2.0;
    /// 4 pt.
    pub const XS: f32 = 4.0;
    /// 8 pt. The default gap between related things.
    pub const SM: f32 = 8.0;
    /// 12 pt.
    pub const MD: f32 = 12.0;
    /// 16 pt. The default padding inside a card.
    pub const LG: f32 = 16.0;
    /// 20 pt.
    pub const XL: f32 = 20.0;
    /// 24 pt. Separation between unrelated groups.
    pub const XXL: f32 = 24.0;
    /// 32 pt. Section breaks on large surfaces.
    pub const HUGE: f32 = 32.0;

    /// The grid unit, for the rare case something must be computed.
    pub const UNIT: f32 = 4.0;

    /// `n` grid units. Prefer a named step.
    #[must_use]
    pub const fn units(n: f32) -> f32 {
        Self::UNIT * n
    }
}

// ---------------------------------------------------------------------------
// Radius
// ---------------------------------------------------------------------------

/// The corner family. One rhythm, so nothing looks borrowed.
///
/// Radii are nested deliberately: an inner element's radius is smaller than its
/// container's by roughly its inset, which is what keeps concentric corners
/// looking parallel rather than pinched.
pub struct Radius;

impl Radius {
    /// 6 pt — chips, swatches, tiny inline surfaces.
    pub const CHIP: f32 = 6.0;
    /// 10 pt — buttons and icon buttons.
    pub const BUTTON: f32 = 10.0;
    /// 13 pt — thumbnails inside a card.
    pub const THUMB: f32 = 13.0;
    /// 17 pt — toolbars and action bars.
    pub const BAR: f32 = 17.0;
    /// 20 pt — capture cards and panels.
    pub const CARD: f32 = 20.0;

    /// A fully rounded end-cap for a pill of the given height.
    #[must_use]
    pub fn pill(height: f32) -> f32 {
        height / 2.0
    }
}

/// Convert a radius in points to epaint's per-corner `u8` form.
///
/// `CornerRadius` is `u8` per corner, so this is where the design system's
/// float meets egui's integer. Rounding and clamping happen once, here.
#[must_use]
pub fn corner(radius: f32) -> CornerRadius {
    CornerRadius::same(quantise_radius(radius))
}

/// A corner radius applied to the top two corners only.
#[must_use]
pub fn corner_top(radius: f32) -> CornerRadius {
    let r = quantise_radius(radius);
    CornerRadius {
        nw: r,
        ne: r,
        sw: 0,
        se: 0,
    }
}

/// A corner radius applied to the bottom two corners only.
#[must_use]
pub fn corner_bottom(radius: f32) -> CornerRadius {
    let r = quantise_radius(radius);
    CornerRadius {
        nw: 0,
        ne: 0,
        sw: r,
        se: r,
    }
}

fn quantise_radius(radius: f32) -> u8 {
    if radius.is_finite() {
        radius.round().clamp(0.0, 255.0) as u8
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Elevation
// ---------------------------------------------------------------------------

/// How far off the surface something sits.
///
/// egui's single built-in shadow cannot express a real drop shadow, so every
/// level here resolves to a *pair* — a tight ambient contact shadow and a wide
/// soft key shadow. That pair is the whole difference between "has a shadow"
/// and "is floating".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Elevation {
    /// Flush with its parent. No shadow at all.
    Flat,
    /// A control resting on a surface.
    Resting,
    /// A panel or card floating over content.
    Raised,
    /// A card lifted by the pointer, or a menu over everything.
    Lifted,
}

impl Elevation {
    /// A multiplier on the shadow geometry, `0.0..=1.4`.
    ///
    /// Exposed because a *dragged* card interpolates between levels — its lift
    /// is continuous, not a step.
    #[must_use]
    pub fn lift(self) -> f32 {
        match self {
            Self::Flat => 0.0,
            Self::Resting => 0.5,
            Self::Raised => 1.0,
            Self::Lifted => 1.4,
        }
    }

    /// The ambient (contact) and key (cast) shadows for this level.
    ///
    /// Returns `None` for [`Elevation::Flat`], which is not the same as a
    /// zero-blur shadow — it means *draw nothing*.
    #[must_use]
    pub fn shadows(self, palette: &Palette) -> Option<(Shadow, Shadow)> {
        if self == Self::Flat {
            return None;
        }
        Some(shadows_for_lift(self.lift(), palette))
    }
}

/// The ambient and key shadows for an arbitrary continuous lift.
///
/// Used directly by anything whose elevation animates, such as a card being
/// picked up.
#[must_use]
pub fn shadows_for_lift(lift: f32, palette: &Palette) -> (Shadow, Shadow) {
    let lift = lift.clamp(0.0, 4.0);
    // Wide and faint rather than tight and dark. A short blur concentrates the
    // whole shadow into a band just under the object, which has a legible
    // outer edge — and an edge is what reads as harsh. Spreading the same
    // shadow much further, at well under half the opacity, gives the object
    // the same weight with nothing in it for the eye to catch.
    //
    // The ambient shadow carries a *spread* and almost no offset, and both are
    // load-bearing. `Shadow::as_shape` grows the corner radius by the spread,
    // so a spread contact shadow follows the object's own curve instead of
    // cutting across it — and with the shape barely offset it surrounds the
    // object rather than pooling below it. Without that, the top corners of a
    // rounded card got almost no shadow at all: the offset had already moved
    // the shape down past them, and the corner curve pulled what was left
    // further inward still.
    let ambient = Shadow {
        offset: [0, quantise_offset(1.0 * lift)],
        blur: quantise_blur(26.0 * lift),
        spread: quantise_spread(2.0 * lift),
        color: palette.ambient_shadow,
    };
    let key = Shadow {
        offset: [0, quantise_offset(10.0 * lift)],
        blur: quantise_blur(88.0 * lift),
        spread: 0,
        color: palette.key_shadow,
    };
    (ambient, key)
}

fn quantise_offset(v: f32) -> i8 {
    v.round().clamp(f32::from(i8::MIN), f32::from(i8::MAX)) as i8
}

fn quantise_blur(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

fn quantise_spread(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

// ---------------------------------------------------------------------------
// Type
// ---------------------------------------------------------------------------

/// The named weights of the embedded Inter family.
///
/// egui selects a *family*, not a weight axis, so each cut is registered as its
/// own named family. [`install_fonts`] must have run on the context before any
/// of these resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Weight {
    /// Inter Regular — body copy.
    Regular,
    /// Inter Medium — labels and dense UI text.
    Medium,
    /// Inter SemiBold — titles and buttons.
    SemiBold,
    /// Inter Bold — display sizes only.
    Bold,
}

impl Weight {
    /// The egui font family this weight is registered under.
    #[must_use]
    pub fn family(self) -> FontFamily {
        match self {
            Self::Regular => FontFamily::Proportional,
            Self::Medium => FontFamily::Name(MEDIUM.into()),
            Self::SemiBold => FontFamily::Name(SEMIBOLD.into()),
            Self::Bold => FontFamily::Name(BOLD.into()),
        }
    }
}

const MEDIUM: &str = "inter-medium";
const SEMIBOLD: &str = "inter-semibold";
const BOLD: &str = "inter-bold";

/// The type ramp.
///
/// Each role pairs a size with a weight; call sites name a role and never a
/// size. Sizes are in logical points and are deliberately fractional — optical
/// sizing at UI scale does not land on integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Text {
    /// 30 pt Bold — onboarding and empty-state headlines.
    Display,
    /// 15 pt Regular — the sentence under a display headline.
    Subtitle,
    /// 15 pt SemiBold — a card or panel title.
    Title,
    /// 15 pt SemiBold — button and pill text.
    Button,
    /// 13.5 pt Medium — the workhorse UI label.
    Label,
    /// 13 pt Regular — running text.
    Body,
    /// 12.5 pt Medium — keyboard shortcut glyph runs.
    Shortcut,
    /// 11.5 pt Regular — captions, dimensions, timestamps.
    Caption,
}

impl Text {
    /// The size in logical points.
    #[must_use]
    pub fn size(self) -> f32 {
        match self {
            Self::Display => 30.0,
            Self::Subtitle | Self::Title | Self::Button => 15.0,
            Self::Label => 13.5,
            Self::Body => 13.0,
            Self::Shortcut => 12.5,
            Self::Caption => 11.5,
        }
    }

    /// The weight this role is set in.
    #[must_use]
    pub fn weight(self) -> Weight {
        match self {
            Self::Display => Weight::Bold,
            Self::Title | Self::Button => Weight::SemiBold,
            Self::Label | Self::Shortcut => Weight::Medium,
            Self::Subtitle | Self::Body | Self::Caption => Weight::Regular,
        }
    }

    /// The resolved [`FontId`] to hand to a painter.
    #[must_use]
    pub fn font(self) -> FontId {
        FontId::new(self.size(), self.weight().family())
    }

    /// Every role, for tests and specimen rendering.
    pub const ALL: &'static [Self] = &[
        Self::Display,
        Self::Subtitle,
        Self::Title,
        Self::Button,
        Self::Label,
        Self::Body,
        Self::Shortcut,
        Self::Caption,
    ];
}

impl From<Text> for FontId {
    fn from(role: Text) -> Self {
        role.font()
    }
}

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

/// Which appearance a palette describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Appearance {
    /// Dark mode. The primary appearance for an overlay over a desktop.
    Dark,
    /// Light mode.
    Light,
}

impl Appearance {
    /// Whether this is the dark appearance.
    #[must_use]
    pub fn is_dark(self) -> bool {
        self == Self::Dark
    }

    /// The opposite appearance.
    #[must_use]
    pub fn inverted(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }
}

/// The resolved semantic colours for one appearance.
///
/// Names describe *role*, never hue — `text_muted`, not `grey60` — so a
/// re-skin is a change here and nowhere else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// The appearance these colours belong to.
    pub appearance: Appearance,

    /// True when the surface sits over a *real* OS material, in which case its
    /// own fill is dialled down and its shadow suppressed — the material does
    /// that work. See [`crate::vibrancy`] for why this is currently never set
    /// on any platform.
    pub over_material: bool,

    /// The signature accent (Scrozz iris — deliberately not CleanShot blue).
    pub accent: Color32,
    /// Accent, brightened: hover and inner lighting.
    pub accent_hi: Color32,
    /// Accent, darkened: the pressed state.
    pub accent_press: Color32,
    /// Foreground on an accent fill.
    pub on_accent: Color32,

    /// Primary text.
    pub text: Color32,
    /// Secondary text — still readable, clearly subordinate.
    pub text_muted: Color32,
    /// Tertiary text — shortcut hints, timestamps. Not for anything essential.
    pub text_faint: Color32,

    /// The fill of a card or panel.
    pub card_fill: Color32,
    /// The fill of something sitting on top of a card.
    pub card_fill_raised: Color32,
    /// A one-pixel border that defines an edge without drawing attention.
    pub hairline: Color32,
    /// The inner lighting along a panel's top edge.
    pub top_highlight: Color32,
    /// The inner shading along a panel's bottom edge.
    pub bottom_shade: Color32,

    /// A control's hover wash.
    pub hover: Color32,
    /// A control's pressed wash.
    pub active: Color32,
    /// The keyboard focus ring (D13 — focus is always visible).
    pub focus_ring: Color32,

    /// Live recording and destructive failure status.
    pub recording: Color32,
    /// Recoverable warning and partial-output status.
    pub warning: Color32,
    /// Successful terminal status.
    pub success: Color32,

    /// The wide, soft cast shadow.
    pub key_shadow: Color32,
    /// The tight contact shadow.
    pub ambient_shadow: Color32,

    /// The border around a thumbnail of arbitrary content.
    pub thumb_border: Color32,
    /// A chip or inset track fill.
    pub chip_fill: Color32,
    /// A separator between rows or groups.
    pub divider: Color32,
}

impl Palette {
    /// The dark palette.
    #[must_use]
    pub const fn dark() -> Self {
        Self {
            appearance: Appearance::Dark,
            over_material: false,

            // A periwinkle accent light enough to glow against a near-black
            // canvas cannot also carry white text: white on `#7C7AFF` is only
            // 3.4:1, and on the hover tint 2.8:1 — below AA either way. So the
            // dark theme inverts the pairing and puts near-black ink on the
            // accent, which reaches 4.7:1 or better across all three states
            // without dulling the accent itself. The press state is only
            // slightly deeper than rest for the same reason: any darker and it
            // drops back under 4.5:1.
            accent: Color32::from_rgb(0x7C, 0x7A, 0xFF),
            accent_hi: Color32::from_rgb(0x9A, 0x97, 0xFF),
            accent_press: Color32::from_rgb(0x6F, 0x6C, 0xF2),
            on_accent: Color32::from_rgb(0x0E, 0x0D, 0x18),

            text: Color32::from_rgb(0xF3, 0xF4, 0xFB),
            text_muted: Color32::from_rgba_unmultiplied_const(0xEB, 0xEE, 0xF8, 160),
            text_faint: Color32::from_rgba_unmultiplied_const(0xEB, 0xEE, 0xF8, 96),

            // Semi-transparent so a material or wallpaper shows through.
            card_fill: Color32::from_rgba_unmultiplied_const(0x1B, 0x1D, 0x27, 180),
            card_fill_raised: Color32::from_rgba_unmultiplied_const(0x24, 0x27, 0x33, 205),
            hairline: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 26),
            top_highlight: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 40),
            bottom_shade: Color32::from_rgba_unmultiplied_const(0x00, 0x00, 0x00, 46),

            hover: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 20),
            active: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 36),
            focus_ring: Color32::from_rgb(0xA8, 0xA4, 0xFF),

            recording: Color32::from_rgb(0xFF, 0x68, 0x75),
            warning: Color32::from_rgb(0xFF, 0xC4, 0x5C),
            success: Color32::from_rgb(0x63, 0xD6, 0x9A),

            key_shadow: Color32::from_rgba_unmultiplied_const(0, 0, 0, 92),
            ambient_shadow: Color32::from_rgba_unmultiplied_const(0, 0, 0, 44),

            thumb_border: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 30),
            chip_fill: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 22),
            divider: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 24),
        }
    }

    /// The light palette.
    #[must_use]
    pub const fn light() -> Self {
        Self {
            appearance: Appearance::Light,
            over_material: false,

            accent: Color32::from_rgb(0x4B, 0x46, 0xE0),
            accent_hi: Color32::from_rgb(0x5B, 0x57, 0xF0),
            accent_press: Color32::from_rgb(0x3B, 0x37, 0xC4),
            on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),

            text: Color32::from_rgb(0x1A, 0x1C, 0x24),
            text_muted: Color32::from_rgba_unmultiplied_const(0x1A, 0x1C, 0x24, 170),
            text_faint: Color32::from_rgba_unmultiplied_const(0x1A, 0x1C, 0x24, 120),

            card_fill: Color32::from_rgba_unmultiplied_const(0xF7, 0xF7, 0xFB, 205),
            card_fill_raised: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 220),
            hairline: Color32::from_rgba_unmultiplied_const(0x1A, 0x1C, 0x24, 26),
            top_highlight: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 235),
            bottom_shade: Color32::from_rgba_unmultiplied_const(0x1A, 0x1C, 0x24, 14),

            hover: Color32::from_rgba_unmultiplied_const(0x1A, 0x1C, 0x24, 16),
            active: Color32::from_rgba_unmultiplied_const(0x1A, 0x1C, 0x24, 30),
            focus_ring: Color32::from_rgb(0x3B, 0x37, 0xC4),

            recording: Color32::from_rgb(0xB8, 0x1F, 0x38),
            warning: Color32::from_rgb(0x8A, 0x51, 0x00),
            success: Color32::from_rgb(0x0E, 0x6B, 0x42),

            key_shadow: Color32::from_rgba_unmultiplied_const(0x1A, 0x1C, 0x28, 46),
            ambient_shadow: Color32::from_rgba_unmultiplied_const(0x1A, 0x1C, 0x28, 22),

            thumb_border: Color32::from_rgba_unmultiplied_const(0x1A, 0x1C, 0x24, 28),
            chip_fill: Color32::from_rgba_unmultiplied_const(0x1A, 0x1C, 0x24, 16),
            divider: Color32::from_rgba_unmultiplied_const(0x1A, 0x1C, 0x24, 22),
        }
    }

    /// The palette for an appearance.
    #[must_use]
    pub const fn for_appearance(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => Self::dark(),
            Appearance::Light => Self::light(),
        }
    }

    /// Whether this palette is the dark one.
    #[must_use]
    pub fn is_dark(&self) -> bool {
        self.appearance.is_dark()
    }

    /// This palette, marked as sitting over a real OS material.
    #[must_use]
    pub fn over_material(mut self, yes: bool) -> Self {
        self.over_material = yes;
        self
    }

    /// The opaque colour a translucent token will actually composite to on this
    /// appearance's canvas.
    ///
    /// Contrast has to be measured against what the eye sees, not against a
    /// half-transparent token, so accessibility checks resolve through here.
    #[must_use]
    pub fn flatten(&self, color: Color32) -> Color32 {
        flatten_onto(color, self.canvas())
    }

    /// The opaque colour behind everything on this appearance.
    ///
    /// The window is genuinely transparent, so this is the assumed worst case
    /// for legibility rather than a colour that is literally painted.
    #[must_use]
    pub fn canvas(&self) -> Color32 {
        if self.is_dark() {
            Color32::from_rgb(0x0B, 0x0C, 0x12)
        } else {
            Color32::from_rgb(0xE9, 0xEC, 0xF5)
        }
    }

    /// A recessed surface: a slider track, a text field, a value box.
    ///
    /// Opaque on purpose. Everything a value is *read out of* has to stay
    /// legible over whatever the transparent window happens to sit on, and a
    /// translucent well over a bright wallpaper is exactly how a percentage
    /// label becomes unreadable.
    #[must_use]
    pub fn well(&self) -> Color32 {
        if self.is_dark() {
            Color32::from_rgb(0x12, 0x14, 0x1C)
        } else {
            Color32::from_rgb(0xDC, 0xE0, 0xEC)
        }
    }

    /// The fill of an interactive control at rest, resolved to an opaque colour.
    #[must_use]
    pub fn control_fill(&self) -> Color32 {
        self.flatten(self.chip_fill)
    }

    /// The fill of an interactive control under the pointer.
    #[must_use]
    pub fn control_fill_hover(&self) -> Color32 {
        flatten_onto(self.hover, self.control_fill())
    }

    /// The fill of an interactive control being pressed, or holding focus.
    #[must_use]
    pub fn control_fill_active(&self) -> Color32 {
        flatten_onto(self.active, self.control_fill())
    }

    /// The accent wash used behind a selected chip or an automatic value.
    #[must_use]
    pub fn accent_wash(&self) -> Color32 {
        flatten_onto(
            self.accent
                .gamma_multiply(if self.is_dark() { 0.22 } else { 0.16 }),
            self.canvas(),
        )
    }

    /// Text drawn on [`Palette::accent_wash`].
    ///
    /// The raw accent is not usable as ink on its own wash in either theme, so
    /// this leans it toward the surface's own text colour until it clears AA.
    #[must_use]
    pub fn on_accent_wash(&self) -> Color32 {
        if self.is_dark() {
            self.accent_hi
        } else {
            self.accent_press
        }
    }
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// A palette plus the appearance it came from: what a surface receives.
///
/// Exists so a surface signature is `&Theme` rather than a growing tuple of
/// loose tokens, and so future per-user overrides (a contrast boost, a larger
/// text scale) have somewhere to live that does not change any call site.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    /// The resolved colours.
    pub palette: Palette,
    /// A multiplier applied to every type-ramp size, mirroring the OS text-size
    /// setting (D13).
    pub text_scale: f32,
}

impl Theme {
    /// The dark theme at default text size.
    #[must_use]
    pub const fn dark() -> Self {
        Self {
            palette: Palette::dark(),
            text_scale: 1.0,
        }
    }

    /// The light theme at default text size.
    #[must_use]
    pub const fn light() -> Self {
        Self {
            palette: Palette::light(),
            text_scale: 1.0,
        }
    }

    /// The theme for an appearance.
    #[must_use]
    pub const fn for_appearance(appearance: Appearance) -> Self {
        Self {
            palette: Palette::for_appearance(appearance),
            text_scale: 1.0,
        }
    }

    /// This theme with a different text scale.
    #[must_use]
    pub fn with_text_scale(mut self, scale: f32) -> Self {
        self.text_scale = if scale.is_finite() {
            scale.clamp(0.75, 2.0)
        } else {
            1.0
        };
        self
    }

    /// A type-ramp role resolved at this theme's text scale.
    #[must_use]
    pub fn font(&self, role: Text) -> FontId {
        FontId::new(role.size() * self.text_scale, role.weight().family())
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

// ---------------------------------------------------------------------------
// Contrast (D13)
// ---------------------------------------------------------------------------

/// WCAG 2.1 contrast thresholds.
pub struct Contrast;

impl Contrast {
    /// AA for body text: 4.5:1.
    pub const AA_TEXT: f32 = 4.5;
    /// AA for text at 18 pt or 14 pt bold, and for UI component boundaries: 3:1.
    pub const AA_LARGE: f32 = 3.0;
    /// AAA for body text: 7:1.
    pub const AAA_TEXT: f32 = 7.0;
}

/// Composite a possibly-translucent colour onto an opaque backdrop.
#[must_use]
pub fn flatten_onto(color: Color32, backdrop: Color32) -> Color32 {
    // `Color32` is premultiplied, so the source term needs no further scaling.
    let a = f32::from(color.a()) / 255.0;
    let mix = |fg: u8, bg: u8| {
        (f32::from(fg) + f32::from(bg) * (1.0 - a))
            .round()
            .min(255.0) as u8
    };
    Color32::from_rgb(
        mix(color.r(), backdrop.r()),
        mix(color.g(), backdrop.g()),
        mix(color.b(), backdrop.b()),
    )
}

/// WCAG 2.1 relative luminance of an opaque colour, `0.0..=1.0`.
#[must_use]
pub fn relative_luminance(color: Color32) -> f32 {
    fn channel(v: u8) -> f32 {
        let c = f32::from(v) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
}

/// WCAG 2.1 contrast ratio between two opaque colours, `1.0..=21.0`.
///
/// Flatten translucent tokens with [`Palette::flatten`] first — a ratio
/// computed against a half-transparent colour is meaningless.
#[must_use]
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Register the four embedded Inter cuts as named families.
///
/// Must run before anything draws text in a non-default weight. In an eframe
/// app that means inside `CreationContext`, before frame one.
///
/// # Timing
///
/// `set_fonts` applies on the *next* begin-pass. Drawing text in a custom
/// family on the same pass that installed the fonts panics with
/// `FontFamily::Name(..) is not bound`. A headless driver must therefore
/// install fonts, request a repaint, and draw on a later pass.
pub fn install_fonts(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
}

/// The font definitions [`install_fonts`] installs.
///
/// Exposed so a headless harness can build a context's fonts without a live
/// `Context`, and so tests can assert the families exist.
#[must_use]
pub fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();

    let cuts: [(&str, &'static [u8]); 4] = [
        (
            "inter-regular",
            include_bytes!("../assets/fonts/Inter-Regular.ttf"),
        ),
        (MEDIUM, include_bytes!("../assets/fonts/Inter-Medium.ttf")),
        (
            SEMIBOLD,
            include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
        ),
        (BOLD, include_bytes!("../assets/fonts/Inter-Bold.ttf")),
    ];
    for (key, bytes) in cuts {
        fonts
            .font_data
            .insert(key.to_owned(), Arc::new(FontData::from_static(bytes)));
    }

    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "inter-regular".to_owned());

    for key in [MEDIUM, SEMIBOLD, BOLD] {
        fonts
            .families
            .insert(FontFamily::Name(key.into()), vec![key.to_owned()]);
    }

    // Give the single-weight families the proportional family's fallback tail,
    // so symbol glyphs (⌘ ⇧ ⌥ ⌃) still resolve if a given Inter cut is missing
    // one. Without this a shortcut hint silently renders as tofu in Medium.
    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for key in [MEDIUM, SEMIBOLD, BOLD] {
        if let Some(family) = fonts.families.get_mut(&FontFamily::Name(key.into())) {
            for name in &fallback {
                if !family.contains(name) {
                    family.push(name.clone());
                }
            }
        }
    }

    fonts
}

/// Replace egui's default `Style`/`Visuals` with the token-driven one.
///
/// Panels are transparent because Scrozz's windows are: the OS desktop, or a
/// captured image, is what sits behind. egui's own widget animation is disabled
/// outright — per D19 controls are instant, and per D25 a still render must not
/// depend on which frame it caught.
pub fn install_style(ctx: &egui::Context, theme: &Theme) {
    let theme = *theme;
    ctx.all_styles_mut(move |style| {
        apply_style(style, &theme);
    });
}

/// Applies the token style to one UI subtree.
///
/// Secondary viewports use this to follow their OS appearance without changing
/// the transparent overlay's deliberately dark style.
pub fn apply_style(style: &mut egui::Style, theme: &Theme) {
    let palette = theme.palette;
    // Start from egui's own palette *for this appearance*. Mutating whatever
    // was there before is how a light window ends up wearing dark widget
    // chrome: the fields below are the ones Scrozz has an opinion about, and
    // every one it does not name still has to be right side up.
    style.visuals = if palette.is_dark() {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let visuals = &mut style.visuals;
    visuals.dark_mode = palette.is_dark();
    // Deliberately *not* an override: one colour for every widget state paints
    // a disabled control exactly like an enabled one, and the type ramp then
    // has no way to say "this value is secondary". State lives in `widgets`.
    visuals.override_text_color = None;
    visuals.weak_text_color = Some(palette.text_muted);
    visuals.widgets = widget_visuals(&palette);
    // Panels stay transparent because Scrozz's windows are: the OS desktop, or
    // a captured image, is what sits behind a `CentralPanel`.
    //
    // A *popup* is not a panel. egui builds every menu, dropdown list and
    // tooltip from `Frame::popup`, which fills with `window_fill` — so leaving
    // that transparent meant a combo box's options floated with the desktop
    // showing straight through them, legible only where a row happened to be
    // selected. Nothing in Scrozz uses `egui::Window`, so this affects popups
    // and only popups.
    visuals.panel_fill = Color32::TRANSPARENT;
    visuals.window_fill = palette.flatten(palette.card_fill_raised);
    visuals.window_stroke = egui::Stroke::new(1.0, palette.hairline);
    visuals.window_shadow = Shadow::NONE;
    // A popup floats over content it must be readable against, so it gets the
    // elevation its own token system says a floating surface has.
    visuals.popup_shadow = Elevation::Lifted
        .shadows(&palette)
        .map_or(Shadow::NONE, |(_, key)| key);
    visuals.window_corner_radius = corner(Radius::CARD);
    visuals.menu_corner_radius = corner(Radius::BUTTON);
    // A translucent accent reads as a highlight over text; an opaque one reads
    // as a block that swallows it.
    visuals.selection.bg_fill = palette.accent.gamma_multiply(0.45);
    visuals.selection.stroke = egui::Stroke::new(1.0, palette.text);
    visuals.hyperlink_color = palette.on_accent_wash();
    visuals.warn_fg_color = palette.warning;
    visuals.error_fg_color = palette.recording;
    visuals.faint_bg_color = palette.chip_fill;
    visuals.extreme_bg_color = palette.well();
    visuals.text_edit_bg_color = Some(palette.well());
    visuals.code_bg_color = palette.well();
    visuals.slider_trailing_fill = true;
    visuals.handle_shape = egui::style::HandleShape::Circle;
    visuals.text_cursor.stroke = egui::Stroke::new(1.5, palette.accent);
    visuals.text_cursor.blink = false;

    // Controls are instant (D19); stills are deterministic (D25).
    style.animation_time = 0.0;
    style.spacing.item_spacing = egui::vec2(Space::SM, Space::SM);
    style.spacing.button_padding = egui::vec2(Space::MD, Space::SM);
    // Deliberately not `spacing.interact_size`: that is egui's *minimum* for
    // every widget everywhere, and raising it globally re-flows surfaces that
    // were laid out against the default. Surfaces that want one control
    // height ask for it locally — see the Scene inspector.
    style.spacing.slider_rail_height = 4.0;
    style.spacing.combo_height = 320.0;
    style.text_styles = text_styles(theme);
}

/// The single control height an inspector's rows agree on, in points.
///
/// One number, so a slider, a combo box and a button in the same row line up
/// without anyone measuring. Applied per surface rather than through
/// [`egui::style::Spacing::interact_size`], which is a global floor.
pub const CONTROL_H: f32 = 28.0;

/// egui's five widget states, resolved from the palette.
///
/// `active` doubles as the keyboard-focus appearance — egui picks it for a
/// focused widget as well as a pressed one — so it carries the focus ring
/// rather than a merely darker fill (D13: focus is always visible).
#[must_use]
pub fn widget_visuals(palette: &Palette) -> egui::style::Widgets {
    use egui::Stroke;
    let radius = corner(Radius::BUTTON);
    let text = Stroke::new(1.0, palette.text);
    egui::style::Widgets {
        noninteractive: egui::style::WidgetVisuals {
            bg_fill: palette.well(),
            weak_bg_fill: palette.flatten(palette.card_fill),
            bg_stroke: Stroke::new(1.0, palette.hairline),
            corner_radius: radius,
            fg_stroke: text,
            expansion: 0.0,
        },
        inactive: egui::style::WidgetVisuals {
            // Not the well: `bg_fill` is the slider rail, the checkbox box and
            // the slider handle, and all three have to read against the panel
            // rather than recede into it the way a text field should.
            bg_fill: palette.control_fill(),
            weak_bg_fill: palette.control_fill(),
            bg_stroke: Stroke::new(1.0, palette.hairline),
            corner_radius: radius,
            fg_stroke: text,
            expansion: 0.0,
        },
        hovered: egui::style::WidgetVisuals {
            bg_fill: palette.control_fill_hover(),
            weak_bg_fill: palette.control_fill_hover(),
            bg_stroke: Stroke::new(1.0, palette.divider),
            corner_radius: radius,
            fg_stroke: text,
            expansion: 0.0,
        },
        active: egui::style::WidgetVisuals {
            bg_fill: palette.control_fill_active(),
            weak_bg_fill: palette.control_fill_active(),
            bg_stroke: Stroke::new(2.0, palette.focus_ring),
            corner_radius: radius,
            fg_stroke: text,
            expansion: 0.0,
        },
        open: egui::style::WidgetVisuals {
            bg_fill: palette.control_fill_active(),
            weak_bg_fill: palette.control_fill_active(),
            bg_stroke: Stroke::new(1.0, palette.accent),
            corner_radius: radius,
            fg_stroke: text,
            expansion: 0.0,
        },
    }
}

/// The type ramp mapped onto egui's built-in text styles.
///
/// Stock widgets are used only for trivial text runs, but when they are used
/// they should still be set in the product's type, not egui's default.
#[must_use]
pub fn text_styles(theme: &Theme) -> std::collections::BTreeMap<egui::TextStyle, FontId> {
    use egui::TextStyle;
    [
        (TextStyle::Heading, theme.font(Text::Title)),
        (TextStyle::Body, theme.font(Text::Body)),
        (TextStyle::Button, theme.font(Text::Button)),
        (TextStyle::Small, theme.font(Text::Caption)),
        (TextStyle::Monospace, theme.font(Text::Body)),
    ]
    .into_iter()
    .collect()
}
