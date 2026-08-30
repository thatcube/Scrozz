//! The native child-window surface used by one pinned capture.
//!
//! A pin is deliberately not a card with different styling. It owns a native
//! viewport, has no synthetic chrome for window captures (D9), and can become
//! mouse-transparent when locked. This module only draws and interprets one
//! frame; geometry reconciliation and persistence remain in the overlay host.

use egui::{
    Align, Align2, Button, Color32, CornerRadius, FontId, Key, Layout, Pos2, Rect, Response, Sense,
    Slider, Stroke, StrokeKind, TextureId, Ui, UiBuilder, Vec2, ViewportCommand, WidgetInfo,
    WidgetType,
};
use scrozz_core::{
    DisplaySet, LogicalPoint, LogicalRect, LogicalSize, MAX_OPACITY, MIN_OPACITY, NudgeStep,
    PinBorder, PinChrome, PinDirection, PinnedSurface,
};

use crate::theme::{Appearance, Theme};

const TOOLBAR_HEIGHT: f32 = 42.0;
const TOOLBAR_INSET: f32 = 8.0;
const TOOLBAR_MIN_WIDTH: f32 = 340.0;
const CONTROL_HEIGHT: f32 = 28.0;
const LOCK_BADGE_HEIGHT: f32 = 27.0;

/// What changed while drawing a pinned capture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PinFrameResponse {
    /// Durable state changed and should be persisted.
    pub changed: bool,
    /// The pin should close.
    pub close: bool,
    /// The user requested a nudge that the compositor cannot honor.
    pub positioning_unavailable: bool,
}

/// Inputs needed to render one pinned capture.
pub struct PinFrame<'a> {
    /// Stable, human-readable capture name.
    pub name: &'a str,
    /// Uploaded capture texture, when ready.
    pub texture: Option<TextureId>,
    /// Explicit source-pixel failure; never rendered as a success-shaped blank.
    pub content_error: Option<&'a str>,
    /// Pure geometry and display state.
    pub surface: &'a mut PinnedSurface,
    /// Displays currently known to the process.
    pub displays: &'a DisplaySet,
    /// Whether this backend can read and set native positions.
    pub positioning: bool,
    /// Whether opacity is applied to the native window.
    pub native_opacity: bool,
    /// Whether the whole native window can become pointer-transparent.
    pub click_through: bool,
    /// Product theme.
    pub theme: &'a Theme,
    /// Hover behavior, overridable by deterministic rendering harnesses.
    pub chrome_visibility: ChromeVisibility,
}

/// Whether hover chrome should be inferred from live input or forced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChromeVisibility {
    /// Follow pointer and keyboard focus.
    #[default]
    Auto,
    /// Keep controls visible, for deterministic goldens and diagnostics.
    Visible,
    /// Keep controls hidden.
    Hidden,
}

