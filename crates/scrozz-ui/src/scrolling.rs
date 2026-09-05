//! The scrolling-capture HUD.
//!
//! A scrolling capture is the one still-capture flow that stays alive after the
//! shutter. This compact rail makes that state visible without taking focus from
//! the page being captured: choose manual or automatic movement, let the first
//! real scroll establish the route, then keep the partial image or discard it.

use egui::{
    Align2, Color32, Id, Rect, Response, Sense, Stroke, StrokeKind, Ui, WidgetInfo, WidgetType,
    pos2, vec2,
};
use scrozz_core::{LogicalRect, ScaleFactor, ScrollAxis, ScrollControl, ScrollDirection};

use crate::{
    crop_chrome::draw_resize_guides,
    harness::{Scene, SceneCtx},
    paint,
    theme::{Radius, Space, Text, Theme, corner, install_fonts, install_style},
};

/// What the scrolling HUD is communicating.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScrollHudStatus {
    /// Waiting for the user to configure and explicitly start capture.
    Configuring,
    /// The platform input path is ready.
    Prepared,
    /// A frame was captured or stitched.
    Capturing,
    /// Automatic input is unavailable and the user should scroll the target.
    WaitingForManualScroll,
    /// The viewport has not moved for this many probes.
    Stalled(u32),
    /// The last accepted viewport is intact; scrolling back can reconnect it.
    WaitingForOverlap,
    /// Pixels are retained in memory until the user chooses Finish or Discard.
    AwaitingFinish(String),
    /// Stitching is complete and the final image is being encoded and persisted.
    Finalizing,
    /// Capture did not start or advance; configuration remains available.
    Failed(String),
}

/// Everything needed to draw one deterministic HUD frame.
#[derive(Clone, Debug, PartialEq)]
pub struct ScrollHudState {
    /// Current phase.
    pub status: ScrollHudStatus,
    /// One-based viewport number, once capture has started.
    pub frame: usize,
    /// Most recent measured displacement in physical pixels.
    pub delta: Option<u32>,
    /// Current stitched length along the detected route, in physical pixels.
    pub output_extent: u32,
    /// Whether Scrozz is posting input rather than watching manual movement.
    pub automatic: bool,
    /// Input mode selected before capture starts.
    pub control: ScrollControl,
    /// Route inferred from real viewport movement.
    pub direction: Option<ScrollDirection>,
    /// Selected area in work-area-local logical coordinates.
    pub selection: Option<LogicalRect>,
    /// Native display geometry that owns the selected area.
    pub surface: Option<ScrollHudSurface>,
}

/// Display-local context needed to place the root and detached controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollHudSurface {
    /// Global logical work area of the selected display.
    pub work_area: LogicalRect,
    /// Native logical-to-physical scale of that display.
    pub scale: ScaleFactor,
}

impl ScrollHudState {
    /// Setup before the first viewport movement establishes a route.
    #[must_use]
    pub const fn configuring() -> Self {
        Self {
            status: ScrollHudStatus::Configuring,
            frame: 0,
            delta: None,
            output_extent: 0,
            automatic: false,
            control: ScrollControl::Manual,
            direction: None,
            selection: None,
            surface: None,
        }
    }

    /// The state immediately after the input path is prepared.
    #[must_use]
    pub const fn prepared(control: ScrollControl) -> Self {
        Self {
            status: ScrollHudStatus::Prepared,
            frame: 0,
            delta: None,
            output_extent: 0,
            automatic: matches!(control, ScrollControl::Automatic),
            control,
            direction: None,
            selection: None,
            surface: None,
        }
    }

    /// Anchors setup and progress to the selected scrolling area.
    #[must_use]
    pub const fn with_selection(mut self, selection: Option<LogicalRect>) -> Self {
        self.selection = selection;
        self
    }

    /// Places the selected region and controls on its owning display.
    #[must_use]
    pub const fn with_surface(
        mut self,
        selection: LogicalRect,
        work_area: LogicalRect,
        scale: ScaleFactor,
    ) -> Self {
        self.selection = Some(selection);
        self.surface = Some(ScrollHudSurface { work_area, scale });
        self
    }

