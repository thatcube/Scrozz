//! Hand-drawn primitives + widgets.
//!
//! This is where the spike earns its answer: almost every pixel here is drawn
//! with `egui::Painter` rather than a stock widget, because stock egui widgets
//! are exactly what the maintainer thinks looks like a debug tool. Cards,
//! shadows, the wallpaper, the faux screenshot, the ⌘⇧⌥ shortcut glyphs and the
//! buttons are all bespoke, driven entirely by the tokens in `theme`.

use crate::icons::IconStore;
use crate::theme::{self, cr, Palette};
use egui::{
    epaint::Shadow, pos2, vec2, Align2, Color32, Pos2, Rect, Response, Sense, Shape, Stroke,
    StrokeKind, Ui,
};
use std::sync::atomic::{AtomicBool, Ordering};

/// When true, live pointer hover is ignored so `--shot` renders are
/// deterministic regardless of where the real mouse happens to sit.
static SCREENSHOT: AtomicBool = AtomicBool::new(false);
pub fn set_screenshot(on: bool) {
    SCREENSHOT.store(on, Ordering::Relaxed);
}
fn shot_mode() -> bool {
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
    let resp = interact(ui, rect, name);
    let painter = ui.painter();
    let hovered = (resp.hovered() && !shot_mode()) || st.force_hover;
    let pressed = resp.is_pointer_button_down_on() && !shot_mode();

    if st.selected {
        soft_shadow(painter, rect.shrink(1.0), theme::R_BTN, pal, 0.5);
        painter.rect_filled(rect, cr(theme::R_BTN), pal.accent);
        painter.rect_filled(
            Rect::from_min_max(rect.left_top(), pos2(rect.right(), rect.center().y)),
            cr(theme::R_BTN),
            pal.accent_hi.linear_multiply(0.10),
        );
    } else if pressed {
        painter.rect_filled(rect, cr(theme::R_BTN), pal.active);
    } else if hovered {
        painter.rect_filled(rect, cr(theme::R_BTN), pal.hover);
    }

    let tint = if st.selected {
        pal.on_accent
    } else if hovered {
        pal.text
    } else {
        pal.text_muted
    };
    icons.draw(painter, name, rect.center(), icon_px, tint);
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
    let painter = ui.painter();
    let hovered = (resp.hovered() && !shot_mode()) || st.force_hover;

    if hovered {
        let hl = rect.shrink2(vec2(6.0, 0.0));
        painter.rect_filled(hl, cr(theme::R_BTN), pal.accent);
    }

    let content = if hovered { pal.on_accent } else { pal.text };
    let icon_tint = if hovered { pal.on_accent } else { pal.text_muted };

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
        draw_shortcut(painter, pal, right, rect.center().y, sc, hovered);
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
fn soft_blob(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32, peak: u8) {
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
