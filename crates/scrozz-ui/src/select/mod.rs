//! Shared region-selector UI building blocks.

#![allow(missing_docs)]

use std::collections::BTreeMap;

use egui::{CursorIcon, Key, PointerButton, TextureHandle, Ui};
use scrozz_core::selection::{
    SelectionCapabilities, SelectionMode, SelectionOptions, SelectionOutcome,
};
use scrozz_core::{Display, DisplayId, LogicalPoint, LogicalRect, LogicalSize, Window};

pub mod frozen;
pub mod geom;
pub mod hud;
pub mod magnifier;
pub mod paint;
pub mod scene;
pub mod state;

pub use frozen::{FrozenDesktop, FrozenDisplayFrame, FrozenPixel};
pub use geom::DisplayLayout;
pub use hud::{HudEntry, HudModel, HudNav};
pub use magnifier::{MagnifierCell, MagnifierConfig, MagnifierGrid};
pub use scene::SelectionScene;
pub use state::{
    AxisDirection, DragModifiers, ResizeHandle, SelectionAnnouncement, SelectionState,
};

#[derive(Debug, Clone, PartialEq)]
pub enum SelectionDecision {
    Pending,
    Selected(SelectionOutcome),
    Cancelled,
}

pub struct SelectionUi {
    layout: DisplayLayout,
    frozen: FrozenDesktop,
    windows: Vec<Window>,
    requested_capabilities: SelectionCapabilities,
    capabilities: SelectionCapabilities,
    state: SelectionState,
    hud: HudModel,
    textures: BTreeMap<String, TextureHandle>,
    immediate: Option<SelectionDecision>,
}

#[derive(Debug, Default)]
struct PointerUpdate {
    changed: bool,
    decision: Option<SelectionDecision>,
}

impl SelectionUi {
    #[must_use]
    pub fn new(
        options: SelectionOptions,
        displays: Vec<Display>,
        frozen_frames: Vec<FrozenDisplayFrame>,
    ) -> Self {
        let layout = DisplayLayout::new(displays);
        let frozen = FrozenDesktop::new(frozen_frames);
        let requested_capabilities = SelectionCapabilities::CLIENT_OVERLAY;
        let windows = Vec::new();
        let capabilities =
            effective_capabilities(requested_capabilities, &options, &frozen, &windows);
        let state = SelectionState::new_with_windows(
            options,
            layout.clone(),
            capabilities,
            windows.clone(),
        );
        let immediate = state.immediate_reuse().map(SelectionDecision::Selected);
        let hud = HudModel::new(state.mode(), capabilities);
        Self {
            layout,
            frozen,
            windows,
            requested_capabilities,
            capabilities,
            state,
            hud,
            textures: BTreeMap::new(),
            immediate,
        }
    }

    #[must_use]
    pub fn with_capabilities(mut self, capabilities: SelectionCapabilities) -> Self {
        self.requested_capabilities = capabilities;
        self.rebuild_state();
        self
    }

    #[must_use]
    pub fn with_windows(mut self, windows: Vec<Window>) -> Self {
        self.windows = windows;
        self.rebuild_state();
        self
    }

    #[must_use]
    pub const fn state(&self) -> &SelectionState {
        &self.state
    }

    #[must_use]
    pub fn state_mut(&mut self) -> &mut SelectionState {
        &mut self.state
    }

    #[must_use]
    pub const fn hud(&self) -> &HudModel {
        &self.hud
    }

    #[must_use]
    pub fn hud_mut(&mut self) -> &mut HudModel {
        &mut self.hud
    }

