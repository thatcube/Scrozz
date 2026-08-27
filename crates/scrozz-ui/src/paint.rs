//! Drawing primitives and controls, built from the token layer.
//!
//! Almost every pixel Scrozz shows is drawn with [`egui::Painter`] rather than
//! by a stock widget. That is the central finding of the UI spike: egui is an
//! excellent immediate-mode *canvas* and a plain widget kit, and its default
//! widgets are exactly what reads as a debug tool. Everything here — elevation,
//! glass, pills, menu rows, shortcut glyphs — is bespoke and driven entirely by
//! [`crate::theme`].
//!
//! # Two kinds of thing live here
//!
//! **Primitives** take an [`egui::Painter`] and draw. They are pure functions of
//! their arguments, do no hit-testing, and never read a clock.
//!
//! **Controls** take a [`egui::Ui`], hit-test, and return an [`egui::Response`].
//! Per decision D19 they contain **no animation whatsoever**: hover and press
//! flip instantly. This is a deliberate reversal of an earlier pass that eased
//! them. An instant state change reads as *responsive*; even a 140 ms hover fade
//! reads as the control lagging behind the pointer. Motion is reserved for
//! objects that move through space — cards — and controls just answer.
//!
//! # Working around egui
//!
//! Three gaps shape most of this module:
//!
//! * **No gradient primitive.** A gradient is a stack of translucent shapes;
//!   see [`bottom_scrim`] and [`soft_blob`].
//! * **No rotation.** `TSTransform` is translate-and-scale only, so a rotated
//!   card is built point-by-point through [`rounded_poly`] and [`rotate_pts`].
//!   Rotated *text* is not achievable at all without render-to-texture, so no
//!   design here relies on it.
//! * **One shadow, and it is flat.** Real elevation is a *pair* of shadows; see
//!   [`soft_shadow`] and [`crate::theme::Elevation`].
//!
//! # Determinism (D25)
//!
//! Nothing here reads a clock or ambient state. Time and preferences arrive
//! inside [`Surface`], and live pointer hover is suppressed by
//! [`Surface::still`] so a golden render does not depend on where the user's
//! mouse happens to be sitting.

use crate::icons::{Icon, IconStore};
use crate::motion::{Motion, fade, lerp_color};
use crate::theme::{Palette, Radius, Space, Text, Theme, corner, corner_bottom, shadows_for_lift};
use egui::{
    Align2, Color32, Id, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2,
    WidgetInfo, WidgetType, epaint::Shadow, pos2, vec2,
};

// ---------------------------------------------------------------------------
// Drawing context
// ---------------------------------------------------------------------------

/// Everything a surface needs in order to draw, passed by value down the tree.
///
/// This replaces the spike's process-wide "screenshot mode" flag. A global made
/// two independent renders impossible to run in the same process, which is
/// precisely what a parallel test suite does; carrying the state explicitly
/// costs one borrow and removes the whole class of problem.
#[derive(Clone, Copy)]
pub struct Surface<'a> {
    /// Colours and type.
    pub theme: &'a Theme,
    /// Uploaded icon textures.
    pub icons: &'a IconStore,
    /// The instant being drawn, and the user's motion preferences.
    pub motion: Motion,
    /// Whether live pointer state may affect what is drawn.
    ///
    /// `false` for a still render: hover and press are ignored so the image
    /// depends only on the scenario and the instant, never on the mouse.
    pub interactive: bool,
}

impl<'a> Surface<'a> {
    /// A live, interactive surface.
    #[must_use]
    pub fn new(theme: &'a Theme, icons: &'a IconStore, motion: Motion) -> Self {
        Self {
            theme,
            icons,
            motion,
            interactive: true,
        }
    }

    /// A surface for a deterministic still: pointer state is ignored and every
    /// duration collapses to zero, so the frame drawn is the animation's
    /// settled state at [`Motion::now`].
    ///
    /// Note this does *not* mean "no animation": a still at t = 180 ms of a card
    /// entry still shows the card part-way in. It means nothing outside the
    /// scenario can perturb the image.
    #[must_use]
    pub fn still(theme: &'a Theme, icons: &'a IconStore, motion: Motion) -> Self {
        Self {
            theme,
            icons,
            motion,
            interactive: false,
        }
    }

    /// The resolved colours.
    #[must_use]
    pub fn palette(&self) -> &Palette {
        &self.theme.palette
    }

    /// A type-ramp role at this surface's text scale.
    #[must_use]
    pub fn font(&self, role: Text) -> egui::FontId {
        self.theme.font(role)
    }

