//! The native child-window surface used by one pinned capture.
//!
//! A pin is deliberately not a card with different styling. It owns a native
//! viewport, has no synthetic chrome for window captures (D9), and can become
//! mouse-transparent when locked. This module only draws and interprets one
//! frame; geometry reconciliation and persistence remain in the overlay host.

use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Key, Pos2, Rect, ResizeDirection, Response,
    Sense, Stroke, StrokeKind, TextureId, Ui, Vec2, ViewportCommand, WidgetInfo, WidgetType,
};
use scrozz_core::{
    DisplaySet, LogicalPoint, LogicalRect, LogicalSize, NudgeStep, PinDirection, PinnedSurface,
};

use crate::theme::{Appearance, Theme};

const CONTROL_DIAMETER: f32 = 38.0;
const CONTROL_INSET: f32 = 8.0;
const CONTROL_GAP: f32 = 8.0;
const ZOOM_PILL_SIZE: Vec2 = Vec2::new(66.0, 30.0);
const GRIP_SIZE: Vec2 = Vec2::new(54.0, 16.0);
const RESIZE_EDGE: f32 = 7.0;
const RESIZE_CORNER: f32 = 16.0;

/// What changed while drawing a pinned capture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PinFrameResponse {
    /// Durable state changed and should be persisted.
    pub changed: bool,
    /// The pin should close.
    pub close: bool,
    /// The user requested a nudge that the compositor cannot honor.
    pub positioning_unavailable: bool,
    /// Open the detached action menu at this window-local point.
    pub menu_at: Option<Pos2>,
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
    /// A global pointer probe says the pointer is over this locked pin.
    pub locked_hovered: bool,
    /// The pointer is over one of the locked pin's interactive control islands.
    pub locked_control_hovered: bool,
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
    let locked_hover_alpha = if state.locked && frame.locked_hovered {
        0.5
    } else {
        1.0
    };
    let content_alpha = if frame.native_opacity { 1.0 } else { opacity } * locked_hover_alpha;
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

    if !state.locked
        && let Some(position) = secondary_release_position(ui)
        && rect.contains(position)
    {
        output.menu_at = Some(position);
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

    let resize_started = !state.locked && begin_resize(ui, rect);
    if !state.locked && !resize_started && body.drag_started() {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }

    let viewport_focused =
        ui.input(|input| input.viewport().focused.is_some_and(std::convert::identity));
    let chrome_visible = chrome_is_visible(
        frame.chrome_visibility,
        if state.locked {
            frame.locked_hovered
        } else {
            body.hovered()
        },
        viewport_focused,
        ui.ctx().memory(|memory| memory.focused().is_some()),
    );
    if chrome_visible {
        draw_controls(ui, rect, &mut frame, &mut output);
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

fn secondary_release_position(ui: &Ui) -> Option<Pos2> {
    ui.input(|input| {
        input.events.iter().rev().find_map(|event| match event {
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed: false,
                ..
            } => Some(*pos),
            _ => None,
        })
    })
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

fn draw_controls(
    ui: &mut Ui,
    window: Rect,
    frame: &mut PinFrame<'_>,
    output: &mut PinFrameResponse,
) {
    let layout = control_layout(window);
    if circle_control(
        ui,
        layout.close,
        ControlIcon::Close,
        true,
        "Close pinned capture",
    )
    .clicked()
    {
        output.close = true;
    }

    if let Some(lock_rect) = layout.lock {
        let can_lock =
            frame.surface.state().locked || frame.click_through && frame.surface.has_lock_escape();
        let help = if frame.surface.state().locked {
            "Unlock pinned capture"
        } else if !frame.click_through {
            "Click-through is unavailable in this desktop session"
        } else if !frame.surface.has_lock_escape() {
            "Enable the Scrozz menu, command forwarding, or a global unlock hotkey first"
        } else {
            "Lock and pass pointer input through"
        };
        let lock = circle_control(ui, lock_rect, ControlIcon::Lock, can_lock, help);
        if lock.clicked() {
            let next = !frame.surface.state().locked;
            if frame.surface.set_locked(next).is_ok() {
                let over_control = ui.input(|input| {
                    input
                        .pointer
                        .latest_pos()
                        .is_some_and(|point| pointer_over_control(window, point))
                });
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::MousePassthrough(next && !over_control));
                output.changed = true;
            }
        }
    }

    if let Some(zoom) = layout.zoom {
        pill(ui, zoom);
        ui.painter().text(
            zoom.center(),
            Align2::CENTER_CENTER,
            format!("{:.0}%", frame.surface.state().scale.get() * 100.0),
            FontId::proportional(12.0),
            Color32::WHITE,
        );
        ui.interact(zoom, ui.id().with("pin-scale"), Sense::hover())
            .on_hover_text("Pinned capture scale")
            .widget_info(|| {
                WidgetInfo::labeled(
                    WidgetType::Label,
                    true,
                    format!("{:.0}% scale", frame.surface.state().scale.get() * 100.0),
                )
            });
    }

    if let Some(grip) = layout.grip {
        pill(ui, grip);
        for offset in [-8.0, 0.0, 8.0] {
            ui.painter().circle_filled(
                Pos2::new(grip.center().x + offset, grip.center().y),
                1.5,
                Color32::from_white_alpha(210),
            );
        }
        let grip = ui
            .interact(grip, ui.id().with("pin-move-grip"), Sense::drag())
            .on_hover_cursor(CursorIcon::Grab)
            .on_hover_text("Drag to move");
        if !frame.surface.state().locked && grip.drag_started() {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }
    }
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

#[derive(Clone, Copy)]
struct ControlLayout {
    close: Rect,
    lock: Option<Rect>,
    zoom: Option<Rect>,
    grip: Option<Rect>,
}

fn control_layout(window: Rect) -> ControlLayout {
    let diameter = CONTROL_DIAMETER
        .min(window.width().max(0.0))
        .min(window.height().max(0.0));
    let y = window.min.y + CONTROL_INSET.min((window.height() - diameter).max(0.0) / 2.0);
    let left = Rect::from_min_size(
        Pos2::new(
            window.min.x + CONTROL_INSET.min(window.width() - diameter),
            y,
        ),
        Vec2::splat(diameter),
    );
    let right = Rect::from_min_size(
        Pos2::new(
            window.max.x - diameter - CONTROL_INSET.min(window.width() - diameter),
            y,
        ),
        Vec2::splat(diameter),
    );
    let enough_for_both = window.width() >= diameter * 2.0 + CONTROL_INSET * 2.0 + CONTROL_GAP;
    let (close, lock) = if cfg!(target_os = "macos") {
        (left, enough_for_both.then_some(right))
    } else {
        (right, enough_for_both.then_some(left))
    };
    let zoom = (window.width() >= ZOOM_PILL_SIZE.x + diameter * 2.0 + CONTROL_INSET * 4.0
        && window.height() >= ZOOM_PILL_SIZE.y + CONTROL_INSET * 2.0)
        .then(|| {
            Rect::from_center_size(
                Pos2::new(
                    window.center().x,
                    window.min.y + CONTROL_INSET + ZOOM_PILL_SIZE.y / 2.0,
                ),
                ZOOM_PILL_SIZE,
            )
        });
    let grip = (window.width() >= GRIP_SIZE.x + CONTROL_INSET * 2.0
        && window.height() >= CONTROL_DIAMETER + GRIP_SIZE.y + CONTROL_INSET * 3.0)
        .then(|| {
            Rect::from_center_size(
                Pos2::new(
                    window.center().x,
                    window.max.y - CONTROL_INSET - GRIP_SIZE.y / 2.0,
                ),
                GRIP_SIZE,
            )
        });
    ControlLayout {
        close,
        lock,
        zoom,
        grip,
    }
}

/// Whether a pointer in pin-local coordinates is over Close or Lock.
#[must_use]
pub fn pointer_over_control(window: Rect, pointer: Pos2) -> bool {
    let layout = control_layout(window);
    layout.close.expand(2.0).contains(pointer)
        || layout
            .lock
            .is_some_and(|rect| rect.expand(2.0).contains(pointer))
}

#[derive(Clone, Copy)]
enum ControlIcon {
    Close,
    Lock,
}

fn circle_control(
    ui: &mut Ui,
    rect: Rect,
    icon: ControlIcon,
    enabled: bool,
    help: &str,
) -> Response {
    let response = ui
        .interact(
            rect,
            ui.id().with(("pin-control", icon as u8)),
            if enabled {
                Sense::click()
            } else {
                Sense::hover()
            },
        )
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(help);
    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let center = rect.center()
        + if pressed {
            Vec2::new(0.0, 1.0)
        } else {
            Vec2::ZERO
        };
    ui.painter().circle_filled(
        center + Vec2::new(0.0, 2.0),
        rect.width() / 2.0,
        Color32::from_black_alpha(if hovered { 72 } else { 48 }),
    );
    ui.painter().circle_filled(
        center,
        rect.width() / 2.0,
        if enabled {
            Color32::from_rgba_unmultiplied(246, 247, 249, if hovered { 248 } else { 224 })
        } else {
            Color32::from_rgba_unmultiplied(210, 212, 218, 150)
        },
    );
    let ink = if enabled {
        Color32::from_rgb(27, 29, 34)
    } else {
        Color32::from_gray(120)
    };
    match icon {
        ControlIcon::Close => draw_close_icon(ui, center, rect.width(), ink),
        ControlIcon::Lock => draw_lock_icon(ui, center, rect.width(), ink),
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, help));
    response
}

fn draw_close_icon(ui: &Ui, center: Pos2, diameter: f32, color: Color32) {
    let arm = diameter * 0.16;
    let stroke = Stroke::new((diameter * 0.075).max(1.5), color);
    ui.painter().line_segment(
        [center + Vec2::new(-arm, -arm), center + Vec2::new(arm, arm)],
        stroke,
    );
    ui.painter().line_segment(
        [center + Vec2::new(arm, -arm), center + Vec2::new(-arm, arm)],
        stroke,
    );
}

fn draw_lock_icon(ui: &Ui, center: Pos2, diameter: f32, color: Color32) {
    let width = diameter * 0.34;
    let body = Rect::from_center_size(
        center + Vec2::new(0.0, diameter * 0.09),
        Vec2::new(width, diameter * 0.28),
    );
    ui.painter()
        .rect_filled(body, CornerRadius::same((diameter * 0.05) as u8), color);
    let shackle = Rect::from_center_size(
        center + Vec2::new(0.0, -diameter * 0.08),
        Vec2::new(width * 0.66, diameter * 0.25),
    );
    ui.painter().rect_stroke(
        shackle,
        CornerRadius::same((diameter * 0.12) as u8),
        Stroke::new((diameter * 0.065).max(1.5), color),
        StrokeKind::Inside,
    );
}

fn pill(ui: &Ui, rect: Rect) {
    ui.painter().rect_filled(
        rect.translate(Vec2::new(0.0, 2.0)),
        CornerRadius::same((rect.height() / 2.0) as u8),
        Color32::from_black_alpha(52),
    );
    ui.painter().rect_filled(
        rect,
        CornerRadius::same((rect.height() / 2.0) as u8),
        Color32::from_rgba_unmultiplied(20, 22, 28, 214),
    );
}

fn begin_resize(ui: &mut Ui, window: Rect) -> bool {
    let handles = resize_handles(window);
    for (index, (direction, rect, cursor)) in handles.into_iter().enumerate() {
        let response = ui
            .interact(rect, ui.id().with(("pin-resize", index)), Sense::drag())
            .on_hover_cursor(cursor);
        if response.drag_started() {
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::BeginResize(direction));
            return true;
        }
    }
    false
}

