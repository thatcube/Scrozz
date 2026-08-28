#![allow(missing_docs)]

use std::collections::BTreeMap;

use egui::{
    Align2, Color32, CornerRadius, Id, Pos2, Rect, Response, Sense, Stroke, StrokeKind,
    TextureHandle, Ui, WidgetInfo, WidgetType, pos2, vec2,
};
use scrozz_core::selection::SelectionMode;
use scrozz_core::{DisplayId, LogicalPoint, LogicalRect};

use crate::{
    paint as chrome,
    theme::{Radius, Space, Text, Theme, corner},
};

use super::{
    frozen::FrozenDesktop,
    geom::DisplayLayout,
    hud::{HudEntry, HudModel},
    magnifier::{MagnifierConfig, MagnifierGrid},
    state::SelectionState,
};

const HUD_HEIGHT: f32 = 84.0;
const BUTTON_HEIGHT: f32 = 34.0;
const HANDLE_SIZE: f32 = 10.0;
const FULL_UV: Rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayAction {
    None,
    Mode(SelectionMode),
    Confirm,
}

pub(crate) struct PaintResult {
    pub canvas: Response,
    pub action: OverlayAction,
    pub pointer_over_controls: bool,
}

#[derive(Clone, Copy)]
pub(super) struct OverlayView<'a> {
    pub layout: &'a DisplayLayout,
    pub frozen: &'a FrozenDesktop,
    pub textures: &'a BTreeMap<String, TextureHandle>,
    pub surface: LogicalRect,
    pub display: Option<&'a DisplayId>,
    pub show_hud: bool,
    pub primary_modifier: bool,
}

struct HudPaintResult {
    action: OverlayAction,
    pointer_over_hud: bool,
}

pub(super) fn draw_overlay(
    ui: &mut Ui,
    theme: &Theme,
    state: &SelectionState,
    hud: &HudModel,
    view: OverlayView<'_>,
) -> PaintResult {
    let desired = super::geom::size_to_vec2(view.surface.size);
    let (canvas_rect, response) = ui.allocate_exact_size(desired, Sense::click_and_drag());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, state.overlay_label()));
    let painter = ui.painter().clone();
    if state.options_ref().freeze
        && matches!(state.mode(), SelectionMode::Region | SelectionMode::Display)
    {
        draw_backdrop(
            &painter,
            canvas_rect,
            theme,
            view,
            state.mode() == SelectionMode::Display,
        );
    }
    let paints_focus = view.display.is_none() || state.focus_display() == view.display;
    let focus = if state.mode() == SelectionMode::AllDisplays {
        Some(view.surface)
    } else if paints_focus {
        state.focus_rect()
    } else {
        None
    };
    if should_draw_scrim(state.mode(), focus.is_some()) {
        draw_scrim(&painter, canvas_rect, focus, view.surface, theme);
    }
    let mut confirmation = HudPaintResult {
        action: OverlayAction::None,
        pointer_over_hud: false,
    };
    if state.mode() == SelectionMode::Region {
        if state.options_ref().draws_crosshair(view.primary_modifier)
            && (view.display.is_none() || state.pointer_display() == view.display)
            && let Some(pointer) = state.pointer()
        {
            draw_crosshair(&painter, canvas_rect, view.surface, pointer, theme);
        }
        if paints_focus && let Some(region) = state.region() {
            draw_selection(
                &painter,
                canvas_rect,
                view.surface,
                region,
                state.constraint().exact.is_none(),
                theme,
            );
            confirmation = draw_size_readout(
                ui,
                canvas_rect,
                view.layout,
                view.surface,
                state,
                region,
                theme,
            );
        }
    } else if paints_focus && let Some(rect) = state.focus_rect() {
        draw_target_highlight(&painter, canvas_rect, view.surface, rect, state, theme);
    }
    if state.options_ref().shows_magnifier(view.primary_modifier)
        && state.mode() == SelectionMode::Region
        && (view.display.is_none() || state.pointer_display() == view.display)
        && let Some(grid) = magnifier_grid(state, view.frozen)
    {
        draw_magnifier(
            &painter,
            canvas_rect,
            view.surface,
            state.pointer().expect("pointer exists"),
            &grid,
            theme,
        );
    }
    let hud = if view.show_hud && state.options_ref().hud {
        draw_hud(ui, canvas_rect, theme, hud, state)
    } else {
        HudPaintResult {
            action: OverlayAction::None,
            pointer_over_hud: false,
        }
    };
    PaintResult {
        canvas: response,
        action: if confirmation.action == OverlayAction::Confirm {
            confirmation.action
        } else {
            hud.action
        },
        pointer_over_controls: confirmation.pointer_over_hud || hud.pointer_over_hud,
    }
}

