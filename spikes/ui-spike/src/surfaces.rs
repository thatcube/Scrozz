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
}

impl Surface {
    pub const ALL: [Surface; 3] = [Surface::Quick, Surface::Menu, Surface::Annotate];

    pub fn key(self) -> &'static str {
        match self {
            Surface::Quick => "quick",
            Surface::Menu => "menu",
            Surface::Annotate => "annotate",
        }
    }

    /// Card size, excluding the shadow margin the app adds around it.
    pub fn size(self) -> Vec2 {
        match self {
            Surface::Quick => vec2(360.0, 300.0),
            Surface::Menu => vec2(296.0, 389.0),
            Surface::Annotate => vec2(754.0, 60.0),
        }
    }

    pub fn show(self, ui: &mut Ui, icons: &IconStore, pal: &Palette, card: Rect) {
        match self {
            Surface::Quick => quick_access(ui, icons, pal, card),
            Surface::Menu => menu(ui, icons, pal, card),
            Surface::Annotate => annotate(ui, icons, pal, card),
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Quick Access Overlay (primary)
// ---------------------------------------------------------------------------
fn quick_access(ui: &mut Ui, icons: &IconStore, pal: &Palette, card: Rect) {
    paint::glass_panel(ui.painter(), card, theme::R_CARD, pal, false);

    let pad = 14.0;
    let inner_l = card.left() + pad;
    let inner_r = card.right() - pad;

    // Thumbnail.
    let thumb = Rect::from_min_size(pos2(inner_l, card.top() + pad), vec2(inner_r - inner_l, 202.0));
    paint::soft_shadow(ui.painter(), thumb, theme::R_THUMB, pal, 0.5);
    paint::thumbnail_mock(ui.painter(), thumb, theme::R_THUMB);

    // Caption scrim + text, sitting inside the bottom of the thumbnail so the
    // gap below stays clear for a floating tooltip.
    paint::bottom_scrim(ui.painter(), thumb, 60.0, theme::R_THUMB, 210);
    let cap_y = thumb.bottom() - 16.0;
    ui.painter().text(
        pos2(thumb.left() + 12.0, cap_y),
        Align2::LEFT_CENTER,
        "Screenshot 2026-01-08 at 14.32",
        theme::ts_label(),
        Color32::from_rgba_unmultiplied(255, 255, 255, 235),
    );
    ui.painter().text(
        pos2(thumb.right() - 12.0, cap_y),
        Align2::RIGHT_CENTER,
        "2048 × 1280",
        theme::ts_caption(),
        Color32::from_rgba_unmultiplied(255, 255, 255, 150),
    );
    ui.painter().rect_stroke(
        thumb,
        cr(theme::R_THUMB),
        Stroke::new(1.0, pal.thumb_border),
        StrokeKind::Inside,
    );

    // Action bar, anchored to the bottom of the card.
    let d = 34.0;
    let gap = 6.0;
    let icon_px = 18.0;
    let bar_cy = card.bottom() - 14.0 - d / 2.0;
    let names = ["grip-vertical", "copy", "device-floppy", "pencil", "pin", "cloud-upload"];
    let mut x = inner_l;
    for (i, name) in names.iter().enumerate() {
        let r = Rect::from_min_size(pos2(x, bar_cy - d / 2.0), vec2(d, d));
        let st = if i == 1 { BtnState::hover() } else { BtnState::default() };
        paint::icon_button(ui, icons, pal, r, name, icon_px, st);
        if i == 1 {
            tooltip(ui, pal, pos2(r.center().x, r.top() - 8.0), "Copy");
        }
        x += d + gap;
    }
    // Close, far right.
    let close = Rect::from_min_size(pos2(inner_r - d, bar_cy - d / 2.0), vec2(d, d));
    paint::icon_button(ui, icons, pal, close, "x", icon_px, BtnState::default());
}

fn tooltip(ui: &mut Ui, pal: &Palette, tip_center_top: egui::Pos2, text: &str) {
    let font = theme::ts_caption();
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, pal.text);
    let pad = vec2(10.0, 6.0);
    let size = galley.size() + pad * 2.0;
    let rect = Rect::from_min_size(
        pos2(tip_center_top.x - size.x / 2.0, tip_center_top.y - size.y - 6.0),
        size,
    );
    paint::soft_shadow(ui.painter(), rect, 8.0, pal, 0.6);
    ui.painter().rect_filled(rect, cr(8.0), pal.card_fill_raised);
    ui.painter().rect_stroke(rect, cr(8.0), Stroke::new(1.0, pal.hairline), StrokeKind::Inside);
    ui.painter().galley(rect.min + pad, galley, pal.text);
    // little downward nub
    let cx = tip_center_top.x;
    let by = rect.bottom();
    ui.painter().add(egui::Shape::convex_polygon(
        vec![pos2(cx - 5.0, by - 0.5), pos2(cx + 5.0, by - 0.5), pos2(cx, by + 5.0)],
        pal.card_fill_raised,
        Stroke::NONE,
    ));
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
