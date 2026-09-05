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
use scrozz_core::{LogicalRect, ScaleFactor, ScrollControl, ScrollDirection};

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
    /// The first viewport is still being acquired; scrolling is not ready yet.
    Starting,
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

    /// No readiness hint until the capture worker has a baseline.
    #[must_use]
    pub fn starting(control: ScrollControl) -> Self {
        Self {
            status: ScrollHudStatus::Starting,
            ..Self::prepared(control)
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
    /// A noninteractive, aspect-fitted view of the accepted stitched pixels.
    pub(crate) fn draw_preview(
        ui: &mut Ui,
        theme: &Theme,
        state: &ScrollHudState,
        texture: egui::TextureId,
        source_px: (u32, u32),
    ) {
        let full = ui.max_rect();
        let selection = state
            .selection
            .and_then(|selection| local_selection_rect(full, selection));
        let controls = Self::control_rect(full, state);
        // macOS scrolling captures an isolated SCWindow, not composited desktop
        // pixels. Other capture paths must leave the selected pixels uncovered.
        let allow_overlap = cfg!(target_os = "macos") && selection.is_some();
        if let Some(image) = preview_rect(full, selection, controls, source_px, allow_overlap) {
            paint::glass_panel(ui.painter(), image.expand(5.0), 10.0, &theme.palette, true);
            ui.painter().image(
                texture,
                image,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
    }

    /// Bounds of the interactive controls in the root overlay's coordinates.
    #[must_use]
    pub(crate) fn control_rect(full: Rect, state: &ScrollHudState) -> Rect {
        let setup = matches!(
            state.status,
            ScrollHudStatus::Configuring | ScrollHudStatus::Failed(_)
        );
        let size = toolbar_size(
            full.width(),
            matches!(state.status, ScrollHudStatus::Failed(_)),
        );
        if let Some(selection) = state
            .selection
            .and_then(|selection| local_selection_rect(full, selection))
        {
            return anchored_panel_rect(full, selection, size.x, size.y);
        }
        let selection = Rect::from_center_size(
            pos2(
                full.center().x,
                if setup {
                    full.top() + Space::XXL
                } else {
                    full.bottom()
                },
            ),
            vec2(1.0, 1.0),
        );
        anchored_panel_rect(full, selection, size.x, size.y)
    }

    /// Size of a detached interactive control island.
    #[must_use]
    pub(crate) fn detached_viewport_size(
        state: &ScrollHudState,
        available_width: f32,
    ) -> egui::Vec2 {
        let viewport_width = (420.0 + Space::LG * 2.0).min(available_width.max(1.0));
        let size = toolbar_size(
            viewport_width,
            matches!(state.status, ScrollHudStatus::Failed(_)),
        );
        vec2(viewport_width, size.y + Space::SM * 2.0)
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
        let size = toolbar_size(
            full.width(),
            matches!(state.status, ScrollHudStatus::Failed(_)),
        );
        let rect = Rect::from_center_size(full.center(), size);
        if matches!(
            state.status,
            ScrollHudStatus::Configuring | ScrollHudStatus::Failed(_)
        ) {
            draw_setup_toolbar(ui, theme, rect, state, interactive)
        } else {
            draw_scrolling_progress_toolbar(ui, theme, rect, state, interactive)
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
        Self::draw_boundary(ui, state);
        if setup {
            draw_setup_toolbar(ui, theme, rect, state, interactive)
        } else {
            draw_scrolling_progress_toolbar(ui, theme, rect, state, interactive)
        }
    }
}

fn preview_rect(
    full: Rect,
    selection: Option<Rect>,
    controls: Rect,
    source_px: (u32, u32),
    allow_overlap: bool,
) -> Option<Rect> {
    if source_px.0 == 0 || source_px.1 == 0 {
        return None;
    }
    // A portal can capture an unknown source. Never place a preview over it.
    let selection = selection?;
    let safe = full.shrink(Space::LG);
    let source = vec2(source_px.0 as f32, source_px.1 as f32);
    let fit = |area: Rect| {
        if area.width() < 20.0 || area.height() < 20.0 {
            return None;
        }
        let scale = (area.width().min(240.0) / source.x)
            .min(area.height().min(620.0) / source.y)
            .min(1.0);
        let size = source * scale;
        Some(Rect::from_center_size(area.center(), size))
    };
    let outside = |obstacle: Rect, area: Rect| {
        [
            Rect::from_min_max(pos2(obstacle.right(), area.top()), area.max),
            Rect::from_min_max(area.min, pos2(obstacle.left(), area.bottom())),
            Rect::from_min_max(area.min, pos2(area.right(), obstacle.top())),
            Rect::from_min_max(pos2(area.left(), obstacle.bottom()), area.max),
        ]
        .map(|rect| rect.intersect(area))
    };
    let regions = outside(selection.expand(Space::LG), safe);
    let mut best: Option<(f32, Rect)> = None;
    for (index, region) in regions.into_iter().enumerate() {
        for area in outside(controls.expand(Space::LG), region) {
            if let Some(rect) = fit(area) {
                let score = rect.area()
                    * match index {
                        0 => 1.12,
                        1 => 1.1,
                        _ => 1.0,
                    };
                if best.is_none_or(|(previous, _)| score > previous) {
                    best = Some((score, rect));
                }
            }
        }
    }
    if best.is_none() && allow_overlap {
        for area in outside(controls.expand(Space::LG), safe) {
            // Only the isolated-window path may use an on-screen corner when
            // a full-screen selection leaves no external strip.
            let corner = Rect::from_min_max(
                pos2((area.right() - 200.0).max(area.left()), area.top()),
                area.max,
            );
            if let Some(rect) = fit(corner)
                && best.is_none_or(|(previous, _)| rect.area() > previous)
            {
                best = Some((rect.area(), rect));
            }
        }
    }
    best.map(|(_, rect)| rect)
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
                scrozz_core::LogicalPoint::new(160.0, 80.0),
                scrozz_core::LogicalSize::new(520.0, 340.0),
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
                    scrozz_core::LogicalPoint::new(160.0, 80.0),
                    scrozz_core::LogicalSize::new(520.0, 340.0),
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
                    scrozz_core::LogicalPoint::new(160.0, 80.0),
                    scrozz_core::LogicalSize::new(520.0, 340.0),
                )),
                surface: None,
            },
        };
        let _ = ScrollingHud::draw(ui, &theme, &state, false);
        let vertical = ctx.millis() < 300;
        let size = if vertical { [128, 614] } else { [640, 50] };
        let mut image = egui::ColorImage::filled(size, Color32::from_rgb(247, 248, 250));
        for y in 6..size[1] - 6 {
            for x in 6..size[0] - 6 {
                if y % 48 < 10 {
                    image.pixels[y * size[0] + x] = Color32::from_rgb(45, 64, 86);
                } else if y % 8 < 3 && x % 120 < 104 {
                    image.pixels[y * size[0] + x] = Color32::from_rgb(180, 186, 194);
                }
            }
        }
        let texture = ui.ctx().load_texture(
            "scrolling-preview-fixture",
            image,
            egui::TextureOptions::LINEAR,
        );
        ui.ctx().data_mut(|data| {
            data.insert_temp(Id::new("scrolling-preview-fixture"), texture.clone())
        });
        let source_px = if vertical { (520, 2_496) } else { (4_320, 340) };
        ScrollingHud::draw_preview(ui, &theme, &state, texture.id(), source_px);
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
    let mut state = ScrollHudState::configuring();
    state.control = control;
    draw_setup_toolbar(ui, theme, layout.rect, &state, interactive)
}