fn magnifier_grid(state: &SelectionState, frozen: &FrozenDesktop) -> Option<MagnifierGrid> {
    let point = state.pointer()?;
    let display = state.pointer_display().or_else(|| state.active_display())?;
    let frame = frozen.frame(display)?;
    Some(super::magnifier::sample(
        frame,
        point,
        MagnifierConfig {
            zoom: state.options_ref().magnifier_zoom,
            ..MagnifierConfig::default()
        },
    ))
}

fn draw_backdrop(
    painter: &egui::Painter,
    canvas_rect: Rect,
    theme: &Theme,
    view: OverlayView<'_>,
    decorate_displays: bool,
) {
    if view.frozen.is_empty() {
        return;
    }
    let palette = theme.palette;
    painter.rect_filled(canvas_rect, CornerRadius::ZERO, Color32::BLACK);
    for display in view
        .layout
        .displays()
        .iter()
        .filter(|display| view.display.is_none_or(|id| display.id == *id))
    {
        let rect = translate(
            DisplayLayout::canvas_rect_in(view.surface, display.bounds),
            canvas_rect.min,
        );
        if let Some(texture) = view.textures.get(&display.id.0) {
            painter.image(texture.id(), rect, FULL_UV, Color32::WHITE);
        } else if let Some(frame) = view.frozen.frame(&display.id) {
            painter.image(
                frame.upload(painter.ctx()).id(),
                rect,
                FULL_UV,
                Color32::WHITE,
            );
        } else {
            painter.rect_filled(rect, corner(Radius::CARD), palette.card_fill_raised);
        }
        if decorate_displays {
            painter.rect_stroke(
                rect,
                corner(Radius::CARD),
                Stroke::new(1.0, palette.hairline),
                StrokeKind::Inside,
            );
            let label_rect = Rect::from_min_size(
                pos2(rect.left() + Space::LG, rect.bottom() - 34.0),
                vec2((display.name.len() as f32 * 8.0 + 30.0).max(96.0), 24.0),
            );
            chrome::glass_panel(painter, label_rect, Radius::BUTTON, &palette, false);
            painter.text(
                label_rect.center(),
                Align2::CENTER_CENTER,
                &display.name,
                theme.font(Text::Caption),
                palette.text,
            );
        }
    }
}

fn should_draw_scrim(mode: SelectionMode, has_focus: bool) -> bool {
    mode != SelectionMode::Region || has_focus
}

fn draw_scrim(
    painter: &egui::Painter,
    canvas_rect: Rect,
    focus: Option<LogicalRect>,
    surface: LogicalRect,
    theme: &Theme,
) {
    let scrim = Color32::from_black_alpha(if theme.palette.is_dark() { 180 } else { 132 });
    let Some(region) = focus else {
        painter.rect_filled(canvas_rect, CornerRadius::ZERO, scrim);
        return;
    };
    let rect = translate(
        DisplayLayout::canvas_rect_in(surface, region),
        canvas_rect.min,
    );
    let top = Rect::from_min_max(canvas_rect.min, pos2(canvas_rect.right(), rect.top()));
    let left = Rect::from_min_max(
        pos2(canvas_rect.left(), rect.top()),
        pos2(rect.left(), rect.bottom()),
    );
    let right = Rect::from_min_max(
        pos2(rect.right(), rect.top()),
        pos2(canvas_rect.right(), rect.bottom()),
    );
    let bottom = Rect::from_min_max(pos2(canvas_rect.left(), rect.bottom()), canvas_rect.max);
    for band in [top, left, right, bottom] {
        if band.width() > 0.0 && band.height() > 0.0 {
            painter.rect_filled(band, CornerRadius::ZERO, scrim);
        }
    }
}