/// Draw one pinned capture into its native viewport.
pub fn draw(ui: &mut Ui, mut frame: PinFrame<'_>) -> PinFrameResponse {
    let mut output = PinFrameResponse::default();
    let before = frame.surface.state().clone();
    let rect = ui.max_rect();
    let state = frame.surface.state().clone();
    let opacity = state.opacity.get() as f32;
    let content_alpha = if frame.native_opacity { 1.0 } else { opacity };
    let radius = state.chrome.corner_radius as f32;
    // A window capture's outermost pixels contain its native antialiasing and
    // shadow. Drawing beyond the viewport clips exactly those D9 edges.
    let body_rect = rect;

    let body = if let Some(texture) = frame.texture {
        let mut image =
            egui::Image::new((texture, body_rect.size())).fit_to_exact_size(body_rect.size());
        if radius > 0.0 {
            image = image.corner_radius(CornerRadius::same(radius.round() as u8));
        }
        let image = image
            .tint(Color32::from_white_alpha(
                (content_alpha * 255.0).round() as u8
            ))
            .sense(Sense::click_and_drag());
        ui.put(body_rect, image)
    } else {
        let response = ui.interact(
            rect,
            ui.id().with("pinned-content"),
            Sense::click_and_drag(),
        );
        let fill = alpha(
            match frame.theme.palette.appearance {
                Appearance::Dark => Color32::from_rgb(28, 30, 40),
                Appearance::Light => Color32::from_rgb(238, 239, 245),
            },
            content_alpha,
        );
        ui.painter()
            .rect_filled(body_rect, CornerRadius::same(radius.round() as u8), fill);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            frame
                .content_error
                .unwrap_or("Capture pixels are unavailable"),
            FontId::proportional(13.0),
            alpha(frame.theme.palette.text, content_alpha),
        );
        response
    };
    body.widget_info(|| WidgetInfo::labeled(WidgetType::Image, true, frame.name));

    // An egui context menu cannot escape a tiny native viewport. When the
    // toolbar cannot fit, keep the whole image as a direct recovery target;
    // unpinning does not delete the capture from History.
    let secondary_released_in_pin = ui.input(|input| {
        input.events.iter().any(|event| {
            matches!(
                event,
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Secondary,
                    pressed: false,
                    ..
                } if rect.contains(*pos)
            )
        })
    });
    if !state.locked {
        if toolbar_fits(rect) {
            body.context_menu(|ui| {
                if ui.button("Close pinned capture").clicked() {
                    output.close = true;
                    ui.close();
                }
                if ui.button("Reset to original size").clicked() {
                    reset_scale(ui, frame.surface, frame.displays);
                    output.changed = true;
                    ui.close();
                }
            });
        } else if secondary_released_in_pin {
            output.close = true;
        }
    }

    if let Some(border) = state.chrome.border {
        ui.painter().rect_stroke(
            rect.shrink(0.5),
            CornerRadius::same(radius.round() as u8),
            Stroke::new(
                border.width as f32,
                alpha(frame.theme.palette.thumb_border, content_alpha),
            ),
            StrokeKind::Inside,
        );
    }

    if !state.locked && body.drag_started() {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }

    let viewport_focused =
        ui.input(|input| input.viewport().focused.is_some_and(std::convert::identity));
    if state.locked {
        draw_lock_badge(
            ui,
            rect,
            frame.theme,
            content_alpha,
            frame.surface.lock_escapes(),
        );
    } else if chrome_is_visible(
        frame.chrome_visibility,
        body.hovered(),
        viewport_focused,
        ui.ctx().memory(|memory| memory.focused().is_some()),
    ) {
        draw_toolbar(ui, rect, &mut frame, &mut output);
    }

    // Controls get first refusal on keys (notably slider arrows and menu
    // Escape); unconsumed keys retain their window-level meaning.
    handle_keys(
        ui,
        frame.surface,
        frame.displays,
        frame.positioning,
        &mut output,
    );

    output.changed |= *frame.surface.state() != before;
    output
}

fn chrome_is_visible(
    visibility: ChromeVisibility,
    hovered: bool,
    viewport_focused: bool,
    widget_focused: bool,
) -> bool {
    matches!(visibility, ChromeVisibility::Visible)
        || matches!(visibility, ChromeVisibility::Auto)
            && (hovered || viewport_focused || widget_focused)
}

fn toolbar_fits(window: Rect) -> bool {
    window.width() >= TOOLBAR_MIN_WIDTH && window.height() >= TOOLBAR_HEIGHT + TOOLBAR_INSET * 2.0
}

fn handle_keys(
    ui: &mut Ui,
    surface: &mut PinnedSurface,
    displays: &DisplaySet,
    positioning: bool,
    output: &mut PinFrameResponse,
) {
    if surface.state().locked {
        return;
    }

    if ui.input(|input| input.key_pressed(Key::Escape)) {
        output.close = true;
        return;
    }

    let pressed = [
        (Key::ArrowLeft, PinDirection::Left),
        (Key::ArrowRight, PinDirection::Right),
        (Key::ArrowUp, PinDirection::Up),
        (Key::ArrowDown, PinDirection::Down),
    ]
    .into_iter()
    .find_map(|(key, direction)| {
        ui.input(|input| input.key_pressed(key))
            .then_some(direction)
    });

    let Some(direction) = pressed else {
        return;
    };
    if !positioning {
        output.positioning_unavailable = true;
        return;
    }

    let modifiers = ui.input(|input| input.modifiers);
    let nudge = if modifiers.shift {
        NudgeStep::Edge
    } else if modifiers.alt {
        NudgeStep::Fine
    } else {
        NudgeStep::Normal
    };
    surface.nudge(direction, nudge, displays);
    send_geometry(ui, surface);
}

