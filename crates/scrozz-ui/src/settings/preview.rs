//! Fictional screenshots for Scene previews.
//!
//! A preset tile has to show what a Scene *does* — how much air it leaves, how
//! round the corners are, whether it drops a shadow, what sits behind it. That
//! needs a picture inside the frame, and the one picture it must never be is
//! the user's. Recent captures are frequently the most sensitive pixels on the
//! machine, and a settings pane that quietly renders them into six thumbnails
//! is a leak waiting for a screen share.
//!
//! So every preview here is drawn from nothing: flat shapes standing in for a
//! window, a document, a browser. They are deliberately abstract — no lorem
//! ipsum, no fake brand, no attempt to look like a real screenshot at a glance
//! — because the tile is a diagram of the framing, not a sample of content.
//!
//! The chrome *is* platform-aware, because "what a window looks like" is the
//! one thing a Scene preview genuinely has to get right: a macOS user judging
//! window padding against a Windows title bar is judging the wrong picture.

use egui::{Color32, Painter, Rect, Stroke, StrokeKind, Vec2, pos2};

use crate::theme::corner;

/// Which desktop's window chrome a preview should imitate.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PreviewPlatform {
    /// Rounded corners, three traffic lights at the leading edge.
    #[default]
    MacOs,
    /// Square corners, minimise/maximise/close glyphs at the trailing edge.
    Windows,
    /// A GNOME-style header bar with a single round close button.
    Linux,
}

impl PreviewPlatform {
    /// The chrome matching the platform the settings window is running on.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// What the fictional capture depicts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Subject {
    /// An application window, complete with the platform's chrome.
    Window,
    /// A free-hand region: content only, deliberately without OS chrome, since
    /// a region rarely contains a whole window and framing one would misdescribe
    /// what the Scene will do.
    Region,
    /// A whole display: a desktop with a menu strip and a floating window.
    FullScreen,
    /// Two displays side by side.
    AllDisplays,
    /// A tall scrolling capture.
    Scrolling,
}

/// The palette a fictional screenshot is drawn in.
///
/// Independent of the settings theme: a preview shows what the Scene will look
/// like around a *captured* image, and captures do not follow the dialog's
/// appearance. Kept light in both themes for exactly that reason.
#[derive(Clone, Copy)]
struct Paper {
    chrome: Color32,
    surface: Color32,
    edge: Color32,
    ink: Color32,
    ink_faint: Color32,
    accent: Color32,
}

impl Paper {
    const fn light() -> Self {
        Self {
            chrome: Color32::from_rgb(0xEC, 0xEE, 0xF3),
            surface: Color32::from_rgb(0xFC, 0xFD, 0xFF),
            edge: Color32::from_rgb(0xD3, 0xD8, 0xE2),
            ink: Color32::from_rgb(0x9A, 0xA3, 0xB4),
            ink_faint: Color32::from_rgb(0xC6, 0xCD, 0xDA),
            accent: Color32::from_rgb(0x6E, 0x7C, 0xF0),
        }
    }
}

/// Draw a fictional screenshot filling `rect`.
///
/// `scale` lets a caller shrink the internal detail for a very small tile; at
/// tile sizes the text lines would otherwise collapse into a grey smear.
pub fn draw(painter: &Painter, rect: Rect, subject: Subject, platform: PreviewPlatform) {
    if rect.width() < 8.0 || rect.height() < 6.0 {
        return;
    }
    let paper = Paper::light();
    match subject {
        Subject::Window => window(painter, rect, platform, &paper),
        Subject::Region => region(painter, rect, &paper),
        Subject::FullScreen => full_screen(painter, rect, platform, &paper),
        Subject::AllDisplays => all_displays(painter, rect, platform, &paper),
        Subject::Scrolling => scrolling(painter, rect, &paper),
    }
}

fn window_radius(platform: PreviewPlatform, height: f32) -> f32 {
    match platform {
        // Sequoia's corner is large; the preview keeps the proportion rather
        // than the absolute value so a 40pt tile still reads as "macOS".
        PreviewPlatform::MacOs => (height * 0.09).clamp(2.0, 9.0),
        PreviewPlatform::Windows => (height * 0.035).clamp(1.0, 4.0),
        PreviewPlatform::Linux => (height * 0.07).clamp(2.0, 8.0),
    }
}