fn draw_target_highlight(
    painter: &egui::Painter,
    canvas_rect: Rect,
    surface: LogicalRect,
    rect: LogicalRect,
    state: &SelectionState,
    theme: &Theme,
) {
    let palette = theme.palette;
    let rect = translate(
        DisplayLayout::canvas_rect_in(surface, rect),
        canvas_rect.min,
    );
    painter.rect_filled(
        rect,
        corner(Radius::CARD),
        Color32::from_rgba_unmultiplied(
            palette.accent.r(),
            palette.accent.g(),
            palette.accent.b(),
            20,
        ),
    );
    painter.rect_stroke(
        rect,
        corner(Radius::CARD),
        Stroke::new(2.0, palette.accent),
        StrokeKind::Inside,
    );
    painter.rect_stroke(
        rect.expand(1.0),
        corner(Radius::CARD + 1.0),
        Stroke::new(1.0, Color32::from_white_alpha(140)),
        StrokeKind::Inside,
    );
    if let Some(label) = target_label(state) {
        let size = vec2((label.len() as f32 * 8.5 + 32.0).max(120.0), 28.0);
        let origin = pos2(
            rect.left().clamp(
                canvas_rect.left() + Space::SM,
                canvas_rect.right() - size.x - Space::SM,
            ),
            (rect.top() - size.y - 10.0).max(canvas_rect.top() + Space::SM),
        );
        let label_rect = Rect::from_min_size(origin, size);
        chrome::glass_panel(painter, label_rect, Radius::BUTTON, &palette, true);
        painter.text(
            label_rect.center(),
            Align2::CENTER_CENTER,
            label,
            theme.font(Text::Caption),
            palette.text,
        );
    }
}

fn draw_crosshair(
    painter: &egui::Painter,
    canvas_rect: Rect,
    surface: LogicalRect,
    pointer: LogicalPoint,
    theme: &Theme,
) {
    let point = DisplayLayout::canvas_pos_in(surface, pointer) + canvas_rect.min.to_vec2();
    let stroke = Stroke::new(1.0, Color32::from_white_alpha(120));
    painter.line_segment(
        [
            pos2(canvas_rect.left(), point.y),
            pos2(canvas_rect.right(), point.y),
        ],
        stroke,
    );
    painter.line_segment(
        [
            pos2(point.x, canvas_rect.top()),
            pos2(point.x, canvas_rect.bottom()),
        ],
        stroke,
    );
    painter.circle_stroke(point, 5.0, Stroke::new(1.5, theme.palette.accent));
}

fn draw_selection(
    painter: &egui::Painter,
    canvas_rect: Rect,
    surface: LogicalRect,
    region: LogicalRect,
    resizable: bool,
    theme: &Theme,
) {
    let palette = theme.palette;
    let rect = translate(
        DisplayLayout::canvas_rect_in(surface, region),
        canvas_rect.min,
    );
    painter.rect_stroke(
        rect,
        corner(Radius::CARD),
        Stroke::new(2.0, palette.accent),
        StrokeKind::Inside,
    );
    painter.rect_stroke(
        rect.expand(1.0),
        corner(Radius::CARD + 1.0),
        Stroke::new(1.0, Color32::from_white_alpha(140)),
        StrokeKind::Inside,
    );
    if resizable {
        for handle in handles(rect) {
            painter.rect_filled(handle, corner(Radius::CHIP), palette.on_accent);
            painter.rect_stroke(
                handle,
                corner(Radius::CHIP),
                Stroke::new(1.0, palette.accent),
                StrokeKind::Inside,
            );
        }
    }
}

fn draw_size_readout(
    ui: &mut Ui,
    canvas_rect: Rect,
    layout: &DisplayLayout,
    surface: LogicalRect,
    state: &SelectionState,
    region: LogicalRect,
    theme: &Theme,
) -> HudPaintResult {
    let Some(display_id) = state.focus_display() else {
        return HudPaintResult {
            action: OverlayAction::None,
            pointer_over_hud: false,
        };
    };
    let Some(display) = layout.display(display_id) else {
        return HudPaintResult {
            action: OverlayAction::None,
            pointer_over_hud: false,
        };
    };
    let pixels = region.to_physical(display.scale);
    let label = format!(
        "{} × {} px · Capture",
        pixels.pixel_width(),
        pixels.pixel_height()
    );
    let region_rect = translate(
        DisplayLayout::canvas_rect_in(surface, region),
        canvas_rect.min,
    );
    let size = vec2((label.len() as f32 * 8.0 + 28.0).max(138.0), 32.0);
    let mut origin = pos2(region_rect.left(), region_rect.top() - size.y - 10.0);
    if origin.y < canvas_rect.top() + Space::SM {
        origin.y = region_rect.bottom() + 10.0;
    }
    origin.x = origin.x.clamp(
        canvas_rect.left() + Space::SM,
        canvas_rect.right() - size.x - Space::SM,
    );
    let rect = Rect::from_min_size(origin, size);
    let response = ui.interact(rect, Id::new("selector-confirm"), Sense::click());
    response.widget_info(|| {
        WidgetInfo::labeled(
            WidgetType::Button,
            true,
            format!(
                "Capture selected region, {} by {} pixels",
                pixels.pixel_width(),
                pixels.pixel_height()
            ),
        )
    });
    chrome::glass_panel(ui.painter(), rect, Radius::BUTTON, &theme.palette, true);
    ui.painter().rect_stroke(
        rect,
        corner(Radius::BUTTON),
        Stroke::new(
            1.0,
            if response.hovered() {
                theme.palette.focus_ring
            } else {
                theme.palette.hairline
            },
        ),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme.font(Text::Button),
        theme.palette.text,
    );
    HudPaintResult {
        action: if response.clicked() {
            OverlayAction::Confirm
        } else {
            OverlayAction::None
        },
        pointer_over_hud: response.hovered(),
    }
}

