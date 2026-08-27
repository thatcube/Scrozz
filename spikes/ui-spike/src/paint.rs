//! Hand-drawn primitives + widgets.
//!
//! This is where the spike earns its answer: almost every pixel here is drawn
//! with `egui::Painter` rather than a stock widget, because stock egui widgets
//! are exactly what the maintainer thinks looks like a debug tool. Cards,
//! shadows, the wallpaper, the faux screenshot, the ⌘⇧⌥ shortcut glyphs and the
//! buttons are all bespoke, driven entirely by the tokens in `theme`.

use crate::icons::IconStore;
use crate::motion;
use crate::theme::{self, cr, Palette};
use egui::{
    epaint::Shadow, pos2, vec2, Align2, Color32, Pos2, Rect, Response, Sense, Shape, Stroke,
    StrokeKind, Ui, Vec2,
};
use std::sync::atomic::{AtomicBool, Ordering};

/// When true, live pointer hover is ignored so `--shot` renders are
/// deterministic regardless of where the real mouse happens to sit.
static SCREENSHOT: AtomicBool = AtomicBool::new(false);
pub fn set_screenshot(on: bool) {
    SCREENSHOT.store(on, Ordering::Relaxed);
    // A deterministic still has no time axis, so every duration collapses too —
    // otherwise the snapshot baseline would depend on which frame we grabbed.
    motion::set_reduce(on);
}
pub fn shot_mode() -> bool {
    SCREENSHOT.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Elevation / glass
// ---------------------------------------------------------------------------

/// Two stacked shadows (key + ambient) approximate a real soft drop shadow far
/// better than egui's single built-in shadow.
pub fn soft_shadow(painter: &egui::Painter, rect: Rect, radius: f32, pal: &Palette, lift: f32) {
    let key = Shadow {
        offset: [0, (16.0 * lift).round() as i8],
        blur: (44.0 * lift).round().clamp(0.0, 255.0) as u8,
        spread: 0,
        color: pal.key_shadow,
    };
    let ambient = Shadow {
        offset: [0, (3.0 * lift).round() as i8],
        blur: (10.0 * lift).round().clamp(0.0, 255.0) as u8,
        spread: 0,
        color: pal.ambient_shadow,
    };
    painter.add(ambient.as_shape(rect, cr(radius)));
    painter.add(key.as_shape(rect, cr(radius)));
}

/// A smooth bottom-up dark scrim for caption legibility over a thumbnail.
/// egui has no gradient primitive, so we stack many faint rounded-bottom rects;
/// the overlap deepens toward the bottom and yields a smooth falloff with no
/// visible banding.
pub fn bottom_scrim(painter: &egui::Painter, area: Rect, height: f32, radius: f32, max_alpha: u8) {
    let steps = 16;
    let r = radius as u8;
    let bottom_only = egui::CornerRadius { nw: 0, ne: 0, sw: r, se: r };
    let per = ((max_alpha as f32) / steps as f32).ceil().max(1.0) as u8;
    for k in 1..=steps {
        let t = k as f32 / steps as f32;
        let top = area.bottom() - height * t;
        let rect = Rect::from_min_max(pos2(area.left(), top), area.max);
        painter.rect_filled(rect, bottom_only, Color32::from_black_alpha(per));
    }
}

/// A glass panel: soft shadow, translucent fill, crisp hairline border, and a
/// 1px inner top highlight that sells the "lit from above" glass look.
pub fn glass_panel(painter: &egui::Painter, rect: Rect, radius: f32, pal: &Palette, raised: bool) {
    if pal.over_material {
        // The window is genuinely transparent (no OS blur we can rely on), so we
        // give the card real body with a soft shadow and a mostly-opaque fill;
        // the desktop only whispers through the edges. Proves transparent,
        // borderless, on-top windows compose correctly.
        soft_shadow(painter, rect, radius, pal, 0.9);
        let base = if raised { pal.card_fill_raised } else { pal.card_fill };
        let a = if pal.is_dark { 232 } else { 236 };
        let tint = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), a);
        painter.rect_filled(rect, cr(radius), tint);
        inner_glass_lighting(painter, rect, radius, pal);
        painter.rect_stroke(rect, cr(radius), Stroke::new(1.0, pal.hairline), StrokeKind::Inside);
        return;
    }
    soft_shadow(painter, rect, radius, pal, 1.0);
    let fill = if raised { pal.card_fill_raised } else { pal.card_fill };
    painter.rect_filled(rect, cr(radius), fill);
    inner_glass_lighting(painter, rect, radius, pal);
    painter.rect_stroke(rect, cr(radius), Stroke::new(1.0, pal.hairline), StrokeKind::Inside);
}

/// Top highlight + faint bottom shade drawn *inside* the panel, hugging the
/// rounded corners via a short inset line.
pub fn inner_glass_lighting(painter: &egui::Painter, rect: Rect, radius: f32, pal: &Palette) {
    let inset = radius * 0.72;
    let top_y = rect.top() + 1.0;
    painter.line_segment(
        [pos2(rect.left() + inset, top_y), pos2(rect.right() - inset, top_y)],
        Stroke::new(1.0, pal.top_highlight),
    );
    let bot_y = rect.bottom() - 1.0;
    painter.line_segment(
        [pos2(rect.left() + inset, bot_y), pos2(rect.right() - inset, bot_y)],
        Stroke::new(1.0, pal.bottom_shade),
    );
}

pub fn divider_v(painter: &egui::Painter, x: f32, y0: f32, y1: f32, pal: &Palette) {
    painter.line_segment([pos2(x, y0), pos2(x, y1)], Stroke::new(1.0, pal.divider));
}