fn draw_toolbar(
    ui: &mut Ui,
    window: Rect,
    frame: &mut PinFrame<'_>,
    output: &mut PinFrameResponse,
) {
    let toolbar = Rect::from_min_max(
        window.min + Vec2::splat(TOOLBAR_INSET),
        Pos2::new(
            window.max.x - TOOLBAR_INSET,
            (window.min.y + TOOLBAR_INSET + TOOLBAR_HEIGHT).min(window.max.y),
        ),
    );
    let palette = &frame.theme.palette;
    let toolbar_fill = match palette.appearance {
        Appearance::Dark => Color32::from_rgba_unmultiplied(19, 20, 28, 232),
        Appearance::Light => Color32::from_rgba_unmultiplied(250, 250, 253, 235),
    };
    ui.painter()
        .rect_filled(toolbar, CornerRadius::same(10), toolbar_fill);
    ui.painter().rect_stroke(
        toolbar,
        CornerRadius::same(10),
        Stroke::new(1.0, palette.hairline),
        StrokeKind::Inside,
    );

    ui.scope_builder(
        UiBuilder::new().max_rect(toolbar.shrink2(Vec2::new(8.0, 7.0))),
        |ui| {
            ui.style_mut().spacing.item_spacing = Vec2::new(5.0, 0.0);
            ui.style_mut().visuals.widgets.inactive.fg_stroke.color = palette.text;
            ui.style_mut().visuals.widgets.hovered.fg_stroke.color = palette.text;
            ui.style_mut().visuals.widgets.active.fg_stroke.color = palette.text;
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let mut opacity = frame.surface.state().opacity.get();
                let opacity_response = ui
                    .add_sized(
                        [78.0, CONTROL_HEIGHT],
                        Slider::new(&mut opacity, MIN_OPACITY..=MAX_OPACITY)
                            .show_value(false)
                            .text("Opacity"),
                    )
                    .on_hover_text(format!("Opacity: {:.0}%", opacity * 100.0));
                if opacity_response.changed() {
                    frame.surface.set_opacity(opacity);
                }

                if compact_button(ui, "-", "Decrease pinned capture size").clicked() {
                    change_scale(ui, frame.surface, frame.displays, 1.0 / 1.1);
                }
                let scale_percent = format!("{:.0}%", frame.surface.state().scale.get() * 100.0);
                ui.label(scale_percent)
                    .on_hover_text("Pinned capture scale");
                if compact_button(ui, "+", "Increase pinned capture size").clicked() {
                    change_scale(ui, frame.surface, frame.displays, 1.1);
                }

                if frame.surface.allows_synthetic_chrome() {
                    ui.menu_button("Style", |ui| {
                        let mut chrome = frame.surface.state().chrome;
                        let mut changed = ui
                            .add(
                                Slider::new(&mut chrome.corner_radius, 0.0..=32.0)
                                    .text("Corner radius"),
                            )
                            .changed();
                        changed |= ui.checkbox(&mut chrome.shadow, "Shadow").changed();
                        let mut border = chrome.border.is_some();
                        if ui.checkbox(&mut border, "Border").changed() {
                            chrome.border = border.then(|| PinBorder::new(1.0));
                            changed = true;
                        }
                        if let Some(mut outline) = chrome.border
                            && ui
                                .add(
                                    Slider::new(&mut outline.width, 0.5..=6.0).text("Border width"),
                                )
                                .changed()
                        {
                            chrome.border = Some(outline);
                            changed = true;
                        }
                        if changed {
                            frame.surface.set_chrome(PinChrome::new(
                                chrome.corner_radius,
                                chrome.shadow,
                                chrome.border,
                            ));
                        }
                    });
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if compact_button(ui, "Close", "Close pinned capture").clicked() {
                        output.close = true;
                    }
                    let can_lock = frame.click_through && frame.surface.has_lock_escape();
                    let lock = ui.add_enabled(
                        can_lock,
                        Button::new("Lock").min_size(Vec2::new(44.0, CONTROL_HEIGHT)),
                    );
                    let lock = if !frame.click_through {
                        lock.on_disabled_hover_text(
                            "Click-through is unavailable in this desktop session",
                        )
                    } else if !frame.surface.has_lock_escape() {
                        lock.on_disabled_hover_text(
                            "Start command forwarding, enable the tray menu, or register an Unlock Pinned Captures hotkey first",
                        )
                    } else {
                        lock.on_hover_text("Lock and pass pointer input through")
                    };
                    if lock.clicked() && frame.surface.set_locked(true).is_ok() {
                        ui.ctx()
                            .send_viewport_cmd(ViewportCommand::MousePassthrough(true));
                    }
                });
            });
        },
    );
}

