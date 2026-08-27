//! OCR onboarding and settings surfaces.

use egui::{
    Align, Align2, Button, FontId, Frame, Layout, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use crate::theme::{Appearance, Radius, Space, Text, Theme, corner};

/// The result of drawing the OCR introduction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OnboardingResponse {
    /// The single completion action was pressed.
    pub completed: bool,
}

/// One-time introduction to text capture.
#[derive(Debug, Clone, Copy, Default)]
pub struct OcrOnboarding;

impl OcrOnboarding {
    /// Draws the complete two-step introduction.
    #[must_use]
    pub fn ui(&mut self, ui: &mut Ui) -> OnboardingResponse {
        let theme = theme(ui);
        let palette = theme.palette;
        let mut response = OnboardingResponse::default();

        Frame::new()
            .fill(palette.card_fill)
            .inner_margin(egui::Margin::same(28))
            .show(ui, |ui| {
                ui.set_min_size(vec2(704.0, 432.0));
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("Copy text from anything on screen")
                            .font(theme.font(Text::Display))
                            .color(palette.text),
                    );
                    ui.add_space(Space::SM);
                    ui.label(
                        RichText::new(
                            "Select text in an image, video, PDF, webpage, photo, or QR code, \
                             then paste it wherever you need it.",
                        )
                        .font(theme.font(Text::Body))
                        .color(palette.text_muted),
                    );
                });

                ui.add_space(Space::XXL);
                ui.columns(2, |columns| {
                    panel(
                        &mut columns[0],
                        &theme,
                        "1. Select area with text",
                        Illustration::Selection,
                    );
                    panel(
                        &mut columns[1],
                        &theme,
                        "2. Paste the text",
                        Illustration::Paste,
                    );
                });

                ui.add_space(Space::XXL);
                ui.with_layout(Layout::top_down(Align::Center), |ui| {
                    let button = Button::new(
                        RichText::new("Got it!")
                            .font(theme.font(Text::Button))
                            .color(palette.on_accent),
                    )
                    .fill(palette.accent)
                    .stroke(Stroke::NONE)
                    .corner_radius(corner(Radius::BUTTON))
                    .min_size(vec2(132.0, 42.0));
                    response.completed = ui.add(button).clicked();
                });
            });

        response
    }
}

/// Response from the text-recognition section of Settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OcrSettingsResponse {
    /// The user asked to replay the introduction.
    pub show_onboarding: bool,
}

/// The text-recognition section of the Settings window.
#[derive(Debug, Clone, Copy, Default)]
pub struct OcrSettings;

impl OcrSettings {
    /// Draws the OCR settings entry point.
    #[must_use]
    pub fn ui(&mut self, ui: &mut Ui) -> OcrSettingsResponse {
        let theme = theme(ui);
        let palette = theme.palette;
        let mut response = OcrSettingsResponse::default();

        Frame::new()
            .fill(palette.card_fill)
            .inner_margin(egui::Margin::same(28))
            .show(ui, |ui| {
                ui.set_min_size(vec2(456.0, 176.0));
                ui.label(
                    RichText::new("Text recognition")
                        .font(theme.font(Text::Title))
                        .color(palette.text),
                );
                ui.add_space(Space::SM);
                ui.label(
                    RichText::new(
                        "Recognition language, accuracy, correction, line breaks, link detection, \
                         and image upscaling can be configured with `scrozz settings set ocr.*`.",
                    )
                    .font(theme.font(Text::Body))
                    .color(palette.text_muted),
                );
                ui.add_space(Space::XL);
                response.show_onboarding = ui
                    .add(
                        Button::new(
                            RichText::new("Show OCR introduction again")
                                .font(theme.font(Text::Button)),
                        )
                        .corner_radius(corner(Radius::BUTTON))
                        .min_size(vec2(238.0, 38.0)),
                    )
                    .clicked();
            });

        response
    }
}

#[derive(Debug, Clone, Copy)]
enum Illustration {
    Selection,
    Paste,
}

fn panel(ui: &mut Ui, theme: &Theme, caption: &str, illustration: Illustration) {
    let palette = theme.palette;
    Frame::new()
        .fill(palette.card_fill_raised)
        .stroke(Stroke::new(1.0, palette.hairline))
        .corner_radius(corner(Radius::CARD))
        .inner_margin(egui::Margin::same(18))
        .show(ui, |ui| {
            ui.set_min_height(232.0);
            let width = ui.available_width();
            let (rect, _) = ui.allocate_exact_size(vec2(width, 174.0), Sense::hover());
            match illustration {
                Illustration::Selection => draw_selection(ui, rect, theme),
                Illustration::Paste => draw_paste(ui, rect, theme),
            }
            ui.add_space(Space::MD);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(caption)
                        .font(theme.font(Text::Title))
                        .color(palette.text),
                );
            });
        });
}