pub fn divider_h(painter: &egui::Painter, x0: f32, x1: f32, y: f32, pal: &Palette) {
    painter.line_segment([pos2(x0, y), pos2(x1, y)], Stroke::new(1.0, pal.divider));
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
pub struct BtnState {
    pub selected: bool,
    pub force_hover: bool,
}

impl BtnState {
    pub fn hover() -> Self {
        Self { selected: false, force_hover: true }
    }
    pub fn on() -> Self {
        Self { selected: true, force_hover: false }
    }
}

fn interact(ui: &mut Ui, rect: Rect, salt: &str) -> Response {
    let id = egui::Id::new(("scrozz", salt, rect.min.x.to_bits(), rect.min.y.to_bits()));
    ui.interact(rect, id, Sense::click())
}

/// Same, but with a caller-supplied stable `Id`. **Animated widgets must use
/// this**: the position-derived id above changes the moment a rect moves, which
/// would reset the widget's animation state every frame it was in flight.
fn interact_id(ui: &mut Ui, rect: Rect, id: egui::Id) -> Response {
    ui.interact(rect, id, Sense::click())
}

/// Round-rect icon button. Muted by default, brightens on hover, accent-filled
/// when selected — the affordance rhythm CleanShot uses on its action bars.
pub fn icon_button(
    ui: &mut Ui,
    icons: &IconStore,
    pal: &Palette,
    rect: Rect,
    name: &str,
    icon_px: f32,
    st: BtnState,
) -> Response {
    let id = egui::Id::new(("scrozz", name, rect.min.x.to_bits(), rect.min.y.to_bits()));
    icon_button_id(ui, icons, pal, rect, id, name, icon_px, st, 1.0, Vec2::ZERO)
}

/// Round-rect icon button with an explicit id, a reveal factor (for chrome that
/// fades in) and a draw-only offset (so a "rise" never moves the hit target).
///
/// The hover wash **fades** (`FAST`) and the press **sinks** (`INSTANT`, a
/// couple of points of scale). Two `motion::anim` calls; egui schedules the
/// repaints for both and stops asking once they settle.
#[allow(clippy::too_many_arguments)]
pub fn icon_button_id(
    ui: &mut Ui,
    icons: &IconStore,
    pal: &Palette,
    rect: Rect,
    id: egui::Id,
    name: &str,
    icon_px: f32,
    st: BtnState,
    reveal: f32,
    offset: Vec2,
) -> Response {
    let resp = interact_id(ui, rect, id);
    let ctx = ui.ctx().clone();
    let live = reveal > 0.85 && !shot_mode();
    let hovered = (resp.hovered() && live) || st.force_hover;
    let pressed = resp.is_pointer_button_down_on() && live;

    let h = motion::anim(&ctx, id.with("hov"), hovered, motion::FAST, motion::ease_out_cubic);
    let d = motion::anim(&ctx, id.with("prs"), pressed, motion::INSTANT, motion::ease_out_cubic);
    // Press reads as the control physically sinking, not as a colour swap.
    let rect = rect.translate(offset).shrink(1.6 * d);
    let painter = ui.painter();

    if st.selected {
        soft_shadow(painter, rect.shrink(1.0), theme::R_BTN, pal, 0.5 * reveal);
        painter.rect_filled(rect, cr(theme::R_BTN), motion::fade(pal.accent, reveal));
        painter.rect_filled(
            Rect::from_min_max(rect.left_top(), pos2(rect.right(), rect.center().y)),
            cr(theme::R_BTN),
            motion::fade(pal.accent_hi.linear_multiply(0.10), reveal),
        );
    } else {
        // Hover wash and press wash are separate layers, so a press during a
        // half-complete hover doesn't snap the background.
        if h * reveal > 0.001 {
            painter.rect_filled(rect, cr(theme::R_BTN), motion::fade(pal.hover, h * reveal));
        }
        if d * reveal > 0.001 {
            painter.rect_filled(rect, cr(theme::R_BTN), motion::fade(pal.active, d * reveal));
        }
    }

    let tint = if st.selected {
        pal.on_accent
    } else {
        lerp_col(pal.text_muted, pal.text, h)
    };
    icons.draw(painter, name, rect.center(), icon_px * (1.0 - 0.04 * d), motion::fade(tint, reveal));
    resp
}

/// A labelled pill button — the primary affordance in the hover-reveal chrome
/// (Copy / Save). `accent` picks the filled treatment.
#[allow(clippy::too_many_arguments)]
pub fn pill_button(
    ui: &mut Ui,
    icons: &IconStore,
    pal: &Palette,
    rect: Rect,
    id: egui::Id,
    icon: &str,
    label: &str,
    accent: bool,
    reveal: f32,
    offset: Vec2,
) -> Response {
    let resp = interact_id(ui, rect, id);
    let ctx = ui.ctx().clone();
    let live = reveal > 0.85 && !shot_mode();
    let hovered = resp.hovered() && live;
    let pressed = resp.is_pointer_button_down_on() && live;

    let h = motion::anim(&ctx, id.with("hov"), hovered, motion::FAST, motion::ease_out_cubic);
    let d = motion::anim(&ctx, id.with("prs"), pressed, motion::INSTANT, motion::ease_out_cubic);
    let rect = rect.translate(offset).shrink(1.8 * d);
    let r = rect.height() / 2.0;
    let painter = ui.painter();

    let base = if accent { pal.accent } else { pal.card_fill_raised };
    // Hover lifts the fill toward the highlight; press pushes it past it.
    let fill = if accent {
        lerp_col(lerp_col(base, pal.accent_hi, h), pal.accent_press, d)
    } else {
        lerp_col(base, pal.hover, h * 0.9 + d * 0.5)
    };
    soft_shadow(painter, rect, r, pal, (0.55 + 0.35 * h - 0.3 * d) * reveal);
    painter.rect_filled(rect, cr(r), motion::fade(fill, reveal));
    painter.rect_filled(
        Rect::from_min_max(rect.left_top(), pos2(rect.right(), rect.center().y)),
        cr(r),
        motion::fade(Color32::from_white_alpha(if accent { 26 } else { 16 }), reveal),
    );
    painter.rect_stroke(
        rect,
        cr(r),
        Stroke::new(
            1.0,
            motion::fade(Color32::from_white_alpha(if accent { 34 } else { 22 }), reveal),
        ),
        StrokeKind::Inside,
    );

    let fg = motion::fade(if accent { pal.on_accent } else { pal.text }, reveal);
    let galley = painter.layout_no_wrap(label.to_owned(), theme::ts_label(), fg);
    let icon_w = 15.0;
    let total = icon_w + 6.0 + galley.size().x;
    let x0 = rect.center().x - total / 2.0;
    icons.draw(painter, icon, pos2(x0 + icon_w / 2.0, rect.center().y), icon_w, fg);
    painter.galley(
        pos2(x0 + icon_w + 6.0, rect.center().y - galley.size().y / 2.0),
        galley,
        fg,
    );
    resp
}

/// A pill button with an icon + label (used for the primary "All-in-One"-style
/// affordance if needed).
pub fn color_swatch(ui: &mut Ui, pal: &Palette, rect: Rect, color: Color32, selected: bool) -> Response {
    let resp = interact(ui, rect, "swatch");
    let painter = ui.painter();
    painter.rect_filled(rect, cr(theme::R_CHIP), color);
    painter.rect_stroke(
        rect,
        cr(theme::R_CHIP),
        Stroke::new(1.0, pal.hairline),
        StrokeKind::Inside,
    );
    if selected {
        let ring = rect.expand(3.0);
        painter.rect_stroke(
            ring,
            cr(theme::R_CHIP + 3.0),
            Stroke::new(2.0, pal.accent),
            StrokeKind::Inside,
        );
    }
    resp
}

/// Stroke-width control: a wedge that thickens left→right with a draggable knob,
/// plus a value chip. Entirely custom — egui has nothing like it.
pub fn stroke_width(ui: &mut Ui, pal: &Palette, rect: Rect, frac: f32) -> Response {
    let resp = interact(ui, rect, "strokew");
    let painter = ui.painter();
    painter.rect_filled(rect, cr(theme::R_BTN), pal.chip_fill);

    let pad = 12.0;
    let track_l = rect.left() + pad;
    let track_r = rect.right() - pad;
    let cy = rect.center().y;
    let wedge = vec![
        pos2(track_l, cy - 0.6),
        pos2(track_r, cy - 4.2),
        pos2(track_r, cy + 4.2),
        pos2(track_l, cy + 0.6),
    ];
    painter.add(Shape::convex_polygon(wedge, pal.text_faint, Stroke::NONE));

    let knob_x = track_l + (track_r - track_l) * frac.clamp(0.0, 1.0);
    painter.circle_filled(pos2(knob_x, cy), 6.5, pal.text);
    painter.circle_stroke(pos2(knob_x, cy), 6.5, Stroke::new(1.0, pal.bottom_shade));
    resp
}

// ---------------------------------------------------------------------------
// Menu row
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub enum Mod {
    Cmd,
    Shift,
    Opt,
    Ctrl,
}

impl Mod {
    pub fn glyph(self) -> char {
        match self {
            Mod::Ctrl => '\u{2303}',  // ⌃
            Mod::Opt => '\u{2325}',   // ⌥
            Mod::Shift => '\u{21E7}', // ⇧
            Mod::Cmd => '\u{2318}',   // ⌘
        }
    }
}

pub struct Shortcut {
    pub mods: &'static [Mod],
    pub key: &'static str,
}

/// A single menu row: leading icon, label, right-aligned shortcut. Hovered rows
/// fill with the accent and flip content to white — native macOS behavior, and
/// a good stress test of optical alignment.
#[allow(clippy::too_many_arguments)]
pub fn menu_row(
    ui: &mut Ui,
    icons: &IconStore,
    pal: &Palette,
    rect: Rect,
    icon: &str,
    label: &str,
    shortcut: Option<&Shortcut>,
    st: BtnState,
) -> Response {
    let resp = interact(ui, rect, label);
    let ctx = ui.ctx().clone();
    let hovered = (resp.hovered() && !shot_mode()) || st.force_hover;
    // Menu highlights are the fastest thing in the app: any perceptible ramp
    // here reads as lag while the pointer sweeps down a list.
    let h = motion::anim(&ctx, resp.id.with("hov"), hovered, motion::INSTANT, motion::ease_out_cubic);
    let painter = ui.painter();

    if h > 0.001 {
        let hl = rect.shrink2(vec2(6.0, 0.0));
        painter.rect_filled(hl, cr(theme::R_BTN), motion::fade(pal.accent, h));
    }

    let content = lerp_col(pal.text, pal.on_accent, h);
    let icon_tint = lerp_col(pal.text_muted, pal.on_accent, h);

    let icon_cx = rect.left() + 6.0 + 15.0 + 9.0;
    icons.draw(painter, icon, pos2(icon_cx, rect.center().y), 17.0, icon_tint);

    let label_x = icon_cx + 15.0 + 12.0;
    painter.text(
        pos2(label_x, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::ts_label(),
        content,
    );

    if let Some(sc) = shortcut {
        let right = rect.right() - 16.0;
        draw_shortcut(painter, pal, right, rect.center().y, sc, h > 0.5);
    }
    resp
}

/// Draw a right-aligned shortcut, e.g. "⇧⌘4". Modifier symbols are rendered as
/// real font glyphs (Inter ships ⌘⇧⌥⌃), exactly as native macOS menus do — far
/// crisper than stroking them by hand at 12px.
pub fn draw_shortcut(
    painter: &egui::Painter,
    pal: &Palette,
    right_x: f32,
    cy: f32,
    sc: &Shortcut,
    on_accent: bool,
) {
    let color = if on_accent { pal.on_accent } else { pal.text_faint };
    let mut s = String::new();
    for m in sc.mods {
        s.push(m.glyph());
    }
    s.push_str(sc.key);
    painter.text(
        pos2(right_x, cy),
        Align2::RIGHT_CENTER,
        s,
        theme::ts_shortcut(),
        color,
    );
}

// ---------------------------------------------------------------------------
// Wallpaper backdrop + faux screenshot thumbnail
// ---------------------------------------------------------------------------

/// A generated aurora wallpaper so showcase shots are repeatable and pretty.
/// Toggleable off at runtime to expose the *real* OS glass instead.
pub fn wallpaper(painter: &egui::Painter, rect: Rect, is_dark: bool) {
    let base = if is_dark {
        Color32::from_rgb(0x0B, 0x0C, 0x12)
    } else {
        Color32::from_rgb(0xE9, 0xEC, 0xF5)
    };
    painter.rect_filled(rect, cr(0.0), base);

    let blobs: &[(f32, f32, f32, Color32)] = if is_dark {
        &[
            (0.18, 0.16, 0.62, Color32::from_rgb(0x3B, 0x2E, 0x86)),
            (0.86, 0.10, 0.52, Color32::from_rgb(0x14, 0x4E, 0x6B)),
            (0.72, 0.88, 0.60, Color32::from_rgb(0x6B, 0x1E, 0x55)),
            (0.30, 0.92, 0.50, Color32::from_rgb(0x17, 0x3A, 0x74)),
        ]
    } else {
        &[
            (0.16, 0.14, 0.60, Color32::from_rgb(0xB9, 0xC2, 0xFF)),
            (0.88, 0.12, 0.52, Color32::from_rgb(0xBF, 0xE6, 0xF2)),
            (0.74, 0.90, 0.58, Color32::from_rgb(0xF2, 0xC7, 0xE4)),
            (0.28, 0.94, 0.48, Color32::from_rgb(0xC7, 0xD4, 0xFF)),
        ]
    };
    let diag = rect.size().length();
    for &(fx, fy, fr, col) in blobs {
        let c = pos2(rect.left() + rect.width() * fx, rect.top() + rect.height() * fy);
        soft_blob(painter, c, diag * fr * 0.5, col, if is_dark { 150 } else { 190 });
    }
}

/// A soft radial glow built from stacked translucent rings (egui has no radial
/// gradient primitive).
pub fn soft_blob(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32, peak: u8) {
    let rings = 26;
    for i in 0..rings {
        let t = i as f32 / rings as f32;
        let r = radius * (1.0 - t);
        let a = (peak as f32 / rings as f32 * (1.0 - t) * 1.6).min(peak as f32);
        let c = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a as u8);
        painter.circle_filled(center, r, c);
    }
}

/// A fully original faux "captured screenshot" — an abstract app window — so we
/// never reproduce anyone's real UI while still showing a believable thumbnail.
pub fn thumbnail_mock(painter: &egui::Painter, rect: Rect, radius: f32) {
    let p = painter.with_clip_rect(rect);
    // Document background with a subtle top-to-bottom feel via two bands.
    p.rect_filled(rect, cr(radius), Color32::from_rgb(0xF4, 0xF5, 0xF9));
    let top_bar = Rect::from_min_max(rect.left_top(), pos2(rect.right(), rect.top() + 34.0));
    p.rect_filled(top_bar, cr(radius), Color32::from_rgb(0xE7, 0xE9, 0xF1));
    p.line_segment(
        [pos2(rect.left(), top_bar.bottom()), pos2(rect.right(), top_bar.bottom())],
        Stroke::new(1.0, Color32::from_rgb(0xD5, 0xD8, 0xE3)),
    );

    // Traffic lights.
    for (i, col) in [
        Color32::from_rgb(0xFF, 0x5F, 0x57),
        Color32::from_rgb(0xFE, 0xBC, 0x2E),
        Color32::from_rgb(0x28, 0xC8, 0x40),
    ]
    .iter()
    .enumerate()
    {
        p.circle_filled(pos2(rect.left() + 18.0 + i as f32 * 16.0, top_bar.center().y), 4.5, *col);
    }
    // Address pill.
    let pill = Rect::from_min_size(pos2(rect.left() + 78.0, top_bar.center().y - 8.0), vec2(rect.width() - 110.0, 16.0));
    p.rect_filled(pill, cr(8.0), Color32::from_rgb(0xF3, 0xF4, 0xF8));
    p.rect_stroke(pill, cr(8.0), Stroke::new(1.0, Color32::from_rgb(0xDA, 0xDD, 0xE7)), StrokeKind::Inside);

    // Sidebar.
    let side = Rect::from_min_max(pos2(rect.left(), top_bar.bottom()), pos2(rect.left() + 74.0, rect.bottom()));
    p.rect_filled(side, cr(0.0), Color32::from_rgb(0xEC, 0xEE, 0xF4));
    for i in 0..5 {
        let y = side.top() + 18.0 + i as f32 * 20.0;
        let sel = i == 1;
        if sel {
            let hl = Rect::from_min_size(pos2(side.left() + 8.0, y - 7.0), vec2(side.width() - 16.0, 16.0));
            p.rect_filled(hl, cr(6.0), Color32::from_rgb(0x7C, 0x7A, 0xFF));
        }
        let c = if sel { Color32::WHITE } else { Color32::from_rgb(0xBF, 0xC4, 0xD2) };
        p.rect_filled(Rect::from_min_size(pos2(side.left() + 14.0, y - 3.0), vec2(6.0, 6.0)), cr(2.0), c);
        p.rect_filled(Rect::from_min_size(pos2(side.left() + 26.0, y - 2.5), vec2(30.0, 5.0)), cr(2.5), c);
    }

    // Main content: heading, paragraph lines, and an image block.
    let mx = side.right() + 16.0;
    let accent = Color32::from_rgb(0x5B, 0x57, 0xF0);
    p.rect_filled(Rect::from_min_size(pos2(mx, top_bar.bottom() + 16.0), vec2(120.0, 9.0)), cr(3.0), accent);
    for i in 0..3 {
        let y = top_bar.bottom() + 36.0 + i as f32 * 12.0;
        let w = [rect.right() - mx - 18.0, rect.right() - mx - 40.0, rect.right() - mx - 70.0][i];
        p.rect_filled(Rect::from_min_size(pos2(mx, y), vec2(w, 6.0)), cr(3.0), Color32::from_rgb(0xCE, 0xD2, 0xDE));
    }
    let img = Rect::from_min_size(pos2(mx, top_bar.bottom() + 82.0), vec2(rect.right() - mx - 18.0, rect.bottom() - (top_bar.bottom() + 82.0) - 16.0));
    if img.height() > 10.0 {
        p.rect_filled(img, cr(8.0), Color32::from_rgb(0xDD, 0xE1, 0xFB));
        p.rect_stroke(img, cr(8.0), Stroke::new(1.0, Color32::from_rgb(0xC7, 0xCC, 0xEE)), StrokeKind::Inside);
        // a little mountain/photo motif
        let b = img.bottom() - 10.0;
        p.add(Shape::convex_polygon(
            vec![pos2(img.left() + 16.0, b), pos2(img.left() + 46.0, b - 26.0), pos2(img.left() + 76.0, b)],
            accent.linear_multiply(0.5),
            Stroke::NONE,
        ));
        p.circle_filled(pos2(img.right() - 26.0, img.top() + 22.0), 9.0, Color32::from_rgb(0xFF, 0xC7, 0x6B));
    }

    // Re-assert the rounded border on top so corners stay clean.
    p.rect_stroke(rect, cr(radius), Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 30)), StrokeKind::Inside);
}