fn draw_setup_toolbar(
    ui: &mut Ui,
    theme: &Theme,
    rect: Rect,
    state: &ScrollHudState,
    interactive: bool,
) -> ScrollHudResponse {
    paint::glass_panel(ui.painter(), rect, Radius::BAR, &theme.palette, true);
    let mut controls = rect;
    if let ScrollHudStatus::Failed(reason) = &state.status {
        let notice = Rect::from_min_size(
            rect.min + vec2(Space::SM, Space::XS),
            vec2(rect.width() - Space::SM * 2.0, 24.0),
        );
        draw_status_text(ui, theme, notice, reason, reason, theme.palette.warning);
        controls.min.y += 28.0;
    }
    let layout = SelectionToolbarLayout::in_rect(controls);

    let manual_response = text_button(
        ui,
        theme,
        layout.manual,
        Id::new("scrozz.scroll.selection.manual"),
        "Manual",
        if state.control == ScrollControl::Manual {
            ButtonTone::Selected
        } else {
            ButtonTone::Neutral
        },
        true,
        interactive,
    )
    .on_hover_text("You scroll. Scrozz follows until you choose Finish.");
    let automatic_response = text_button(
        ui,
        theme,
        layout.automatic,
        Id::new("scrozz.scroll.selection.automatic"),
        "Auto",
        if state.control == ScrollControl::Automatic {
            ButtonTone::Selected
        } else {
            ButtonTone::Neutral
        },
        true,
        interactive,
    )
    .on_hover_text("Scroll once. Scrozz continues in that direction until you choose Finish.");
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
        "Start",
        ButtonTone::Primary,
        true,
        interactive,
    );
    start_response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Start capture"));

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
    ScrollHudResponse { rect, action }
}