fn draw_magnifier(
    painter: &egui::Painter,
    canvas_rect: Rect,
    surface: LogicalRect,
    pointer: LogicalPoint,
    grid: &MagnifierGrid,
    theme: &Theme,
) {
    let point = DisplayLayout::canvas_pos_in(surface, pointer) + canvas_rect.min.to_vec2();
    let cell = grid.zoom as f32 / painter.ctx().pixels_per_point().max(f32::EPSILON);
    let side = grid.side as f32 * cell;
    let size = vec2(side + 18.0, side + 18.0);
    let mut origin = pos2(point.x + 22.0, point.y + 22.0);
    if origin.x + size.x > canvas_rect.right() - Space::SM {
        origin.x = point.x - size.x - 22.0;
    }
    if origin.y + size.y > canvas_rect.bottom() - Space::SM {
        origin.y = point.y - size.y - 22.0;
    }
    origin.x = origin.x.clamp(
        canvas_rect.left() + Space::SM,
        canvas_rect.right() - size.x - Space::SM,
    );
    origin.y = origin.y.clamp(
        canvas_rect.top() + Space::SM,
        canvas_rect.bottom() - size.y - Space::SM,
    );
    let rect = Rect::from_min_size(origin, size);
    chrome::glass_panel(painter, rect, Radius::CARD, &theme.palette, true);
    let pixels = Rect::from_min_max(rect.min + vec2(9.0, 9.0), rect.max - vec2(9.0, 9.0));
    for (index, sample) in grid.cells.iter().enumerate() {
        let x = index % grid.side;
        let y = index / grid.side;
        let cell_rect = Rect::from_min_size(
            pos2(
                pixels.left() + x as f32 * cell,
                pixels.top() + y as f32 * cell,
            ),
            vec2(cell, cell),
        );
        painter.rect_filled(cell_rect, CornerRadius::ZERO, sample.pixel.to_color32());
    }
    let centre = grid.centre_index();
    let x = centre % grid.side;
    let y = centre / grid.side;
    let focus = Rect::from_min_size(
        pos2(
            pixels.left() + x as f32 * cell,
            pixels.top() + y as f32 * cell,
        ),
        vec2(cell, cell),
    );
    painter.rect_stroke(
        focus.expand(1.0),
        CornerRadius::ZERO,
        Stroke::new(2.0, theme.palette.accent),
        StrokeKind::Inside,
    );
}

fn draw_hud(
    ui: &mut Ui,
    canvas_rect: Rect,
    theme: &Theme,
    hud: &HudModel,
    state: &SelectionState,
) -> HudPaintResult {
    let entries = hud.entries();
    let width = 98.0 * entries.len() as f32
        + Space::SM * (entries.len().saturating_sub(1)) as f32
        + Space::XL * 2.0;
    let rect = Rect::from_min_size(
        pos2(
            canvas_rect.center().x - width / 2.0,
            canvas_rect.top() + Space::LG,
        ),
        vec2(width, HUD_HEIGHT),
    );
    chrome::glass_panel(ui.painter(), rect, Radius::CARD, &theme.palette, true);
    ui.painter().text(
        pos2(rect.center().x, rect.top() + 18.0),
        Align2::CENTER_CENTER,
        hud_title(state),
        theme.font(Text::Caption),
        theme.palette.text_muted,
    );
    let mut action = OverlayAction::None;
    let pointer_over_hud = ui
        .ctx()
        .pointer_latest_pos()
        .is_some_and(|pointer| rect.contains(pointer));
    let mut x = rect.left() + Space::XL;
    for entry in entries {
        let button_rect =
            Rect::from_min_size(pos2(x, rect.top() + 32.0), vec2(98.0, BUTTON_HEIGHT));
        let response = hud_button(ui, button_rect, theme, entry);
        if response.clicked() && entry.enabled {
            action = OverlayAction::Mode(entry.mode);
        }
        x += 98.0 + Space::SM;
    }
    HudPaintResult {
        action,
        pointer_over_hud,
    }
}