/// An alternate full "capture" face — a dusk photo — so a stack reads as several
/// *different* screenshots rather than the same one duplicated.
pub fn face_photo(painter: &egui::Painter, rect: Rect, radius: f32) {
    let p = painter.with_clip_rect(rect);
    // Dusk sky base, lightening toward the horizon via stacked bands.
    p.rect_filled(rect, cr(radius), Color32::from_rgb(0x24, 0x2C, 0x5A));
    let bands = 10;
    for k in 0..bands {
        let t = k as f32 / bands as f32;
        let y0 = rect.top() + rect.height() * 0.30 * t;
        let y1 = rect.top() + rect.height() * 0.30 * (t + 1.0 / bands as f32);
        let col = lerp_col(
            Color32::from_rgb(0x2C, 0x36, 0x60),
            Color32::from_rgb(0xE7, 0x9A, 0x7C),
            t,
        );
        p.rect_filled(Rect::from_min_max(pos2(rect.left(), y0), pos2(rect.right(), y1)), cr(0.0), col);
    }
    // Sun glow low on the horizon.
    let sun = pos2(rect.left() + rect.width() * 0.68, rect.top() + rect.height() * 0.42);
    soft_blob(&p, sun, rect.width() * 0.22, Color32::from_rgb(0xFF, 0xD9, 0xA0), 210);
    p.circle_filled(sun, 12.0, Color32::from_rgb(0xFF, 0xE8, 0xC8));
    // Foreground ridge line.
    let base = rect.top() + rect.height() * 0.62;
    p.add(Shape::convex_polygon(
        vec![
            pos2(rect.left(), rect.bottom()),
            pos2(rect.left(), base + 8.0),
            pos2(rect.left() + rect.width() * 0.28, base - 20.0),
            pos2(rect.left() + rect.width() * 0.5, base + 4.0),
            pos2(rect.left() + rect.width() * 0.72, base - 26.0),
            pos2(rect.right(), base + 10.0),
            pos2(rect.right(), rect.bottom()),
        ],
        Color32::from_rgb(0x15, 0x18, 0x2E),
        Stroke::NONE,
    ));
    p.rect_stroke(rect, cr(radius), Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 40)), StrokeKind::Inside);
}

