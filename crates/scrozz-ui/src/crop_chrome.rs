use egui::{Color32, Painter, Rect, Stroke, pos2};

const MARK_LENGTH: f32 = 28.0;
const MARK_STROKE: f32 = 4.0;

pub(crate) fn draw_resize_guides(painter: &Painter, rect: Rect) {
    let shadow = Stroke::new(MARK_STROKE + 2.0, Color32::from_black_alpha(170));
    let mark = Stroke::new(MARK_STROKE, Color32::WHITE);
    let segments = [
        [rect.left_top(), pos2(rect.left() + MARK_LENGTH, rect.top())],
        [rect.left_top(), pos2(rect.left(), rect.top() + MARK_LENGTH)],
        [
            rect.right_top(),
            pos2(rect.right() - MARK_LENGTH, rect.top()),
        ],
        [
            rect.right_top(),
            pos2(rect.right(), rect.top() + MARK_LENGTH),
        ],
        [
            rect.left_bottom(),
            pos2(rect.left() + MARK_LENGTH, rect.bottom()),
        ],
        [
            rect.left_bottom(),
            pos2(rect.left(), rect.bottom() - MARK_LENGTH),
        ],
        [
            rect.right_bottom(),
            pos2(rect.right() - MARK_LENGTH, rect.bottom()),
        ],
        [
            rect.right_bottom(),
            pos2(rect.right(), rect.bottom() - MARK_LENGTH),
        ],
        [
            pos2(rect.center().x - MARK_LENGTH * 0.5, rect.top()),
            pos2(rect.center().x + MARK_LENGTH * 0.5, rect.top()),
        ],
        [
            pos2(rect.center().x - MARK_LENGTH * 0.5, rect.bottom()),
            pos2(rect.center().x + MARK_LENGTH * 0.5, rect.bottom()),
        ],
        [
            pos2(rect.left(), rect.center().y - MARK_LENGTH * 0.5),
            pos2(rect.left(), rect.center().y + MARK_LENGTH * 0.5),
        ],
        [
            pos2(rect.right(), rect.center().y - MARK_LENGTH * 0.5),
            pos2(rect.right(), rect.center().y + MARK_LENGTH * 0.5),
        ],
    ];
    for segment in segments {
        painter.line_segment(segment, shadow);
        painter.line_segment(segment, mark);
    }
}