    /// This surface at a different instant, for drawing one element ahead of or
    /// behind the rest (a stagger step, say).
    #[must_use]
    pub fn at(mut self, motion: Motion) -> Self {
        self.motion = motion;
        self
    }
}

// ---------------------------------------------------------------------------
// Elevation and glass
// ---------------------------------------------------------------------------

/// A real soft drop shadow: an ambient contact shadow plus a wide key shadow.
///
/// egui's single built-in shadow cannot express this, and one shadow is the
/// difference between "has a shadow" and "is floating". `lift` is continuous so
/// a card being picked up can interpolate its elevation rather than step.
pub fn soft_shadow(painter: &egui::Painter, rect: Rect, radius: f32, palette: &Palette, lift: f32) {
    if lift <= 0.0 {
        return;
    }
    let (ambient, key) = shadows_for_lift(lift, palette);
    let cr = corner(radius);
    painter.add(ambient.as_shape(rect, cr));
    painter.add(key.as_shape(rect, cr));
}

/// A bottom-up dark scrim, for caption legibility over arbitrary content.
///
/// egui has no gradient, so this stacks 16 faint rounded-bottom rectangles. The
/// overlap deepens toward the bottom and the falloff is smooth with no visible
/// banding — an honest workaround, not a placeholder.
pub fn bottom_scrim(painter: &egui::Painter, area: Rect, height: f32, radius: f32, peak: u8) {
    const STEPS: u32 = 16;
    #[allow(clippy::cast_precision_loss)]
    let per = (f32::from(peak) / STEPS as f32).ceil().max(1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let per = per as u8;
    let cr = corner_bottom(radius);
    for step in 1..=STEPS {
        #[allow(clippy::cast_precision_loss)]
        let t = step as f32 / STEPS as f32;
        let rect = Rect::from_min_max(pos2(area.left(), area.bottom() - height * t), area.max);
        painter.rect_filled(rect, cr, Color32::from_black_alpha(per));
    }
}

/// A glass panel: elevation, translucent fill, hairline border, inner lighting.
///
/// When the palette reports it is sitting over a real OS material
/// ([`Palette::over_material`]) the fill is dialled *up* toward opaque and the
/// elevation reduced, because the material behind is already doing the frosting
/// and doubling the two reads as murk. With no material — which is the case on
/// every platform today, see [`crate::vibrancy`] — the panel carries the whole
/// effect itself.
pub fn glass_panel(
    painter: &egui::Painter,
    rect: Rect,
    radius: f32,
    palette: &Palette,
    raised: bool,
) {
    let base = if raised {
        palette.card_fill_raised
    } else {
        palette.card_fill
    };
    let (lift, fill) = if palette.over_material {
        let a = if palette.is_dark() { 232 } else { 236 };
        (
            0.9,
            Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), a),
        )
    } else {
        (1.0, base)
    };

    soft_shadow(painter, rect, radius, palette, lift);
    painter.rect_filled(rect, corner(radius), fill);
    inner_glass_lighting(painter, rect, radius, palette);
    painter.rect_stroke(
        rect,
        corner(radius),
        Stroke::new(1.0, palette.hairline),
        StrokeKind::Inside,
    );
}

/// The top highlight and bottom shade that sell "lit from above".
///
/// Drawn *inside* the panel and inset by most of the corner radius, so the
/// lines stop where the curve begins rather than cutting across it.
pub fn inner_glass_lighting(painter: &egui::Painter, rect: Rect, radius: f32, palette: &Palette) {
    let inset = radius * 0.72;
    let (l, r) = (rect.left() + inset, rect.right() - inset);
    if r <= l {
        return;
    }
    let top = rect.top() + 1.0;
    painter.line_segment(
        [pos2(l, top), pos2(r, top)],
        Stroke::new(1.0, palette.top_highlight),
    );
    let bottom = rect.bottom() - 1.0;
    painter.line_segment(
        [pos2(l, bottom), pos2(r, bottom)],
        Stroke::new(1.0, palette.bottom_shade),
    );
}

/// A vertical separator.
pub fn divider_v(painter: &egui::Painter, x: f32, y0: f32, y1: f32, palette: &Palette) {
    painter.line_segment(
        [pos2(x, y0), pos2(x, y1)],
        Stroke::new(1.0, palette.divider),
    );
}