fn lerp_col(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    // Alpha has to be interpolated too — `from_rgb` would force 255 and quietly
    // make every muted tint opaque. Color32 is premultiplied, so a component-wise
    // lerp is the correct blend.
    Color32::from_rgba_premultiplied(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
        f(a.a(), b.a()),
    )
}

/// A cheap "capture" card for the ones peeking out behind the hero — only their
/// top and left slivers are visible, so a header band + a content wash in a
/// distinct hue is enough to read as a different screenshot in the deck.
pub fn mini_capture_card(
    painter: &egui::Painter,
    rect: Rect,
    radius: f32,
    header: Color32,
    body: Color32,
    dim: u8,
) {
    let p = painter.with_clip_rect(rect);
    p.rect_filled(rect, cr(radius), body);
    let hb = Rect::from_min_max(rect.left_top(), pos2(rect.right(), rect.top() + 26.0));
    p.rect_filled(hb, cr(radius), header);
    // three faux traffic dots
    for i in 0..3 {
        p.circle_filled(
            pos2(rect.left() + 14.0 + i as f32 * 12.0, hb.center().y),
            3.0,
            Color32::from_rgba_unmultiplied(0, 0, 0, 40),
        );
    }
    // a couple of content lines
    for i in 0..2 {
        let y = hb.bottom() + 16.0 + i as f32 * 12.0;
        let w = [rect.width() * 0.5, rect.width() * 0.36][i];
        p.rect_filled(
            Rect::from_min_size(pos2(rect.left() + 16.0, y), vec2(w, 6.0)),
            cr(3.0),
            Color32::from_rgba_unmultiplied(0, 0, 0, 32),
        );
    }
    // depth veil so cards deeper in the deck recede
    if dim > 0 {
        p.rect_filled(rect, cr(radius), Color32::from_black_alpha(dim));
    }
    p.rect_stroke(rect, cr(radius), Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 46)), StrokeKind::Inside);
    p.line_segment(
        [pos2(rect.left() + radius * 0.7, rect.top() + 1.0), pos2(rect.right() - radius * 0.7, rect.top() + 1.0)],
        Stroke::new(1.0, Color32::from_white_alpha(if dim > 40 { 12 } else { 26 })),
    );
}