fn window(painter: &Painter, rect: Rect, platform: PreviewPlatform, paper: &Paper) {
    let radius = window_radius(platform, rect.height());
    let bar_h = (rect.height() * 0.17).clamp(5.0, 16.0);
    painter.rect_filled(rect, corner(radius), paper.surface);
    let bar = Rect::from_min_size(rect.min, Vec2::new(rect.width(), bar_h));
    painter.rect_filled(bar, crate::theme::corner_top(radius), paper.chrome);
    painter.line_segment(
        [
            pos2(rect.left(), bar.bottom()),
            pos2(rect.right(), bar.bottom()),
        ],
        Stroke::new(1.0, paper.edge),
    );
    chrome_controls(painter, bar, platform, paper);
    let body = Rect::from_min_max(pos2(rect.left(), bar.bottom()), rect.max);
    document(painter, body.shrink(body.height() * 0.12), paper, true);
    painter.rect_stroke(
        rect,
        corner(radius),
        Stroke::new(1.0, paper.edge),
        StrokeKind::Inside,
    );
}

fn chrome_controls(painter: &Painter, bar: Rect, platform: PreviewPlatform, paper: &Paper) {
    let r = (bar.height() * 0.16).clamp(1.2, 3.4);
    let y = bar.center().y;
    match platform {
        PreviewPlatform::MacOs => {
            let colors = [
                Color32::from_rgb(0xFF, 0x5F, 0x57),
                Color32::from_rgb(0xFE, 0xBC, 0x2E),
                Color32::from_rgb(0x28, 0xC8, 0x40),
            ];
            for (index, color) in colors.into_iter().enumerate() {
                let x = bar.left() + r * 2.6 + index as f32 * r * 3.2;
                painter.circle_filled(pos2(x, y), r, color);
            }
        }
        PreviewPlatform::Windows => {
            // Three glyph boxes at the trailing edge — the silhouette that
            // makes a Windows title bar recognisable at thumbnail size.
            let w = r * 3.4;
            for index in 0..3 {
                let cx = bar.right() - w * (index as f32 + 0.7);
                let glyph = Rect::from_center_size(pos2(cx, y), Vec2::splat(r * 1.5));
                let stroke = Stroke::new(1.0, paper.ink);
                match index {
                    0 => {
                        painter.line_segment([glyph.left_top(), glyph.right_bottom()], stroke);
                        painter.line_segment([glyph.right_top(), glyph.left_bottom()], stroke);
                    }
                    1 => {
                        painter.rect_stroke(glyph, corner(0.0), stroke, StrokeKind::Inside);
                    }
                    _ => {
                        painter.line_segment(
                            [
                                pos2(glyph.left(), glyph.center().y),
                                pos2(glyph.right(), glyph.center().y),
                            ],
                            stroke,
                        );
                    }
                }
            }
        }
        PreviewPlatform::Linux => {
            let cx = bar.right() - r * 3.0;
            painter.circle_filled(pos2(cx, y), r * 1.4, paper.edge);
            let arm = r * 0.7;
            let stroke = Stroke::new(1.0, paper.ink);
            painter.line_segment([pos2(cx - arm, y - arm), pos2(cx + arm, y + arm)], stroke);
            painter.line_segment([pos2(cx + arm, y - arm), pos2(cx - arm, y + arm)], stroke);
        }
    }
}

/// Abstract page content: an optional sidebar, a title bar and text lines.
fn document(painter: &Painter, rect: Rect, paper: &Paper, sidebar: bool) {
    if rect.width() < 6.0 || rect.height() < 6.0 {
        return;
    }
    let mut body = rect;
    if sidebar && rect.width() > 34.0 {
        let width = rect.width() * 0.24;
        let rail = Rect::from_min_size(rect.min, Vec2::new(width, rect.height()));
        painter.rect_filled(rail, corner(2.0), paper.ink_faint.gamma_multiply(0.45));
        let mut y = rail.top() + 4.0;
        let step = (rail.height() / 6.0).clamp(3.5, 8.0);
        while y < rail.bottom() - 2.0 {
            painter.rect_filled(
                Rect::from_min_size(
                    pos2(rail.left() + 3.0, y),
                    Vec2::new(rail.width() - 6.0, 1.6),
                ),
                corner(1.0),
                paper.ink_faint,
            );
            y += step;
        }
        body = Rect::from_min_max(
            pos2(rail.right() + rect.width() * 0.06, rect.top()),
            rect.max,
        );
    }
    let line_h = (body.height() * 0.075).clamp(1.6, 3.4);
    let gap = line_h * 2.1;
    // A heading block, then a settled column of body lines. Concrete enough to
    // read as "a document", abstract enough to be plainly fictional.
    painter.rect_filled(
        Rect::from_min_size(body.min, Vec2::new(body.width() * 0.46, line_h * 2.0)),
        corner(1.5),
        paper.accent.gamma_multiply(0.75),
    );
    let mut y = body.top() + line_h * 2.0 + gap;
    let widths = [0.96, 0.86, 0.92, 0.62, 0.9, 0.74, 0.88, 0.5];
    for width in widths {
        if y + line_h > body.bottom() {
            break;
        }
        painter.rect_filled(
            Rect::from_min_size(
                pos2(body.left(), y),
                Vec2::new(body.width() * width, line_h),
            ),
            corner(1.0),
            paper.ink_faint,
        );
        y += gap;
    }
}