    /// Records the route inferred from viewport pixels.
    #[must_use]
    pub const fn with_direction(mut self, direction: ScrollDirection) -> Self {
        self.direction = Some(direction);
        self
    }
}

/// A user decision emitted by the HUD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollHudAction {
    /// Select who moves the target.
    SetControl(ScrollControl),
    /// Begin and learn the route from the first real scroll.
    Start,
    /// Leave setup without capturing.
    Cancel,
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

/// Draws scrolling setup and the capture progress rail.
pub struct ScrollingHud;

impl ScrollingHud {
    /// Bounds of the interactive controls in the root overlay's coordinates.
    #[must_use]
    pub(crate) fn control_rect(full: Rect, state: &ScrollHudState) -> Rect {
        let setup = matches!(
            state.status,
            ScrollHudStatus::Configuring | ScrollHudStatus::Failed(_)
        );
        if let Some(selection) = state
            .selection
            .and_then(|selection| local_selection_rect(full, selection))
        {
            let preferred_width: f32 = if setup { 420.0 } else { 440.0 };
            let width = bounded_panel_width(full, preferred_width);
            let compact = width < if setup { 380.0 } else { 400.0 };
            let height = if compact {
                94.0
            } else if setup {
                46.0
            } else {
                54.0
            };
            return anchored_panel_rect(full, selection, width, height);
        }

        let preferred_width: f32 = if setup { 600.0 } else { 440.0 };
        let width = preferred_width.min((full.width() - Space::HUGE).max(280.0));
        let height = if setup { 164.0 } else { 150.0 };
        hud_rect(full, width, height, state)
    }

    /// Size of a detached interactive control island.
    #[must_use]
    pub(crate) fn detached_viewport_size(
        state: &ScrollHudState,
        available_width: f32,
    ) -> egui::Vec2 {
        let setup = matches!(
            state.status,
            ScrollHudStatus::Configuring | ScrollHudStatus::Failed(_)
        );
        let content_width: f32 = if setup { 420.0 } else { 440.0 };
        let content_height = if setup { 46.0 } else { 54.0 };
        vec2(
            (content_width + Space::LG * 2.0).min(available_width.max(1.0)),
            content_height + Space::SM * 2.0,
        )
    }

    /// Draws only the persistent region boundary into the click-through root.
    pub(crate) fn draw_boundary(ui: &mut Ui, state: &ScrollHudState) {
        let full = ui.max_rect();
        draw_selection_mask(ui.painter(), full, state.selection);
        if matches!(
            state.status,
            ScrollHudStatus::Configuring | ScrollHudStatus::Failed(_)
        ) && let Some(selection) = state
            .selection
            .and_then(|selection| local_selection_rect(full, selection))
        {
            draw_resize_guides(ui.painter(), selection);
        }
    }

    /// Draws controls inside a separate native viewport with no full-screen hit area.
    #[must_use]
    pub(crate) fn draw_detached(
        ui: &mut Ui,
        theme: &Theme,
        state: &ScrollHudState,
        interactive: bool,
    ) -> ScrollHudResponse {
        let full = ui.max_rect();
        let fake_selection = Rect::from_min_size(
            pos2(
                full.center().x - 1.0,
                full.top() + Space::SM - Space::MD - 1.0,
            ),
            vec2(2.0, 1.0),
        );
        if matches!(
            state.status,
            ScrollHudStatus::Configuring | ScrollHudStatus::Failed(_)
        ) {
            draw_scrolling_selection_toolbar(ui, theme, fake_selection, state.control, interactive)
        } else {
            draw_scrolling_progress_toolbar(ui, theme, fake_selection, state, interactive)
        }
    }

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
        let setup = matches!(
            state.status,
            ScrollHudStatus::Configuring | ScrollHudStatus::Failed(_)
        );
        let rect = Self::control_rect(full, state);
        let palette = &theme.palette;