/// The caption strip (filename + dimensions) over the bottom of a hero capture.
pub fn hero_caption(painter: &egui::Painter, rect: Rect, radius: f32, name: &str, dims: &str) {
    bottom_scrim(painter, rect, 58.0, radius, 205);
    let cy = rect.bottom() - 15.0;
    painter.text(
        pos2(rect.left() + 13.0, cy),
        Align2::LEFT_CENTER,
        name,
        theme::ts_label(),
        Color32::from_rgba_unmultiplied(255, 255, 255, 236),
    );
    painter.text(
        pos2(rect.right() - 13.0, cy),
        Align2::RIGHT_CENTER,
        dims,
        theme::ts_caption(),
        Color32::from_rgba_unmultiplied(255, 255, 255, 150),
    );
}

/// The prominent grab handle at the top of the hero card — the whole point of
/// the redesign: the card itself is the draggable object, so the affordance is a
/// visible pill, not a hairline. A soft scrim keeps it legible over any capture.
pub fn grabber(painter: &egui::Painter, hero: Rect) {
    // A tab that straddles the top edge, so it reads as a handle on the card
    // rather than an element floating inside the capture.
    let pill = Rect::from_center_size(pos2(hero.center().x, hero.top()), vec2(66.0, 20.0));
    soft_shadow(painter, pill, 10.0, &crate::theme::Palette::dark(), 0.5);
    painter.rect_filled(pill, cr(10.0), Color32::from_rgb(0x2A, 0x2C, 0x36));
    painter.rect_stroke(pill, cr(10.0), Stroke::new(1.0, Color32::from_white_alpha(30)), StrokeKind::Inside);
    // two rows of three dots — an unambiguous "grip".
    let c = pill.center();
    for r in 0..2 {
        for k in 0..3 {
            let x = c.x - 7.0 + k as f32 * 7.0;
            let y = c.y - 3.5 + r as f32 * 7.0;
            painter.circle_filled(pos2(x, y), 1.5, Color32::from_white_alpha(200));
        }
    }
}

