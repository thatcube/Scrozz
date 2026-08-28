#![allow(missing_docs)]

use scrozz_core::selection::{
    SelectionCapabilities, SelectionMode, SelectionOptions, SelectionOutcome, SelectionSource,
    SizeConstraint,
};
use scrozz_core::{
    CaptureTarget, Display, DisplayId, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor, Window,
    WindowId,
};

use super::geom::{self, DisplayLayout};

const HANDLE_RADIUS: f64 = 7.0;
const EDGE_BAND: f64 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeHandle {
    NorthWest,
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DragModifiers {
    pub shift: bool,
    pub alt: bool,
    pub space: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionAnnouncement(pub String);

#[derive(Debug, Clone, PartialEq)]
pub struct SelectionState {
    options: SelectionOptions,
    layout: DisplayLayout,
    windows: Vec<Window>,
    capabilities: SelectionCapabilities,
    mode: SelectionMode,
    region: Option<LogicalRect>,
    remembered: Option<LogicalRect>,
    remembered_display: Option<DisplayId>,
    region_display: Option<DisplayId>,
    active_display: Option<DisplayId>,
    hovered_display: Option<DisplayId>,
    hovered_window: Option<WindowId>,
    last_pointer: Option<LogicalPoint>,
    phase: Phase,
    drag_modifiers: DragModifiers,
    axis_lock: Option<AxisLock>,
    gesture_changed: bool,
    announcement: Option<SelectionAnnouncement>,
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle,
    Creating {
        anchor: LogicalPoint,
        display: DisplayId,
        space_move: Option<SpaceMove>,
    },
    Moving {
        grab: (f64, f64),
        display: DisplayId,
    },
    Resizing {
        handle: ResizeHandle,
        display: DisplayId,
    },
    PlacingExact {
        display: DisplayId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct SpaceMove {
    pointer: LogicalPoint,
    region: LogicalRect,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AxisLock {
    Pending {
        pointer: LogicalPoint,
        dx: f64,
        dy: f64,
    },
    Horizontal {
        dy: f64,
    },
    Vertical {
        dx: f64,
    },
}

impl SelectionState {
    #[must_use]
    pub fn new(
        options: SelectionOptions,
        layout: DisplayLayout,
        capabilities: SelectionCapabilities,
    ) -> Self {
        Self::new_with_windows(options, layout, capabilities, Vec::new())
    }

    #[must_use]
    pub fn new_with_windows(
        options: SelectionOptions,
        layout: DisplayLayout,
        capabilities: SelectionCapabilities,
        windows: Vec<Window>,
    ) -> Self {
        let remembered = options.remembered.and_then(|rect| {
            let display = options
                .remembered_display
                .as_ref()
                .and_then(|id| layout.display(id))
                .filter(|display| geom::contains_rect(display.bounds, rect))
                .or_else(|| layout.display_owning_rect(rect))?;
            normalize_region(display.bounds, rect, options.constraint)
                .map(|rect| (rect, display.id.clone()))
        });
        let remembered_display = remembered.as_ref().map(|(_, display)| display.clone());
        let remembered = remembered.map(|(rect, _)| rect);
        let region_display = remembered_display.clone();
        let active_display = remembered_display
            .clone()
            .or_else(|| {
                layout
                    .displays()
                    .iter()
                    .find(|display| display.is_primary)
                    .map(|display| display.id.clone())
            })
            .or_else(|| layout.displays().first().map(|display| display.id.clone()));
        let mut state = Self {
            mode: if capabilities.supports(options.mode) {
                options.mode
            } else {
                default_mode(capabilities)
            },
            announcement: remembered.map(|rect| {
                SelectionAnnouncement(format!("Remembered selection {}", describe_rect(rect)))
            }),
            options,
            layout,
            windows,
            capabilities,
            region: remembered,
            remembered,
            remembered_display,
            region_display,
            active_display,
            hovered_display: None,
            hovered_window: None,
            last_pointer: None,
            phase: Phase::Idle,
            drag_modifiers: DragModifiers::default(),
            axis_lock: None,
            gesture_changed: false,
        };
        if !state.capabilities.supports(state.mode) {
            state.mode = default_mode(state.capabilities);
        }
        state
    }

    #[must_use]
    pub const fn mode(&self) -> SelectionMode {
        self.mode
    }

    #[must_use]
    pub const fn region(&self) -> Option<LogicalRect> {
        self.region
    }

    #[must_use]
    pub fn active_display(&self) -> Option<&DisplayId> {
        self.active_display.as_ref()
    }

    #[must_use]
    pub fn pointer_display(&self) -> Option<&DisplayId> {
        self.hovered_display.as_ref()
    }

    #[must_use]
    pub fn is_interacting(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    #[must_use]
    pub const fn gesture_changed(&self) -> bool {
        self.gesture_changed
    }

    #[must_use]
    pub const fn constraint(&self) -> SizeConstraint {
        self.options.constraint
    }

    #[must_use]
    pub const fn pointer(&self) -> Option<LogicalPoint> {
        self.last_pointer
    }

    #[must_use]
    pub const fn options_ref(&self) -> &SelectionOptions {
        &self.options
    }

    pub fn set_drag_modifiers(&mut self, modifiers: DragModifiers) {
        if self.drag_modifiers == modifiers {
            return;
        }
        let previous = self.drag_modifiers;
        self.drag_modifiers = modifiers;

        let Phase::Creating {
            mut anchor,
            display,
            space_move: _,
        } = self.phase.clone()
        else {
            return;
        };
        let Some(pointer) = self.last_pointer else {
            return;
        };

        if !previous.space && modifiers.space {
            if let Some(region) = self.region {
                self.phase = Phase::Creating {
                    anchor,
                    display,
                    space_move: Some(SpaceMove { pointer, region }),
                };
            }
            return;
        }

        if previous.space && !modifiers.space {
            if let Some(region) = self.region {
                anchor = if modifiers.alt {
                    geom::centre(region)
                } else {
                    opposite_corner(region, pointer)
                };
            }
            self.phase = Phase::Creating {
                anchor,
                display,
                space_move: None,
            };
            self.axis_lock = None;
            self.pointer_moved_after_hover(pointer);
            return;
        }

        if !previous.shift && modifiers.shift {
            self.axis_lock = pending_axis_lock(anchor, pointer);
        } else if previous.shift && !modifiers.shift {
            self.axis_lock = None;
        }

        if self.is_interacting() {
            self.pointer_moved_after_hover(pointer);
        }
    }

    #[must_use]
    pub fn hovered_display(&self) -> Option<&Display> {
        self.hovered_display
            .as_ref()
            .and_then(|display| self.layout.display(display))
    }

    #[must_use]
    pub fn hovered_window(&self) -> Option<&Window> {
        self.hovered_window
            .as_ref()
            .and_then(|window| self.window(window))
    }

    #[must_use]
    pub(crate) fn focus_rect(&self) -> Option<LogicalRect> {
        match self.mode {
            SelectionMode::Region => self.region,
            SelectionMode::Window => self.hovered_window().map(|window| window.bounds),
            SelectionMode::Display => self.hovered_display().map(|display| display.bounds),
            SelectionMode::AllDisplays => self.layout.desktop_bounds(),
        }
    }

    #[must_use]
    pub(crate) fn focus_display(&self) -> Option<&DisplayId> {
        match self.mode {
            SelectionMode::Region => self.region.and(self.region_display.as_ref()),
            SelectionMode::Window => self.hovered_window().map(|window| &window.display),
            SelectionMode::Display => self.hovered_display.as_ref(),
            SelectionMode::AllDisplays => None,
        }
    }

    #[must_use]
    pub(crate) fn overlay_label(&self) -> String {
        let mut label = format!("Selection overlay, {} mode", self.mode.label());
        match self.mode {
            SelectionMode::Region => {
                if let Some(rect) = self.region {
                    label.push_str(", ");
                    label.push_str(&describe_rect(rect));
                    label.push_str(", activate to capture");
                }
            }
            SelectionMode::Window => {
                if let Some(window) = self.hovered_window() {
                    label.push_str(", ");
                    label.push_str(&window_label(window));
                }
            }
            SelectionMode::Display => {
                if let Some(display) = self.hovered_display() {
                    label.push_str(", ");
                    label.push_str(&display.name);
                }
            }
            SelectionMode::AllDisplays => label.push_str(", every display"),
        }
        label
    }

    pub fn hover(&mut self, point: LogicalPoint) {
        let display = self
            .layout
            .display_at_point(point)
            .map(|display| display.id.clone());
        self.update_hover(point, display);
    }

    pub fn hover_on(&mut self, display: &DisplayId, point: LogicalPoint) {
        let display = self
            .layout
            .display(display)
            .map(|display| display.id.clone());
        self.update_hover(point, display);
    }

    fn update_hover(&mut self, point: LogicalPoint, display: Option<DisplayId>) {
        self.last_pointer = Some(point);
        let previous_display = self.hovered_display.clone();
        let previous_window = self.hovered_window.clone();
        self.hovered_display = display;
        self.hovered_window = self
            .window_at_point_on(point, self.hovered_display.as_ref())
            .map(|window| window.id.clone());
        if self.mode != SelectionMode::Region || self.region.is_none() {
            if let Some(window) = self.hovered_window() {
                self.active_display = Some(window.display.clone());
            } else if let Some(display) = self.hovered_display.clone() {
                self.active_display = Some(display);
            }
        }
        match self.mode {
            SelectionMode::Window if previous_window != self.hovered_window => {
                self.announcement = Some(SelectionAnnouncement(self.hovered_window().map_or_else(
                    || "No visible window under pointer".to_owned(),
                    |window| format!("Window target {}", window_label(window)),
                )));
            }
            SelectionMode::Display if previous_display != self.hovered_display => {
                self.announcement =
                    Some(SelectionAnnouncement(self.hovered_display().map_or_else(
                        || "No display under pointer".to_owned(),
                        |display| format!("Display target {}", display.name),
                    )));
            }
            _ => {}
        }
    }

    pub fn pointer_pressed(&mut self, point: LogicalPoint) {
        self.hover(point);
        self.pointer_pressed_after_hover(point, None);
    }

    pub fn pointer_pressed_on(&mut self, display: &DisplayId, point: LogicalPoint) {
        self.hover_on(display, point);
        self.pointer_pressed_after_hover(point, Some(display));
    }

    fn pointer_pressed_after_hover(&mut self, point: LogicalPoint, display: Option<&DisplayId>) {
        self.gesture_changed = false;
        match self.mode {
            SelectionMode::Region => self.begin_region_gesture(point, display),
            SelectionMode::Display | SelectionMode::AllDisplays | SelectionMode::Window => {
                self.phase = Phase::Idle;
            }
        }
    }

    pub fn pointer_moved(&mut self, point: LogicalPoint) {
        self.hover(point);
        self.pointer_moved_after_hover(point);
    }

    pub fn pointer_moved_on(&mut self, display: &DisplayId, point: LogicalPoint) {
        self.hover_on(display, point);
        self.pointer_moved_after_hover(point);
    }

    fn pointer_moved_after_hover(&mut self, point: LogicalPoint) {
        let previous_region = self.region;
        match self.phase.clone() {
            Phase::Idle => {}
            Phase::Creating {
                anchor,
                display,
                space_move,
            } => {
                let Some(bounds) = self.layout.desktop_bounds() else {
                    return;
                };
                let point = geom::clamp_point(bounds, point);
                if let Some(space_move) = space_move {
                    let delta = constrained_delta(
                        point.x - space_move.pointer.x,
                        point.y - space_move.pointer.y,
                        self.drag_modifiers.shift,
                    );
                    let moved = LogicalRect::new(
                        LogicalPoint::new(
                            space_move.region.origin.x + delta.0,
                            space_move.region.origin.y + delta.1,
                        ),
                        space_move.region.size,
                    );
                    let rect = geom::clamp_rect(bounds, moved);
                    self.region = Some(rect);
                    self.region_display = self.region_display_for(rect, Some(&display));
                    if self.region != previous_region {
                        self.gesture_changed = true;
                    }
                    return;
                }
                let rect = if let Some(exact) = self.options.constraint.exact {
                    self.phase = Phase::PlacingExact {
                        display: display.clone(),
                    };
                    let Some(display_bounds) = self.display_bounds(&display) else {
                        return;
                    };
                    let Some(rect) = place_exact(display_bounds, point, exact) else {
                        self.reject_exact_size(display_bounds, exact);
                        return;
                    };
                    rect
                } else {
                    let Some(scale) = self.layout.display(&display).map(|display| display.scale)
                    else {
                        return;
                    };
                    if self.drag_modifiers.shift && self.axis_lock.is_none() {
                        self.axis_lock = pending_axis_lock(anchor, point);
                    }
                    if self.drag_modifiers.shift {
                        self.axis_lock = resolve_axis_lock(self.axis_lock, point);
                    }
                    let Some(raw) = dragged_region(
                        bounds,
                        anchor,
                        point,
                        scale,
                        self.drag_modifiers,
                        self.axis_lock,
                    ) else {
                        self.region = None;
                        self.region_display = None;
                        return;
                    };
                    let aspect = if self.drag_modifiers.shift {
                        scrozz_core::selection::AspectLock::Free
                    } else {
                        self.options.constraint.aspect
                    };
                    fit_aspect_rect(bounds, anchor, raw, aspect)
                };
                self.region = Some(rect);
                self.region_display = self.region_display_for(rect, Some(&display));
            }
            Phase::Moving { grab, display } => {
                if let (Some(bounds), Some(current)) = (self.display_bounds(&display), self.region)
                {
                    let rect = LogicalRect::new(
                        LogicalPoint::new(point.x - grab.0, point.y - grab.1),
                        current.size,
                    );
                    self.region = Some(geom::clamp_rect(bounds, rect));
                }
            }
            Phase::Resizing { handle, display } => {
                if let (Some(bounds), Some(current)) = (self.display_bounds(&display), self.region)
                {
                    let point = geom::clamp_point(bounds, point);
                    let constraint = self.constraint_for_display(&display);
                    self.region = Some(resize_rect(bounds, current, point, handle, constraint));
                }
            }
            Phase::PlacingExact { display } => {
                if let Some(bounds) = self.display_bounds(&display) {
                    let exact = self.options.constraint.exact.unwrap_or_else(|| {
                        self.region
                            .map_or(LogicalSize::new(0.0, 0.0), |rect| rect.size)
                    });
                    self.region = place_exact(bounds, point, exact);
                    if self.region.is_none() {
                        self.reject_exact_size(bounds, exact);
                    }
                }
            }
        }
        if !matches!(self.phase, Phase::Idle) && self.region != previous_region {
            self.gesture_changed = true;
        }
    }

    pub fn pointer_released(&mut self, point: LogicalPoint) {
        self.pointer_moved(point);
        self.finish_pointer_release();
    }

    pub fn pointer_released_on(&mut self, display: &DisplayId, point: LogicalPoint) {
        self.pointer_moved_on(display, point);
        self.finish_pointer_release();
    }

    fn finish_pointer_release(&mut self) {
        if matches!(
            self.phase,
            Phase::Creating { .. } | Phase::PlacingExact { .. }
        ) && self
            .region
            .is_some_and(|rect| !self.options.constraint.is_satisfied_by(rect))
        {
            self.region = None;
            self.region_display = None;
            self.announcement = Some(SelectionAnnouncement(format!(
                "Selection too small; minimum is {} by {} points",
                self.options.constraint.minimum.width as i32,
                self.options.constraint.minimum.height as i32
            )));
        }
        self.phase = Phase::Idle;
        self.axis_lock = None;
    }

    pub fn keyboard_nudge(&mut self, direction: AxisDirection, fast: bool) {
        let Some(rect) = self.region else {
            return;
        };
        let Some(display) = self.region_owner(rect).cloned() else {
            return;
        };
        self.region_display = Some(display.id.clone());
        self.active_display = Some(display.id);
        let bounds = display.bounds;
        let step = if fast { 10.0 } else { 1.0 };
        let (dx, dy) = delta(direction, step);
        self.region = Some(geom::clamp_rect(
            bounds,
            LogicalRect::new(
                LogicalPoint::new(rect.origin.x + dx, rect.origin.y + dy),
                rect.size,
            ),
        ));
        self.announcement = self.region.map(|rect| {
            SelectionAnnouncement(format!("Selection moved to {}", describe_rect(rect)))
        });
    }

    pub fn keyboard_resize(&mut self, direction: AxisDirection, fast: bool) {
        if self.options.constraint.exact.is_some() {
            self.announcement = Some(SelectionAnnouncement(
                "Exact-size selections can be moved but not resized".to_owned(),
            ));
            return;
        }
        let Some(rect) = self.region else {
            return;
        };
        let Some(display) = self.region_owner(rect).cloned() else {
            return;
        };
        let constraint = self.constraint_for_display(&display.id);
        self.region_display = Some(display.id.clone());
        self.active_display = Some(display.id);
        let bounds = display.bounds;
        let step = if fast { 10.0 } else { 1.0 };
        let point = match direction {
            AxisDirection::Left => {
                LogicalPoint::new(rect.origin.x - step, rect.origin.y + rect.size.height / 2.0)
            }
            AxisDirection::Right => LogicalPoint::new(
                geom::right(rect) + step,
                rect.origin.y + rect.size.height / 2.0,
            ),
            AxisDirection::Up => {
                LogicalPoint::new(rect.origin.x + rect.size.width / 2.0, rect.origin.y - step)
            }
            AxisDirection::Down => LogicalPoint::new(
                rect.origin.x + rect.size.width / 2.0,
                geom::bottom(rect) + step,
            ),
        };
        let handle = match direction {
            AxisDirection::Left => ResizeHandle::West,
            AxisDirection::Right => ResizeHandle::East,
            AxisDirection::Up => ResizeHandle::North,
            AxisDirection::Down => ResizeHandle::South,
        };
        self.region = Some(resize_rect(bounds, rect, point, handle, constraint));
        self.announcement = self.region.map(|rect| {
            SelectionAnnouncement(format!("Selection resized to {}", describe_size(rect.size)))
        });
    }

    pub fn set_aspect_lock(&mut self, aspect: scrozz_core::selection::AspectLock) {
        let aspect = if self.capabilities.aspect_lock {
            aspect
        } else {
            scrozz_core::selection::AspectLock::Free
        };
        if let Some(exact) = self.options.constraint.exact
            && !size_matches_aspect(exact, aspect)
        {
            self.announcement = Some(SelectionAnnouncement(
                "The exact size does not match that aspect ratio".to_owned(),
            ));
            return;
        }
        let previous = self.options.constraint.aspect;
        self.options.constraint.aspect = aspect;
        if let Some(rect) = self.region
            && let Some(bounds) = self.region_owner(rect).map(|display| display.bounds)
        {
            let reshaped = fit_aspect_rect(bounds, rect.origin, rect, aspect);
            if self.options.constraint.is_satisfied_by(reshaped) {
                self.region = Some(reshaped);
            } else {
                self.options.constraint.aspect = previous;
                self.announcement = Some(SelectionAnnouncement(
                    "That aspect ratio does not fit on this display".to_owned(),
                ));
            }
        }
    }

    pub fn set_exact_size(&mut self, exact: Option<LogicalSize>) {
        let exact = if self.capabilities.exact_size {
            exact
        } else {
            None
        };
        if let Some(size) = exact
            && !size_matches_aspect(size, self.options.constraint.aspect)
        {
            self.announcement = Some(SelectionAnnouncement(
                "The exact size does not match the active aspect ratio".to_owned(),
            ));
            return;
        }
        self.options.constraint.exact = exact;
        if let (Some(size), Some(rect)) = (self.options.constraint.exact, self.region)
            && let Some(bounds) = self.region_owner(rect).map(|display| display.bounds)
        {
            self.region = place_exact(bounds, rect.origin, size);
            if self.region.is_none() {
                self.reject_exact_size(bounds, size);
            }
        }
    }

    #[must_use]
    pub fn restore_remembered(&mut self) -> bool {
        let Some(rect) = self.remembered else {
            return false;
        };
        self.region = Some(rect);
        self.region_display = self.remembered_display.clone();
        self.active_display = self.remembered_display.clone();
        self.announcement = Some(SelectionAnnouncement(format!(
            "Remembered selection restored {}",
            describe_rect(rect)
        )));
        true
    }

    #[must_use]
    pub fn set_mode(&mut self, mode: SelectionMode) -> bool {
        if !self.capabilities.supports(mode) {
            self.announcement = Some(SelectionAnnouncement(format!(
                "{} capture is unavailable",
                mode.label()
            )));
            return false;
        }
        self.mode = mode;
        self.phase = Phase::Idle;
        self.axis_lock = None;
        if mode == SelectionMode::Region && self.region.is_none() {
            let _ = self.restore_remembered();
        }
        self.announcement = Some(SelectionAnnouncement(format!(
            "{} mode. {}",
            mode.label(),
            mode.description()
        )));
        true
    }

    #[must_use]
    pub fn cancel(&mut self) -> bool {
        self.phase = Phase::Idle;
        self.axis_lock = None;
        self.announcement = Some(SelectionAnnouncement("Selection cancelled".to_owned()));
        true
    }

    #[must_use]
    pub fn commit(&mut self) -> Option<SelectionOutcome> {
        let outcome = match self.mode {
            SelectionMode::Region => self.commit_region(SelectionSource::ClientOverlay),
            SelectionMode::Display => self.commit_display(),
            SelectionMode::AllDisplays => Some(SelectionOutcome {
                mode: SelectionMode::AllDisplays,
                target: CaptureTarget::AllDisplays,
                rect: self.layout.desktop_bounds(),
                display: None,
                scale: ScaleFactor::IDENTITY,
                source: SelectionSource::ClientOverlay,
            }),
            SelectionMode::Window => self.commit_window().or_else(|| {
                self.announcement = Some(SelectionAnnouncement(
                    "No visible window under pointer".to_owned(),
                ));
                None
            }),
        };
        if let Some(outcome) = &outcome {
            self.announcement = Some(SelectionAnnouncement(self.selected_announcement(outcome)));
        }
        outcome
    }

    #[must_use]
    pub fn immediate_reuse(&self) -> Option<SelectionOutcome> {
        if !self.options.reuse_immediately {
            return None;
        }
        let rect = self.remembered?;
        let display = self.remembered_display.as_ref()?;
        let owner = self.layout.display(display)?;
        Some(SelectionOutcome::region(
            rect,
            Some(owner.id.clone()),
            owner.scale,
            SelectionSource::Remembered,
        ))
    }

    #[must_use]
    pub fn take_announcement(&mut self) -> Option<SelectionAnnouncement> {
        self.announcement.take()
    }

    #[must_use]
    pub fn handle_at_point(&self, point: LogicalPoint) -> Option<ResizeHandle> {
        if self.options.constraint.exact.is_some() {
            return None;
        }
        let rect = self.region?;
        handle_at_point(rect, point)
    }

    fn begin_region_gesture(&mut self, point: LogicalPoint, display: Option<&DisplayId>) {
        self.axis_lock = None;
        let Some(display) = display
            .and_then(|id| self.layout.display(id))
            .map(|display| display.id.clone())
            .or_else(|| {
                self.layout
                    .display_at_point(point)
                    .map(|display| display.id.clone())
            })
            .or_else(|| self.region_display.clone())
            .or_else(|| self.active_display.clone())
        else {
            return;
        };
        let adjusts_existing = self.region_display.as_ref() == Some(&display);
        self.region_display = Some(display.clone());
        self.active_display = Some(display.clone());
        if adjusts_existing && let Some(handle) = self.handle_at_point(point) {
            self.phase = Phase::Resizing { handle, display };
            return;
        }
        if adjusts_existing
            && let Some(rect) = self.region
            && geom::contains_point(rect, point)
        {
            self.phase = Phase::Moving {
                grab: (point.x - rect.origin.x, point.y - rect.origin.y),
                display,
            };
            return;
        }
        if self.options.constraint.exact.is_some() {
            let exact = self
                .options
                .constraint
                .exact
                .expect("the exact-size branch has a size");
            let Some(bounds) = self.display_bounds(&display) else {
                return;
            };
            let Some(rect) = place_exact(bounds, point, exact) else {
                self.reject_exact_size(bounds, exact);
                return;
            };
            self.phase = Phase::PlacingExact {
                display: display.clone(),
            };
            self.region = Some(rect);
        } else {
            self.phase = Phase::Creating {
                anchor: point,
                display: display.clone(),
                space_move: None,
            };
            self.region = None;
        }
    }

    fn commit_region(&self, source: SelectionSource) -> Option<SelectionOutcome> {
        let rect = self.region?;
        if rect.is_empty() || !self.options.constraint.is_satisfied_by(rect) {
            return None;
        }
        let display = self.region_owner(rect);
        Some(SelectionOutcome::region(
            rect,
            display.map(|display| display.id.clone()),
            display.map_or(ScaleFactor::IDENTITY, |display| display.scale),
            source,
        ))
    }

    fn reject_exact_size(&mut self, bounds: LogicalRect, exact: LogicalSize) {
        self.region = None;
        self.region_display = None;
        self.phase = Phase::Idle;
        self.announcement = Some(SelectionAnnouncement(format!(
            "Exact selection {} does not fit this {} by {} point display",
            describe_size(exact),
            bounds.size.width.round() as i32,
            bounds.size.height.round() as i32
        )));
    }

    fn commit_display(&self) -> Option<SelectionOutcome> {
        let display = self.hovered_display().or_else(|| {
            self.active_display
                .as_ref()
                .and_then(|display| self.layout.display(display))
        })?;
        Some(SelectionOutcome {
            mode: SelectionMode::Display,
            target: CaptureTarget::Display(display.id.clone()),
            rect: Some(display.bounds),
            display: Some(display.id.clone()),
            scale: display.scale,
            source: SelectionSource::ClientOverlay,
        })
    }

    fn commit_window(&self) -> Option<SelectionOutcome> {
        let window = self.hovered_window()?;
        let display = self.layout.display(&window.display)?;
        Some(SelectionOutcome {
            mode: SelectionMode::Window,
            target: CaptureTarget::Window(window.id.clone()),
            rect: Some(window.bounds),
            display: Some(window.display.clone()),
            scale: display.scale,
            source: SelectionSource::ClientOverlay,
        })
    }

    fn display_bounds(&self, id: &DisplayId) -> Option<LogicalRect> {
        self.layout.display(id).map(|display| display.bounds)
    }

    fn constraint_for_display(&self, id: &DisplayId) -> SizeConstraint {
        let Some(display) = self.layout.display(id) else {
            return self.options.constraint;
        };
        let pixel = 1.0 / display.scale.get();
        let mut constraint = self.options.constraint;
        constraint.minimum.width = constraint.minimum.width.max(pixel);
        constraint.minimum.height = constraint.minimum.height.max(pixel);
        constraint
    }

    fn window(&self, id: &WindowId) -> Option<&Window> {
        self.windows.iter().find(|window| window.id == *id)
    }

    fn window_at_point_on(
        &self,
        point: LogicalPoint,
        display: Option<&DisplayId>,
    ) -> Option<&Window> {
        let display_coordinates_are_ambiguous = display.is_some()
            && self
                .layout
                .displays()
                .iter()
                .filter(|candidate| geom::contains_point(candidate.bounds, point))
                .nth(1)
                .is_some();
        // `windows` is an invocation-scoped native z-order snapshot. Finding
        // the first eligible frame makes occlusion fall out naturally: an
        // underlying window wins only where every eligible window above it is
        // absent. A window spanning adjacent displays remains selectable from
        // either viewport. We restrict by owning display only where mixed-DPI
        // logical layouts overlap and the same numeric point is genuinely
        // ambiguous between two native viewports.
        self.windows.iter().find(|window| {
            window.is_visible
                && (!display_coordinates_are_ambiguous
                    || display.is_none_or(|display| window.display == *display))
                && geom::contains_point(window.bounds, point)
        })
    }

    fn region_owner(&self, rect: LogicalRect) -> Option<&Display> {
        self.region_display
            .as_ref()
            .and_then(|display| self.layout.display(display))
            .filter(|display| geom::contains_rect(display.bounds, rect))
            .or_else(|| self.layout.display_owning_rect(rect))
    }

    fn region_display_for(
        &self,
        rect: LogicalRect,
        preferred: Option<&DisplayId>,
    ) -> Option<DisplayId> {
        preferred
            .and_then(|id| self.layout.display(id))
            .filter(|display| geom::contains_rect(display.bounds, rect))
            .or_else(|| self.layout.display_owning_rect(rect))
            .map(|display| display.id.clone())
    }

    fn selected_announcement(&self, outcome: &SelectionOutcome) -> String {
        match outcome.mode {
            SelectionMode::Region => format!(
                "Region selected {}",
                outcome.rect.map_or_else(String::new, describe_rect)
            ),
            SelectionMode::Display => outcome
                .display
                .as_ref()
                .and_then(|display| self.layout.display(display))
                .map_or_else(
                    || "Display selected".to_owned(),
                    |display| format!("Display selected {}", display.name),
                ),
            SelectionMode::AllDisplays => "All displays selected".to_owned(),
            SelectionMode::Window => match &outcome.target {
                CaptureTarget::Window(id) => self.window(id).map_or_else(
                    || "Window selected".to_owned(),
                    |window| format!("Window selected {}", window_label(window)),
                ),
                _ => "Window selected".to_owned(),
            },
        }
    }
}

fn default_mode(capabilities: SelectionCapabilities) -> SelectionMode {
    if capabilities.supports(SelectionMode::Region) {
        SelectionMode::Region
    } else if capabilities.supports(SelectionMode::Window) {
        SelectionMode::Window
    } else if capabilities.supports(SelectionMode::Display) {
        SelectionMode::Display
    } else {
        SelectionMode::AllDisplays
    }
}

fn delta(direction: AxisDirection, step: f64) -> (f64, f64) {
    match direction {
        AxisDirection::Left => (-step, 0.0),
        AxisDirection::Right => (step, 0.0),
        AxisDirection::Up => (0.0, -step),
        AxisDirection::Down => (0.0, step),
    }
}

fn place_exact(bounds: LogicalRect, point: LogicalPoint, size: LogicalSize) -> Option<LogicalRect> {
    if size.width > bounds.size.width || size.height > bounds.size.height {
        return None;
    }
    Some(geom::clamp_rect(bounds, LogicalRect::new(point, size)))
}

fn normalize_region(
    bounds: LogicalRect,
    rect: LogicalRect,
    constraint: SizeConstraint,
) -> Option<LogicalRect> {
    let normalized = if let Some(exact) = constraint.exact {
        place_exact(bounds, rect.origin, exact)?
    } else {
        fit_aspect_rect(bounds, rect.origin, rect, constraint.aspect)
    };
    constraint.is_satisfied_by(normalized).then_some(normalized)
}

fn size_matches_aspect(size: LogicalSize, aspect: scrozz_core::selection::AspectLock) -> bool {
    let Some(ratio) = aspect.value() else {
        return true;
    };
    ((size.width / size.height) - ratio).abs() <= 1e-9 * ratio.max(1.0)
}

fn fit_aspect_rect(
    bounds: LogicalRect,
    anchor: LogicalPoint,
    raw: LogicalRect,
    aspect: scrozz_core::selection::AspectLock,
) -> LogicalRect {
    let Some(ratio) = aspect.value() else {
        return geom::clamp_rect(bounds, raw);
    };
    let reshaped = aspect.reshape(anchor, raw);
    let sign_x = if geom::right(reshaped) > anchor.x + f64::EPSILON {
        1.0
    } else {
        -1.0
    };
    let sign_y = if geom::bottom(reshaped) > anchor.y + f64::EPSILON {
        1.0
    } else {
        -1.0
    };
    let available_width = if sign_x > 0.0 {
        geom::right(bounds) - anchor.x
    } else {
        anchor.x - bounds.origin.x
    };
    let available_height = if sign_y > 0.0 {
        geom::bottom(bounds) - anchor.y
    } else {
        anchor.y - bounds.origin.y
    };
    let width = reshaped
        .size
        .width
        .min(available_width.max(0.0))
        .min(available_height.max(0.0) * ratio);
    let height = width / ratio;
    LogicalRect::from_corners(
        anchor,
        LogicalPoint::new(anchor.x + sign_x * width, anchor.y + sign_y * height),
    )
}

fn dragged_region(
    bounds: LogicalRect,
    anchor: LogicalPoint,
    point: LogicalPoint,
    scale: ScaleFactor,
    modifiers: DragModifiers,
    axis_lock: Option<AxisLock>,
) -> Option<LogicalRect> {
    let (mut dx, mut dy) = (point.x - anchor.x, point.y - anchor.y);
    if modifiers.shift {
        match axis_lock {
            Some(AxisLock::Pending { .. }) => {}
            Some(AxisLock::Horizontal { dy: locked }) => dy = locked,
            Some(AxisLock::Vertical { dx: locked }) => dx = locked,
            None => {}
        }
    }
    if dx.abs() <= f64::EPSILON && dy.abs() <= f64::EPSILON {
        return None;
    }

    let pixel = 1.0 / scale.get();
    let dragged = LogicalPoint::new(anchor.x + dx, anchor.y + dy);
    let mut rect = if modifiers.alt {
        LogicalRect::from_corners(LogicalPoint::new(anchor.x - dx, anchor.y - dy), dragged)
    } else {
        LogicalRect::from_corners(anchor, dragged)
    };
    if rect.size.width < pixel {
        let x = if modifiers.alt {
            anchor.x - pixel / 2.0
        } else if anchor.x + pixel <= geom::right(bounds) {
            anchor.x
        } else {
            anchor.x - pixel
        };
        rect.origin.x = x;
        rect.size.width = pixel;
    }
    if rect.size.height < pixel {
        let y = if modifiers.alt {
            anchor.y - pixel / 2.0
        } else if anchor.y + pixel <= geom::bottom(bounds) {
            anchor.y
        } else {
            anchor.y - pixel
        };
        rect.origin.y = y;
        rect.size.height = pixel;
    }
    Some(geom::clamp_rect(bounds, rect))
}

fn pending_axis_lock(anchor: LogicalPoint, point: LogicalPoint) -> Option<AxisLock> {
    let (dx, dy) = (point.x - anchor.x, point.y - anchor.y);
    if dx.abs() <= f64::EPSILON && dy.abs() <= f64::EPSILON {
        None
    } else {
        Some(AxisLock::Pending {
            pointer: point,
            dx,
            dy,
        })
    }
}

fn resolve_axis_lock(lock: Option<AxisLock>, point: LogicalPoint) -> Option<AxisLock> {
    let Some(AxisLock::Pending { pointer, dx, dy }) = lock else {
        return lock;
    };
    let movement_x = point.x - pointer.x;
    let movement_y = point.y - pointer.y;
    if movement_x.abs() <= f64::EPSILON && movement_y.abs() <= f64::EPSILON {
        lock
    } else if movement_x.abs() >= movement_y.abs() {
        Some(AxisLock::Horizontal { dy })
    } else {
        Some(AxisLock::Vertical { dx })
    }
}

fn constrained_delta(dx: f64, dy: f64, axis_locked: bool) -> (f64, f64) {
    if !axis_locked {
        return (dx, dy);
    }
    if dx.abs() >= dy.abs() {
        (dx, 0.0)
    } else {
        (0.0, dy)
    }
}

fn opposite_corner(region: LogicalRect, pointer: LogicalPoint) -> LogicalPoint {
    let centre = geom::centre(region);
    LogicalPoint::new(
        if pointer.x >= centre.x {
            region.origin.x
        } else {
            geom::right(region)
        },
        if pointer.y >= centre.y {
            region.origin.y
        } else {
            geom::bottom(region)
        },
    )
}

fn window_label(window: &Window) -> String {
    match (&window.title, &window.application) {
        (Some(title), Some(application)) if !title.is_empty() => {
            format!("{title} — {application}")
        }
        (Some(title), _) if !title.is_empty() => title.clone(),
        (None, Some(application)) => application.clone(),
        _ => "Untitled window".to_owned(),
    }
}

fn resize_rect(
    bounds: LogicalRect,
    current: LogicalRect,
    point: LogicalPoint,
    handle: ResizeHandle,
    constraint: SizeConstraint,
) -> LogicalRect {
    if constraint.exact.is_some() {
        return current;
    }
    let left = current.origin.x;
    let right = geom::right(current);
    let top = current.origin.y;
    let bottom = geom::bottom(current);

    let rect = match handle {
        ResizeHandle::NorthWest => {
            let point = LogicalPoint::new(
                point.x.min(right - constraint.minimum.width),
                point.y.min(bottom - constraint.minimum.height),
            );
            let raw = LogicalRect::from_corners(LogicalPoint::new(right, bottom), point);
            fit_aspect_rect(
                bounds,
                LogicalPoint::new(right, bottom),
                raw,
                constraint.aspect,
            )
        }
        ResizeHandle::NorthEast => {
            let point = LogicalPoint::new(
                point.x.max(left + constraint.minimum.width),
                point.y.min(bottom - constraint.minimum.height),
            );
            let raw = LogicalRect::from_corners(LogicalPoint::new(left, bottom), point);
            fit_aspect_rect(
                bounds,
                LogicalPoint::new(left, bottom),
                raw,
                constraint.aspect,
            )
        }
        ResizeHandle::SouthEast => {
            let point = LogicalPoint::new(
                point.x.max(left + constraint.minimum.width),
                point.y.max(top + constraint.minimum.height),
            );
            let raw = LogicalRect::from_corners(LogicalPoint::new(left, top), point);
            fit_aspect_rect(bounds, LogicalPoint::new(left, top), raw, constraint.aspect)
        }
        ResizeHandle::SouthWest => {
            let point = LogicalPoint::new(
                point.x.min(right - constraint.minimum.width),
                point.y.max(top + constraint.minimum.height),
            );
            let raw = LogicalRect::from_corners(LogicalPoint::new(right, top), point);
            fit_aspect_rect(
                bounds,
                LogicalPoint::new(right, top),
                raw,
                constraint.aspect,
            )
        }
        ResizeHandle::West => {
            if let Some(ratio) = constraint.aspect.value() {
                resize_horizontal_locked(
                    bounds,
                    current,
                    (right - point.x).max(0.0),
                    handle,
                    ratio,
                    constraint.minimum,
                )
            } else {
                let new_left = point.x.min(right - constraint.minimum.width);
                let width = right - new_left;
                LogicalRect::new(
                    LogicalPoint::new(new_left, top),
                    LogicalSize::new(width, current.size.height),
                )
            }
        }
        ResizeHandle::East => {
            if let Some(ratio) = constraint.aspect.value() {
                resize_horizontal_locked(
                    bounds,
                    current,
                    (point.x - left).max(0.0),
                    handle,
                    ratio,
                    constraint.minimum,
                )
            } else {
                let new_right = point.x.max(left + constraint.minimum.width);
                let width = new_right - left;
                LogicalRect::new(current.origin, LogicalSize::new(width, current.size.height))
            }
        }
        ResizeHandle::North => {
            if let Some(ratio) = constraint.aspect.value() {
                resize_vertical_locked(
                    bounds,
                    current,
                    (bottom - point.y).max(0.0),
                    handle,
                    ratio,
                    constraint.minimum,
                )
            } else {
                let new_top = point.y.min(bottom - constraint.minimum.height);
                let height = bottom - new_top;
                LogicalRect::new(
                    LogicalPoint::new(left, new_top),
                    LogicalSize::new(current.size.width, height),
                )
            }
        }
        ResizeHandle::South => {
            if let Some(ratio) = constraint.aspect.value() {
                resize_vertical_locked(
                    bounds,
                    current,
                    (point.y - top).max(0.0),
                    handle,
                    ratio,
                    constraint.minimum,
                )
            } else {
                let new_bottom = point.y.max(top + constraint.minimum.height);
                let height = new_bottom - top;
                LogicalRect::new(current.origin, LogicalSize::new(current.size.width, height))
            }
        }
    };
    let rect = preserve_resize_anchor(current, rect.size, handle, constraint);
    if geom::contains_rect(bounds, rect) && constraint.is_satisfied_by(rect) {
        rect
    } else {
        current
    }
}

fn resize_horizontal_locked(
    bounds: LogicalRect,
    current: LogicalRect,
    requested_width: f64,
    handle: ResizeHandle,
    ratio: f64,
    minimum: LogicalSize,
) -> LogicalRect {
    let left = current.origin.x;
    let right = geom::right(current);
    let centre_y = current.origin.y + current.size.height / 2.0;
    let horizontal_limit = if handle == ResizeHandle::West {
        right - bounds.origin.x
    } else {
        geom::right(bounds) - left
    };
    let centred_height_limit = 2.0
        * (centre_y - bounds.origin.y)
            .min(geom::bottom(bounds) - centre_y)
            .max(0.0);
    let maximum = horizontal_limit.min(centred_height_limit * ratio);
    let minimum = minimum.width.max(minimum.height * ratio);
    let width = requested_width.clamp(minimum.min(maximum), maximum);
    let height = width / ratio;
    let x = if handle == ResizeHandle::West {
        right - width
    } else {
        left
    };
    LogicalRect::new(
        LogicalPoint::new(x, centre_y - height / 2.0),
        LogicalSize::new(width, height),
    )
}

fn resize_vertical_locked(
    bounds: LogicalRect,
    current: LogicalRect,
    requested_height: f64,
    handle: ResizeHandle,
    ratio: f64,
    minimum: LogicalSize,
) -> LogicalRect {
    let top = current.origin.y;
    let bottom = geom::bottom(current);
    let centre_x = current.origin.x + current.size.width / 2.0;
    let vertical_limit = if handle == ResizeHandle::North {
        bottom - bounds.origin.y
    } else {
        geom::bottom(bounds) - top
    };
    let centred_width_limit = 2.0
        * (centre_x - bounds.origin.x)
            .min(geom::right(bounds) - centre_x)
            .max(0.0);
    let maximum = vertical_limit.min(centred_width_limit / ratio);
    let minimum = minimum.height.max(minimum.width / ratio);
    let height = requested_height.clamp(minimum.min(maximum), maximum);
    let width = height * ratio;
    let y = if handle == ResizeHandle::North {
        bottom - height
    } else {
        top
    };
    LogicalRect::new(
        LogicalPoint::new(centre_x - width / 2.0, y),
        LogicalSize::new(width, height),
    )
}

fn preserve_resize_anchor(
    current: LogicalRect,
    resized: LogicalSize,
    handle: ResizeHandle,
    constraint: SizeConstraint,
) -> LogicalRect {
    let (width, height) = if let Some(ratio) = constraint.aspect.value() {
        let width = resized
            .width
            .max(resized.height * ratio)
            .max(constraint.minimum.width)
            .max(constraint.minimum.height * ratio);
        (width, width / ratio)
    } else {
        (
            resized.width.max(constraint.minimum.width),
            resized.height.max(constraint.minimum.height),
        )
    };
    let size = LogicalSize::new(width, height);
    let right = geom::right(current);
    let bottom = geom::bottom(current);
    let centre_x = current.origin.x + current.size.width / 2.0;
    let centre_y = current.origin.y + current.size.height / 2.0;
    let origin = match handle {
        ResizeHandle::NorthWest => LogicalPoint::new(right - width, bottom - height),
        ResizeHandle::North => LogicalPoint::new(centre_x - width / 2.0, bottom - height),
        ResizeHandle::NorthEast => LogicalPoint::new(current.origin.x, bottom - height),
        ResizeHandle::East => LogicalPoint::new(current.origin.x, centre_y - height / 2.0),
        ResizeHandle::SouthEast => current.origin,
        ResizeHandle::South => LogicalPoint::new(centre_x - width / 2.0, current.origin.y),
        ResizeHandle::SouthWest => LogicalPoint::new(right - width, current.origin.y),
        ResizeHandle::West => LogicalPoint::new(right - width, centre_y - height / 2.0),
    };
    LogicalRect::new(origin, size)
}

fn handle_at_point(rect: LogicalRect, point: LogicalPoint) -> Option<ResizeHandle> {
    let left = rect.origin.x;
    let right = geom::right(rect);
    let top = rect.origin.y;
    let bottom = geom::bottom(rect);
    let near_left = (point.x - left).abs() <= HANDLE_RADIUS;
    let near_right = (point.x - right).abs() <= HANDLE_RADIUS;
    let near_top = (point.y - top).abs() <= HANDLE_RADIUS;
    let near_bottom = (point.y - bottom).abs() <= HANDLE_RADIUS;

    if near_left && near_top {
        Some(ResizeHandle::NorthWest)
    } else if near_right && near_top {
        Some(ResizeHandle::NorthEast)
    } else if near_right && near_bottom {
        Some(ResizeHandle::SouthEast)
    } else if near_left && near_bottom {
        Some(ResizeHandle::SouthWest)
    } else if near_top && point.x >= left - EDGE_BAND && point.x <= right + EDGE_BAND {
        Some(ResizeHandle::North)
    } else if near_bottom && point.x >= left - EDGE_BAND && point.x <= right + EDGE_BAND {
        Some(ResizeHandle::South)
    } else if near_left && point.y >= top - EDGE_BAND && point.y <= bottom + EDGE_BAND {
        Some(ResizeHandle::West)
    } else if near_right && point.y >= top - EDGE_BAND && point.y <= bottom + EDGE_BAND {
        Some(ResizeHandle::East)
    } else {
        None
    }
}

fn describe_rect(rect: LogicalRect) -> String {
    format!(
        "at {}, {} sized {}",
        rect.origin.x.round() as i32,
        rect.origin.y.round() as i32,
        describe_size(rect.size)
    )
}

fn describe_size(size: LogicalSize) -> String {
    format!(
        "{} by {} points",
        size.width.round() as i32,
        size.height.round() as i32
    )
}
