//! The three surfaces. Each lays out its own geometry and reports a `size()`
//! (card only) so the app can size a transparent window around it with room for
//! the shadow.

use crate::icons::IconStore;
use crate::paint::{self, BtnState, Mod, Shortcut};
use crate::theme::{self, cr, Palette};
use egui::{pos2, vec2, Align2, Color32, Rect, Stroke, StrokeKind, Ui, Vec2};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Quick,
    Menu,
    Annotate,
    Onboard,
}

/// Which state of the drag-first Quick Access stack to render. The overlay is a
/// *stack* of recent captures, and its whole reason for existing is grab-and-drag
/// — so we render three moments of that story.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum QuickVariant {
    /// Resting deck of captures with the grab handle + secondary action pill.
    Stack,
    /// Top capture mid-swipe, flung away to dismiss.
    Swipe,
    /// Top capture lifted out and dragged toward another app (the hero move).
    Drag,
}

impl QuickVariant {
    pub fn parse(s: &str) -> Self {
        match s {
            "swipe" => Self::Swipe,
            "drag" => Self::Drag,
            _ => Self::Stack,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Self::Stack => "stack",
            Self::Swipe => "swipe",
            Self::Drag => "drag",
        }
    }
    /// Scene size (excluding the app's shadow margin). Each state needs different
    /// room: swipe needs vertical travel, drag needs a drop target beside it.
    pub fn scene(self) -> Vec2 {
        match self {
            Self::Stack => vec2(384.0, 322.0),
            Self::Swipe => vec2(384.0, 392.0),
            Self::Drag => vec2(600.0, 340.0),
        }
    }
}

impl Surface {
    pub const ALL: [Surface; 4] =
        [Surface::Quick, Surface::Menu, Surface::Annotate, Surface::Onboard];