fn hud_title(state: &SelectionState) -> String {
    let mut parts = vec![format!("{} mode", state.mode().label())];
    if let Some(exact) = state.options_ref().constraint.exact {
        parts.push(format!("{} × {}", exact.width as i32, exact.height as i32));
    }
    if let Some(ratio) = state.options_ref().constraint.aspect.value() {
        parts.push(format!("ratio {:.2}:1", ratio));
    }
    parts.join(" · ")
}

fn hud_button(ui: &mut Ui, rect: Rect, theme: &Theme, entry: HudEntry) -> Response {
    let response = ui.interact(
        rect,
        Id::new(("selector-hud", entry.mode.slug())),
        Sense::click(),
    );
    response.widget_info(|| {
        WidgetInfo::selected(
            WidgetType::RadioButton,
            entry.enabled,
            entry.selected,
            if entry.enabled {
                format!("{}, {}", entry.label, entry.description)
            } else {
                format!("{}, unavailable", entry.label)
            },
        )
    });
    let palette = theme.palette;
    let fill = if entry.selected {
        palette.accent
    } else if response.hovered() && entry.enabled {
        palette.hover
    } else {
        palette.card_fill_raised
    };
    ui.painter().rect_filled(rect, corner(Radius::BUTTON), fill);
    ui.painter().rect_stroke(
        rect,
        corner(Radius::BUTTON),
        Stroke::new(
            1.0,
            if entry.focused {
                palette.focus_ring
            } else {
                palette.hairline
            },
        ),
        StrokeKind::Inside,
    );
    let colour = if entry.selected {
        palette.on_accent
    } else if entry.enabled {
        palette.text
    } else {
        palette.text_faint
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        entry.label,
        theme.font(Text::Button),
        colour,
    );
    response
}

fn handles(rect: Rect) -> [Rect; 8] {
    [
        Rect::from_center_size(rect.left_top(), vec2(HANDLE_SIZE, HANDLE_SIZE)),
        Rect::from_center_size(
            pos2(rect.center().x, rect.top()),
            vec2(HANDLE_SIZE, HANDLE_SIZE),
        ),
        Rect::from_center_size(rect.right_top(), vec2(HANDLE_SIZE, HANDLE_SIZE)),
        Rect::from_center_size(
            pos2(rect.right(), rect.center().y),
            vec2(HANDLE_SIZE, HANDLE_SIZE),
        ),
        Rect::from_center_size(rect.right_bottom(), vec2(HANDLE_SIZE, HANDLE_SIZE)),
        Rect::from_center_size(
            pos2(rect.center().x, rect.bottom()),
            vec2(HANDLE_SIZE, HANDLE_SIZE),
        ),
        Rect::from_center_size(rect.left_bottom(), vec2(HANDLE_SIZE, HANDLE_SIZE)),
        Rect::from_center_size(
            pos2(rect.left(), rect.center().y),
            vec2(HANDLE_SIZE, HANDLE_SIZE),
        ),
    ]
}

fn translate(rect: Rect, offset: Pos2) -> Rect {
    rect.translate(offset.to_vec2())
}

fn target_label(state: &SelectionState) -> Option<String> {
    match state.mode() {
        SelectionMode::Window => {
            state
                .hovered_window()
                .map(|window| match (&window.title, &window.application) {
                    (Some(title), Some(application)) if !title.is_empty() => {
                        format!("{title} — {application}")
                    }
                    (Some(title), _) if !title.is_empty() => title.clone(),
                    (None, Some(application)) => application.clone(),
                    _ => "Untitled window".to_owned(),
                })
        }
        SelectionMode::Display => state.hovered_display().map(|display| display.name.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_capture_does_not_dim_before_a_region_exists() {
        assert!(!should_draw_scrim(SelectionMode::Region, false));
        assert!(should_draw_scrim(SelectionMode::Region, true));
    }

    #[test]
    fn semantic_targets_keep_their_targeting_scrim() {
        assert!(should_draw_scrim(SelectionMode::Window, false));
        assert!(should_draw_scrim(SelectionMode::Display, false));
    }
}