        Self::draw_boundary(ui, state);
        if setup
            && let Some(selection) = state
                .selection
                .and_then(|selection| local_selection_rect(full, selection))
        {
            return draw_scrolling_selection_toolbar(
                ui,
                theme,
                selection,
                state.control,
                interactive,
            );
        }
        if !setup
            && let Some(selection) = state
                .selection
                .and_then(|selection| local_selection_rect(full, selection))
        {
            return draw_scrolling_progress_toolbar(ui, theme, selection, state, interactive);
        }
        let painter = ui.painter();
        paint::glass_panel(painter, rect, Radius::BAR, palette, true);
        if !setup && let Some(direction) = state.direction {
            Self::axis_rail(
                painter,
                rect,
                direction.axis(),
                palette.accent,
                palette.chip_fill,
            );
        }

        let content = rect.shrink2(vec2(Space::XL, Space::LG));
        painter.text(
            content.left_top(),
            Align2::LEFT_TOP,
            "Scrolling capture",
            theme.font(Text::Title),
            palette.text,
        );

        let action = if setup {
            let instruction = match &state.status {
                ScrollHudStatus::Failed(reason) => reason.as_str(),
                _ => "Choose Manual or Auto. After Start, scroll once in any direction.",
            };
            painter.text(
                pos2(content.left(), content.top() + 25.0),
                Align2::LEFT_TOP,
                instruction,
                theme.font(Text::Caption),
                if matches!(state.status, ScrollHudStatus::Failed(_)) {
                    palette.warning
                } else {
                    palette.text_muted
                },
            );
            let controls_y = content.top() + 52.0;
            let gap = Space::SM;
            let button_w = (content.width() - gap) * 0.5;
            let manual =
                Rect::from_min_size(pos2(content.left(), controls_y), vec2(button_w, 34.0));
            let automatic =
                Rect::from_min_size(pos2(manual.right() + gap, controls_y), manual.size());
            let manual_response = text_button(
                ui,
                theme,
                manual,
                Id::new("scrozz.scroll.manual"),
                "Manual",
                if state.control == ScrollControl::Manual {
                    ButtonTone::Selected
                } else {
                    ButtonTone::Neutral
                },
                true,
                interactive,
            );
            let automatic_response = text_button(
                ui,
                theme,
                automatic,
                Id::new("scrozz.scroll.automatic"),
                "Auto",
                if state.control == ScrollControl::Automatic {
                    ButtonTone::Selected
                } else {
                    ButtonTone::Neutral
                },
                true,
                interactive,
            );
            let actions_y = content.bottom() - 34.0;
            let cancel = Rect::from_min_size(pos2(content.left(), actions_y), vec2(92.0, 34.0));
            let start =
                Rect::from_min_size(pos2(content.right() - 132.0, actions_y), vec2(132.0, 34.0));
            let cancel_response = text_button(
                ui,
                theme,
                cancel,
                Id::new("scrozz.scroll.cancel"),
                "Cancel",
                ButtonTone::Neutral,
                true,
                interactive,
            );
            let start_response = text_button(
                ui,
                theme,
                start,
                Id::new("scrozz.scroll.start"),
                "Start capture",
                ButtonTone::Primary,
                true,
                interactive,
            );

            if manual_response.clicked() {
                Some(ScrollHudAction::SetControl(ScrollControl::Manual))
            } else if automatic_response.clicked() {
                Some(ScrollHudAction::SetControl(ScrollControl::Automatic))
            } else if start_response.clicked() {
                Some(ScrollHudAction::Start)
            } else if cancel_response.clicked() {
                Some(ScrollHudAction::Cancel)
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
                "Finish",
                ButtonTone::Primary,
                can_keep,
                interactive,
            );
            let abort_response = text_button(
                ui,
                theme,
                discard,
                Id::new("scrozz.scroll.abort"),
                "Discard",
                ButtonTone::Neutral,
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

/// Deterministic setup and progress states for the visual harness.
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

        if ctx.millis() <= 99 {
            let selection = LogicalRect::new(
                scrozz_core::LogicalPoint::new(80.0, 8.0),
                scrozz_core::LogicalSize::new(480.0, 60.0),
            );
            draw_selection_mask(ui.painter(), ui.max_rect(), Some(selection));
            if let Some(rect) = local_selection_rect(ui.max_rect(), selection) {
                draw_resize_guides(ui.painter(), rect);
                let _ = draw_scrolling_selection_toolbar(
                    ui,
                    &theme,
                    rect,
                    ScrollControl::Manual,
                    false,
                );
            }
            return;
        }

        let state = match ctx.millis() {
            100..=299 => ScrollHudState {
                status: ScrollHudStatus::Capturing,
                frame: 3,
                delta: Some(612),
                output_extent: 2_496,
                automatic: true,
                control: ScrollControl::Automatic,
                direction: Some(ScrollDirection::Down),
                selection: Some(LogicalRect::new(
                    scrozz_core::LogicalPoint::new(80.0, 8.0),
                    scrozz_core::LogicalSize::new(480.0, 75.0),
                )),
                surface: None,
            },
            _ => ScrollHudState {
                status: ScrollHudStatus::Capturing,
                frame: 5,
                delta: Some(788),
                output_extent: 4_320,
                automatic: false,
                control: ScrollControl::Manual,
                direction: Some(ScrollDirection::Right),
                selection: Some(LogicalRect::new(
                    scrozz_core::LogicalPoint::new(80.0, 8.0),
                    scrozz_core::LogicalSize::new(480.0, 75.0),
                )),
                surface: None,
            },
        };
        let _ = ScrollingHud::draw(ui, &theme, &state, false);
    }
}

pub(crate) fn draw_scrolling_selection_toolbar(
    ui: &mut Ui,
    theme: &Theme,
    selection: Rect,
    control: ScrollControl,
    interactive: bool,
) -> ScrollHudResponse {
    let layout = selection_toolbar_layout(ui.max_rect(), selection);
    paint::glass_panel(
        ui.painter(),
        layout.options,
        Radius::BAR,
        &theme.palette,
        true,
    );

    let manual_response = text_button(
        ui,
        theme,
        layout.manual,
        Id::new("scrozz.scroll.selection.manual"),
        "Manual",
        if control == ScrollControl::Manual {
            ButtonTone::Selected
        } else {
            ButtonTone::Neutral
        },
        true,
        interactive,
    );
    let automatic_response = text_button(
        ui,
        theme,
        layout.automatic,
        Id::new("scrozz.scroll.selection.automatic"),
        "Auto",
        if control == ScrollControl::Automatic {
            ButtonTone::Selected
        } else {
            ButtonTone::Neutral
        },
        true,
        interactive,
    );
    let cancel_response = text_button(
        ui,
        theme,
        layout.cancel,
        Id::new("scrozz.scroll.selection.cancel"),
        "Cancel",
        ButtonTone::Neutral,
        true,
        interactive,
    );
    let start_response = text_button(
        ui,
        theme,
        layout.start,
        Id::new("scrozz.scroll.selection.start"),
        if layout.compact && layout.start.width() < 100.0 {
            "Start"
        } else {
            "Start capture"
        },
        ButtonTone::LightPrimary,
        true,
        interactive,
    );

    let action = if manual_response.clicked() {
        Some(ScrollHudAction::SetControl(ScrollControl::Manual))
    } else if automatic_response.clicked() {
        Some(ScrollHudAction::SetControl(ScrollControl::Automatic))
    } else if cancel_response.clicked() {
        Some(ScrollHudAction::Cancel)
    } else if start_response.clicked() {
        Some(ScrollHudAction::Start)
    } else {
        None
    };
    ScrollHudResponse {
        rect: layout.rect,
        action,
    }
}

#[derive(Clone, Copy)]
struct SelectionToolbarLayout {
    rect: Rect,
    options: Rect,
    manual: Rect,
    automatic: Rect,
    cancel: Rect,
    start: Rect,
    compact: bool,
}

fn selection_toolbar_layout(full: Rect, selection: Rect) -> SelectionToolbarLayout {
    let width = bounded_panel_width(full, 420.0);
    let compact = width < 380.0;
    let height = if compact { 94.0 } else { 46.0 };
    let rect = anchored_panel_rect(full, selection, width, height);
    let gap = Space::XS;
    let group_gap = Space::SM;
    let row_height = if compact {
        (rect.height() - gap) * 0.5
    } else {
        rect.height()
    };
    let start_width = 148.0_f32.min(rect.width() * 0.3);
    let options_width = if compact {
        rect.width()
    } else {
        rect.width() - start_width - group_gap
    };
    let options = Rect::from_min_size(rect.min, vec2(options_width.max(1.0), row_height));
    let inner = options.shrink2(vec2(Space::XS, Space::XS));
    let cancel_width = 72.0_f32.min(inner.width());
    let choice_width = if compact {
        ((inner.width() - gap) * 0.5).max(1.0)
    } else {
        ((inner.width() - cancel_width - gap - group_gap) * 0.5).max(1.0)
    };
    let manual = Rect::from_min_size(inner.min, vec2(choice_width, inner.height().max(1.0)));
    let automatic = Rect::from_min_size(pos2(manual.right() + gap, inner.top()), manual.size());
    let (cancel, start) = if compact {
        let action_width = ((rect.width() - gap) * 0.5).max(1.0);
        let top = rect.bottom() - row_height;
        (
            Rect::from_min_size(pos2(rect.left(), top), vec2(action_width, row_height)),
            Rect::from_min_size(
                pos2(rect.left() + action_width + gap, top),
                vec2(action_width, row_height),
            ),
        )
    } else {
        (
            Rect::from_min_size(
                pos2(automatic.right() + group_gap, inner.top()),
                vec2(cancel_width, inner.height().max(1.0)),
            ),
            Rect::from_min_size(
                pos2(options.right() + group_gap, rect.top()),
                vec2(start_width.max(1.0), rect.height()),
            ),
        )
    };
    SelectionToolbarLayout {
        rect,
        options,
        manual,
        automatic,
        cancel,
        start,
        compact,
    }
}

fn draw_scrolling_progress_toolbar(
    ui: &mut Ui,
    theme: &Theme,
    selection: Rect,
    state: &ScrollHudState,
    interactive: bool,
) -> ScrollHudResponse {
    let full = ui.max_rect();
    let width = bounded_panel_width(full, 440.0);
    let compact = width < 400.0;
    let height = if compact { 94.0 } else { 54.0 };
    let rect = anchored_panel_rect(full, selection, width, height);
    paint::glass_panel(ui.painter(), rect, Radius::BAR, &theme.palette, true);
    let content = rect.shrink2(vec2(Space::SM, Space::XS));
    let gap = Space::XS;
    let (finish_width, discard_width) = if compact {
        let available = (content.width() - gap).max(2.0);
        (available * 0.55, available * 0.45)
    } else {
        (104.0, 88.0)
    };
    let row_height = if compact {
        (content.height() - gap) * 0.5
    } else {
        content.height()
    };
    let buttons_width = finish_width + discard_width + gap;
    let buttons_left = if compact {
        content.center().x - buttons_width * 0.5
    } else {
        content.right() - buttons_width
    };
    let buttons_top = if compact {
        content.bottom() - row_height
    } else {
        content.top()
    };
    let finish = Rect::from_min_size(
        pos2(buttons_left, buttons_top),
        vec2(finish_width, row_height),
    );
    let discard = Rect::from_min_size(
        pos2(finish.right() + gap, buttons_top),
        vec2(discard_width, row_height),
    );
    let info_width = if compact {
        content.width()
    } else {
        (buttons_left - Space::SM - content.left()).max(80.0)
    };
    let info = Rect::from_min_size(content.min, vec2(info_width, row_height));
    let status = status_line(state);
    ui.painter().text(
        pos2(info.left(), info.top() + 2.0),
        Align2::LEFT_TOP,
        status,
        theme.font(Text::Body),
        theme.palette.text,
    );
    ui.painter().text(
        pos2(info.left(), info.top() + 24.0),
        Align2::LEFT_TOP,
        detail_line(state),
        theme.font(Text::Caption),
        theme.palette.text_muted,
    );

    let keep_enabled = state.delta.is_some()
        && !matches!(
            state.status,
            ScrollHudStatus::Configuring | ScrollHudStatus::Failed(_) | ScrollHudStatus::Finalizing
        );
    let finish_response = text_button(
        ui,
        theme,
        finish,
        Id::new("scrozz.scroll.progress.finish"),
        "Finish",
        ButtonTone::Primary,
        keep_enabled,
        interactive,
    );
    let discard_response = text_button(
        ui,
        theme,
        discard,
        Id::new("scrozz.scroll.progress.discard"),
        "Discard",
        ButtonTone::Neutral,
        !matches!(state.status, ScrollHudStatus::Finalizing),
        interactive,
    );
    let action = if finish_response.clicked() {
        Some(ScrollHudAction::Keep)
    } else if discard_response.clicked() {
        Some(ScrollHudAction::Abort)
    } else {
        None
    };
    ScrollHudResponse { rect, action }
}

fn draw_selection_mask(painter: &egui::Painter, full: Rect, selection: Option<LogicalRect>) {
    let Some(selected) = selection.and_then(|selection| local_selection_rect(full, selection))
    else {
        return;
    };
    let scrim = Color32::from_black_alpha(152);
    for rect in [
        Rect::from_min_max(full.min, pos2(full.right(), selected.top())),
        Rect::from_min_max(
            pos2(full.left(), selected.bottom()),
            pos2(full.right(), full.bottom()),
        ),
        Rect::from_min_max(
            pos2(full.left(), selected.top()),
            pos2(selected.left(), selected.bottom()),
        ),
        Rect::from_min_max(
            pos2(selected.right(), selected.top()),
            pos2(full.right(), selected.bottom()),
        ),
    ] {
        if rect.is_positive() {
            painter.rect_filled(rect, 0.0, scrim);
        }
    }
    painter.rect_stroke(
        selected.expand(2.0),
        0.0,
        Stroke::new(4.0, Color32::from_black_alpha(190)),
        StrokeKind::Inside,
    );
    painter.rect_stroke(
        selected,
        0.0,
        Stroke::new(2.0, Color32::WHITE),
        StrokeKind::Inside,
    );
}

fn local_selection_rect(full: Rect, selection: LogicalRect) -> Option<Rect> {
    let rect = Rect::from_min_size(
        full.min + vec2(selection.origin.x as f32, selection.origin.y as f32),
        vec2(selection.size.width as f32, selection.size.height as f32),
    )
    .intersect(full);
    rect.is_positive().then_some(rect)
}

fn bounded_panel_width(full: Rect, preferred: f32) -> f32 {
    preferred.min((full.width() - Space::SM * 2.0).max(1.0))
}

fn anchored_panel_rect(full: Rect, selection: Rect, width: f32, height: f32) -> Rect {
    let gap = Space::MD;
    let horizontal_margin = Space::SM.min((full.width() * 0.5).max(0.0));
    let vertical_margin = Space::SM.min((full.height() * 0.5).max(0.0));
    let width = width.min((full.width() - horizontal_margin * 2.0).max(1.0));
    let height = height.min((full.height() - vertical_margin * 2.0).max(1.0));
    let min_left = full.left() + horizontal_margin;
    let max_left = (full.right() - width - horizontal_margin).max(min_left);
    let left = (selection.center().x - width * 0.5).clamp(min_left, max_left);
    let below = selection.bottom() + gap;
    let above = selection.top() - gap - height;
    let min_top = full.top() + vertical_margin;
    let max_top = (full.bottom() - height - vertical_margin).max(min_top);
    let top = if below + height <= full.bottom() - vertical_margin {
        below
    } else if above >= min_top {
        above
    } else {
        below.clamp(min_top, max_top)
    };
    Rect::from_min_size(pos2(left, top), vec2(width, height))
}

fn hud_rect(full: Rect, width: f32, height: f32, state: &ScrollHudState) -> Rect {
    if let Some(selected) = state
        .selection
        .and_then(|selection| local_selection_rect(full, selection))
    {
        return anchored_panel_rect(full, selected, width, height);
    }

    let top = if matches!(
        state.status,
        ScrollHudStatus::Configuring | ScrollHudStatus::Failed(_)
    ) {
        full.top() + Space::XXL
    } else {
        full.bottom() - Space::XXL - height
    };
    Rect::from_min_size(
        pos2(full.center().x - width * 0.5, top),
        vec2(width, height),
    )
}

fn status_line(state: &ScrollHudState) -> String {
    match &state.status {
        ScrollHudStatus::Configuring | ScrollHudStatus::Failed(_) => String::new(),
        ScrollHudStatus::Prepared if state.automatic => {
            "Scroll once. Scrozz will continue.".to_owned()
        }
        ScrollHudStatus::Prepared => "Scroll in any direction.".to_owned(),
        ScrollHudStatus::Capturing if state.automatic => match state.direction {
            Some(direction) => format!("Scrolling {}…", direction_word(direction)),
            None => "Waiting for your first scroll…".to_owned(),
        },
        ScrollHudStatus::Capturing => "Following your scroll…".to_owned(),
        ScrollHudStatus::WaitingForManualScroll if state.direction.is_none() && state.automatic => {
            "Scroll once. Scrozz will continue.".to_owned()
        }
        ScrollHudStatus::WaitingForManualScroll if state.direction.is_none() => {
            "Scroll in any direction.".to_owned()
        }
        ScrollHudStatus::WaitingForManualScroll => format!(
            "Keep scrolling {}.",
            direction_word(state.direction.expect("checked above"))
        ),
        ScrollHudStatus::Stalled(_) if state.delta.is_some() => "No new movement.".to_owned(),
        ScrollHudStatus::Stalled(_) => "Scroll once or discard.".to_owned(),
        ScrollHudStatus::WaitingForOverlap => "Scroll back slowly to reconnect.".to_owned(),
        ScrollHudStatus::AwaitingFinish(_) => "Paused. Finish or discard.".to_owned(),
        ScrollHudStatus::Finalizing => "Finalizing the stitched image…".to_owned(),
    }
}

fn detail_line(state: &ScrollHudState) -> String {
    if let ScrollHudStatus::AwaitingFinish(reason) = &state.status {
        return reason.clone();
    }
    let route = match state.direction {
        Some(direction) => direction_label(direction),
        None => "Detecting direction",
    };
    let extent = match state.direction.map(ScrollDirection::axis) {
        Some(ScrollAxis::Horizontal) => "wide",
        _ => "tall",
    };
    let delta = state.delta.map_or_else(
        || "measuring overlap".to_owned(),
        |delta| format!("Δ {delta} px"),
    );
    let stall = match &state.status {
        ScrollHudStatus::Stalled(count) => format!(" · idle probe {count}"),
        _ => String::new(),
    };
    format!(
        "{route} · frame {} · {delta} · {} px {extent}{stall}",
        state.frame.max(1),
        state.output_extent
    )
}

const fn direction_label(direction: ScrollDirection) -> &'static str {
    match direction {
        ScrollDirection::Up => "Up",
        ScrollDirection::Down => "Down",
        ScrollDirection::Left => "Left",
        ScrollDirection::Right => "Right",
    }
}