/// A small stack-count badge, notification-style, on the hero's top-left corner.
pub fn count_badge(painter: &egui::Painter, corner: Pos2, n: u32, pal: &Palette) {
    let r = 12.0;
    let c = corner + vec2(2.0, 2.0);
    let key = Shadow { offset: [0, 2], blur: 8, spread: 0, color: pal.key_shadow };
    painter.add(key.as_shape(Rect::from_center_size(c, vec2(r * 2.0, r * 2.0)), cr(r)));
    painter.circle_filled(c, r, pal.accent);
    painter.circle_stroke(c, r, Stroke::new(1.0, Color32::from_white_alpha(40)));
    painter.text(c + vec2(0.0, 0.5), Align2::CENTER_CENTER, n.to_string(), theme::ts_caption(), pal.on_accent);
}

// ---------------------------------------------------------------------------
// Rotation + motion (swipe) and drag-out scene
// ---------------------------------------------------------------------------

/// Approximate a rounded rectangle as a convex polygon so it can be rotated
/// (egui's `TSTransform` is translate+scale only — no rotation — so a rotated
/// card has to be built from points).
fn rounded_poly(rect: Rect, radius: f32) -> Vec<Pos2> {
    let r = radius.min(rect.width() * 0.5).min(rect.height() * 0.5);
    let seg = 5;
    let mut pts = Vec::with_capacity(seg * 4 + 4);
    let corners = [
        (pos2(rect.right() - r, rect.bottom() - r), 0.0_f32),
        (pos2(rect.left() + r, rect.bottom() - r), 90.0),
        (pos2(rect.left() + r, rect.top() + r), 180.0),
        (pos2(rect.right() - r, rect.top() + r), 270.0),
    ];
    for (c, a0) in corners {
        for i in 0..=seg {
            let a = (a0 + 90.0 * (i as f32 / seg as f32)).to_radians();
            pts.push(pos2(c.x + r * a.cos(), c.y + r * a.sin()));
        }
    }
    pts
}

fn rotate_pts(pts: &mut [Pos2], center: Pos2, radians: f32) {
    let (s, c) = radians.sin_cos();
    for p in pts.iter_mut() {
        let dx = p.x - center.x;
        let dy = p.y - center.y;
        *p = pos2(center.x + dx * c - dy * s, center.y + dx * s + dy * c);
    }
}

fn fill_rot_rect(painter: &egui::Painter, rect: Rect, pivot: Pos2, angle: f32, color: Color32) {
    let mut pts = vec![rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom()];
    rotate_pts(&mut pts, pivot, angle);
    painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
}

fn scale_a(base: u8, alpha: u8) -> u8 {
    (base as u32 * alpha as u32 / 255) as u8
}

/// A capture card mid-swipe: rotated and translucent. Built as a simplified
/// rotated face (a full `thumbnail_mock` can't rotate) — plenty to read as
/// "the top screenshot is being flung away".
pub fn flung_capture(painter: &egui::Painter, rect: Rect, radius: f32, angle: f32, alpha: u8) {
    let c = rect.center();
    // soft-ish shadow: a couple of offset rotated dark polys
    for (dy, a) in [(11.0, 34u8), (5.0, 22u8)] {
        let mut sh = rounded_poly(rect.translate(vec2(0.0, dy)), radius);
        rotate_pts(&mut sh, c + vec2(0.0, dy), angle);
        painter.add(Shape::convex_polygon(sh, Color32::from_black_alpha(scale_a(a, alpha)), Stroke::NONE));
    }
    // face
    let mut face = rounded_poly(rect, radius);
    rotate_pts(&mut face, c, angle);
    painter.add(Shape::convex_polygon(
        face,
        Color32::from_rgba_unmultiplied(0xF3, 0xF5, 0xFA, alpha),
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, scale_a(46, alpha))),
    ));
    // window chrome strip + dots
    let header = Rect::from_min_max(rect.left_top(), pos2(rect.right(), rect.top() + 28.0));
    fill_rot_rect(painter, header, c, angle, Color32::from_rgba_unmultiplied(0xE4, 0xE7, 0xF0, alpha));
    for i in 0..3 {
        let mut d = [pos2(rect.left() + 16.0 + i as f32 * 13.0, rect.top() + 14.0)];
        rotate_pts(&mut d, c, angle);
        painter.circle_filled(d[0], 3.0, Color32::from_rgba_unmultiplied(0, 0, 0, scale_a(46, alpha)));
    }
    // an accent heading + a few content lines so it reads as an app capture
    fill_rot_rect(
        painter,
        Rect::from_min_size(pos2(rect.left() + 18.0, rect.top() + 46.0), vec2(96.0, 9.0)),
        c,
        angle,
        Color32::from_rgba_unmultiplied(0x5B, 0x57, 0xF0, alpha),
    );
    for i in 0..3 {
        fill_rot_rect(
            painter,
            Rect::from_min_size(pos2(rect.left() + 18.0, rect.top() + 66.0 + i as f32 * 13.0), vec2([150.0, 120.0, 92.0][i], 6.0)),
            c,
            angle,
            Color32::from_rgba_unmultiplied(0xCE, 0xD2, 0xDE, alpha),
        );
    }
}