/// A horizontal separator.
pub fn divider_h(painter: &egui::Painter, x0: f32, x1: f32, y: f32, palette: &Palette) {
    painter.line_segment(
        [pos2(x0, y), pos2(x1, y)],
        Stroke::new(1.0, palette.divider),
    );
}

/// A soft radial falloff, built from concentric circles.
///
/// The other half of the no-gradient workaround. Useful for glows and for the
/// generated backdrops a headless render needs.
pub fn soft_blob(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32, peak: u8) {
    const RINGS: u32 = 26;
    let peak = f32::from(peak);
    for ring in 0..RINGS {
        #[allow(clippy::cast_precision_loss)]
        let t = ring as f32 / RINGS as f32;
        let a = (peak / RINGS as f32 * (1.0 - t) * 1.6).min(peak);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let a = a as u8;
        painter.circle_filled(
            center,
            radius * (1.0 - t),
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a),
        );
    }
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// The non-pointer state of a control.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControlState {
    /// Drawn in its selected, accent-filled treatment.
    pub selected: bool,
    /// Drawn hovered regardless of the pointer.
    ///
    /// For specimen and documentation renders, where every state must appear at
    /// once and no pointer exists.
    pub force_hover: bool,
    /// Whether the control accepts input.
    ///
    /// Reported to assistive technology (D13) as well as drawn, so a disabled
    /// control is disabled in every sense and not merely dimmed.
    pub enabled: bool,
}

impl ControlState {
    /// Enabled, unselected, not forced.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            selected: false,
            force_hover: false,
            enabled: true,
        }
    }

    /// Enabled and selected.
    #[must_use]
    pub const fn on() -> Self {
        Self {
            selected: true,
            force_hover: false,
            enabled: true,
        }
    }

    /// Enabled and drawn as hovered.
    #[must_use]
    pub const fn hovered() -> Self {
        Self {
            selected: false,
            force_hover: true,
            enabled: true,
        }
    }

    /// Disabled.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            selected: false,
            force_hover: false,
            enabled: false,
        }
    }

    /// This state with `selected` set.
    #[must_use]
    pub const fn selected(mut self, yes: bool) -> Self {
        self.selected = yes;
        self
    }
}

/// How much of a piece of revealed chrome is showing, and where it sits.
///
/// The card's hover-reveal choreography animates; the buttons inside it do not
/// (D19). Passing the animation *through* the control like this keeps that
/// distinction explicit: the control never computes motion, it is merely drawn
/// at a position and opacity the card chose.
///
/// The offset is applied to drawing only, never to the hit rectangle — a target
/// that slides out from under the pointer mid-animation is unclickable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reveal {
    /// `0.0` hidden, `1.0` fully shown.
    pub opacity: f32,
    /// A draw-only translation.
    pub offset: Vec2,
}

impl Reveal {
    /// Fully shown, not offset.
    pub const SHOWN: Self = Self {
        opacity: 1.0,
        offset: Vec2::ZERO,
    };

    /// A partial reveal.
    #[must_use]
    pub fn new(opacity: f32, offset: Vec2) -> Self {
        Self {
            opacity: opacity.clamp(0.0, 1.0),
            offset,
        }
    }

    /// Whether the chrome is settled enough to accept a click.
    ///
    /// Mid-fade chrome is deliberately inert: a control that is 30 % visible is
    /// not something the user has decided to press.
    #[must_use]
    pub fn is_live(self) -> bool {
        self.opacity > 0.85
    }
}

impl Default for Reveal {
    fn default() -> Self {
        Self::SHOWN
    }
}

/// The pointer state a control should draw, having accounted for stills,
/// reveal and forcing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Pointer {
    hovered: bool,
    pressed: bool,
    focused: bool,
}

fn pointer_state(
    surface: &Surface<'_>,
    response: &Response,
    state: ControlState,
    reveal: Reveal,
) -> Pointer {
    let live = state.enabled && reveal.is_live() && surface.interactive;
    Pointer {
        hovered: (live && response.hovered()) || (state.force_hover && state.enabled),
        pressed: live && response.is_pointer_button_down_on(),
        // Focus is *not* gated on `interactive`: a still that documents the
        // focus ring must be able to show it (D13).
        focused: state.enabled && response.has_focus(),
    }
}

/// Draw the keyboard focus ring around a control.
///
/// Focus is always visible (D13). It is drawn *outside* the control's own
/// rectangle so it never competes with the selected treatment, and a control
/// can be simultaneously selected and focused without either becoming unclear.
pub fn focus_ring(painter: &egui::Painter, rect: Rect, radius: f32, palette: &Palette) {
    let ring = rect.expand(2.5);
    painter.rect_stroke(
        ring,
        corner(radius + 2.5),
        Stroke::new(2.0, palette.focus_ring),
        StrokeKind::Inside,
    );
}