    pub fn key(self) -> &'static str {
        match self {
            Surface::Quick => "quick",
            Surface::Menu => "menu",
            Surface::Annotate => "annotate",
            Surface::Onboard => "onboard",
        }
    }

    /// Card size, excluding the shadow margin the app adds around it.
    pub fn size(self) -> Vec2 {
        match self {
            Surface::Quick => QuickVariant::Stack.scene(),
            Surface::Menu => vec2(296.0, 389.0),
            Surface::Annotate => vec2(754.0, 60.0),
            Surface::Onboard => vec2(520.0, 552.0),
        }
    }

    pub fn show(self, ui: &mut Ui, icons: &IconStore, pal: &Palette, card: Rect) {
        match self {
            Surface::Quick => quick(ui, icons, pal, card, QuickVariant::Stack),
            Surface::Menu => menu(ui, icons, pal, card),
            Surface::Annotate => annotate(ui, icons, pal, card),
            Surface::Onboard => onboard(ui, icons, pal, card),
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Quick Access Overlay (primary) — a drag-first stack of captures.
//
// The overlay is NOT a single card with a button row. It's a physical stack of
// recent screenshots in the corner: the top one grabbable and draggable straight
// into another app, swipe-to-dismiss, with copy/save/annotate/pin/close kept as
// secondary actions. This renders three states of that model.
// ---------------------------------------------------------------------------

const HERO_W: f32 = 300.0;
const HERO_H: f32 = 188.0;
const BX: f32 = 11.0; // deck horizontal peek
const BY: f32 = 13.0; // deck vertical peek

pub fn quick(ui: &mut Ui, icons: &IconStore, pal: &Palette, scene: Rect, variant: QuickVariant) {
    let p = ui.painter().clone();
    let rt = theme::R_THUMB;
    match variant {
        QuickVariant::Stack => {
            let pill_h = 46.0;
            let hero = Rect::from_min_size(
                pos2(
                    scene.right() - 30.0 - HERO_W,
                    scene.bottom() - pill_h - 16.0 - HERO_H,
                ),
                vec2(HERO_W, HERO_H),
            );
            deck_behind(&p, hero, pal);
            draw_hero(&p, hero, pal, true, "Screen 14.32.08", "2048 × 1280", true);
            paint::count_badge(&p, hero.left_top(), 4, pal);
            action_pill(ui, icons, pal, hero, pill_h);
        }
        QuickVariant::Swipe => {
            let rest = Rect::from_min_size(
                pos2(scene.right() - 30.0 - HERO_W, scene.top() + 24.0),
                vec2(HERO_W, HERO_H),
            );
            // The stack that remains once the top capture is flung: the next
            // capture is now the front, shown as a genuine (different) photo.
            deck_behind_n(&p, rest, pal, 2);
            draw_hero(&p, rest, pal, true, "Screen 14.30.02", "1920 × 1080", true);

            // The just-captured top card, flung downward to dismiss: smaller and
            // rotated so it reads as receding, with faint echoes of its path.
            let ang = 12.0_f32.to_radians();
            let fsz = rest.size() * 0.82;
            let flung = Rect::from_min_size(rest.min + vec2(56.0, 206.0), fsz);
            paint::ghost_card(&p, Rect::from_min_size(rest.min + vec2(18.0, 78.0), fsz), rt, ang * 0.3, 46);
            paint::ghost_card(&p, Rect::from_min_size(rest.min + vec2(37.0, 142.0), fsz), rt, ang * 0.62, 30);
            paint::flung_capture(&p, flung, rt, ang, 245);
        }
        QuickVariant::Drag => drag_scene(&p, pal, scene),
    }
}

/// The drag-out hero scene: a capture lifted out of the corner stack and dragged
/// straight into another app — the interaction the maintainer calls core.
fn drag_scene(p: &egui::Painter, pal: &Palette, scene: Rect) {
    let cw = 196.0;
    let ch = 124.0;
    let rt = theme::R_THUMB;

    // Drop target (another app) on the left, primed to receive.
    let chat = Rect::from_min_size(pos2(scene.left() + 24.0, scene.top() + 46.0), vec2(238.0, 250.0));
    paint::drop_chat(p, chat, pal, true);
    paint::chip_label(p, pos2(chat.center().x, chat.center().y), "Drop to send", pal);

    // Source stack on the right (top capture removed — it's in flight).
    let src = Rect::from_min_size(
        pos2(scene.right() - 28.0 - cw, scene.bottom() - 34.0 - ch),
        vec2(cw, ch),
    );
    for i in [2.0_f32, 1.0] {
        let r = Rect::from_min_size(src.min + vec2(-BX * i, -BY * i), src.size()).shrink(3.0 * i);
        let (h, b, dim) = deck_style(i as i32);
        paint::mini_capture_card(p, r, rt, h, b, dim);
    }
    paint::soft_shadow(p, src, rt, pal, 0.7);
    paint::thumbnail_mock(p, src, rt);
    p.rect_stroke(src, cr(rt), Stroke::new(1.0, pal.thumb_border), StrokeKind::Inside);

    // The lifted card in flight — larger, higher shadow, a whisper of tilt.
    let lifted = Rect::from_center_size(pos2(scene.left() + 320.0, scene.top() + 150.0), vec2(cw * 1.06, ch * 1.06));
    paint::motion_streaks(p, pos2(lifted.right() - 6.0, lifted.center().y), vec2(-1.0, 0.15), 3, 13.0);
    paint::soft_shadow(p, lifted, rt, pal, 1.5);
    paint::thumbnail_mock(p, lifted, rt);
    paint::hero_caption(p, lifted, rt, "Screen 14.32.08", "2048 × 1280");
    p.rect_stroke(lifted, cr(rt), Stroke::new(1.0, pal.thumb_border), StrokeKind::Inside);
    paint::drag_badge(p, lifted.left_top() + vec2(1.0, 1.0), 1, pal);
    paint::arrow_cursor(p, pos2(lifted.center().x + 8.0, lifted.center().y + 6.0));
}

fn deck_style(i: i32) -> (Color32, Color32, u8) {
    match i {
        3 => (Color32::from_rgb(0xD7, 0xEC, 0xE4), Color32::from_rgb(0xF0, 0xF6, 0xF4), 96),
        2 => (Color32::from_rgb(0xEC, 0xDA, 0xEC), Color32::from_rgb(0xF6, 0xF2, 0xFA), 58),
        _ => (Color32::from_rgb(0xD9, 0xDD, 0xEC), Color32::from_rgb(0xF3, 0xF4, 0xFA), 24),
    }
}

fn deck_behind(p: &egui::Painter, hero: Rect, pal: &Palette) {
    deck_behind_n(p, hero, pal, 3);
}

fn deck_behind_n(p: &egui::Painter, hero: Rect, pal: &Palette, n: i32) {
    let rt = theme::R_THUMB;
    // One soft shadow under the backmost extent grounds the whole stack.
    let back = Rect::from_min_size(hero.min + vec2(-BX * n as f32, -BY * n as f32), hero.size());
    paint::soft_shadow(p, back.shrink(4.0), rt, pal, 0.7);
    for i in (1..=n).rev() {
        let f = i as f32;
        let r = Rect::from_min_size(hero.min + vec2(-BX * f, -BY * f), hero.size()).shrink(3.0 * f);
        let (h, b, dim) = deck_style(i);
        paint::mini_capture_card(p, r, rt, h, b, dim);
    }
}

fn draw_hero(
    p: &egui::Painter,
    hero: Rect,
    pal: &Palette,
    photo: bool,
    name: &str,
    dims: &str,
    grab: bool,
) {
    let rt = theme::R_THUMB;
    paint::soft_shadow(p, hero, rt, pal, 1.0);
    if photo {
        paint::face_photo(p, hero, rt);
    } else {
        paint::thumbnail_mock(p, hero, rt);
    }
    paint::hero_caption(p, hero, rt, name, dims);
    p.rect_stroke(hero, cr(rt), Stroke::new(1.0, pal.thumb_border), StrokeKind::Inside);
    if grab {
        paint::grabber(p, hero);
    }
}

/// The secondary action pill floating below the hero — copy/save/annotate/pin/
/// upload, then a divider and close. Deliberately quieter than the grab handle.
fn action_pill(ui: &mut Ui, icons: &IconStore, pal: &Palette, hero: Rect, pill_h: f32) {
    let d = 34.0;
    let gap = 6.0;
    let icon_px = 18.0;
    let inpad = 9.0;
    let div_w = 13.0;
    let names = ["copy", "device-floppy", "pencil", "pin", "cloud-upload"];
    let n = names.len() as f32;
    let content_w = n * d + (n - 1.0) * gap + div_w + d;
    let pill_w = content_w + inpad * 2.0;
    let pill = Rect::from_center_size(
        pos2(hero.center().x, hero.bottom() + 16.0 + pill_h / 2.0),
        vec2(pill_w, pill_h),
    );
    paint::glass_panel(ui.painter(), pill, theme::R_BAR, pal, true);

    let cy = pill.center().y;
    let mut x = pill.left() + inpad;
    for name in names {
        let r = Rect::from_min_size(pos2(x, cy - d / 2.0), vec2(d, d));
        paint::icon_button(ui, icons, pal, r, name, icon_px, BtnState::default());
        x += d + gap;
    }
    let dx = x + div_w * 0.5 - gap * 0.5;
    paint::divider_v(ui.painter(), dx, cy - 13.0, cy + 13.0, pal);
    x += div_w;
    let close = Rect::from_min_size(pos2(x, cy - d / 2.0), vec2(d, d));
    paint::icon_button(ui, icons, pal, close, "x", icon_px, BtnState::default());
}

// ---------------------------------------------------------------------------
// 2. Menu bar dropdown
// ---------------------------------------------------------------------------
enum Row {
    Item { icon: &'static str, label: &'static str, sc: Option<Shortcut>, hover: bool },
    Divider,
}

fn menu(ui: &mut Ui, icons: &IconStore, pal: &Palette, card: Rect) {
    paint::glass_panel(ui.painter(), card, theme::R_CARD, pal, false);

    use Mod::*;
    let rows = [
        Row::Item { icon: "layout-grid", label: "All-in-One", sc: Some(Shortcut { mods: &[Shift, Cmd], key: "A" }), hover: false },
        Row::Item { icon: "viewfinder", label: "Capture Area", sc: Some(Shortcut { mods: &[Shift, Cmd], key: "4" }), hover: true },
        Row::Item { icon: "app-window", label: "Capture Window", sc: Some(Shortcut { mods: &[Shift, Cmd], key: "5" }), hover: false },
        Row::Item { icon: "device-desktop", label: "Capture Fullscreen", sc: Some(Shortcut { mods: &[Shift, Cmd], key: "3" }), hover: false },
        Row::Divider,
        Row::Item { icon: "arrow-bar-to-down", label: "Scrolling Capture", sc: Some(Shortcut { mods: &[Shift, Cmd], key: "6" }), hover: false },
        Row::Item { icon: "video", label: "Record Screen", sc: Some(Shortcut { mods: &[Shift, Cmd], key: "7" }), hover: false },
        Row::Divider,
        Row::Item { icon: "scan", label: "Capture Text (OCR)", sc: Some(Shortcut { mods: &[Shift, Cmd], key: "8" }), hover: false },
        Row::Item { icon: "history", label: "History", sc: Some(Shortcut { mods: &[Cmd], key: "Y" }), hover: false },
        Row::Divider,
        Row::Item { icon: "settings", label: "Preferences…", sc: Some(Shortcut { mods: &[Cmd], key: "," }), hover: false },
        Row::Item { icon: "power", label: "Quit Scrozz", sc: Some(Shortcut { mods: &[Cmd], key: "Q" }), hover: false },
    ];

    let row_h = 34.0;
    let mut y = card.top() + 8.0;
    for row in &rows {
        match row {
            Row::Item { icon, label, sc, hover } => {
                let r = Rect::from_min_size(pos2(card.left(), y), vec2(card.width(), row_h));
                let st = if *hover { BtnState::hover() } else { BtnState::default() };
                paint::menu_row(ui, icons, pal, r, icon, label, sc.as_ref(), st);
                y += row_h;
            }
            Row::Divider => {
                paint::divider_h(ui.painter(), card.left() + 14.0, card.right() - 14.0, y + 5.5, pal);
                y += 11.0;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Annotation toolbar strip
// ---------------------------------------------------------------------------
fn annotate(ui: &mut Ui, icons: &IconStore, pal: &Palette, card: Rect) {
    paint::glass_panel(ui.painter(), card, theme::R_BAR, pal, true);

    let cy = card.center().y;
    let d = 40.0;
    let gap = 4.0;
    let icon_px = 20.0;
    let mut x = card.left() + 10.0;

    // tool name, selected?, forced hover?
    let tools: [(&str, bool, bool); 11] = [
        ("crop", false, false),
        ("arrow-up-right", true, false),
        ("square", false, false),
        ("circle", false, false),
        ("line", false, false),
        ("letter-t", false, true),
        ("highlight", false, false),
        ("droplet", false, false),
        ("grid-dots", false, false),
        ("list-numbers", false, false),
        ("pencil", false, false),
    ];
    for (name, selected, hover) in tools {
        let r = Rect::from_min_size(pos2(x, cy - d / 2.0), vec2(d, d));
        let st = BtnState { selected, force_hover: hover };
        paint::icon_button(ui, icons, pal, r, name, icon_px, st);
        x += d + gap;
    }

    // group divider
    x += 6.0;
    paint::divider_v(ui.painter(), x, cy - 15.0, cy + 15.0, pal);
    x += 6.0 + 6.0;

    // colour swatch
    let sw = Rect::from_min_size(pos2(x, cy - d / 2.0), vec2(d, d));
    let swatch_rect = sw.shrink(7.0);
    paint::color_swatch(ui, pal, swatch_rect, Color32::from_rgb(0xFF, 0x5A, 0x5A), true);
    x += d + gap + 2.0;

    // stroke width
    let swid = Rect::from_min_size(pos2(x, cy - 15.0), vec2(84.0, 30.0));
    paint::stroke_width(ui, pal, swid, 0.58);
    x += 84.0 + 6.0;

    // group divider
    paint::divider_v(ui.painter(), x, cy - 15.0, cy + 15.0, pal);
    x += 6.0 + 6.0;

    // undo / redo
    for name in ["arrow-back-up", "arrow-forward-up"] {
        let r = Rect::from_min_size(pos2(x, cy - d / 2.0), vec2(d, d));
        paint::icon_button(ui, icons, pal, r, name, icon_px, BtnState::default());
        x += d + gap;
    }
}

// ---------------------------------------------------------------------------
// 4. Onboarding step — exercises the large end of the type scale (huge title,
//    muted two-line subtitle, one hero visual, a single primary pill in a
//    hairline-separated footer). Restraint + vertical rhythm are the whole test.
// ---------------------------------------------------------------------------
fn onboard(ui: &mut Ui, icons: &IconStore, pal: &Palette, card: Rect) {
    let p = ui.painter().clone();
    paint::glass_panel(&p, card, theme::R_CARD, pal, false);

    // A soft accent glow high in the card, behind the hero tile.
    paint::soft_blob(&p, pos2(card.center().x, card.top() + 120.0), 150.0, pal.accent, 26);

    // Faux window traffic lights, top-left.
    let dots = [
        Color32::from_rgb(0xFF, 0x5F, 0x57),
        Color32::from_rgb(0xFE, 0xBC, 0x2E),
        Color32::from_rgb(0x28, 0xC8, 0x40),
    ];
    for (i, c) in dots.iter().enumerate() {
        p.circle_filled(pos2(card.left() + 24.0 + i as f32 * 19.0, card.top() + 24.0), 6.0, *c);
    }

    // Hero tile: an accent-filled rounded square with a capture glyph.
    let tile = Rect::from_center_size(pos2(card.center().x, card.top() + 128.0), vec2(108.0, 108.0));
    paint::soft_shadow(&p, tile, theme::R_CARD, pal, 1.2);
    p.rect_filled(tile, cr(theme::R_CARD), pal.accent);
    p.rect_filled(
        Rect::from_min_max(tile.left_top(), pos2(tile.right(), tile.center().y)),
        cr(theme::R_CARD),
        pal.accent_hi.linear_multiply(0.12),
    );
    p.rect_stroke(tile, cr(theme::R_CARD), Stroke::new(1.0, Color32::from_white_alpha(28)), StrokeKind::Inside);
    icons.draw(&p, "viewfinder", tile.center(), 52.0, pal.on_accent);

    // Title + two-line subtitle, centred.
    p.text(
        pos2(card.center().x, card.top() + 224.0),
        Align2::CENTER_CENTER,
        "Ready to go.",
        theme::ts_display(),
        pal.text,
    );
    for (i, line) in [
        "Scrozz lives in your menu bar. Press ⇧⌘4 to grab an area,",
        "then drag the capture straight into any app.",
    ]
    .iter()
    .enumerate()
    {
        p.text(
            pos2(card.center().x, card.top() + 266.0 + i as f32 * 24.0),
            Align2::CENTER_CENTER,
            *line,
            theme::ts_subtitle(),
            pal.text_muted,
        );
    }

    // A restrained three-item reassurance row.
    let feats = ["Free forever", "Open source", "No account"];
    let fy = card.top() + 344.0;
    let fw = 150.0;
    let total = fw * feats.len() as f32;
    let mut fx = card.center().x - total / 2.0;
    for f in feats {
        let c = pos2(fx + fw / 2.0, fy);
        icons.draw(&p, "check", pos2(c.x - 46.0, c.y), 15.0, pal.accent_hi);
        p.text(pos2(c.x - 34.0, c.y), Align2::LEFT_CENTER, f, theme::ts_caption(), pal.text_muted);
        fx += fw;
    }

    // Footer bar: hairline, a muted "skip" on the left, one primary pill right.
    let footer_top = card.bottom() - 78.0;
    paint::divider_h(&p, card.left(), card.right(), footer_top, pal);
    let fcy = (footer_top + card.bottom()) / 2.0;
    p.text(
        pos2(card.left() + 26.0, fcy),
        Align2::LEFT_CENTER,
        "Skip",
        theme::ts_button(),
        pal.text_muted,
    );

    // Primary pill (drawn by painter for the fully-rounded accent look; it's a
    // display mock, so it needs no interaction).
    let pill = Rect::from_min_size(pos2(0.0, 0.0), vec2(188.0, 46.0));
    let pill = Rect::from_center_size(pos2(card.right() - 26.0 - pill.width() / 2.0, fcy), pill.size());
    paint::soft_shadow(&p, pill, pill.height() / 2.0, pal, 0.9);
    p.rect_filled(pill, cr(pill.height() / 2.0), pal.accent);
    p.rect_filled(
        Rect::from_min_max(pill.left_top(), pos2(pill.right(), pill.center().y)),
        cr(pill.height() / 2.0),
        pal.accent_hi.linear_multiply(0.14),
    );
    p.rect_stroke(pill, cr(pill.height() / 2.0), Stroke::new(1.0, Color32::from_white_alpha(26)), StrokeKind::Inside);
    p.text(
        pos2(pill.center().x - 12.0, pill.center().y),
        Align2::CENTER_CENTER,
        "Start Capturing",
        theme::ts_button(),
        pal.on_accent,
    );
    icons.draw(&p, "chevron-right", pos2(pill.right() - 22.0, pill.center().y), 17.0, pal.on_accent);
}