/// A capture card face that can be **rotated and faded live** — the drawing
/// primitive behind the interactive stack.
///
/// epaint has no rotation for images, rounded rects or text galleys, so every
/// element here is a polygon pushed through [`rotate_pts`]. That is also why
/// there is no text on the face: rotated glyphs are simply not achievable
/// without a render-to-texture path (see FINDINGS §2).
///
/// `variant` picks one of four content layouts so a stack of captures doesn't
/// look like four copies of the same screenshot.
pub fn capture_face(
    painter: &egui::Painter,
    rect: Rect,
    radius: f32,
    angle: f32,
    alpha: u8,
    variant: usize,
    lift: f32,
) {
    if alpha == 0 {
        return;
    }
    let c = rect.center();
    // Shadow: two offset rotated polys. `lift` scales both the offset and the
    // opacity, so a pressed/dragged card visibly leaves the surface.
    for (dy, a) in [(11.0 * lift, 34u8), (5.0 * lift, 22u8)] {
        let mut sh = rounded_poly(rect.translate(vec2(0.0, dy)), radius);
        rotate_pts(&mut sh, c + vec2(0.0, dy), angle);
        painter.add(Shape::convex_polygon(
            sh,
            Color32::from_black_alpha(scale_a((a as f32 * lift.min(1.6)) as u8, alpha)),
            Stroke::NONE,
        ));
    }

    let pal4 = [
        // (page, header, accent, line)
        ([0xF3, 0xF5, 0xFA], [0xE4, 0xE7, 0xF0], [0x5B, 0x57, 0xF0], [0xCE, 0xD2, 0xDE]),
        ([0x1C, 0x1F, 0x2A], [0x14, 0x17, 0x20], [0x3F, 0xC1, 0x8F], [0x39, 0x3F, 0x52]),
        ([0xFA, 0xF4, 0xEE], [0xF0, 0xE6, 0xDA], [0xE0, 0x7A, 0x3C], [0xDC, 0xCE, 0xBE]),
        ([0xEE, 0xF4, 0xFB], [0xDD, 0xE8, 0xF6], [0x2A, 0x7B, 0xE0], [0xC2, 0xD3, 0xE6]),
    ];
    let (page, head, acc, line) = pal4[variant % 4];
    let rgba = |c: [u8; 3], a: u8| Color32::from_rgba_unmultiplied(c[0], c[1], c[2], scale_a(a, alpha));

    let mut face = rounded_poly(rect, radius);
    rotate_pts(&mut face, c, angle);
    painter.add(Shape::convex_polygon(
        face,
        rgba(page, 255),
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, scale_a(46, alpha))),
    ));

    let header = Rect::from_min_max(rect.left_top(), pos2(rect.right(), rect.top() + 28.0));
    fill_rot_rect(painter, header, c, angle, rgba(head, 255));
    for i in 0..3 {
        let mut d = [pos2(rect.left() + 16.0 + i as f32 * 13.0, rect.top() + 14.0)];
        rotate_pts(&mut d, c, angle);
        painter.circle_filled(d[0], 3.0, Color32::from_rgba_unmultiplied(0, 0, 0, scale_a(46, alpha)));
    }

    let bar = |r: Rect, col: [u8; 3], a: u8| fill_rot_rect(painter, r, c, angle, rgba(col, a));
    let l = rect.left() + 18.0;
    let t = rect.top() + 46.0;
    match variant % 4 {
        // A document: accent heading over ragged body copy.
        0 => {
            bar(Rect::from_min_size(pos2(l, t), vec2(96.0, 9.0)), acc, 255);
            for i in 0..3 {
                bar(
                    Rect::from_min_size(pos2(l, t + 20.0 + i as f32 * 13.0), vec2([150.0, 120.0, 92.0][i], 6.0)),
                    line,
                    255,
                );
            }
        }
        // A code editor: a gutter and indented syntax-coloured runs.
        1 => {
            bar(Rect::from_min_size(pos2(l - 6.0, t - 6.0), vec2(2.0, rect.height() - 62.0)), line, 160);
            for i in 0..6 {
                let indent = [0.0, 14.0, 28.0, 28.0, 14.0, 0.0][i];
                let w = [82.0, 128.0, 96.0, 64.0, 110.0, 74.0][i];
                bar(
                    Rect::from_min_size(pos2(l + 6.0 + indent, t + i as f32 * 12.0), vec2(w, 5.0)),
                    if i % 3 == 0 { acc } else { line },
                    if i % 3 == 0 { 255 } else { 200 },
                );
            }
        }
        // A dashboard: a big accent block beside stacked metric rows.
        2 => {
            bar(Rect::from_min_size(pos2(l, t), vec2(74.0, 52.0)), acc, 235);
            for i in 0..4 {
                bar(
                    Rect::from_min_size(pos2(l + 86.0, t + i as f32 * 14.0), vec2([104.0, 78.0, 118.0, 60.0][i], 7.0)),
                    line,
                    255,
                );
            }
        }
        // A chat transcript: alternating bubbles, one accent-filled.
        _ => {
            for i in 0..4 {
                let mine = i % 2 == 1;
                let w = [128.0, 92.0, 104.0, 70.0][i];
                let x = if mine { rect.right() - 18.0 - w } else { l };
                bar(
                    Rect::from_min_size(pos2(x, t + i as f32 * 17.0), vec2(w, 12.0)),
                    if mine { acc } else { line },
                    if mine { 235 } else { 255 },
                );
            }
        }
    }
}

/// A faint rounded, rotated card *outline* — an echo of a moving card's earlier
/// positions, so a swipe reads as motion in a still frame.
pub fn ghost_card(painter: &egui::Painter, rect: Rect, radius: f32, angle: f32, alpha: u8) {
    let mut poly = rounded_poly(rect, radius);
    rotate_pts(&mut poly, rect.center(), angle);
    painter.add(Shape::convex_polygon(
        poly,
        Color32::TRANSPARENT,
        Stroke::new(1.5, Color32::from_white_alpha(alpha)),
    ));
}

/// A few tapered streaks trailing a moving card, to read motion in a still.
pub fn motion_streaks(painter: &egui::Painter, from: Pos2, dir: Vec2, count: usize, spread: f32) {
    let dir = dir.normalized();
    let perp = vec2(-dir.y, dir.x);
    for i in 0..count {
        let off = (i as f32 - (count as f32 - 1.0) / 2.0) * spread;
        let base = from + perp * off;
        let len = 26.0 - (off.abs() * 0.25);
        let a = (70.0 - off.abs() * 1.2).clamp(20.0, 70.0) as u8;
        painter.line_segment(
            [base, base - dir * len],
            Stroke::new(2.4, Color32::from_white_alpha(a)),
        );
    }
}

fn dashed_path(painter: &egui::Painter, pts: &[Pos2], stroke: Stroke, dash: f32, gap: f32) {
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let len = (b - a).length();
        if len <= 0.01 {
            continue;
        }
        let dir = (b - a) / len;
        let step = dash + gap;
        let mut t = 0.0;
        while t < len {
            let s = a + dir * t;
            let e = a + dir * (t + dash).min(len);
            painter.line_segment([s, e], stroke);
            t += step;
        }
    }
}