/// A round-rect icon button.
///
/// Muted by default, brightens on hover, accent-filled when selected. `id` must
/// be stable across frames and independent of position: an id derived from the
/// rectangle changes the moment the button moves, which drops focus and breaks
/// press tracking during any card animation.
#[allow(
    clippy::too_many_arguments,
    reason = "an immediate-mode painter needs UI, surface, frame-stable identity, content, state, and reveal explicitly; bundling them would hide the accessibility and animation contracts"
)]
pub fn icon_button(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    id: Id,
    icon: Icon,
    label: &str,
    state: ControlState,
    reveal: Reveal,
) -> Response {
    let response = ui.interact(rect, id, sense_for(state));
    response.widget_info(|| {
        WidgetInfo::selected(WidgetType::Button, state.enabled, state.selected, label)
    });

    let p = pointer_state(surface, &response, state, reveal);
    let palette = surface.palette();
    let rect = rect.translate(reveal.offset);
    let painter = ui.painter();
    let opacity = if state.enabled {
        reveal.opacity
    } else {
        reveal.opacity * 0.4
    };
    let cr = corner(Radius::BUTTON);

    if p.focused {
        focus_ring(painter, rect, Radius::BUTTON, palette);
    }

    if state.selected {
        soft_shadow(
            painter,
            rect.shrink(1.0),
            Radius::BUTTON,
            palette,
            0.5 * opacity,
        );
        painter.rect_filled(rect, cr, fade(palette.accent, opacity));
        // A faint top-half wash: the closest egui gets to a lit fill.
        painter.rect_filled(
            Rect::from_min_max(rect.left_top(), pos2(rect.right(), rect.center().y)),
            cr,
            fade(palette.accent_hi.linear_multiply(0.10), opacity),
        );
    } else if p.pressed {
        painter.rect_filled(rect, cr, fade(palette.active, opacity));
    } else if p.hovered {
        painter.rect_filled(rect, cr, fade(palette.hover, opacity));
    }

    let tint = if state.selected {
        palette.on_accent
    } else if p.hovered {
        palette.text
    } else {
        palette.text_muted
    };
    surface.icons.draw(
        painter,
        icon,
        rect.center(),
        crate::icons::SIZE,
        fade(tint, opacity),
    );
    response
}

/// A labelled pill button — the primary affordance in revealed card chrome.
///
/// `accent` picks the filled treatment. There is exactly one accent pill in any
/// group; everything else is the quiet variant.
#[allow(
    clippy::too_many_arguments,
    reason = "an immediate-mode painter needs UI, surface, frame-stable identity, content, treatment, and reveal explicitly; a bag-of-options struct would weaken the call site"
)]
pub fn pill_button(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    id: Id,
    icon: Icon,
    label: &str,
    accent: bool,
    reveal: Reveal,
) -> Response {
    pill_button_with_state(
        ui,
        surface,
        rect,
        id,
        icon,
        label,
        accent,
        ControlState::new(),
        reveal,
    )
}