fn resize_handles(window: Rect) -> [(ResizeDirection, Rect, CursorIcon); 8] {
    let corner = RESIZE_CORNER
        .min(window.width().max(0.0))
        .min(window.height().max(0.0));
    let edge = RESIZE_EDGE
        .min(window.width().max(0.0))
        .min(window.height().max(0.0));
    [
        (
            ResizeDirection::NorthWest,
            Rect::from_min_size(window.min, Vec2::splat(corner)),
            CursorIcon::ResizeNwSe,
        ),
        (
            ResizeDirection::NorthEast,
            Rect::from_min_size(
                Pos2::new(window.max.x - corner, window.min.y),
                Vec2::splat(corner),
            ),
            CursorIcon::ResizeNeSw,
        ),
        (
            ResizeDirection::SouthWest,
            Rect::from_min_size(
                Pos2::new(window.min.x, window.max.y - corner),
                Vec2::splat(corner),
            ),
            CursorIcon::ResizeNeSw,
        ),
        (
            ResizeDirection::SouthEast,
            Rect::from_min_size(window.max - Vec2::splat(corner), Vec2::splat(corner)),
            CursorIcon::ResizeNwSe,
        ),
        (
            ResizeDirection::North,
            Rect::from_min_max(
                Pos2::new(window.min.x + corner, window.min.y),
                Pos2::new(window.max.x - corner, window.min.y + edge),
            ),
            CursorIcon::ResizeVertical,
        ),
        (
            ResizeDirection::South,
            Rect::from_min_max(
                Pos2::new(window.min.x + corner, window.max.y - edge),
                Pos2::new(window.max.x - corner, window.max.y),
            ),
            CursorIcon::ResizeVertical,
        ),
        (
            ResizeDirection::West,
            Rect::from_min_max(
                Pos2::new(window.min.x, window.min.y + corner),
                Pos2::new(window.min.x + edge, window.max.y - corner),
            ),
            CursorIcon::ResizeHorizontal,
        ),
        (
            ResizeDirection::East,
            Rect::from_min_max(
                Pos2::new(window.max.x - edge, window.min.y + corner),
                Pos2::new(window.max.x, window.max.y - corner),
            ),
            CursorIcon::ResizeHorizontal,
        ),
    ]
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
                chrome_visibility: ChromeVisibility::Visible,
                locked_hovered: self.locked,
                locked_control_hovered: false,
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
    use egui::{Event, Modifiers, PointerButton, UiBuilder};
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
    fn secondary_click_opens_the_detached_menu_even_for_a_tiny_pin() {
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
        assert!(size.y <= CONTROL_DIAMETER);

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
                        locked_hovered: false,
                        locked_control_hovered: false,
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
        let released = pass(vec![event(false)]);
        assert!(!released.close);
        assert_eq!(released.menu_at, Some(point));
        let outside = Pos2::new(size.x + 100.0, size.y + 100.0);
        let outside_event = |pressed| Event::PointerButton {
            pos: outside,
            button: PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        let _ = pass(vec![Event::PointerMoved(outside), outside_event(true)]);
        assert!(
            pass(vec![outside_event(false), Event::PointerMoved(point)])
                .menu_at
                .is_none(),
            "a release outside the pin must not borrow a later inside pointer position"
        );
    }

    #[test]
    fn secondary_click_opens_the_detached_menu_for_a_regular_pin() {
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
                            locked_hovered: false,
                            locked_control_hovered: false,
                        },
                    ));
                },
            );
            output.textures_delta.clear();
            response.expect("headless pin draw ran")
        };

        let _ = pass(vec![Event::PointerMoved(point), event(true)]);
        let released = pass(vec![Event::PointerMoved(point), event(false)]);
        assert!(!released.close);
        assert_eq!(released.menu_at, Some(point));
    }

    #[test]
    fn responsive_controls_keep_close_when_lock_and_zoom_do_not_fit() {
        let tiny = control_layout(Rect::from_min_size(Pos2::ZERO, Vec2::new(54.0, 32.0)));
        assert!(tiny.lock.is_none());
        assert!(tiny.zoom.is_none());
        assert!(tiny.grip.is_none());
        assert!(tiny.close.width() > 0.0);

        let full = control_layout(Rect::from_min_size(Pos2::ZERO, Vec2::new(420.0, 260.0)));
        assert!(full.lock.is_some());
        assert!(full.zoom.is_some());
        assert!(full.grip.is_some());
    }

    #[test]
    fn locked_control_hit_regions_cover_close_and_lock_only() {
        let window = Rect::from_min_size(Pos2::ZERO, Vec2::new(420.0, 260.0));
        let layout = control_layout(window);
        assert!(pointer_over_control(window, layout.close.center()));
        assert!(pointer_over_control(
            window,
            layout.lock.expect("wide pins show lock").center()
        ));
        assert!(!pointer_over_control(window, window.center()));
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
