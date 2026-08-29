use egui::{Button, Frame, Response, RichText, Stroke, Ui, Vec2, vec2};

use crate::theme::{Radius, Space, Text, Theme, corner};

pub(crate) const CONTROL_HEIGHT: f32 = 34.0;

pub(crate) fn panel<R>(
    ui: &mut Ui,
    theme: &Theme,
    width: f32,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> egui::InnerResponse<R> {
    Frame::new()
        .fill(theme.palette.card_fill)
        .stroke(Stroke::new(1.0, theme.palette.hairline))
        .corner_radius(corner(Radius::CARD))
        .inner_margin(egui::Margin::same(Space::LG as i8))
        .show(ui, |ui| {
            ui.set_width(width);
            add_contents(ui)
        })
}

pub(crate) fn button(
    ui: &mut Ui,
    theme: &Theme,
    label: &str,
    emphasized: bool,
    enabled: bool,
) -> Response {
    let fill = if emphasized && enabled {
        theme.palette.accent
    } else {
        theme.palette.card_fill_raised
    };
    let foreground = if emphasized && enabled {
        theme.palette.on_accent
    } else {
        theme.palette.text
    };
    ui.add_enabled(
        enabled,
        Button::new(
            RichText::new(label)
                .font(theme.font(Text::Button))
                .color(foreground),
        )
        .fill(fill)
        .stroke(Stroke::new(1.0, theme.palette.hairline))
        .corner_radius(corner(Radius::BUTTON))
        .min_size(vec2(76.0, CONTROL_HEIGHT)),
    )
}

pub(crate) fn choice(
    ui: &mut Ui,
    theme: &Theme,
    label: &str,
    selected: bool,
    enabled: bool,
) -> Response {
    let fill = if selected {
        theme.palette.accent
    } else {
        theme.palette.chip_fill
    };
    let foreground = if selected {
        theme.palette.on_accent
    } else {
        theme.palette.text_muted
    };
    ui.add_enabled(
        enabled,
        Button::new(
            RichText::new(label)
                .font(theme.font(Text::Label))
                .color(foreground),
        )
        .selected(selected)
        .fill(fill)
        .stroke(Stroke::new(1.0, theme.palette.hairline))
        .corner_radius(corner(Radius::CHIP))
        .min_size(Vec2::new(52.0, 30.0)),
    )
}

pub(crate) fn heading(ui: &mut Ui, theme: &Theme, text: &str) -> Response {
    ui.label(
        RichText::new(text)
            .font(theme.font(Text::Title))
            .color(theme.palette.text),
    )
}

pub(crate) fn section_label(ui: &mut Ui, theme: &Theme, text: &str) -> Response {
    ui.label(
        RichText::new(text)
            .font(theme.font(Text::Label))
            .color(theme.palette.text_muted),
    )
}

pub(crate) fn body(ui: &mut Ui, theme: &Theme, text: impl Into<String>) -> Response {
    ui.label(
        RichText::new(text)
            .font(theme.font(Text::Body))
            .color(theme.palette.text_muted),
    )
}

pub(crate) fn caption(ui: &mut Ui, theme: &Theme, text: impl Into<String>) -> Response {
    ui.label(
        RichText::new(text)
            .font(theme.font(Text::Caption))
            .color(theme.palette.text_faint),
    )
}

pub(crate) fn rule(ui: &mut Ui, theme: &Theme) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        Stroke::new(1.0, theme.palette.divider),
    );
}

pub(crate) fn install_scene_theme(ctx: &egui::Context) {
    let theme = if ctx.theme() == egui::Theme::Dark {
        Theme::dark()
    } else {
        Theme::light()
    };
    crate::theme::install_fonts(ctx);
    crate::theme::install_style(ctx, &theme);
}

pub(crate) fn scene_theme(theme: egui::Theme) -> Theme {
    match theme {
        egui::Theme::Dark => Theme::dark(),
        egui::Theme::Light => Theme::light(),
    }
}

pub(crate) fn format_duration(duration: std::time::Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3_600;
    let minutes = total % 3_600 / 60;
    let seconds = total % 60;
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}
