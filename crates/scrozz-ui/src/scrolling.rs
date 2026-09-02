//! The scrolling-capture HUD.
//!
//! A scrolling capture is the one still-capture flow that stays alive after the
//! shutter. This compact rail makes that state visible without taking focus from
//! the page being captured: choose an axis, watch measured progress, then keep
//! the partial image or discard it.

use egui::{
    Align2, Color32, Id, Rect, Response, Sense, Stroke, StrokeKind, Ui, WidgetInfo, WidgetType,
    pos2, vec2,
};
use scrozz_core::ScrollAxis;

use crate::{
    harness::{Scene, SceneCtx},
    paint,
    theme::{Radius, Space, Text, Theme, corner, install_fonts, install_style},
};

/// What the scrolling HUD is communicating.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScrollHudStatus {
    /// Waiting for the user to choose how the target moves.
    ChoosingAxis,
    /// The platform input path is ready.
    Prepared,
    /// A frame was captured or stitched.
    Capturing,
    /// Automatic input is unavailable and the user should scroll the target.
    WaitingForManualScroll,
    /// The viewport has not moved for this many probes.
    Stalled(u32),
    /// Stitching is complete and the final image is being encoded and persisted.
    Finalizing,
}

/// Everything needed to draw one deterministic HUD frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScrollHudState {
    /// The selected axis, or the initial choice highlighted in the picker.
    pub axis: ScrollAxis,
    /// Current phase.
    pub status: ScrollHudStatus,
    /// One-based viewport number, once capture has started.
    pub frame: usize,
    /// Most recent measured displacement in physical pixels.
    pub delta: Option<u32>,
    /// Current stitched length along [`Self::axis`], in physical pixels.
    pub output_extent: u32,
    /// Whether Scrozz is posting input rather than watching manual movement.
    pub automatic: bool,
}

impl ScrollHudState {
    /// An axis picker with the common vertical choice highlighted.
    #[must_use]
    pub const fn choosing(axis: ScrollAxis) -> Self {
        Self {
            axis,
            status: ScrollHudStatus::ChoosingAxis,
            frame: 0,
            delta: None,
            output_extent: 0,
            automatic: false,
        }
    }

    /// The state immediately after the input path is prepared.
    #[must_use]
    pub const fn prepared(axis: ScrollAxis, automatic: bool) -> Self {
        Self {
            axis,
            status: ScrollHudStatus::Prepared,
            frame: 0,
            delta: None,
            output_extent: 0,
            automatic,
        }
    }
}

/// A user decision emitted by the HUD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollHudAction {
    /// Begin along this axis.
    Start(ScrollAxis),
    /// Finish now and keep the partial stitched image.
    Keep,
    /// Cancel and discard the partial image.
    Abort,
}

/// Geometry and action produced by one HUD frame.
#[derive(Debug)]
pub struct ScrollHudResponse {
    /// The whole interactive HUD, for overlay hit testing.
    pub rect: Rect,
    /// At most one decision per frame.
    pub action: Option<ScrollHudAction>,
}

/// Draws the axis picker and capture progress rail.
pub struct ScrollingHud;

