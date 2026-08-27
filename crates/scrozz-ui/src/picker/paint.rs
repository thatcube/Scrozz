//! Painting the interactive window picker.
//!
//! The selected hole and its outline use the exact logical bounds supplied by
//! the backend. In particular, the outline is square: guessing the platform's
//! window corner radius would make the picker promise geometry the captured
//! pixels may not have.

use egui::{
    Align2, Color32, CornerRadius, Key, Painter, Pos2, Rect, Stroke, StrokeKind, Ui, Vec2, pos2,
    vec2,
};
use scrozz_core::{LogicalPoint, LogicalRect};

use crate::theme::{Radius, Space, Text, Theme, corner};

use super::Highlight;
use super::WindowPicker;

const SCRIM_ALPHA: u8 = 88;
const SELECTION_TINT: f32 = 0.16;
const OUTLINE_WIDTH: f32 = 5.0;
const INNER_OUTLINE_WIDTH: f32 = 2.0;
const LABEL_GAP: f32 = Space::SM;
const LABEL_PADDING_X: f32 = 11.0;
const LABEL_PADDING_Y: f32 = 6.0;

/// What the picker UI asks its owner to do after one frame of input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Keep choosing.
    None,
    /// Close without a capture.
    Cancel,
    /// Re-enumerate and commit the focused window.
    Commit,
}

/// Geometry derived for one picker frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// The window's true bounds in overlay-local points, without clipping,
    /// expansion, inset, or corner adjustment.
    pub highlight: Rect,
    /// Opaque scrim rectangles surrounding the visible part of the window.
    pub scrim: Vec<Rect>,
}

impl Layout {
    /// Resolves global desktop coordinates into one overlay's local coordinates.
    #[must_use]
    pub fn new(viewport: Rect, desktop_origin: LogicalPoint, bounds: LogicalRect) -> Self {
        let highlight = local_rect(bounds, desktop_origin);
        let scrim = scrim_around(viewport, highlight);
        Self { highlight, scrim }
    }
}

/// Paints the desktop scrim, exact window outline, and source label.
///
/// `desktop_origin` is the global logical coordinate represented by
/// `viewport.min`. Supplying it explicitly keeps negative-origin and multi-
/// display desktops correct rather than assuming the primary display starts at
/// zero.
pub fn draw(
    painter: &Painter,
    viewport: Rect,
    desktop_origin: LogicalPoint,
    highlight: Option<&Highlight>,
    theme: &Theme,
) {
    let Some(highlight) = highlight else {
        painter.rect_filled(
            viewport,
            CornerRadius::ZERO,
            Color32::from_black_alpha(SCRIM_ALPHA),
        );
        return;
    };

    let layout = Layout::new(viewport, desktop_origin, highlight.bounds);
    for rect in &layout.scrim {
        painter.rect_filled(
            *rect,
            CornerRadius::ZERO,
            Color32::from_black_alpha(SCRIM_ALPHA),
        );
    }

    painter.rect_filled(
        layout.highlight.intersect(viewport),
        CornerRadius::ZERO,
        theme.palette.accent.linear_multiply(SELECTION_TINT),
    );
    painter.rect_stroke(
        layout.highlight,
        CornerRadius::ZERO,
        Stroke::new(OUTLINE_WIDTH, theme.palette.accent_hi),
        StrokeKind::Inside,
    );
    painter.rect_stroke(
        layout.highlight.shrink(OUTLINE_WIDTH),
        CornerRadius::ZERO,
        Stroke::new(INNER_OUTLINE_WIDTH, Color32::WHITE),
        StrokeKind::Inside,
    );

    let (width, height) = highlight.pixel_size();
    let text = format!(
        "{}  {width} x {height}  -  Return to capture",
        highlight.label()
    );
    paint_label(painter, viewport, layout.highlight, &text, theme);
}

/// Applies pointer and keyboard input to a picker and paints the resulting frame.
///
/// Pointer positions are viewport-local; `desktop_origin` translates them into
/// the global logical coordinate system used by platform window enumeration.
pub fn interact(
    ui: &mut Ui,
    picker: &mut WindowPicker,
    desktop_origin: LogicalPoint,
    theme: &Theme,
    notice: Option<&str>,
) -> Intent {
    interact_inner(ui, picker, desktop_origin, theme, notice, true)
}

/// Drives one monitor-sized picker viewport.
///
/// Unlike [`interact`], an absent pointer does not clear focus because another
/// monitor viewport may currently own the pointer.
pub fn interact_display(
    ui: &mut Ui,
    picker: &mut WindowPicker,
    desktop_origin: LogicalPoint,
    theme: &Theme,
    notice: Option<&str>,
) -> Intent {
    interact_inner(ui, picker, desktop_origin, theme, notice, false)
}