#[derive(Clone, Copy)]
struct SelectionToolbarLayout {
    rect: Rect,
    manual: Rect,
    automatic: Rect,
    cancel: Rect,
    start: Rect,
}

fn selection_toolbar_layout(full: Rect, selection: Rect) -> SelectionToolbarLayout {
    let size = toolbar_size(full.width(), false);
    SelectionToolbarLayout::in_rect(anchored_panel_rect(full, selection, size.x, size.y))
}

impl SelectionToolbarLayout {
    fn in_rect(rect: Rect) -> Self {
        let compact = rect.width() < 380.0;
        let inner = rect.shrink(Space::XS);
        let gap = Space::XS;
        let height = if compact {
            (inner.height() - gap) * 0.5
        } else {
            inner.height()
        };
        let choice_width = if compact {
            (inner.width() - gap) * 0.5
        } else {
            80.0
        };
        let manual = Rect::from_min_size(inner.min, vec2(choice_width, height));
        let automatic = Rect::from_min_size(pos2(manual.right() + gap, inner.top()), manual.size());
        let action_left = if compact {
            inner.left()
        } else {
            automatic.right() + Space::MD
        };
        let action_top = if compact {
            automatic.bottom() + gap
        } else {
            inner.top()
        };
        let action_width = (inner.right() - action_left - gap) * 0.5;
        let cancel = Rect::from_min_size(pos2(action_left, action_top), vec2(action_width, height));
        let start = Rect::from_min_size(pos2(cancel.right() + gap, action_top), cancel.size());
        Self {
            rect,
            manual,
            automatic,
            cancel,
            start,
        }
    }
}

fn toolbar_size(available_width: f32, failed: bool) -> egui::Vec2 {
    let width = 420.0_f32.min((available_width - Space::SM * 2.0).max(1.0));
    let height = if width < 380.0 { 88.0 } else { 48.0 };
    vec2(width, height + if failed { 28.0 } else { 0.0 })
}

#[derive(Clone, Copy)]
struct ProgressToolbarLayout {
    status: Rect,
    discard: Rect,
    finish: Rect,
}

impl ProgressToolbarLayout {
    fn in_rect(rect: Rect) -> Self {
        let compact = rect.width() < 380.0;
        let content = rect.shrink(Space::XS);
        let gap = Space::XS;
        let height = if compact {
            (content.height() - gap) * 0.5
        } else {
            content.height()
        };
        let button_width = if compact {
            (content.width() - gap) * 0.5
        } else {
            80.0
        };
        let top = content.bottom() - height;
        let finish = Rect::from_min_size(
            pos2(content.right() - button_width, top),
            vec2(button_width, height),
        );
        let discard =
            Rect::from_min_size(pos2(finish.left() - gap - button_width, top), finish.size());
        let status = Rect::from_min_max(
            content.min,
            pos2(
                if compact {
                    content.right()
                } else {
                    discard.left() - Space::SM
                },
                content.top() + height,
            ),
        );
        Self {
            status,
            discard,
            finish,
        }
    }
}