/// [`pill_button`] with an explicit [`ControlState`].
#[allow(clippy::too_many_arguments)]
pub fn pill_button_with_state(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    id: Id,
    icon: Icon,
    label: &str,
    accent: bool,
    state: ControlState,
    reveal: Reveal,
) -> Response {
    let response = ui.interact(rect, id, sense_for(state));
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, state.enabled, label));

    let p = pointer_state(surface, &response, state, reveal);
    let palette = surface.palette();
    let rect = rect.translate(reveal.offset);
    let radius = Radius::pill(rect.height());
    let opacity = if state.enabled {
        reveal.opacity
    } else {
        reveal.opacity * 0.4
    };
    let painter = ui.painter();

    let h = f32::from(u8::from(p.hovered));
    let d = f32::from(u8::from(p.pressed));

    let base = if accent {
        palette.accent
    } else {
        palette.card_fill_raised
    };
    // Hover lifts the fill toward the highlight; press pushes it past it.
    let fill = if accent {
        lerp_color(
            lerp_color(base, palette.accent_hi, h),
            palette.accent_press,
            d,
        )
    } else {
        lerp_color(base, palette.hover, h.mul_add(0.9, d * 0.5))
    };

    if p.focused {
        focus_ring(painter, rect, radius, palette);
    }

    soft_shadow(
        painter,
        rect,
        radius,
        palette,
        h.mul_add(0.35, 0.55 - 0.3 * d) * opacity,
    );
    painter.rect_filled(rect, corner(radius), fade(fill, opacity));
    painter.rect_filled(
        Rect::from_min_max(rect.left_top(), pos2(rect.right(), rect.center().y)),
        corner(radius),
        fade(
            Color32::from_white_alpha(if accent { 26 } else { 16 }),
            opacity,
        ),
    );
    painter.rect_stroke(
        rect,
        corner(radius),
        Stroke::new(
            1.0,
            fade(
                Color32::from_white_alpha(if accent { 34 } else { 22 }),
                opacity,
            ),
        ),
        StrokeKind::Inside,
    );

    let fg = fade(
        if accent {
            palette.on_accent
        } else {
            palette.text
        },
        opacity,
    );
    let galley = painter.layout_no_wrap(label.to_owned(), surface.font(Text::Button), fg);
    let icon_w = 15.0;
    let total = icon_w + Space::SM - 2.0 + galley.size().x;
    let x0 = rect.center().x - total / 2.0;
    surface.icons.draw(
        painter,
        icon,
        pos2(x0 + icon_w / 2.0, rect.center().y),
        icon_w,
        fg,
    );
    painter.galley(
        pos2(
            x0 + icon_w + Space::SM - 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        fg,
    );
    response
}

/// A colour swatch, for the annotation palette.
pub fn color_swatch(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    id: Id,
    color: Color32,
    label: &str,
    selected: bool,
) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::selected(WidgetType::RadioButton, true, selected, label));

    let palette = surface.palette();
    let painter = ui.painter();
    let cr = corner(Radius::CHIP);
    painter.rect_filled(rect, cr, color);
    painter.rect_stroke(
        rect,
        cr,
        Stroke::new(1.0, palette.hairline),
        StrokeKind::Inside,
    );

    // Selection is a ring, never colour alone — the swatch *is* a colour, so
    // colour cannot also be the state indicator (D13).
    if selected {
        painter.rect_stroke(
            rect.expand(3.0),
            corner(Radius::CHIP + 3.0),
            Stroke::new(2.0, palette.accent),
            StrokeKind::Inside,
        );
    }
    if response.has_focus() {
        focus_ring(painter, rect.expand(3.0), Radius::CHIP + 3.0, palette);
    }
    response
}

/// A stroke-width control: a wedge that thickens left to right, with a knob.
///
/// Entirely bespoke — egui has nothing resembling it — and a good example of
/// why a canvas is the right foundation.
pub fn stroke_width(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    id: Id,
    fraction: f32,
) -> Response {
    let response = ui.interact(rect, id, Sense::click_and_drag());
    let fraction = fraction.clamp(0.0, 1.0);
    response.widget_info(|| WidgetInfo::slider(true, f64::from(fraction), "Stroke width"));

    let palette = surface.palette();
    let painter = ui.painter();
    painter.rect_filled(rect, corner(Radius::BUTTON), palette.chip_fill);

    let track_l = rect.left() + Space::MD;
    let track_r = rect.right() - Space::MD;
    let cy = rect.center().y;
    painter.add(Shape::convex_polygon(
        vec![
            pos2(track_l, cy - 0.6),
            pos2(track_r, cy - 4.2),
            pos2(track_r, cy + 4.2),
            pos2(track_l, cy + 0.6),
        ],
        palette.text_faint,
        Stroke::NONE,
    ));

    let knob = pos2(track_l + (track_r - track_l) * fraction, cy);
    if response.has_focus() {
        focus_ring(painter, rect, Radius::BUTTON, palette);
    }
    painter.circle_filled(knob, 6.5, palette.text);
    painter.circle_stroke(knob, 6.5, Stroke::new(1.0, palette.bottom_shade));
    response
}

fn sense_for(state: ControlState) -> Sense {
    if state.enabled {
        Sense::click()
    } else {
        Sense::hover()
    }
}

// ---------------------------------------------------------------------------
// Menus and shortcuts
// ---------------------------------------------------------------------------

/// A keyboard modifier, rendered as its platform glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mod {
    /// Command (macOS) / Super.
    Cmd,
    /// Shift.
    Shift,
    /// Option (macOS) / Alt.
    Opt,
    /// Control.
    Ctrl,
}