    #[must_use]
    pub fn update(&mut self, ui: &mut Ui) -> SelectionDecision {
        let surface = self.layout.desktop_bounds().unwrap_or_else(|| {
            LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(0.0, 0.0))
        });
        self.update_surface(ui, surface, None, true)
    }

    /// Draws and drives one display-local selector viewport.
    #[must_use]
    pub fn update_display(&mut self, ui: &mut Ui, display: &DisplayId) -> SelectionDecision {
        let Some(surface) = self.layout.display(display).map(|display| display.bounds) else {
            return SelectionDecision::Cancelled;
        };
        self.update_surface(ui, surface, Some(display), true)
    }

    fn update_surface(
        &mut self,
        ui: &mut Ui,
        surface: LogicalRect,
        surface_display: Option<&DisplayId>,
        show_hud: bool,
    ) -> SelectionDecision {
        if ui.ctx().input(|input| input.viewport().close_requested()) {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            let _ = self.state.cancel();
            return SelectionDecision::Cancelled;
        }
        if let Some(decision) = self.immediate.take() {
            return decision;
        }
        self.ensure_textures(ui.ctx());
        self.sync_hud();
        let focused = ui
            .ctx()
            .input(|input| input.viewport().focused.unwrap_or(true));
        if focused && self.handle_keyboard(ui) {
            return SelectionDecision::Cancelled;
        }
        let drag_modifiers = ui.ctx().input(|input| DragModifiers {
            shift: input.modifiers.shift,
            alt: input.modifiers.alt,
            space: input.key_down(Key::Space),
        });
        self.state.set_drag_modifiers(drag_modifiers);
        let primary_modifier = ui.ctx().input(|input| input.modifiers.command);
        let paint = paint::draw_overlay(
            ui,
            &theme_for(ui),
            &self.state,
            &self.hud,
            paint::OverlayView {
                layout: &self.layout,
                frozen: &self.frozen,
                textures: &self.textures,
                surface,
                display: surface_display,
                show_hud,
                primary_modifier,
            },
        );
        if self.state.mode() == SelectionMode::Region {
            ui.ctx().set_cursor_icon(if paint.pointer_over_controls {
                CursorIcon::PointingHand
            } else {
                CursorIcon::Crosshair
            });
        }
        match paint.action {
            paint::OverlayAction::None => {}
            paint::OverlayAction::Mode(mode) => {
                let _ = self.set_mode(mode);
            }
            paint::OverlayAction::Confirm => {
                if let Some(outcome) = self.state.commit() {
                    return SelectionDecision::Selected(outcome);
                }
            }
        }
        let pointer = self.handle_pointer(
            ui,
            &paint.canvas,
            paint.pointer_over_controls,
            surface,
            surface_display,
        );
        if pointer.changed {
            ui.ctx().request_repaint();
        }
        if let Some(decision) = pointer.decision {
            return decision;
        }
        if !self.state.options_ref().commits_region_on_release()
            && paint.canvas.clicked()
            && !self.state.gesture_changed()
            && let Some(outcome) = self.state.commit()
        {
            self.immediate = Some(SelectionDecision::Selected(outcome));
        }
        self.immediate.take().unwrap_or(SelectionDecision::Pending)
    }

    #[must_use]
    pub fn set_mode(&mut self, mode: SelectionMode) -> bool {
        let changed = self.state.set_mode(mode);
        if changed {
            let _ = self.hud.select(mode);
        }
        changed
    }

    #[must_use]
    pub fn take_announcement(&mut self) -> Option<SelectionAnnouncement> {
        self.state.take_announcement()
    }

    fn ensure_textures(&mut self, ctx: &egui::Context) {
        if !self.textures.is_empty() {
            return;
        }
        self.textures = self.frozen.upload_all(ctx);
    }

    fn sync_hud(&mut self) {
        self.hud.set_capabilities(self.capabilities);
        let _ = self.hud.select(self.state.mode());
    }

    fn rebuild_state(&mut self) {
        let options = self.state.options_ref().clone();
        self.capabilities = effective_capabilities(
            self.requested_capabilities,
            &options,
            &self.frozen,
            &self.windows,
        );
        self.state = SelectionState::new_with_windows(
            options,
            self.layout.clone(),
            self.capabilities,
            self.windows.clone(),
        );
        self.hud = HudModel::new(self.state.mode(), self.capabilities);
        self.immediate = self
            .state
            .immediate_reuse()
            .map(SelectionDecision::Selected);
    }

    fn handle_pointer(
        &mut self,
        ui: &Ui,
        canvas: &egui::Response,
        pointer_over_hud: bool,
        surface: LogicalRect,
        surface_display: Option<&DisplayId>,
    ) -> PointerUpdate {
        let (latest, pressed, released, down, cancelled) = ui.ctx().input(|input| {
            (
                input.pointer.latest_pos(),
                input.pointer.button_pressed(PointerButton::Primary),
                input.pointer.button_released(PointerButton::Primary),
                input.pointer.button_down(PointerButton::Primary),
                input.pointer.button_pressed(PointerButton::Secondary),
            )
        });
        if cancelled {
            let _ = self.state.cancel();
            return PointerUpdate {
                changed: true,
                decision: Some(SelectionDecision::Cancelled),
            };
        }
        let Some(point) =
            latest.filter(|point| canvas.rect.contains(*point) || self.state.is_interacting())
        else {
            return PointerUpdate::default();
        };
        let point = DisplayLayout::point_from_canvas_in(surface, point - canvas.rect.min.to_vec2());
        if pointer_over_hud && !self.state.is_interacting() {
            return PointerUpdate::default();
        }
        let moved = self.state.pointer() != Some(point);
        let mut changed = false;
        let mut decision = None;
        if pressed && canvas.rect.contains(latest.expect("latest exists")) {
            if let Some(display) = surface_display {
                self.state.pointer_pressed_on(display, point);
            } else {
                self.state.pointer_pressed(point);
            }
            changed = true;
            if !self.state.mode().is_freehand()
                && let Some(outcome) = self.state.commit()
            {
                self.immediate = Some(SelectionDecision::Selected(outcome));
            }
        } else if down && moved {
            if let Some(display) = surface_display {
                self.state.pointer_moved_on(display, point);
            } else {
                self.state.pointer_moved(point);
            }
            changed = true;
        } else if moved {
            if let Some(display) = surface_display {
                self.state.hover_on(display, point);
            } else {
                self.state.hover(point);
            }
            changed = true;
        }
        if released {
            if let Some(display) = surface_display {
                self.state.pointer_released_on(display, point);
            } else {
                self.state.pointer_released(point);
            }
            changed = true;
            if self.state.options_ref().commits_region_on_release() {
                decision = if self.state.gesture_changed() {
                    self.state.commit().map(SelectionDecision::Selected)
                } else {
                    let _ = self.state.cancel();
                    Some(SelectionDecision::Cancelled)
                };
                if decision.is_none() {
                    let _ = self.state.cancel();
                    decision = Some(SelectionDecision::Cancelled);
                }
            }
        }
        PointerUpdate { changed, decision }
    }

    fn handle_keyboard(&mut self, ui: &Ui) -> bool {
        let (shift, alt, tab, left, right, up, down, enter, space, escape) =
            ui.ctx().input(|input| {
                (
                    input.modifiers.shift,
                    input.modifiers.alt,
                    input.key_pressed(Key::Tab),
                    input.key_pressed(Key::ArrowLeft),
                    input.key_pressed(Key::ArrowRight),
                    input.key_pressed(Key::ArrowUp),
                    input.key_pressed(Key::ArrowDown),
                    input.key_pressed(Key::Enter),
                    input.key_pressed(Key::Space),
                    input.key_pressed(Key::Escape),
                )
            });
        if escape {
            let _ = self.state.cancel();
            return true;
        }
        if self.state.options_ref().hud {
            if tab {
                ui.ctx().input_mut(|input| {
                    let modifiers = input.modifiers;
                    let _ = input.consume_key(modifiers, Key::Tab);
                });
                self.hud.navigate(if shift {
                    HudNav::Previous
                } else {
                    HudNav::Next
                });
            }
            if space && !self.state.is_interacting() {
                ui.ctx().input_mut(|input| {
                    let modifiers = input.modifiers;
                    let _ = input.consume_key(modifiers, Key::Space);
                });
                if let Some(mode) = self.hud.activate_focused() {
                    let _ = self.state.set_mode(mode);
                }
            }
        }
        if self.state.mode() == SelectionMode::Region && self.capabilities.keyboard_adjustment {
            if left {
                self.handle_arrow(AxisDirection::Left, alt, shift);
            }
            if right {
                self.handle_arrow(AxisDirection::Right, alt, shift);
            }
            if up {
                self.handle_arrow(AxisDirection::Up, alt, shift);
            }
            if down {
                self.handle_arrow(AxisDirection::Down, alt, shift);
            }
        }
        if enter && let Some(outcome) = self.state.commit() {
            self.immediate = Some(SelectionDecision::Selected(outcome));
        }
        false
    }

    fn handle_arrow(&mut self, direction: AxisDirection, resize: bool, fast: bool) {
        if resize {
            self.state.keyboard_resize(direction, fast);
        } else {
            self.state.keyboard_nudge(direction, fast);
        }
    }
}

fn theme_for(ui: &Ui) -> crate::theme::Theme {
    if ui.visuals().dark_mode {
        crate::theme::Theme::dark()
    } else {
        crate::theme::Theme::light()
    }
}

fn effective_capabilities(
    requested: SelectionCapabilities,
    options: &SelectionOptions,
    frozen: &FrozenDesktop,
    windows: &[Window],
) -> SelectionCapabilities {
    let has_frozen = frozen.frames().next().is_some();
    let has_visible_windows = windows.iter().any(|window| window.is_visible);
    SelectionCapabilities {
        magnifier: requested.magnifier && options.needs_magnifier_frame() && has_frozen,
        frozen_screen: requested.frozen_screen && has_frozen,
        hud: requested.hud && options.hud,
        window_picking: requested.window_picking && has_visible_windows,
        ..requested
    }
}