fn compact_button(ui: &mut Ui, label: &str, help: &str) -> Response {
    ui.add_sized(
        [if label.len() > 2 { 44.0 } else { 27.0 }, CONTROL_HEIGHT],
        Button::new(label),
    )
    .on_hover_text(help)
}

fn change_scale(ui: &Ui, surface: &mut PinnedSurface, displays: &DisplaySet, factor: f64) {
    surface.set_scale(surface.state().scale.get() * factor, displays);
    send_geometry(ui, surface);
}

fn reset_scale(ui: &Ui, surface: &mut PinnedSurface, displays: &DisplaySet) {
    surface.set_scale(1.0, displays);
    send_geometry(ui, surface);
}

fn send_geometry(ui: &Ui, surface: &PinnedSurface) {
    let frame = viewport_rect(surface.state().frame, ui.ctx().zoom_factor());
    ui.ctx()
        .send_viewport_cmd(ViewportCommand::OuterPosition(frame.min));
    ui.ctx()
        .send_viewport_cmd(ViewportCommand::InnerSize(frame.size()));
}

fn draw_lock_badge(
    ui: &Ui,
    window: Rect,
    theme: &Theme,
    opacity: f32,
    escapes: &[scrozz_core::LockEscape],
) {
    let width = 202.0_f32.min((window.width() - 16.0).max(0.0));
    if width <= 0.0 || window.height() < LOCK_BADGE_HEIGHT + 8.0 {
        return;
    }
    let rect = Rect::from_min_size(
        Pos2::new(
            window.center().x - width / 2.0,
            window.max.y - LOCK_BADGE_HEIGHT - 8.0,
        ),
        Vec2::new(width, LOCK_BADGE_HEIGHT),
    );
    let fill = match theme.palette.appearance {
        Appearance::Dark => Color32::from_rgba_unmultiplied(12, 13, 18, 218),
        Appearance::Light => Color32::from_rgba_unmultiplied(250, 250, 253, 226),
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(8), alpha(fill, opacity));
    let label = if escapes.contains(&scrozz_core::LockEscape::TrayMenu) {
        "Locked - unlock from the Scrozz menu"
    } else if escapes.contains(&scrozz_core::LockEscape::GlobalHotkey) {
        "Locked - use the global unlock hotkey"
    } else if escapes.contains(&scrozz_core::LockEscape::CommandLine) {
        "Locked - run `scrozz history unlock-pins`"
    } else {
        "Locked"
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(11.0),
        alpha(theme.palette.text, opacity),
    );
    ui.interact(rect, ui.id().with("pin-lock-guidance"), Sense::hover())
        .widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, label));
}

fn alpha(color: Color32, amount: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        ((f32::from(color.a()) * amount.clamp(0.0, 1.0)).round()) as u8,
    )
}

/// Deterministic harness scene for the pinned-window surface.
pub struct PinnedScene {
    locked: bool,
    texture: std::sync::Mutex<Option<egui::TextureHandle>>,
}

impl PinnedScene {
    /// Hover controls over a display/region-style pin.
    #[must_use]
    pub fn hover() -> Self {
        Self {
            locked: false,
            texture: std::sync::Mutex::new(None),
        }
    }

    /// A click-through, window-provenance pin with no synthetic chrome.
    #[must_use]
    pub fn locked() -> Self {
        Self {
            locked: true,
            texture: std::sync::Mutex::new(None),
        }
    }
}

impl crate::harness::Scene for PinnedScene {
    fn name(&self) -> &str {
        if self.locked {
            "pinned-capture-locked"
        } else {
            "pinned-capture-hover"
        }
    }