impl ScrollingHud {
    /// Draw one frame.
    ///
    /// `interactive` is false in golden renders so ambient pointer state cannot
    /// change pixels or synthesize actions.
    #[must_use]
    pub fn draw(
        ui: &mut Ui,
        theme: &Theme,
        state: &ScrollHudState,
        interactive: bool,
    ) -> ScrollHudResponse {
        let full = ui.max_rect();
        let width = 420.0_f32.min((full.width() - Space::HUGE).max(280.0));
        let height = if state.status == ScrollHudStatus::ChoosingAxis {
            164.0
        } else {
            150.0
        };
        let rect = hud_rect(full, width, height, &state.status);
        let painter = ui.painter();
        let palette = &theme.palette;

        paint::glass_panel(painter, rect, Radius::BAR, palette, true);
        Self::axis_rail(painter, rect, state.axis, palette.accent, palette.chip_fill);

        let content = rect.shrink2(vec2(Space::XL, Space::LG));
        painter.text(
            content.left_top(),
            Align2::LEFT_TOP,
            "Scrolling capture",
            theme.font(Text::Title),
            palette.text,
        );

        let action = if state.status == ScrollHudStatus::ChoosingAxis {
            painter.text(
                pos2(content.left(), content.top() + 25.0),
                Align2::LEFT_TOP,
                "Choose the direction the page can travel.",
                theme.font(Text::Body),
                palette.text_muted,
            );
            let buttons_y = content.bottom() - 44.0;
            let gap = Space::SM;
            let button_w = (content.width() - gap) * 0.5;
            let vertical =
                Rect::from_min_size(pos2(content.left(), buttons_y), vec2(button_w, 38.0));
            let horizontal =
                Rect::from_min_size(pos2(vertical.right() + gap, buttons_y), vertical.size());
            let vertical_response = text_button(
                ui,
                theme,
                vertical,
                Id::new("scrozz.scroll.vertical"),
                "↓  Tall page",
                state.axis == ScrollAxis::Vertical,
                true,
                interactive,
            );
            let horizontal_response = text_button(
                ui,
                theme,
                horizontal,
                Id::new("scrozz.scroll.horizontal"),
                "→  Wide canvas",
                state.axis == ScrollAxis::Horizontal,
                true,
                interactive,
            );
            if vertical_response.clicked() {
                Some(ScrollHudAction::Start(ScrollAxis::Vertical))
            } else if horizontal_response.clicked() {
                Some(ScrollHudAction::Start(ScrollAxis::Horizontal))
            } else {
                None
            }
        } else {
            let status = status_line(state);
            painter.text(
                pos2(content.left(), content.top() + 25.0),
                Align2::LEFT_TOP,
                status,
                theme.font(Text::Body),
                palette.text_muted,
            );
            let detail = detail_line(state);
            painter.text(
                pos2(content.left(), content.top() + 48.0),
                Align2::LEFT_TOP,
                detail,
                theme.font(Text::Caption),
                palette.text_faint,
            );

            let buttons_y = content.bottom() - 36.0;
            if state.automatic {
                painter.text(
                    pos2(content.left(), buttons_y + 8.0),
                    Align2::LEFT_TOP,
                    "Capture Scrolling again: keep · twice: discard",
                    theme.font(Text::Caption),
                    palette.text,
                );
                return ScrollHudResponse { rect, action: None };
            }
            let discard =
                Rect::from_min_size(pos2(content.right() - 88.0, buttons_y), vec2(88.0, 32.0));
            let keep = Rect::from_min_size(
                pos2(discard.left() - Space::SM - 104.0, buttons_y),
                vec2(104.0, 32.0),
            );
            let can_keep = state.delta.is_some() && state.output_extent > 0;
            let keep_response = text_button(
                ui,
                theme,
                keep,
                Id::new("scrozz.scroll.keep"),
                "Keep now",
                can_keep,
                can_keep,
                interactive,
            );
            let abort_response = text_button(
                ui,
                theme,
                discard,
                Id::new("scrozz.scroll.abort"),
                "Discard",
                false,
                true,
                interactive,
            );
            if keep_response.clicked() {
                Some(ScrollHudAction::Keep)
            } else if abort_response.clicked() {
                Some(ScrollHudAction::Abort)
            } else {
                None
            }
        };

        ScrollHudResponse { rect, action }
    }