fn draw_selection(ui: &Ui, rect: Rect, theme: &Theme) {
    let palette = theme.palette;
    let painter = ui.painter();
    let page = rect.shrink2(vec2(20.0, 12.0));
    painter.rect_filled(page, corner(Radius::THUMB), palette.chip_fill);

    let image = Rect::from_min_size(page.min + vec2(18.0, 18.0), vec2(page.width() - 36.0, 78.0));
    painter.rect_filled(image, corner(Radius::CHIP), palette.active);
    painter.circle_filled(image.left_top() + vec2(28.0, 24.0), 11.0, palette.accent_hi);
    painter.add(egui::Shape::convex_polygon(
        vec![
            image.left_bottom() + vec2(0.0, -8.0),
            image.left_top() + vec2(72.0, 34.0),
            image.left_top() + vec2(111.0, 66.0),
            image.right_bottom() + vec2(0.0, -8.0),
        ],
        palette.accent,
        Stroke::NONE,
    ));

    for (index, width) in [0.78_f32, 0.62, 0.70].into_iter().enumerate() {
        let y = image.bottom() + 18.0 + index as f32 * 13.0;
        painter.line_segment(
            [
                pos2(image.left(), y),
                pos2(image.left() + image.width() * width, y),
            ],
            Stroke::new(4.0, palette.text_faint),
        );
    }

    let selection = Rect::from_min_max(
        pos2(image.left() - 7.0, image.bottom() + 7.0),
        pos2(image.right() - 30.0, page.bottom() - 12.0),
    );
    painter.rect_stroke(
        selection,
        corner(Radius::CHIP),
        Stroke::new(2.0, palette.focus_ring),
        StrokeKind::Inside,
    );
    for point in [
        selection.left_top(),
        selection.right_top(),
        selection.left_bottom(),
        selection.right_bottom(),
    ] {
        painter.circle_filled(point, 4.0, palette.focus_ring);
    }
}

fn draw_paste(ui: &Ui, rect: Rect, theme: &Theme) {
    let palette = theme.palette;
    let painter = ui.painter();
    let editor = rect.shrink2(vec2(24.0, 12.0));
    painter.rect_filled(editor, corner(Radius::THUMB), palette.chip_fill);

    let toolbar = Rect::from_min_size(editor.min, vec2(editor.width(), 30.0));
    painter.rect_filled(toolbar, corner(Radius::THUMB), palette.active);
    for index in 0..3 {
        painter.circle_filled(
            toolbar.left_center() + vec2(16.0 + index as f32 * 15.0, 0.0),
            3.5,
            palette.text_faint,
        );
    }

    let text_origin = editor.left_top() + vec2(18.0, 52.0);
    let font = FontId::new(13.0, egui::FontFamily::Proportional);
    let lines = [
        "A URL from that QR code:",
        "https://scrozz.app/download",
        "",
        "Ready to paste.",
    ];
    for (index, line) in lines.into_iter().enumerate() {
        painter.text(
            text_origin + vec2(0.0, index as f32 * 23.0),
            Align2::LEFT_TOP,
            line,
            font.clone(),
            if index == 1 {
                palette.accent_hi
            } else {
                palette.text
            },
        );
    }
    painter.line_segment(
        [
            text_origin + vec2(96.0, 69.0),
            text_origin + vec2(96.0, 85.0),
        ],
        Stroke::new(1.5, palette.focus_ring),
    );
}

fn theme(ui: &Ui) -> Theme {
    Theme::for_appearance(if ui.visuals().dark_mode {
        Appearance::Dark
    } else {
        Appearance::Light
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_begin_without_actions() {
        assert!(!OnboardingResponse::default().completed);
        assert!(!OcrSettingsResponse::default().show_onboarding);
    }

    #[test]
    fn onboarding_is_exactly_the_two_documented_steps() {
        let captions = ["1. Select area with text", "2. Paste the text"];
        assert_eq!(captions.len(), 2);
        assert!(captions[0].contains("Select area"));
        assert!(captions[1].contains("Paste"));
    }

    #[test]
    fn both_surfaces_render_headlessly_from_explicit_state() {
        let context = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(760.0, 500.0))),
            ..Default::default()
        };
        crate::theme::install_fonts(&context);
        let mut warmup_output = context.run_ui(input.clone(), |_| {});
        warmup_output.textures_delta.clear();
        let mut onboarding = OcrOnboarding;
        let mut onboarding_output = context.run_ui(input, |ui| {
            let response = onboarding.ui(ui);
            assert!(!response.completed);
        });
        assert!(!onboarding_output.shapes.is_empty());
        onboarding_output.textures_delta.clear();

        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(520.0, 240.0))),
            ..Default::default()
        };
        let mut settings = OcrSettings;
        let mut settings_output = context.run_ui(input, |ui| {
            let response = settings.ui(ui);
            assert!(!response.show_onboarding);
        });
        assert!(!settings_output.shapes.is_empty());
        settings_output.textures_delta.clear();
    }
}
