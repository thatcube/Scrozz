//! The ordinary settings window and its About surface.

use egui::{Align, Color32, Layout, RichText, Sense, TextureHandle, TextureOptions, Vec2};

use crate::theme::{Appearance, Space, Text, Theme};

const SETTINGS_VIEWPORT: &str = "scrozz-settings";
const WINDOW_SIZE: Vec2 = Vec2::new(680.0, 430.0);
const ICON_SIZE: f32 = 150.0;

/// Identity displayed in Settings > About.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    /// Marketing/application version.
    pub version: &'static str,
    /// Package build number.
    pub build: &'static str,
}

impl BuildInfo {
    /// A compact, copy-friendly representation of this exact build.
    #[must_use]
    pub fn label(self) -> String {
        format!("Version {} (Build {})", self.version, self.build)
    }
}

/// Persistent state for the settings viewport.
#[derive(Default)]
pub struct SettingsWindow {
    open: bool,
    focus_requested: bool,
    icon: Option<TextureHandle>,
}

impl SettingsWindow {
    /// Opens or focuses the settings window.
    pub fn open(&mut self) {
        self.open = true;
        self.focus_requested = true;
    }

    /// Draws the settings viewport while it is open.
    pub fn show(&mut self, ctx: &egui::Context, build: BuildInfo) {
        if !self.open {
            return;
        }

        let icon = self
            .icon
            .get_or_insert_with(|| {
                ctx.load_texture(
                    "scrozz-settings-icon",
                    embedded_app_icon(),
                    TextureOptions::LINEAR,
                )
            })
            .clone();
        let mut open = true;

        let mut builder = egui::ViewportBuilder::default()
            .with_title("Scrozz Settings")
            .with_app_id("com.thatcube.Scrozz.settings")
            .with_inner_size(WINDOW_SIZE)
            .with_min_inner_size(WINDOW_SIZE)
            .with_max_inner_size(WINDOW_SIZE)
            .with_resizable(false);
        if std::mem::take(&mut self.focus_requested) {
            builder = builder.with_active(true);
        }

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(SETTINGS_VIEWPORT),
            builder,
            |settings_ui, _class| {
                if settings_ui
                    .ctx()
                    .input(|input| input.viewport().close_requested())
                {
                    open = false;
                }
                draw_about(settings_ui, &icon, build);
            },
        );

        self.open = open;
    }
}

fn draw_about(ui: &mut egui::Ui, icon: &TextureHandle, build: BuildInfo) {
    let appearance = if ui.visuals().dark_mode {
        Appearance::Dark
    } else {
        Appearance::Light
    };
    let theme = Theme::for_appearance(appearance);
    let palette = theme.palette;

    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, palette.canvas());
    egui::Frame::new()
        .inner_margin(egui::Margin::same(Space::XXL as i8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(
                    RichText::new("Settings")
                        .font(theme.font(Text::Title))
                        .color(palette.text),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    egui::Frame::new()
                        .fill(palette.active)
                        .corner_radius(9)
                        .inner_margin(egui::Margin::symmetric(14, 7))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new("About")
                                    .font(theme.font(Text::Label))
                                    .color(palette.accent_hi),
                            );
                        });
                });
            });

            ui.add_space(Space::XXL);
            ui.separator();
            ui.add_space(Space::HUGE);

            ui.horizontal(|ui| {
                ui.add_space(Space::XL);
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(ICON_SIZE), Sense::hover());
                ui.painter().image(
                    icon.id(),
                    rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );

                ui.add_space(Space::HUGE);
                ui.vertical(|ui| {
                    ui.add_space(Space::MD);
                    ui.label(
                        RichText::new("Scrozz")
                            .font(theme.font(Text::Display))
                            .color(palette.text),
                    );
                    ui.add_space(Space::XS);
                    ui.label(
                        RichText::new("Screenshots and screen recording, without limits.")
                            .font(theme.font(Text::Subtitle))
                            .color(palette.text_muted),
                    );
                    ui.add_space(Space::XL);

                    egui::Frame::new()
                        .fill(palette.card_fill_raised)
                        .stroke(egui::Stroke::new(1.0, palette.hairline))
                        .corner_radius(12)
                        .inner_margin(egui::Margin::symmetric(16, 11))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(build.label())
                                    .font(theme.font(Text::Label))
                                    .color(palette.text),
                            );
                        });

                    ui.add_space(Space::XL);
                    ui.label(
                        RichText::new("Free forever. Open source.")
                            .font(theme.font(Text::Body))
                            .color(palette.text_faint),
                    );
                });
            });
        });
}

fn embedded_app_icon() -> egui::ColorImage {
    let image = image::load_from_memory(include_bytes!("../../../assets/icons/icon-256.png"))
        .expect("the embedded Scrozz icon must be valid PNG")
        .into_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_label_names_both_identifiers() {
        assert_eq!(
            BuildInfo {
                version: "2026.8.28",
                build: "92",
            }
            .label(),
            "Version 2026.8.28 (Build 92)"
        );
    }
}