    fn setup(&self, ctx: &egui::Context) {
        crate::theme::install_fonts(ctx);
        let texture = ctx.load_texture(
            "scrozz.pinned.golden",
            synthetic_capture(crate::harness::DEFAULT_SEED),
            egui::TextureOptions::LINEAR,
        );
        *self.texture.lock().expect("pinned scene texture poisoned") = Some(texture);
    }

    fn ui(&self, ui: &mut Ui, context: &crate::harness::SceneCtx<'_>) {
        let bounds = ui.max_rect();
        let display = scrozz_core::Display {
            id: scrozz_core::DisplayId("golden".into()),
            name: "Golden display".into(),
            bounds: logical_rect(bounds),
            work_area: logical_rect(bounds),
            scale: scrozz_core::ScaleFactor::new(f64::from(context.pixels_per_point)),
            is_primary: true,
        };
        let displays = DisplaySet::new(vec![display.clone()]);
        let policy = if self.locked {
            scrozz_core::PinChromePolicy::Forbidden
        } else {
            scrozz_core::PinChromePolicy::Allowed
        };
        let mut surface = PinnedSurface::on_display(
            scrozz_core::PinId("golden-pin".into()),
            LogicalSize::new(560.0, 360.0),
            &display,
            policy,
            vec![scrozz_core::LockEscape::TrayMenu],
        )
        .expect("golden pin dimensions");
        surface.set_opacity(0.88);
        if self.locked {
            let _ = surface.set_locked(true);
        }

        let texture = self
            .texture
            .lock()
            .expect("pinned scene texture poisoned")
            .as_ref()
            .map(egui::TextureHandle::id);
        let theme = Theme::for_appearance(match context.theme {
            egui::Theme::Dark => Appearance::Dark,
            egui::Theme::Light => Appearance::Light,
        });
        let _ = draw(
            ui,
            PinFrame {
                name: "Quarterly plan",
                texture,
                content_error: None,
                surface: &mut surface,
                displays: &displays,
                positioning: true,
                native_opacity: false,
                click_through: true,
                theme: &theme,
                chrome_visibility: if self.locked {
                    ChromeVisibility::Hidden
                } else {
                    ChromeVisibility::Visible
                },
            },
        );
    }
}

fn synthetic_capture(seed: u64) -> egui::ColorImage {
    const WIDTH: usize = 112;
    const HEIGHT: usize = 72;
    let accent = (seed as u8).wrapping_add(91);
    let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let band = ((x / 14 + y / 12) % 2) as u8;
            pixels.push(Color32::from_rgb(
                32_u8.saturating_add((x * 80 / WIDTH) as u8),
                52_u8.saturating_add((y * 92 / HEIGHT) as u8),
                accent.saturating_add(band * 24),
            ));
        }
    }
    egui::ColorImage::new([WIDTH, HEIGHT], pixels)
}

/// Convert unzoomed harness geometry to the pure core model.
fn logical_rect(rect: Rect) -> LogicalRect {
    LogicalRect::new(
        LogicalPoint::new(f64::from(rect.min.x), f64::from(rect.min.y)),
        LogicalSize::new(f64::from(rect.width()), f64::from(rect.height())),
    )
}

/// Convert egui's zoomed viewport points to native logical window coordinates.
///
/// egui-winit multiplies viewport geometry by both the GUI zoom and native DPI.
/// The durable model intentionally includes only native logical coordinates, so
/// GUI zoom must be removed at the viewport boundary.
#[must_use]
pub fn native_logical_rect(rect: Rect, zoom_factor: f32) -> LogicalRect {
    let zoom = sane_zoom(zoom_factor);
    LogicalRect::new(
        LogicalPoint::new(f64::from(rect.min.x) * zoom, f64::from(rect.min.y) * zoom),
        LogicalSize::new(
            f64::from(rect.width()) * zoom,
            f64::from(rect.height()) * zoom,
        ),
    )
}