impl Mod {
    /// The glyph shown in a menu.
    ///
    /// On macOS these are the standard symbols. Elsewhere they are spelled
    /// words, because ⌘ on Windows means nothing and ⌥ is not Alt.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        if cfg!(target_os = "macos") {
            match self {
                Self::Ctrl => "\u{2303}",
                Self::Opt => "\u{2325}",
                Self::Shift => "\u{21E7}",
                Self::Cmd => "\u{2318}",
            }
        } else {
            match self {
                Self::Ctrl => "Ctrl+",
                Self::Opt => "Alt+",
                Self::Shift => "Shift+",
                Self::Cmd => "Win+",
            }
        }
    }

    /// The word an assistive technology should speak (D13).
    ///
    /// A screen reader announcing "⌘" is useless; it must say "Command".
    #[must_use]
    pub const fn spoken(self) -> &'static str {
        match self {
            Self::Ctrl => "Control",
            Self::Opt if cfg!(target_os = "macos") => "Option",
            Self::Opt => "Alt",
            Self::Shift => "Shift",
            Self::Cmd if cfg!(target_os = "macos") => "Command",
            Self::Cmd => "Windows",
        }
    }
}

/// A keyboard shortcut shown beside a menu item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shortcut {
    /// Modifiers, in the platform's conventional order.
    pub mods: &'static [Mod],
    /// The key itself, already in display form ("4", "↩").
    pub key: &'static str,
}

impl Shortcut {
    /// The glyph run drawn in a menu, e.g. `⇧⌘4`.
    #[must_use]
    pub fn glyphs(&self) -> String {
        let mut s = String::new();
        for m in self.mods {
            s.push_str(m.glyph());
        }
        s.push_str(self.key);
        s
    }

    /// The spoken form, e.g. `Shift Command 4` (D13).
    #[must_use]
    pub fn spoken(&self) -> String {
        let mut parts: Vec<&str> = self.mods.iter().map(|m| m.spoken()).collect();
        parts.push(self.key);
        parts.join(" ")
    }
}

/// A menu row: leading icon, label, right-aligned shortcut.
///
/// A hovered row fills with the accent and flips its content to white, which is
/// native macOS behaviour and a good test of optical alignment. The highlight is
/// instant — a menu highlight that ramps reads as lag while the pointer sweeps a
/// list, and it is the clearest case for D19.
#[allow(
    clippy::too_many_arguments,
    reason = "the row's geometry, stable identity, accessible content, optional shortcut, and control state are separate immediate-mode inputs"
)]
pub fn menu_row(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    id: Id,
    icon: Icon,
    label: &str,
    shortcut: Option<&Shortcut>,
    state: ControlState,
) -> Response {
    let response = ui.interact(rect, id, sense_for(state));
    response.widget_info(|| {
        let described = shortcut.map_or_else(
            || label.to_owned(),
            |sc| format!("{label}, {}", sc.spoken()),
        );
        WidgetInfo::labeled(WidgetType::Button, state.enabled, described)
    });

    let p = pointer_state(surface, &response, state, Reveal::SHOWN);
    let palette = surface.palette();
    let painter = ui.painter();
    let highlight = f32::from(u8::from(p.hovered));

    if p.hovered {
        painter.rect_filled(
            rect.shrink2(vec2(Space::SM - 2.0, 0.0)),
            corner(Radius::BUTTON),
            palette.accent,
        );
    }
    if p.focused && !p.hovered {
        focus_ring(
            painter,
            rect.shrink2(vec2(Space::SM - 2.0, 0.0)),
            Radius::BUTTON,
            palette,
        );
    }

    let dim = if state.enabled { 1.0 } else { 0.4 };
    let content = fade(lerp_color(palette.text, palette.on_accent, highlight), dim);
    let icon_tint = fade(
        lerp_color(palette.text_muted, palette.on_accent, highlight),
        dim,
    );

    let icon_cx = rect.left() + 6.0 + 15.0 + 9.0;
    surface.icons.draw(
        painter,
        icon,
        pos2(icon_cx, rect.center().y),
        17.0,
        icon_tint,
    );
    painter.text(
        pos2(icon_cx + 15.0 + Space::MD, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        surface.font(Text::Label),
        content,
    );

    if let Some(sc) = shortcut {
        draw_shortcut(
            painter,
            surface,
            rect.right() - Space::LG,
            rect.center().y,
            sc,
            p.hovered,
        );
    }
    response
}

/// Draw a right-aligned shortcut such as `⇧⌘4`.
///
/// Modifier symbols are real font glyphs — Inter ships ⌘⇧⌥⌃ — exactly as native
/// macOS menus do, and far crisper than stroking them by hand at 12 pt.
pub fn draw_shortcut(
    painter: &egui::Painter,
    surface: &Surface<'_>,
    right_x: f32,
    center_y: f32,
    shortcut: &Shortcut,
    on_accent: bool,
) {
    let palette = surface.palette();
    painter.text(
        pos2(right_x, center_y),
        Align2::RIGHT_CENTER,
        shortcut.glyphs(),
        surface.font(Text::Shortcut),
        if on_accent {
            palette.on_accent
        } else {
            palette.text_faint
        },
    );
}

// ---------------------------------------------------------------------------
// Labels, badges and captions
// ---------------------------------------------------------------------------

/// Where a badge takes its colour from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeTone {
    /// The product accent — a count, a neutral quantity.
    Accent,
    /// Attention red — the number of items being dragged.
    Alert,
}