fn draw_scrolling_progress_toolbar(
    ui: &mut Ui,
    theme: &Theme,
    rect: Rect,
    state: &ScrollHudState,
    interactive: bool,
) -> ScrollHudResponse {
    paint::glass_panel(ui.painter(), rect, Radius::BAR, &theme.palette, true);
    let layout = ProgressToolbarLayout::in_rect(rect);
    let warning = matches!(
        state.status,
        ScrollHudStatus::WaitingForOverlap | ScrollHudStatus::AwaitingFinish(_)
    );
    let dot = pos2(layout.status.left() + Space::MD, layout.status.center().y);
    ui.painter().circle_filled(
        dot,
        3.0,
        if warning {
            theme.palette.warning
        } else {
            theme.palette.text_muted
        },
    );
    let status_rect = Rect::from_min_max(
        pos2(dot.x + Space::MD, layout.status.top()),
        layout.status.max,
    );
    draw_status_text(
        ui,
        theme,
        status_rect,
        status_line(state),
        status_hint(state),
        theme.palette.text,
    );

    let keep_enabled = state.delta.is_some()
        && !matches!(
            state.status,
            ScrollHudStatus::Starting
                | ScrollHudStatus::Configuring
                | ScrollHudStatus::Failed(_)
                | ScrollHudStatus::Finalizing
        );
    let finish_response = text_button(
        ui,
        theme,
        layout.finish,
        Id::new("scrozz.scroll.progress.finish"),
        "Finish",
        ButtonTone::Primary,
        keep_enabled,
        interactive,
    );
    let discard_response = text_button(
        ui,
        theme,
        layout.discard,
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

fn status_line(state: &ScrollHudState) -> &'static str {
    match &state.status {
        ScrollHudStatus::Configuring | ScrollHudStatus::Failed(_) => "",
        ScrollHudStatus::Starting => "Getting ready",
        ScrollHudStatus::WaitingForOverlap => "Scroll back a little",
        ScrollHudStatus::AwaitingFinish(_) => "Capture paused",
        ScrollHudStatus::Finalizing => "Finishing",
        _ if state.delta.is_some() && state.automatic => "Auto scrolling",
        _ if state.delta.is_some() => "Capturing",
        _ if state.automatic => "Scroll to start Auto",
        _ => "Scroll to begin",
    }
}

fn status_hint(state: &ScrollHudState) -> &str {
    match &state.status {
        ScrollHudStatus::Starting => "Preparing the first frame. Wait before scrolling.",
        ScrollHudStatus::WaitingForOverlap => {
            "Scroll back slowly until capture reconnects, or Finish to keep what was captured."
        }
        ScrollHudStatus::AwaitingFinish(reason) | ScrollHudStatus::Failed(reason) => reason,
        ScrollHudStatus::Finalizing => "Preparing your screenshot.",
        _ if state.delta.is_some() => "Finish when you have everything. Discard keeps nothing.",
        _ if state.automatic => "Scroll once in any direction. Scrozz will continue for you.",
        _ => "Scroll slowly in one direction. Finish when you have everything.",
    }
}

fn draw_status_text(
    ui: &mut Ui,
    theme: &Theme,
    rect: Rect,
    text: &str,
    hint: &str,
    color: Color32,
) {
    let response = ui.interact(rect, Id::new("scrozz.scroll.status"), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    response.on_hover_text(hint);
    draw_fitted_text(
        ui,
        rect,
        text,
        theme.font(Text::Body),
        color,
        Align2::LEFT_CENTER,
    );
}

fn draw_fitted_text(
    ui: &Ui,
    rect: Rect,
    text: &str,
    font: egui::FontId,
    color: Color32,
    align: Align2,
) {
    let mut job =
        egui::text::LayoutJob::simple(text.to_owned(), font, color, rect.width().max(1.0));
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    let at = if align == Align2::LEFT_CENTER {
        pos2(rect.left(), rect.center().y - galley.size().y * 0.5)
    } else {
        rect.center() - galley.size() * 0.5
    };
    ui.painter()
        .with_clip_rect(rect.intersect(ui.clip_rect()))
        .galley(at, galley, color);
}

#[derive(Clone, Copy)]
enum ButtonTone {
    Neutral,
    Selected,
    Primary,
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
    response.widget_info(|| {
        WidgetInfo::selected(
            WidgetType::Button,
            enabled,
            matches!(tone, ButtonTone::Selected),
            label,
        )
    });

    let palette = &theme.palette;
    let hovered = interactive && enabled && response.hovered();
    let fill = if !enabled {
        palette.chip_fill
    } else if matches!(tone, ButtonTone::Primary) {
        if hovered {
            palette.accent_hi
        } else {
            palette.accent
        }
    } else if matches!(tone, ButtonTone::Selected) {
        palette.text
    } else if hovered {
        palette.hover
    } else {
        Color32::TRANSPARENT
    };
    let foreground = if !enabled {
        palette.text_faint
    } else if matches!(tone, ButtonTone::Primary) {
        palette.on_accent
    } else if matches!(tone, ButtonTone::Selected) {
        if palette.is_dark() {
            palette.on_accent
        } else {
            Color32::WHITE
        }
    } else {
        palette.text
    };
    let radius = Radius::pill(rect.height());
    ui.painter().rect_filled(rect, corner(radius), fill);
    if response.has_focus() && enabled {
        ui.painter().rect_stroke(
            rect.shrink(2.0),
            corner(radius),
            Stroke::new(2.0, palette.focus_ring),
            StrokeKind::Inside,
        );
    }
    draw_fitted_text(
        ui,
        rect.shrink2(vec2(Space::XS, 0.0)),
        label,
        theme.font(Text::Button),
        foreground,
        Align2::CENTER_CENTER,
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
    fn live_preview_fits_the_screen_without_covering_capture_or_controls() {
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(1_280.0, 800.0));
        let selection = Rect::from_min_size(pos2(240.0, 120.0), vec2(680.0, 420.0));
        let controls = anchored_panel_rect(screen, selection, 420.0, 48.0);
        for size in [(680, 420), (680, 4_200), (680, 90_000), (20_000, 420)] {
            let preview =
                preview_rect(screen, Some(selection), controls, size, false).expect("preview");
            assert!(screen.contains_rect(preview.expand(5.0)));
            assert!(!preview.expand(5.0).intersects(selection));
            assert!(!preview.expand(5.0).intersects(controls));
            let aspect = size.0 as f32 / size.1 as f32;
            assert!((preview.aspect_ratio() - aspect).abs() < 0.001, "{size:?}");
        }
    }

    #[test]
    fn full_screen_preview_fallback_requires_isolated_capture() {
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(1_280.0, 800.0));
        let controls = Rect::from_min_size(pos2(420.0, 720.0), vec2(420.0, 48.0));
        assert!(preview_rect(screen, Some(screen), controls, (1_280, 5_000), false).is_none());
        let preview = preview_rect(screen, Some(screen), controls, (1_280, 5_000), true)
            .expect("isolated preview");
        assert!(screen.contains_rect(preview.expand(5.0)));
        assert!(!preview.expand(5.0).intersects(controls));
        assert!(preview_rect(screen, None, controls, (1_280, 5_000), true).is_none());
    }

    #[test]
    fn polling_and_direction_changes_do_not_change_normal_status_text() {
        for control in [ScrollControl::Manual, ScrollControl::Automatic] {
            let mut state = ScrollHudState::prepared(control);
            for delta in [None, Some(612)] {
                state.delta = delta;
                let expected = match (control, delta.is_some()) {
                    (ScrollControl::Automatic, true) => "Auto scrolling",
                    (ScrollControl::Automatic, false) => "Scroll to start Auto",
                    (ScrollControl::Manual, true) => "Capturing",
                    (ScrollControl::Manual, false) => "Scroll to begin",
                };
                for status in [
                    ScrollHudStatus::Prepared,
                    ScrollHudStatus::Capturing,
                    ScrollHudStatus::WaitingForManualScroll,
                    ScrollHudStatus::Stalled(1),
                    ScrollHudStatus::Stalled(999),
                ] {
                    state.status = status;
                    for direction in [
                        ScrollDirection::Up,
                        ScrollDirection::Down,
                        ScrollDirection::Left,
                        ScrollDirection::Right,
                    ] {
                        state.direction = Some(direction);
                        assert_eq!(status_line(&state), expected);
                    }
                }
            }
        }
        assert_eq!(
            status_line(&ScrollHudState::starting(ScrollControl::Manual)),
            "Getting ready"
        );
    }

    #[test]
    fn progress_rail_vacates_the_setup_click_location() {
        let full = Rect::from_min_size(pos2(0.0, 0.0), vec2(1_024.0, 768.0));
        let chooser_state = ScrollHudState::configuring();
        let progress_state = ScrollHudState::prepared(ScrollControl::Automatic);
        let chooser = ScrollingHud::control_rect(full, &chooser_state);
        let progress = ScrollingHud::control_rect(full, &progress_state);

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
        let button = SelectionToolbarLayout::in_rect(chooser).start;
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
        for width in [120.0, 180.0, 360.0, 420.0, 800.0] {
            let full = Rect::from_min_size(pos2(0.0, 0.0), vec2(width, 120.0));
            let selection =
                Rect::from_min_size(pos2(20.0, 30.0), vec2((width - 40.0).max(1.0), 50.0));
            let controls = selection_toolbar_layout(full, selection);
            for rect in [
                controls.rect,
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
            let state = ScrollHudState::prepared(ScrollControl::Manual);
            let size = ScrollingHud::detached_viewport_size(&state, width);
            let viewport = Rect::from_min_size(pos2(0.0, 0.0), size);
            let rail = Rect::from_center_size(viewport.center(), toolbar_size(size.x, false));
            let progress = ProgressToolbarLayout::in_rect(rail);
            for control in [progress.status, progress.finish, progress.discard] {
                assert!(rail.contains_rect(control), "{control:?} escaped {rail:?}");
            }
            assert!(!progress.finish.intersects(progress.discard));
            assert!(!progress.status.intersects(progress.finish));
        }
    }

    #[test]
    fn normal_polling_is_pixel_identical_without_flashing_counters_or_labels() {
        use crate::harness::{
            Background, RenderSpec, Scenario, SceneRegistry, SoftwareRenderer, VirtualClock,
        };

        struct Rail(ScrollHudState);
        impl Scene for Rail {
            fn name(&self) -> &str {
                "scroll-rail"
            }
            fn setup(&self, ctx: &egui::Context) {
                install_fonts(ctx);
            }
            fn ui(&self, ui: &mut Ui, _ctx: &SceneCtx<'_>) {
                let theme = Theme::dark();
                install_style(ui.ctx(), &theme);
                let _ = ScrollingHud::draw_detached(ui, &theme, &self.0, false);
            }
        }
        let render = |state, millis| {
            let mut registry = SceneRegistry::empty();
            registry.register(Scenario::ScrollingCapture, Box::new(Rail(state)));
            SoftwareRenderer::new(registry)
                .render(
                    &RenderSpec::golden(
                        Scenario::ScrollingCapture,
                        VirtualClock::from_millis(millis),
                    )
                    .with_size_pt((452.0, 64.0))
                    .with_background(Background::Transparent),
                )
                .expect("render rail")
                .fingerprint()
        };
        for started in [false, true] {
            let mut state = ScrollHudState::prepared(ScrollControl::Manual);
            state.delta = started.then_some(100);
            let baseline = render(state.clone(), 0);
            for (frame, status) in [
                (1, ScrollHudStatus::WaitingForManualScroll),
                (2, ScrollHudStatus::Capturing),
                (999, ScrollHudStatus::Stalled(900)),
            ] {
                state.status = status;
                state.frame = frame;
                state.output_extent = 20_000;
                state.direction = Some(ScrollDirection::Up);
                assert_eq!(render(state.clone(), 8_000), baseline);
            }
        }
    }

    #[test]
    fn paused_controls_remain_actionable_and_finalizing_controls_do_not() {
        use egui::accesskit::{Action, ActionRequest};
        for detached in [false, true] {
            for status in [
                ScrollHudStatus::Starting,
                ScrollHudStatus::WaitingForOverlap,
                ScrollHudStatus::AwaitingFinish("Capture limit reached".into()),
                ScrollHudStatus::Finalizing,
            ] {
                let ctx = egui::Context::default();
                ctx.enable_accesskit();
                install_fonts(&ctx);
                let theme = Theme::dark();
                install_style(&ctx, &theme);
                let mut state = ScrollHudState::prepared(ScrollControl::Manual);
                state.status = status.clone();
                state.delta = Some(10);
                let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(452.0, 180.0));
                let draw = |events| {
                    let mut action = None;
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(screen),
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            action = if detached {
                                ScrollingHud::draw_detached(ui, &theme, &state, true)
                            } else {
                                ScrollingHud::draw(ui, &theme, &state, true)
                            }
                            .action;
                        },
                    );
                    output.textures_delta.clear();
                    (output, action)
                };
                draw(vec![]);
                let (output, _) = draw(vec![]);
                let update = output.platform_output.accesskit_update.expect("controls");
                for (label, expected) in [
                    ("Finish", ScrollHudAction::Keep),
                    ("Discard", ScrollHudAction::Abort),
                ] {
                    let node = update
                        .nodes
                        .iter()
                        .find(|(_, node)| node.label() == Some(label))
                        .expect("button");
                    let disabled = status == ScrollHudStatus::Finalizing
                        || (label == "Finish" && status == ScrollHudStatus::Starting);
                    assert_eq!(node.1.is_disabled(), disabled);
                    if !disabled {
                        let (_, action) =
                            draw(vec![egui::Event::AccessKitActionRequest(ActionRequest {
                                action: Action::Click,
                                target_tree: update.tree_id,
                                target_node: node.0,
                                data: None,
                            })]);
                        assert_eq!(action, Some(expected));
                    }
                }
            }
        }
    }
}