    fn axis_rail(
        painter: &egui::Painter,
        panel: Rect,
        axis: ScrollAxis,
        accent: Color32,
        track: Color32,
    ) {
        let anchor = pos2(
            panel.right() - Space::XL - 24.0,
            panel.top() + Space::XL + 5.0,
        );
        let (from, to) = match axis {
            ScrollAxis::Vertical => (
                pos2(anchor.x, anchor.y - 10.0),
                pos2(anchor.x, anchor.y + 10.0),
            ),
            ScrollAxis::Horizontal => (
                pos2(anchor.x - 10.0, anchor.y),
                pos2(anchor.x + 10.0, anchor.y),
            ),
        };
        painter.line_segment([from, to], Stroke::new(5.0, track));
        painter.line_segment([from, to], Stroke::new(2.0, accent));
        painter.circle_filled(to, 3.5, accent);
    }
}

/// Deterministic axis-picker and progress states for the visual harness.
pub struct ScrollingScene;

impl Scene for ScrollingScene {
    fn name(&self) -> &'static str {
        "scrolling-capture"
    }

    fn setup(&self, ctx: &egui::Context) {
        install_fonts(ctx);
    }

    fn ui(&self, ui: &mut Ui, ctx: &SceneCtx<'_>) {
        let theme = match ctx.theme {
            egui::Theme::Dark => Theme::dark(),
            egui::Theme::Light => Theme::light(),
        };
        install_style(ui.ctx(), &theme);

        let state = match ctx.millis() {
            0..=99 => ScrollHudState::choosing(ScrollAxis::Vertical),
            100..=299 => ScrollHudState {
                axis: ScrollAxis::Vertical,
                status: ScrollHudStatus::Capturing,
                frame: 3,
                delta: Some(612),
                output_extent: 2_496,
                automatic: true,
            },
            _ => ScrollHudState {
                axis: ScrollAxis::Horizontal,
                status: ScrollHudStatus::Capturing,
                frame: 5,
                delta: Some(788),
                output_extent: 4_320,
                automatic: false,
            },
        };
        let _ = ScrollingHud::draw(ui, &theme, &state, false);
    }
}

fn hud_rect(full: Rect, width: f32, height: f32, status: &ScrollHudStatus) -> Rect {
    let top = if matches!(status, ScrollHudStatus::ChoosingAxis) {
        full.top() + Space::XXL
    } else {
        full.bottom() - Space::XXL - height
    };
    Rect::from_min_size(
        pos2(full.center().x - width * 0.5, top),
        vec2(width, height),
    )
}

fn status_line(state: &ScrollHudState) -> &'static str {
    match state.status {
        ScrollHudStatus::ChoosingAxis => "",
        ScrollHudStatus::Prepared if state.automatic => "Ready — Scrozz will move the page.",
        ScrollHudStatus::Prepared => "Ready — scroll the page when prompted.",
        ScrollHudStatus::Capturing if state.automatic => "Capturing while the page moves…",
        ScrollHudStatus::Capturing => "Following your scroll…",
        ScrollHudStatus::WaitingForManualScroll => match state.axis {
            ScrollAxis::Vertical => "Scroll down in the target. Scrozz is watching.",
            ScrollAxis::Horizontal => "Scroll right in the target. Scrozz is watching.",
        },
        ScrollHudStatus::Stalled(_) if state.delta.is_some() => {
            "No new movement. Keep scrolling or finish now."
        }
        ScrollHudStatus::Stalled(_) => "No movement yet. Scroll the target or discard.",
        ScrollHudStatus::Finalizing => "Finalizing the stitched image…",
    }
}

fn detail_line(state: &ScrollHudState) -> String {
    let axis = match state.axis {
        ScrollAxis::Vertical => "Vertical",
        ScrollAxis::Horizontal => "Horizontal",
    };
    let extent = match state.axis {
        ScrollAxis::Vertical => "tall",
        ScrollAxis::Horizontal => "wide",
    };
    let delta = state.delta.map_or_else(
        || "measuring overlap".to_owned(),
        |delta| format!("Δ {delta} px"),
    );
    let stall = match state.status {
        ScrollHudStatus::Stalled(count) => format!(" · idle probe {count}"),
        _ => String::new(),
    };
    format!(
        "{axis} · frame {} · {delta} · {} px {extent}{stall}",
        state.frame.max(1),
        state.output_extent
    )
}

