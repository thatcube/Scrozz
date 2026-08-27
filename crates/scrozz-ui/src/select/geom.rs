use egui::{Pos2, Rect, Vec2, pos2, vec2};
use scrozz_core::{Display, DisplayId, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor};

/// A stable, pure view of the connected logical desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayLayout {
    displays: Vec<Display>,
    desktop_bounds: Option<LogicalRect>,
}

impl DisplayLayout {
    /// Creates a layout from the measured displays.
    #[must_use]
    pub fn new(mut displays: Vec<Display>) -> Self {
        displays.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        let desktop_bounds = union_rect(displays.iter().map(|display| display.bounds));
        Self {
            displays,
            desktop_bounds,
        }
    }

    /// The measured displays, in stable id order.
    #[must_use]
    pub fn displays(&self) -> &[Display] {
        &self.displays
    }

    /// The union of every display's logical bounds.
    #[must_use]
    pub const fn desktop_bounds(&self) -> Option<LogicalRect> {
        self.desktop_bounds
    }

    /// The display with `id`, if present.
    #[must_use]
    pub fn display(&self, id: &DisplayId) -> Option<&Display> {
        self.displays.iter().find(|display| display.id == *id)
    }

    /// The display under `point`, preferring the smallest containing display.
    #[must_use]
    pub fn display_at_point(&self, point: LogicalPoint) -> Option<&Display> {
        self.displays
            .iter()
            .filter(|display| contains_point(display.bounds, point))
            .min_by(compare_display_specificity)
    }

    /// The single display that wholly owns `rect`.
    #[must_use]
    pub fn display_owning_rect(&self, rect: LogicalRect) -> Option<&Display> {
        self.displays
            .iter()
            .filter(|display| contains_rect(display.bounds, rect))
            .min_by(compare_display_specificity)
    }

    /// Converts a global logical point into the local canvas used by egui.
    #[must_use]
    pub fn canvas_pos(&self, point: LogicalPoint) -> Pos2 {
        let Some(bounds) = self.desktop_bounds else {
            return point_to_pos2(point);
        };
        Self::canvas_pos_in(bounds, point)
    }

    /// Converts a canvas point back into the global logical desktop.
    #[must_use]
    pub fn point_from_canvas(&self, point: Pos2) -> LogicalPoint {
        let Some(bounds) = self.desktop_bounds else {
            return pos2_to_point(point);
        };
        Self::point_from_canvas_in(bounds, point)
    }

    /// Converts a global logical point into one selector surface.
    #[must_use]
    pub fn canvas_pos_in(surface: LogicalRect, point: LogicalPoint) -> Pos2 {
        pos2(
            (point.x - surface.origin.x) as f32,
            (point.y - surface.origin.y) as f32,
        )
    }

    /// Converts a selector-surface point back into global logical coordinates.
    #[must_use]
    pub fn point_from_canvas_in(surface: LogicalRect, point: Pos2) -> LogicalPoint {
        LogicalPoint::new(
            surface.origin.x + f64::from(point.x),
            surface.origin.y + f64::from(point.y),
        )
    }

    /// Converts a global logical rectangle into the local egui canvas.
    #[must_use]
    pub fn canvas_rect(&self, rect: LogicalRect) -> Rect {
        let min = self.canvas_pos(rect.origin);
        Rect::from_min_size(min, size_to_vec2(rect.size))
    }

    /// Converts a global logical rectangle into one selector surface.
    #[must_use]
    pub fn canvas_rect_in(surface: LogicalRect, rect: LogicalRect) -> Rect {
        let min = Self::canvas_pos_in(surface, rect.origin);
        Rect::from_min_size(min, size_to_vec2(rect.size))
    }

    /// The canvas size needed to show the full desktop.
    #[must_use]
    pub fn canvas_size(&self) -> Vec2 {
        self.desktop_bounds
            .map_or(Vec2::ZERO, |rect| size_to_vec2(rect.size))
    }

    /// Clamps `point` to a display's logical bounds.
    #[must_use]
    pub fn clamp_point_to_display(
        &self,
        id: &DisplayId,
        point: LogicalPoint,
    ) -> Option<LogicalPoint> {
        self.display(id)
            .map(|display| clamp_point(display.bounds, point))
    }

    /// Clamps `rect` so it stays wholly within `id`.
    #[must_use]
    pub fn clamp_rect_to_display(&self, id: &DisplayId, rect: LogicalRect) -> Option<LogicalRect> {
        self.display(id)
            .map(|display| clamp_rect(display.bounds, rect))
    }
}

/// Converts a Scrozz logical point to egui space without applying a display scale.
#[must_use]
pub fn point_to_pos2(point: LogicalPoint) -> Pos2 {
    pos2(point.x as f32, point.y as f32)
}

/// Converts an egui point to Scrozz logical space.
#[must_use]
pub fn pos2_to_point(point: Pos2) -> LogicalPoint {
    LogicalPoint::new(f64::from(point.x), f64::from(point.y))
}

/// Converts a Scrozz logical size to egui space.
#[must_use]
pub fn size_to_vec2(size: LogicalSize) -> Vec2 {
    vec2(size.width as f32, size.height as f32)
}