fn interact_inner(
    ui: &mut Ui,
    picker: &mut WindowPicker,
    desktop_origin: LogicalPoint,
    theme: &Theme,
    notice: Option<&str>,
    clear_absent_pointer: bool,
) -> Intent {
    let viewport = ui.max_rect();
    let input = ui.ctx().input(|input| {
        (
            input.pointer.hover_pos(),
            input.pointer.primary_clicked(),
            input.key_pressed(Key::Escape),
            input.key_pressed(Key::Tab),
            input.modifiers.shift,
            input.key_pressed(Key::Enter) || input.key_pressed(Key::Space),
        )
    });

    if let Some(position) = input.0 {
        picker.point_at(LogicalPoint::new(
            desktop_origin.x + f64::from(position.x),
            desktop_origin.y + f64::from(position.y),
        ));
    } else if clear_absent_pointer {
        picker.clear_pointer();
    }

    if input.3 {
        if input.4 {
            picker.focus_prev();
        } else {
            picker.focus_next();
        }
    }

    let highlight = picker.highlight();
    draw(
        ui.painter(),
        viewport,
        desktop_origin,
        highlight.as_ref(),
        theme,
    );

    let message = notice.or_else(|| picker.is_empty().then_some("No visible windows"));
    if let Some(message) = message {
        ui.painter().text(
            viewport.center(),
            Align2::CENTER_CENTER,
            message,
            theme.font(Text::Body),
            theme.palette.text,
        );
    }

    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    if input.2 {
        Intent::Cancel
    } else if input.1 || input.5 {
        Intent::Commit
    } else {
        Intent::None
    }
}

fn paint_label(painter: &Painter, viewport: Rect, highlight: Rect, text: &str, theme: &Theme) {
    let galley = painter.layout_no_wrap(
        text.to_owned(),
        theme.font(Text::Body),
        theme.palette.on_accent,
    );
    let size = galley.size() + vec2(LABEL_PADDING_X * 2.0, LABEL_PADDING_Y * 2.0);
    let center = label_center(viewport, highlight, size);
    let rect = Rect::from_center_size(center, size);
    let radius = Radius::pill(rect.height());

    painter.rect_filled(rect, corner(radius), theme.palette.accent);
    painter.galley(
        rect.min + vec2(LABEL_PADDING_X, LABEL_PADDING_Y),
        galley,
        theme.palette.on_accent,
    );
}

fn label_center(viewport: Rect, highlight: Rect, size: Vec2) -> Pos2 {
    let half = size / 2.0;
    let x = highlight
        .center()
        .x
        .clamp(viewport.left() + half.x, viewport.right() - half.x);
    let below = highlight.bottom() + LABEL_GAP + half.y;
    let above = highlight.top() - LABEL_GAP - half.y;
    let y = if below + half.y <= viewport.bottom() {
        below
    } else {
        above.max(viewport.top() + half.y)
    };
    pos2(x, y)
}

#[allow(clippy::cast_possible_truncation)]
fn local_rect(bounds: LogicalRect, desktop_origin: LogicalPoint) -> Rect {
    Rect::from_min_size(
        pos2(
            (bounds.origin.x - desktop_origin.x) as f32,
            (bounds.origin.y - desktop_origin.y) as f32,
        ),
        vec2(bounds.size.width as f32, bounds.size.height as f32),
    )
}

fn scrim_around(viewport: Rect, highlight: Rect) -> Vec<Rect> {
    if !viewport.intersects(highlight) {
        return vec![viewport];
    }

    let hole = viewport.intersect(highlight);
    [
        Rect::from_min_max(viewport.min, pos2(viewport.right(), hole.top())),
        Rect::from_min_max(pos2(viewport.left(), hole.bottom()), viewport.max),
        Rect::from_min_max(
            pos2(viewport.left(), hole.top()),
            pos2(hole.left(), hole.bottom()),
        ),
        Rect::from_min_max(
            pos2(hole.right(), hole.top()),
            pos2(viewport.right(), hole.bottom()),
        ),
    ]
    .into_iter()
    .filter(|rect| rect.is_positive())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::LogicalSize;

    #[test]
    fn layout_preserves_true_bounds_and_leaves_exactly_one_hole() {
        let viewport = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 800.0));
        let bounds = LogicalRect::new(
            LogicalPoint::new(200.0, 100.0),
            LogicalSize::new(600.0, 400.0),
        );
        let layout = Layout::new(viewport, LogicalPoint::new(0.0, 0.0), bounds);

        assert_eq!(
            layout.highlight,
            Rect::from_min_size(pos2(200.0, 100.0), vec2(600.0, 400.0))
        );
        let scrim_area: f32 = layout.scrim.iter().map(Rect::area).sum();
        assert_eq!(scrim_area, viewport.area() - layout.highlight.area());
    }

    #[test]
    fn negative_desktop_origins_translate_without_changing_size() {
        let viewport = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 800.0));
        let bounds = LogicalRect::new(
            LogicalPoint::new(-1200.0, 50.0),
            LogicalSize::new(500.0, 300.0),
        );
        let layout = Layout::new(viewport, LogicalPoint::new(-1440.0, 0.0), bounds);

        assert_eq!(
            layout.highlight,
            Rect::from_min_size(pos2(240.0, 50.0), vec2(500.0, 300.0))
        );
    }
}