#[allow(clippy::too_many_arguments)]
fn text_button(
    ui: &mut Ui,
    theme: &Theme,
    rect: Rect,
    id: Id,
    label: &str,
    accent: bool,
    enabled: bool,
    interactive: bool,
) -> Response {
    let sense = if interactive && enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, id, sense);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));

    let palette = &theme.palette;
    let hovered = interactive && enabled && response.hovered();
    let fill = if !enabled {
        palette.chip_fill
    } else if accent {
        if hovered {
            palette.accent_hi
        } else {
            palette.accent
        }
    } else if hovered {
        palette.hover
    } else {
        palette.chip_fill
    };
    let foreground = if !enabled {
        palette.text_faint
    } else if accent {
        palette.on_accent
    } else {
        palette.text
    };
    let radius = Radius::pill(rect.height());
    ui.painter().rect_filled(rect, corner(radius), fill);
    ui.painter().rect_stroke(
        rect,
        corner(radius),
        Stroke::new(1.0, palette.hairline),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme.font(Text::Button),
        foreground,
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_state_keeps_the_selected_axis() {
        let state = ScrollHudState::prepared(ScrollAxis::Horizontal, true);
        assert_eq!(state.axis, ScrollAxis::Horizontal);
        assert_eq!(state.status, ScrollHudStatus::Prepared);
        assert!(state.automatic);
    }

    #[test]
    fn detail_names_the_output_dimension_for_each_axis() {
        let mut state = ScrollHudState::prepared(ScrollAxis::Vertical, true);
        state.frame = 3;
        state.output_extent = 2_400;
        assert!(detail_line(&state).contains("tall"));
        state.axis = ScrollAxis::Horizontal;
        assert!(detail_line(&state).contains("wide"));
    }

    #[test]
    fn progress_rail_vacates_the_axis_picker_click_location() {
        let full = Rect::from_min_size(pos2(0.0, 0.0), vec2(1_024.0, 768.0));
        let chooser = hud_rect(full, 420.0, 164.0, &ScrollHudStatus::ChoosingAxis);
        let progress = hud_rect(full, 420.0, 150.0, &ScrollHudStatus::Capturing);

        assert!(!chooser.intersects(progress));
        assert!(chooser.top() < full.center().y);
        assert!(progress.top() > full.center().y);
    }

    #[test]
    fn axis_picker_emits_start_from_an_isolated_hud_surface() {
        let ctx = egui::Context::default();
        let state = ScrollHudState::choosing(ScrollAxis::Vertical);
        let theme = Theme::for_appearance(crate::theme::Appearance::Light);
        install_fonts(&ctx);
        install_style(&ctx, &theme);
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(1_024.0, 768.0));

        let draw = |input| {
            let mut response = None;
            let mut output = ctx.run_ui(input, |ui| {
                response = Some(ScrollingHud::draw(ui, &theme, &state, true));
            });
            output.textures_delta.clear();
            response.expect("HUD response")
        };
        let empty = egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        };
        let chooser = draw(empty).rect;
        let content = chooser.shrink2(vec2(Space::XL, Space::LG));
        let button = Rect::from_min_size(
            pos2(content.left(), content.bottom() - 44.0),
            vec2((content.width() - Space::SM) * 0.5, 38.0),
        );
        let input = |pressed| egui::RawInput {
            screen_rect: Some(screen),
            events: vec![
                egui::Event::PointerMoved(button.center()),
                egui::Event::PointerButton {
                    pos: button.center(),
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };

        assert_eq!(draw(input(true)).action, None);
        assert_eq!(
            draw(input(false)).action,
            Some(ScrollHudAction::Start(ScrollAxis::Vertical))
        );
    }
}