/// A minimal "other app" (a chat window) used as the drag-out drop target, so
/// the hero interaction — drag a capture straight into another app — reads at a
/// glance. Deliberately light, to look like a different application than Scrozz.
pub fn drop_chat(painter: &egui::Painter, rect: Rect, pal: &Palette, active: bool) {
    soft_shadow(painter, rect, 16.0, pal, 0.8);
    painter.rect_filled(rect, cr(16.0), Color32::from_rgb(0xFB, 0xFB, 0xFD));
    // title bar
    let tb = Rect::from_min_max(rect.left_top(), pos2(rect.right(), rect.top() + 34.0));
    painter.rect_filled(tb, cr(16.0), Color32::from_rgb(0xEF, 0xF0, 0xF4));
    for (i, col) in [
        Color32::from_rgb(0xFF, 0x5F, 0x57),
        Color32::from_rgb(0xFE, 0xBC, 0x2E),
        Color32::from_rgb(0x28, 0xC8, 0x40),
    ]
    .iter()
    .enumerate()
    {
        painter.circle_filled(pos2(tb.left() + 16.0 + i as f32 * 15.0, tb.center().y), 4.5, *col);
    }
    painter.text(tb.center(), Align2::CENTER_CENTER, "Messages", theme::ts_caption(), Color32::from_rgb(0x6A, 0x6E, 0x7A));
    painter.line_segment(
        [pos2(rect.left(), tb.bottom()), pos2(rect.right(), tb.bottom())],
        Stroke::new(1.0, Color32::from_rgb(0xDD, 0xDF, 0xE6)),
    );

    // incoming + outgoing bubbles
    let incoming = Rect::from_min_size(pos2(rect.left() + 16.0, tb.bottom() + 18.0), vec2(rect.width() * 0.52, 26.0));
    painter.rect_filled(incoming, cr(13.0), Color32::from_rgb(0xE9, 0xEA, 0xEE));
    for i in 0..2 {
        painter.rect_filled(
            Rect::from_min_size(pos2(incoming.left() + 12.0, incoming.top() + 8.0 + i as f32 * 8.0), vec2([90.0, 60.0][i], 4.0)),
            cr(2.0),
            Color32::from_rgb(0xB6, 0xB9, 0xC2),
        );
    }
    let outgoing = Rect::from_min_size(pos2(rect.right() - 16.0 - rect.width() * 0.46, incoming.bottom() + 12.0), vec2(rect.width() * 0.46, 22.0));
    painter.rect_filled(outgoing, cr(11.0), Color32::from_rgb(0x36, 0x8A, 0xFF));
    painter.rect_filled(
        Rect::from_min_size(pos2(outgoing.left() + 12.0, outgoing.center().y - 2.0), vec2(outgoing.width() - 40.0, 4.0)),
        cr(2.0),
        Color32::from_white_alpha(200),
    );

    // input bar
    let input = Rect::from_min_size(pos2(rect.left() + 14.0, rect.bottom() - 40.0), vec2(rect.width() - 28.0, 26.0));
    painter.rect_filled(input, cr(13.0), Color32::from_rgb(0xF0, 0xF1, 0xF4));
    painter.rect_stroke(input, cr(13.0), Stroke::new(1.0, Color32::from_rgb(0xDD, 0xDF, 0xE6)), StrokeKind::Inside);
    painter.text(pos2(input.left() + 12.0, input.center().y), Align2::LEFT_CENTER, "iMessage", theme::ts_caption(), Color32::from_rgb(0xA6, 0xAA, 0xB4));

    if active {
        // accent wash + dashed accent border + a "drop" chip
        painter.rect_filled(rect, cr(16.0), Color32::from_rgba_unmultiplied(pal.accent.r(), pal.accent.g(), pal.accent.b(), 30));
        let mut ring = rounded_poly(rect.shrink(5.0), 13.0);
        ring.push(ring[0]);
        dashed_path(painter, &ring, Stroke::new(2.0, pal.accent), 7.0, 5.0);
    }
}

/// A macOS-style arrow cursor (white fill, dark keyline) with a soft shadow, so
/// the drag scene obviously depicts a pointer dragging the card.
pub fn arrow_cursor(painter: &egui::Painter, tip: Pos2) {
    let s = 1.35;
    let raw = [
        (0.0, 0.0),
        (0.0, 17.0),
        (4.2, 12.9),
        (7.0, 18.7),
        (9.7, 17.5),
        (6.9, 11.8),
        (12.0, 11.8),
    ];
    let pts: Vec<Pos2> = raw.iter().map(|(x, y)| pos2(tip.x + x * s, tip.y + y * s)).collect();
    let mut sh: Vec<Pos2> = pts.iter().map(|p| pos2(p.x + 1.0, p.y + 2.0)).collect();
    sh.push(sh[0]);
    painter.add(Shape::convex_polygon(sh, Color32::from_black_alpha(60), Stroke::NONE));
    painter.add(Shape::convex_polygon(pts.clone(), Color32::WHITE, Stroke::new(1.3, Color32::from_rgb(0x1A, 0x1C, 0x24))));
}

/// The red count pill that macOS shows on a drag proxy ("1" item).
pub fn drag_badge(painter: &egui::Painter, center: Pos2, n: u32, pal: &Palette) {
    let r = 11.0;
    let key = Shadow { offset: [0, 2], blur: 8, spread: 0, color: pal.key_shadow };
    painter.add(key.as_shape(Rect::from_center_size(center, vec2(r * 2.0, r * 2.0)), cr(r)));
    painter.circle_filled(center, r, Color32::from_rgb(0xF2, 0x45, 0x3D));
    painter.circle_stroke(center, r, Stroke::new(1.5, Color32::WHITE));
    painter.text(center + vec2(0.0, 0.5), Align2::CENTER_CENTER, n.to_string(), theme::ts_caption(), Color32::WHITE);
}

/// A small floating label chip (accent-filled) for hints like "Drop to send".
pub fn chip_label(painter: &egui::Painter, center: Pos2, text: &str, pal: &Palette) {
    let font = theme::ts_caption();
    let galley = painter.layout_no_wrap(text.to_owned(), font, pal.on_accent);
    let pad = vec2(11.0, 6.0);
    let rect = Rect::from_center_size(center, galley.size() + pad * 2.0);
    let key = Shadow { offset: [0, 3], blur: 12, spread: 0, color: pal.key_shadow };
    painter.add(key.as_shape(rect, cr(rect.height() / 2.0)));
    painter.rect_filled(rect, cr(rect.height() / 2.0), pal.accent);
    painter.galley(rect.min + pad, galley, pal.on_accent);
}