/// A small circular count badge, notification-style.
///
/// Generalised from the spike's two near-identical copies. The number is always
/// drawn as text as well as encoded by the badge itself, so the meaning never
/// rests on colour (D13).
pub fn badge(
    painter: &egui::Painter,
    surface: &Surface<'_>,
    center: Pos2,
    count: u32,
    tone: BadgeTone,
) {
    let palette = surface.palette();
    let radius = 11.5;
    let (fill, ring, text) = match tone {
        BadgeTone::Accent => (
            palette.accent,
            Color32::from_white_alpha(40),
            palette.on_accent,
        ),
        BadgeTone::Alert => (
            Color32::from_rgb(0xF2, 0x45, 0x3D),
            Color32::WHITE,
            Color32::WHITE,
        ),
    };
    let shadow = Shadow {
        offset: [0, 2],
        blur: 8,
        spread: 0,
        color: palette.key_shadow,
    };
    painter.add(shadow.as_shape(
        Rect::from_center_size(center, Vec2::splat(radius * 2.0)),
        corner(radius),
    ));
    painter.circle_filled(center, radius, fill);
    painter.circle_stroke(center, radius, Stroke::new(1.0, ring));
    painter.text(
        center + vec2(0.0, 0.5),
        Align2::CENTER_CENTER,
        count.to_string(),
        surface.font(Text::Caption),
        text,
    );
}

/// A floating accent-filled label chip, for hints such as "Drop to send".
pub fn chip_label(painter: &egui::Painter, surface: &Surface<'_>, center: Pos2, text: &str) {
    let palette = surface.palette();
    let galley = painter.layout_no_wrap(
        text.to_owned(),
        surface.font(Text::Caption),
        palette.on_accent,
    );
    let pad = vec2(11.0, 6.0);
    let rect = Rect::from_center_size(center, galley.size() + pad * 2.0);
    let radius = Radius::pill(rect.height());
    let shadow = Shadow {
        offset: [0, 3],
        blur: 12,
        spread: 0,
        color: palette.key_shadow,
    };
    painter.add(shadow.as_shape(rect, corner(radius)));
    painter.rect_filled(rect, corner(radius), palette.accent);
    painter.galley(rect.min + pad, galley, palette.on_accent);
}

/// A caption strip over the bottom of a capture: name on the left, a secondary
/// detail on the right, over a scrim that keeps both legible against arbitrary
/// image content.
pub fn caption_strip(
    painter: &egui::Painter,
    surface: &Surface<'_>,
    rect: Rect,
    radius: f32,
    primary: &str,
    secondary: &str,
) {
    bottom_scrim(painter, rect, 58.0, radius, 205);
    let cy = rect.bottom() - 15.0;
    painter.text(
        pos2(rect.left() + 13.0, cy),
        Align2::LEFT_CENTER,
        primary,
        surface.font(Text::Label),
        Color32::from_rgba_unmultiplied(255, 255, 255, 236),
    );
    painter.text(
        pos2(rect.right() - 13.0, cy),
        Align2::RIGHT_CENTER,
        secondary,
        surface.font(Text::Caption),
        Color32::from_rgba_unmultiplied(255, 255, 255, 150),
    );
}

// ---------------------------------------------------------------------------
// Rotation
// ---------------------------------------------------------------------------