fn region(painter: &Painter, rect: Rect, paper: &Paper) {
    // No OS chrome: a region is a rectangle of content, and drawing a title bar
    // around it would promise framing the Scene will not apply.
    painter.rect_filled(rect, corner(2.0), paper.surface);
    document(painter, rect.shrink(rect.height() * 0.14), paper, false);
    painter.rect_stroke(
        rect,
        corner(2.0),
        Stroke::new(1.0, paper.edge),
        StrokeKind::Inside,
    );
}

fn full_screen(painter: &Painter, rect: Rect, platform: PreviewPlatform, paper: &Paper) {
    let desktop = Color32::from_rgb(0x5B, 0x69, 0xC4);
    painter.rect_filled(rect, corner(2.0), desktop);
    let strip_h = (rect.height() * 0.09).clamp(2.5, 7.0);
    let strip = Rect::from_min_size(rect.min, Vec2::new(rect.width(), strip_h));
    painter.rect_filled(strip, crate::theme::corner_top(2.0), paper.chrome);
    for index in 0..4 {
        let w = rect.width() * 0.06;
        painter.rect_filled(
            Rect::from_min_size(
                pos2(
                    strip.left() + 3.0 + index as f32 * (w + 3.0),
                    strip.center().y - 1.0,
                ),
                Vec2::new(w, 2.0),
            ),
            corner(1.0),
            paper.ink_faint,
        );
    }
    let inner = Rect::from_min_max(
        pos2(
            rect.left() + rect.width() * 0.13,
            strip.bottom() + rect.height() * 0.1,
        ),
        pos2(
            rect.right() - rect.width() * 0.09,
            rect.bottom() - rect.height() * 0.08,
        ),
    );
    window(painter, inner, platform, paper);
    painter.rect_stroke(
        rect,
        corner(2.0),
        Stroke::new(1.0, paper.edge),
        StrokeKind::Inside,
    );
}

fn all_displays(painter: &Painter, rect: Rect, platform: PreviewPlatform, paper: &Paper) {
    let gap = (rect.width() * 0.035).clamp(1.5, 5.0);
    let each = (rect.width() - gap) / 2.0;
    let left = Rect::from_min_size(
        pos2(rect.left(), rect.center().y - rect.height() * 0.42),
        Vec2::new(each, rect.height() * 0.84),
    );
    let right = Rect::from_min_size(
        pos2(left.right() + gap, rect.center().y - rect.height() * 0.34),
        Vec2::new(each, rect.height() * 0.68),
    );
    full_screen(painter, left, platform, paper);
    full_screen(painter, right, platform, paper);
}

fn scrolling(painter: &Painter, rect: Rect, paper: &Paper) {
    // A tall page whose content runs past the visible frame: the stub at the
    // bottom is what tells the eye this capture kept going.
    painter.rect_filled(rect, corner(2.0), paper.surface);
    document(
        painter,
        rect.shrink2(Vec2::new(rect.width() * 0.12, rect.height() * 0.06)),
        paper,
        false,
    );
    let fade = Rect::from_min_size(
        pos2(rect.left(), rect.bottom() - rect.height() * 0.1),
        Vec2::new(rect.width(), rect.height() * 0.1),
    );
    painter.rect_filled(fade, crate::theme::corner_bottom(2.0), paper.chrome);
    painter.rect_stroke(
        rect,
        corner(2.0),
        Stroke::new(1.0, paper.edge),
        StrokeKind::Inside,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_chrome_follows_the_host() {
        let platform = PreviewPlatform::current();
        if cfg!(target_os = "macos") {
            assert_eq!(platform, PreviewPlatform::MacOs);
        } else if cfg!(target_os = "windows") {
            assert_eq!(platform, PreviewPlatform::Windows);
        } else {
            assert_eq!(platform, PreviewPlatform::Linux);
        }
    }

    #[test]
    fn window_radius_stays_bounded() {
        for platform in [
            PreviewPlatform::MacOs,
            PreviewPlatform::Windows,
            PreviewPlatform::Linux,
        ] {
            for height in [4.0_f32, 40.0, 400.0] {
                let radius = window_radius(platform, height);
                assert!(
                    (1.0..=9.0).contains(&radius),
                    "{platform:?} {height} -> {radius}"
                );
            }
        }
    }
}