/// Converts an egui vector to Scrozz logical space.
#[must_use]
pub fn vec2_to_size(size: Vec2) -> LogicalSize {
    LogicalSize::new(f64::from(size.x), f64::from(size.y))
}

/// Converts a Scrozz logical rectangle to egui space.
#[must_use]
pub fn rect_to_egui(rect: LogicalRect) -> Rect {
    Rect::from_min_size(point_to_pos2(rect.origin), size_to_vec2(rect.size))
}

/// Converts an egui rectangle to Scrozz logical space.
#[must_use]
pub fn rect_from_egui(rect: Rect) -> LogicalRect {
    LogicalRect::new(pos2_to_point(rect.min), vec2_to_size(rect.size()))
}

/// The right edge of a logical rectangle.
#[must_use]
pub fn right(rect: LogicalRect) -> f64 {
    rect.origin.x + rect.size.width
}

/// The bottom edge of a logical rectangle.
#[must_use]
pub fn bottom(rect: LogicalRect) -> f64 {
    rect.origin.y + rect.size.height
}

/// The centre of a logical rectangle.
#[must_use]
pub fn centre(rect: LogicalRect) -> LogicalPoint {
    LogicalPoint::new(
        rect.origin.x + rect.size.width / 2.0,
        rect.origin.y + rect.size.height / 2.0,
    )
}

/// Whether `bounds` contains `point`, including the edges.
#[must_use]
pub fn contains_point(bounds: LogicalRect, point: LogicalPoint) -> bool {
    point.x >= bounds.origin.x
        && point.y >= bounds.origin.y
        && point.x <= right(bounds)
        && point.y <= bottom(bounds)
}

/// Whether `bounds` wholly contains `rect`.
#[must_use]
pub fn contains_rect(bounds: LogicalRect, rect: LogicalRect) -> bool {
    rect.origin.x >= bounds.origin.x
        && rect.origin.y >= bounds.origin.y
        && right(rect) <= right(bounds)
        && bottom(rect) <= bottom(bounds)
}

/// Clamps `point` into `bounds`.
#[must_use]
pub fn clamp_point(bounds: LogicalRect, point: LogicalPoint) -> LogicalPoint {
    LogicalPoint::new(
        point.x.clamp(bounds.origin.x, right(bounds)),
        point.y.clamp(bounds.origin.y, bottom(bounds)),
    )
}

/// Clamps `rect` so it fits within `bounds`, shrinking only when necessary.
#[must_use]
pub fn clamp_rect(bounds: LogicalRect, rect: LogicalRect) -> LogicalRect {
    let width = rect.size.width.min(bounds.size.width);
    let height = rect.size.height.min(bounds.size.height);
    let x = rect.origin.x.clamp(bounds.origin.x, right(bounds) - width);
    let y = rect
        .origin
        .y
        .clamp(bounds.origin.y, bottom(bounds) - height);
    LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(width, height))
}

/// Converts a point in a display's logical space into a local physical pixel.
#[must_use]
pub fn logical_to_local_physical(display: &Display, point: LogicalPoint) -> (u32, u32) {
    local_physical_components(display.bounds, display.scale, point)
}

/// Converts a logical size to whole physical pixels using the owning display scale.
#[must_use]
pub fn logical_size_to_physical(size: LogicalSize, scale: ScaleFactor) -> (u32, u32) {
    let rect = LogicalRect::new(LogicalPoint::new(0.0, 0.0), size).to_physical(scale);
    (rect.pixel_width(), rect.pixel_height())
}

fn local_physical_components(
    bounds: LogicalRect,
    scale: ScaleFactor,
    point: LogicalPoint,
) -> (u32, u32) {
    let s = scale.get();
    let width_px = (bounds.size.width * s).round().max(1.0);
    let height_px = (bounds.size.height * s).round().max(1.0);
    let x = ((point.x - bounds.origin.x) * s)
        .floor()
        .clamp(0.0, width_px - 1.0);
    let y = ((point.y - bounds.origin.y) * s)
        .floor()
        .clamp(0.0, height_px - 1.0);
    (x as u32, y as u32)
}

fn union_rect(rects: impl Iterator<Item = LogicalRect>) -> Option<LogicalRect> {
    let mut rects = rects.peekable();
    let first = rects.peek().copied()?;
    let (mut left, mut top, mut right_edge, mut bottom_edge) =
        (first.origin.x, first.origin.y, right(first), bottom(first));
    for rect in rects {
        left = left.min(rect.origin.x);
        top = top.min(rect.origin.y);
        right_edge = right_edge.max(right(rect));
        bottom_edge = bottom_edge.max(bottom(rect));
    }
    Some(LogicalRect::new(
        LogicalPoint::new(left, top),
        LogicalSize::new(right_edge - left, bottom_edge - top),
    ))
}

fn compare_display_specificity(a: &&Display, b: &&Display) -> std::cmp::Ordering {
    area(a.bounds)
        .partial_cmp(&area(b.bounds))
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| b.is_primary.cmp(&a.is_primary))
        .then_with(|| a.id.0.cmp(&b.id.0))
}

fn area(rect: LogicalRect) -> f64 {
    rect.size.width * rect.size.height
}