/// Convert durable native logical geometry to egui's zoomed viewport points.
#[must_use]
pub fn viewport_rect(rect: LogicalRect, zoom_factor: f32) -> Rect {
    let zoom = sane_zoom(zoom_factor) as f32;
    Rect::from_min_size(
        Pos2::new(rect.origin.x as f32 / zoom, rect.origin.y as f32 / zoom),
        Vec2::new(
            rect.size.width as f32 / zoom,
            rect.size.height as f32 / zoom,
        ),
    )
}

fn sane_zoom(zoom_factor: f32) -> f64 {
    if zoom_factor.is_finite() && zoom_factor > 0.0 {
        f64::from(zoom_factor)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Modifiers, PointerButton};
    use scrozz_core::{Display, DisplayId, LockEscape, PinChromePolicy, PinId, ScaleFactor};

    fn displays() -> DisplaySet {
        DisplaySet::new(vec![Display {
            id: DisplayId("main".into()),
            name: "Main".into(),
            bounds: LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(1440.0, 900.0)),
            work_area: LogicalRect::new(
                LogicalPoint::new(0.0, 24.0),
                LogicalSize::new(1440.0, 876.0),
            ),
            scale: ScaleFactor::new(2.0),
            is_primary: true,
        }])
    }

    #[test]
    fn locked_pins_suppress_keyboard_mutations() {
        let set = displays();
        let mut surface = PinnedSurface::on_display(
            PinId("pin".into()),
            LogicalSize::new(200.0, 100.0),
            &set.displays()[0],
            PinChromePolicy::Allowed,
            vec![LockEscape::TrayMenu],
        )
        .expect("pin dimensions");
        let context = egui::Context::default();
        context.begin_pass(egui::RawInput {
            events: vec![egui::Event::Key {
                key: Key::Escape,
                physical_key: Some(Key::Escape),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        });
        let mut response = PinFrameResponse::default();
        let mut ui = egui::Ui::new(
            context.clone(),
            egui::Id::new("unlocked"),
            UiBuilder::new().max_rect(Rect::from_min_size(Pos2::ZERO, Vec2::splat(200.0))),
        );
        handle_keys(&mut ui, &mut surface, &set, true, &mut response);
        let mut output = context.end_pass();
        output.textures_delta.clear();
        assert!(response.close);

        surface.set_locked(true).unwrap();
        let locked_state = surface.state().clone();
        let context = egui::Context::default();
        context.begin_pass(egui::RawInput {
            events: [Key::Escape, Key::ArrowRight]
                .into_iter()
                .map(|key| egui::Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                })
                .collect(),
            ..Default::default()
        });
        let mut response = PinFrameResponse::default();
        let mut ui = egui::Ui::new(
            context.clone(),
            egui::Id::new("locked"),
            UiBuilder::new().max_rect(Rect::from_min_size(Pos2::ZERO, Vec2::splat(200.0))),
        );
        handle_keys(&mut ui, &mut surface, &set, true, &mut response);
        let mut output = context.end_pass();
        output.textures_delta.clear();
        assert!(!response.close);
        assert!(!response.changed);
        assert_eq!(surface.state(), &locked_state);
    }

    #[test]
    fn focused_pin_viewports_reveal_automatic_chrome() {
        assert!(chrome_is_visible(
            ChromeVisibility::Auto,
            false,
            true,
            false
        ));
        assert!(!chrome_is_visible(
            ChromeVisibility::Auto,
            false,
            false,
            false
        ));
    }

    #[test]
    fn secondary_click_closes_a_pin_too_small_for_its_toolbar() {
        let set = displays();
        let mut surface = PinnedSurface::on_display(
            PinId("tiny".into()),
            LogicalSize::new(200.0, 100.0),
            &set.displays()[0],
            PinChromePolicy::Allowed,
            vec![LockEscape::TrayMenu],
        )
        .expect("pin dimensions");
        surface.set_scale(0.0, &set);
        let size = Vec2::new(
            surface.state().frame.size.width as f32,
            surface.state().frame.size.height as f32,
        );
        assert!(size.y < TOOLBAR_HEIGHT);

        let context = egui::Context::default();
        let point = Pos2::new(2.0, 2.0);
        let event = |pressed| Event::PointerButton {
            pos: point,
            button: PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        let mut pass = |events| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                focused: true,
                events,
                ..Default::default()
            };
            let mut response = None;
            let mut output = context.run_ui(input, |ui| {
                response = Some(draw(
                    ui,
                    PinFrame {
                        name: "Tiny pin",
                        texture: None,
                        content_error: None,
                        surface: &mut surface,
                        displays: &set,
                        positioning: true,
                        native_opacity: false,
                        click_through: true,
                        theme: &Theme::for_appearance(Appearance::Dark),
                        chrome_visibility: ChromeVisibility::Auto,
                    },
                ));
            });
            output.textures_delta.clear();
            response.expect("headless pin draw ran")
        };

        assert!(
            !pass(vec![Event::PointerMoved(point), event(true)]).close,
            "secondary press alone must not close the pin"
        );
        let outside = Pos2::new(size.x + 100.0, size.y + 100.0);
        assert!(
            pass(vec![event(false), Event::PointerMoved(outside)]).close,
            "secondary release anywhere on the tiny pin must close it"
        );
        let outside_event = |pressed| Event::PointerButton {
            pos: outside,
            button: PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        let _ = pass(vec![Event::PointerMoved(outside), outside_event(true)]);
        assert!(
            !pass(vec![outside_event(false), Event::PointerMoved(point)]).close,
            "a release outside the pin must not borrow a later inside pointer position"
        );
    }

    #[test]
    fn secondary_click_does_not_immediately_close_a_toolbar_sized_pin() {
        let set = displays();
        let mut surface = PinnedSurface::on_display(
            PinId("regular".into()),
            LogicalSize::new(560.0, 360.0),
            &set.displays()[0],
            PinChromePolicy::Allowed,
            vec![LockEscape::TrayMenu],
        )
        .expect("pin dimensions");
        let size = Vec2::new(
            surface.state().frame.size.width as f32,
            surface.state().frame.size.height as f32,
        );
        assert!(toolbar_fits(Rect::from_min_size(Pos2::ZERO, size)));

        let context = egui::Context::default();
        let point = Pos2::new(size.x / 2.0, size.y / 2.0);
        let event = |pressed| Event::PointerButton {
            pos: point,
            button: PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        let mut pass = |events| {
            let mut response = None;
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                    focused: true,
                    events,
                    ..Default::default()
                },
                |ui| {
                    response = Some(draw(
                        ui,
                        PinFrame {
                            name: "Regular pin",
                            texture: None,
                            content_error: None,
                            surface: &mut surface,
                            displays: &set,
                            positioning: true,
                            native_opacity: false,
                            click_through: true,
                            theme: &Theme::for_appearance(Appearance::Dark),
                            chrome_visibility: ChromeVisibility::Auto,
                        },
                    ));
                },
            );
            output.textures_delta.clear();
            response.expect("headless pin draw ran")
        };

        let _ = pass(vec![Event::PointerMoved(point), event(true)]);
        assert!(
            !pass(vec![Event::PointerMoved(point), event(false)]).close,
            "a regular pin should open its context menu rather than close immediately"
        );
    }

    #[test]
    fn reset_size_uses_the_absolute_original_scale() {
        let set = displays();
        let mut surface = PinnedSurface::on_display(
            PinId("reset".into()),
            LogicalSize::new(200.0, 100.0),
            &set.displays()[0],
            PinChromePolicy::Allowed,
            vec![LockEscape::TrayMenu],
        )
        .expect("pin dimensions");
        surface.set_scale(0.5, &set);
        assert_eq!(surface.state().scale.get(), 0.5);

        let context = egui::Context::default();
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            reset_scale(ui, &mut surface, &set);
        });
        output.textures_delta.clear();

        assert_eq!(surface.state().scale.get(), 1.0);
    }

    #[test]
    fn viewport_geometry_round_trips_without_persisting_gui_zoom() {
        let native = LogicalRect::new(
            LogicalPoint::new(120.0, 80.0),
            LogicalSize::new(420.0, 270.0),
        );
        let viewport = viewport_rect(native, 2.0);
        assert_eq!(viewport.min, Pos2::new(60.0, 40.0));
        assert_eq!(viewport.size(), Vec2::new(210.0, 135.0));
        assert_eq!(native_logical_rect(viewport, 2.0), native);
        assert_eq!(viewport_rect(native, f32::NAN), viewport_rect(native, 1.0));
    }
}