const fn direction_word(direction: ScrollDirection) -> &'static str {
    match direction {
        ScrollDirection::Up => "up",
        ScrollDirection::Down => "down",
        ScrollDirection::Left => "left",
        ScrollDirection::Right => "right",
    }
}

#[derive(Clone, Copy)]
enum ButtonTone {
    Neutral,
    Selected,
    Primary,
    LightPrimary,
}

#[allow(clippy::too_many_arguments)]
fn text_button(
    ui: &mut Ui,
    theme: &Theme,
    rect: Rect,
    id: Id,
    label: &str,
    tone: ButtonTone,
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
    } else if matches!(tone, ButtonTone::LightPrimary) {
        if hovered {
            Color32::WHITE
        } else {
            palette.text
        }
    } else if matches!(tone, ButtonTone::Primary) {
        if hovered {
            palette.accent_hi
        } else {
            palette.accent
        }
    } else if matches!(tone, ButtonTone::Selected) {
        if hovered {
            palette.hover
        } else {
            palette.card_fill_raised
        }
    } else if hovered {
        palette.hover
    } else {
        palette.chip_fill
    };
    let foreground = if !enabled {
        palette.text_faint
    } else if matches!(tone, ButtonTone::LightPrimary) {
        palette.card_fill
    } else if matches!(tone, ButtonTone::Primary) {
        palette.on_accent
    } else if matches!(tone, ButtonTone::Selected) {
        palette.accent_hi
    } else {
        palette.text
    };
    let radius = Radius::pill(rect.height());
    ui.painter().rect_filled(rect, corner(radius), fill);
    ui.painter().rect_stroke(
        rect,
        corner(radius),
        Stroke::new(
            if matches!(tone, ButtonTone::Selected) {
                1.5
            } else {
                1.0
            },
            if matches!(tone, ButtonTone::Selected) {
                palette.accent
            } else {
                palette.hairline
            },
        ),
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
    fn prepared_state_starts_without_inventing_a_route() {
        let state = ScrollHudState::prepared(ScrollControl::Automatic);
        assert_eq!(state.status, ScrollHudStatus::Prepared);
        assert!(state.automatic);
        assert_eq!(state.direction, None);
    }

    #[test]
    fn detail_names_the_detected_route_and_output_dimension() {
        let mut state = ScrollHudState::prepared(ScrollControl::Automatic)
            .with_direction(ScrollDirection::Down);
        state.frame = 3;
        state.output_extent = 2_400;
        assert!(detail_line(&state).contains("tall"));
        assert!(detail_line(&state).contains("Down"));
        state = state.with_direction(ScrollDirection::Right);
        assert!(detail_line(&state).contains("wide"));
        assert!(detail_line(&state).contains("Right"));
    }

    #[test]
    fn progress_rail_vacates_the_setup_click_location() {
        let full = Rect::from_min_size(pos2(0.0, 0.0), vec2(1_024.0, 768.0));
        let chooser_state = ScrollHudState::configuring();
        let progress_state = ScrollHudState::prepared(ScrollControl::Automatic);
        let chooser = hud_rect(full, 600.0, 164.0, &chooser_state);
        let progress = hud_rect(full, 440.0, 150.0, &progress_state);

        assert!(!chooser.intersects(progress));
        assert!(chooser.top() < full.center().y);
        assert!(progress.top() > full.center().y);
    }

    #[test]
    fn setup_requires_an_explicit_start_button_press() {
        let ctx = egui::Context::default();
        let state = ScrollHudState::configuring();
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
            pos2(content.right() - 132.0, content.bottom() - 34.0),
            vec2(132.0, 34.0),
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
        assert_eq!(draw(input(false)).action, Some(ScrollHudAction::Start));
    }

    #[test]
    fn anchored_controls_stay_inside_a_narrow_surface() {
        for width in [120.0, 180.0] {
            let full = Rect::from_min_size(pos2(0.0, 0.0), vec2(width, 120.0));
            let selection =
                Rect::from_min_size(pos2(20.0, 30.0), vec2((width - 40.0).max(1.0), 50.0));
            let controls = selection_toolbar_layout(full, selection);
            for rect in [
                controls.rect,
                controls.options,
                controls.manual,
                controls.automatic,
                controls.cancel,
                controls.start,
            ] {
                assert!(full.contains_rect(rect), "{rect:?} escaped {full:?}");
                assert!(rect.is_positive());
            }
            assert!(
                ScrollingHud::detached_viewport_size(&ScrollHudState::configuring(), width).x
                    <= width
            );
        }
    }
}