/// Approximate a rounded rectangle as a convex polygon.
///
/// The only way to rotate one: egui's `TSTransform` is translate-and-scale, and
/// `rect_filled` takes an axis-aligned `Rect`. Five segments per corner is
/// indistinguishable from a true arc at card sizes.
///
/// Note the corresponding limits: a rotated card **cannot be clipped** (egui's
/// clip rectangles are axis-aligned), and rotated **text is not possible at
/// all**. A swipe therefore rotates the card's shape and fades its contents,
/// rather than rotating the contents with it.
#[must_use]
pub fn rounded_poly(rect: Rect, radius: f32) -> Vec<Pos2> {
    const SEGMENTS: u32 = 5;
    let r = radius
        .min(rect.width() * 0.5)
        .min(rect.height() * 0.5)
        .max(0.0);
    let mut pts = Vec::with_capacity(SEGMENTS as usize * 4 + 4);
    let corners = [
        (pos2(rect.right() - r, rect.bottom() - r), 0.0_f32),
        (pos2(rect.left() + r, rect.bottom() - r), 90.0),
        (pos2(rect.left() + r, rect.top() + r), 180.0),
        (pos2(rect.right() - r, rect.top() + r), 270.0),
    ];
    for (c, a0) in corners {
        for i in 0..=SEGMENTS {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32 / SEGMENTS as f32;
            let (sin, cos) = (a0 + 90.0 * t).to_radians().sin_cos();
            pts.push(pos2(r.mul_add(cos, c.x), r.mul_add(sin, c.y)));
        }
    }
    pts
}

/// Rotate points about a centre, in radians.
pub fn rotate_pts(pts: &mut [Pos2], center: Pos2, radians: f32) {
    let (sin, cos) = radians.sin_cos();
    for p in pts.iter_mut() {
        let (dx, dy) = (p.x - center.x, p.y - center.y);
        *p = pos2(
            center.x + dx.mul_add(cos, -(dy * sin)),
            center.y + dx.mul_add(sin, dy * cos),
        );
    }
}

/// Fill a rotated rectangle.
pub fn fill_rot_rect(
    painter: &egui::Painter,
    rect: Rect,
    pivot: Pos2,
    radians: f32,
    color: Color32,
) {
    let mut pts = vec![
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];
    rotate_pts(&mut pts, pivot, radians);
    painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// Fill a rotated *rounded* rectangle.
pub fn fill_rot_round_rect(
    painter: &egui::Painter,
    rect: Rect,
    radius: f32,
    pivot: Pos2,
    radians: f32,
    color: Color32,
) {
    let mut pts = rounded_poly(rect, radius);
    rotate_pts(&mut pts, pivot, radians);
    painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// A faint rotated card *outline* — an echo of a moving card's earlier
/// position, so a swipe reads as motion even in a single still frame (D25).
pub fn ghost_card(painter: &egui::Painter, rect: Rect, radius: f32, radians: f32, alpha: u8) {
    let mut poly = rounded_poly(rect, radius);
    rotate_pts(&mut poly, rect.center(), radians);
    painter.add(Shape::convex_polygon(
        poly,
        Color32::TRANSPARENT,
        Stroke::new(1.5, Color32::from_white_alpha(alpha)),
    ));
}

/// Tapered streaks trailing a moving object, for the same reason as
/// [`ghost_card`].
pub fn motion_streaks(
    painter: &egui::Painter,
    from: Pos2,
    direction: Vec2,
    count: usize,
    spread: f32,
) {
    if count == 0 || direction == Vec2::ZERO {
        return;
    }
    let dir = direction.normalized();
    let perp = vec2(-dir.y, dir.x);
    for i in 0..count {
        #[allow(clippy::cast_precision_loss)]
        let off = (i as f32 - (count as f32 - 1.0) / 2.0) * spread;
        let base = from + perp * off;
        let len = off.abs().mul_add(-0.25, 26.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let a = off.abs().mul_add(-1.2, 70.0).clamp(20.0, 70.0) as u8;
        painter.line_segment(
            [base, base - dir * len],
            Stroke::new(2.4, Color32::from_white_alpha(a)),
        );
    }
}

/// A dashed polyline.
///
/// egui strokes are solid, so a marching-ants selection edge or a drag guide has
/// to be built segment by segment.
pub fn dashed_path(painter: &egui::Painter, pts: &[Pos2], stroke: Stroke, dash: f32, gap: f32) {
    let step = (dash + gap).max(0.01);
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let len = (b - a).length();
        if len <= 0.01 {
            continue;
        }
        let dir = (b - a) / len;
        let mut t = 0.0;
        while t < len {
            painter.line_segment([a + dir * t, a + dir * (t + dash).min(len)], stroke);
            t += step;
        }
    }
}
