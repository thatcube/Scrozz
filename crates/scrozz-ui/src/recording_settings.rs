//! Privacy-forward recording interaction settings.

use egui::{Color32, ComboBox, Frame, RichText, Stroke, Ui};
use scrozz_core::CursorMode;
use scrozz_record::{
    EngineCapabilities, RecordingSettings,
    settings::{ClickStyle, KeystrokeScope, OverlayAnchor, OverlaySize, OverlayTheme, Rgba8},
};

use crate::{
    harness::{Scene, SceneCtx},
    recording_controls::{body, button, caption, heading, panel, rule, section_label},
    theme::{Radius, Space, Text, Theme, corner},
};

/// Semantic request raised by the recording settings panel.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordingSettingsAction {
    /// Persist and apply a changed settings value.
    Changed(RecordingSettings),
    /// Close the panel.
    Close,
    /// Save settings and begin target selection.
    StartRecording,
}

/// Result of drawing recording settings.
#[derive(Debug)]
pub struct RecordingSettingsResponse {
    /// Actions requested during this pass.
    pub actions: Vec<RecordingSettingsAction>,
    /// Current edited settings.
    pub settings: RecordingSettings,
    /// Response for the whole panel.
    pub response: egui::Response,
}

/// Compact recording preferences surface shared by all desktop platforms.
pub struct RecordingSettingsPanel<'a> {
    settings: RecordingSettings,
    capabilities: EngineCapabilities,
    theme: &'a Theme,
}

impl<'a> RecordingSettingsPanel<'a> {
    /// Creates a panel from caller-owned values.
    #[must_use]
    pub const fn new(
        settings: RecordingSettings,
        capabilities: EngineCapabilities,
        theme: &'a Theme,
    ) -> Self {
        Self {
            settings,
            capabilities,
            theme,
        }
    }

    /// Draws the settings panel.
    pub fn show(mut self, ui: &mut Ui) -> RecordingSettingsResponse {
        let before = self.settings;
        let inner = panel(ui, self.theme, 500.0, |ui| {
            ui.horizontal(|ui| {
                heading(ui, self.theme, "Recording");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button(ui, self.theme, "Record", true, self.capabilities.video).clicked() {
                        ui.data_mut(|data| {
                            data.insert_temp(egui::Id::new("recording-settings-start"), true);
                        });
                    }
                    if button(ui, self.theme, "Done", false, true).clicked() {
                        ui.data_mut(|data| {
                            data.insert_temp(egui::Id::new("recording-settings-close"), true);
                        });
                    }
                });
            });
            body(
                ui,
                self.theme,
                "Choose what viewers see. Input monitoring starts only with a recording.",
            );
            ui.add_space(Space::MD);
            privacy_rail(ui, self.theme, self.settings.needs_input_monitoring());
            ui.add_space(Space::LG);
            rule(ui, self.theme);
            ui.add_space(Space::MD);

            section_label(ui, self.theme, "Pointer");
            let mut cursor = self.settings.cursor == CursorMode::Visible;
            if ui.checkbox(&mut cursor, "Show pointer").changed() {
                self.settings.cursor = if cursor {
                    CursorMode::Visible
                } else {
                    CursorMode::Hidden
                };
                if !cursor {
                    self.settings.cursor_smoothing = false;
                }
            }
            ui.add_enabled_ui(cursor, |ui| {
                ui.checkbox(
                    &mut self.settings.cursor_smoothing,
                    "Smooth pointer movement",
                )
                .on_hover_text(
                    "Uses a consistent high-contrast recording pointer with bounded deterministic smoothing. Original timing stays intact.",
                );
            });

            ui.add_space(Space::MD);
            section_label(ui, self.theme, "Clicks");
            ui.add_enabled_ui(self.capabilities.click_capture, |ui| {
                ui.checkbox(&mut self.settings.clicks.enabled, "Highlight clicks");
                ui.add_enabled_ui(self.settings.clicks.enabled, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for (style, label) in [
                            (ClickStyle::Outline, "Outline"),
                            (ClickStyle::Filled, "Filled"),
                        ] {
                            ui.selectable_value(&mut self.settings.clicks.style, style, label);
                        }
                        ui.checkbox(&mut self.settings.clicks.animate, "Animate");
                    });
                    option_size(ui, "recording-click-size", &mut self.settings.clicks.size);
                    let mut color = Color32::from_rgba_unmultiplied(
                        self.settings.clicks.color.r,
                        self.settings.clicks.color.g,
                        self.settings.clicks.color.b,
                        self.settings.clicks.color.a,
                    );
                    ui.horizontal(|ui| {
                        caption(ui, self.theme, "Color");
                        if ui.color_edit_button_srgba(&mut color).changed() {
                            self.settings.clicks.color = rgba8_from_picker(color);
                        }
                    });
                });
            });
            if !self.capabilities.click_capture {
                caption(ui, self.theme, input_unavailable());
            }

            ui.add_space(Space::MD);
            section_label(ui, self.theme, "Keystrokes");
            ui.add_enabled_ui(self.capabilities.key_capture, |ui| {
                ui.checkbox(&mut self.settings.keystrokes.enabled, "Show keystrokes");
                ui.add_enabled_ui(self.settings.keystrokes.enabled, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.radio_value(
                            &mut self.settings.keystrokes.scope,
                            KeystrokeScope::ModifiersOnly,
                            "Shortcuts only",
                        );
                        ui.radio_value(
                            &mut self.settings.keystrokes.scope,
                            KeystrokeScope::All,
                            "All keys",
                        );
                    });
                    if self.settings.keystrokes.scope == KeystrokeScope::All {
                        privacy_warning(ui, self.theme);
                    } else {
                        caption(
                            ui,
                            self.theme,
                            "Recommended. Plain typing is discarded before it enters memory.",
                        );
                    }
                    option_size(ui, "recording-key-size", &mut self.settings.keystrokes.size);
                    option_position(ui, &mut self.settings.keystrokes.position);
                    option_theme(ui, &mut self.settings.keystrokes.theme);
                });
            });
            if !self.capabilities.key_capture {
                caption(ui, self.theme, input_unavailable());
            }

            ui.add_space(Space::MD);
            section_label(ui, self.theme, "After recording");
            ui.checkbox(
                &mut self.settings.after_capture.open_editor,
                "Open the video editor",
            )
            .on_hover_text(
                "Retains a private temporary source tied to this editor session so interactions can be toggled before export.",
            );
            ui.checkbox(
                &mut self.settings.after_capture.recent_captures_overlay,
                "Add to Recent Captures",
            );
        });
        let mut actions = Vec::new();
        if before != self.settings {
            actions.push(RecordingSettingsAction::Changed(self.settings));
        }
        let close = ui
            .data_mut(|data| data.remove_temp::<bool>(egui::Id::new("recording-settings-close")))
            .unwrap_or(false);
        if close {
            actions.push(RecordingSettingsAction::Close);
        }
        let start = ui
            .data_mut(|data| data.remove_temp::<bool>(egui::Id::new("recording-settings-start")))
            .unwrap_or(false);
        if start {
            actions.push(RecordingSettingsAction::StartRecording);
        }
        RecordingSettingsResponse {
            actions,
            settings: self.settings,
            response: inner.response,
        }
    }
}

fn rgba8_from_picker(color: Color32) -> Rgba8 {
    let [red, green, blue, alpha] = color.to_srgba_unmultiplied();
    Rgba8::rgba(red, green, blue, alpha)
}

fn privacy_rail(ui: &mut Ui, theme: &Theme, monitoring: bool) {
    Frame::new()
        .fill(theme.palette.chip_fill)
        .stroke(Stroke::new(1.0, theme.palette.hairline))
        .corner_radius(corner(Radius::CHIP))
        .inner_margin(egui::Margin::same(Space::SM as i8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(
                    if monitoring {
                        theme.palette.warning
                    } else {
                        theme.palette.success
                    },
                    if monitoring {
                        "Input monitoring on"
                    } else {
                        "Input monitoring off"
                    },
                );
                caption(
                    ui,
                    theme,
                    "Local only · active recording only · no key-event sidecars",
                );
            });
        });
}

pub(crate) fn recording_indicator(ui: &mut Ui, theme: &Theme, settings: RecordingSettings) {
    Frame::new()
        .fill(theme.palette.chip_fill)
        .stroke(Stroke::new(1.0, theme.palette.hairline))
        .corner_radius(corner(Radius::CHIP))
        .inner_margin(egui::Margin::same(Space::SM as i8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(theme.palette.recording, "● LIVE");
                caption(
                    ui,
                    theme,
                    format!(
                        "Pointer {} · Clicks {} · Keys {}",
                        if settings.shows_cursor() { "on" } else { "off" },
                        if settings.clicks.enabled { "on" } else { "off" },
                        if settings.keystrokes.enabled {
                            match settings.keystrokes.scope {
                                KeystrokeScope::ModifiersOnly => "shortcuts",
                                KeystrokeScope::All => "all",
                            }
                        } else {
                            "off"
                        }
                    ),
                );
            });
        });
}

fn privacy_warning(ui: &mut Ui, theme: &Theme) {
    Frame::new()
        .fill(theme.palette.warning.linear_multiply(0.12))
        .stroke(Stroke::new(
            1.0,
            theme.palette.warning.linear_multiply(0.72),
        ))
        .corner_radius(corner(Radius::CHIP))
        .inner_margin(egui::Margin::same(Space::SM as i8))
        .show(ui, |ui| {
            ui.label(
                RichText::new("Privacy warning")
                    .font(theme.font(Text::Label))
                    .color(theme.palette.warning),
            );
            body(ui, theme, all_keys_warning());
        });
}

fn all_keys_warning() -> &'static str {
    "All keys can expose messages, searches, and secrets. Secure or uncertain fields are still suppressed."
}

fn option_size(ui: &mut Ui, id: &str, size: &mut OverlaySize) {
    ComboBox::from_id_salt(id)
        .selected_text(match size {
            OverlaySize::Small => "Small",
            OverlaySize::Medium => "Medium",
            OverlaySize::Large => "Large",
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(size, OverlaySize::Small, "Small");
            ui.selectable_value(size, OverlaySize::Medium, "Medium");
            ui.selectable_value(size, OverlaySize::Large, "Large");
        });
}

fn option_position(ui: &mut Ui, position: &mut OverlayAnchor) {
    ComboBox::from_id_salt("recording-key-position")
        .selected_text(anchor_label(*position))
        .show_ui(ui, |ui| {
            for anchor in OverlayAnchor::ALL {
                ui.selectable_value(position, anchor, anchor_label(anchor));
            }
        });
}

fn option_theme(ui: &mut Ui, theme: &mut OverlayTheme) {
    ComboBox::from_id_salt("recording-key-theme")
        .selected_text(match theme {
            OverlayTheme::Adaptive => "Adaptive contrast",
            OverlayTheme::Dark => "Dark",
            OverlayTheme::Light => "Light",
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(theme, OverlayTheme::Adaptive, "Adaptive contrast");
            ui.selectable_value(theme, OverlayTheme::Dark, "Dark");
            ui.selectable_value(theme, OverlayTheme::Light, "Light");
        });
}

fn anchor_label(anchor: OverlayAnchor) -> &'static str {
    match anchor {
        OverlayAnchor::TopLeft => "Top left",
        OverlayAnchor::TopCenter => "Top center",
        OverlayAnchor::TopRight => "Top right",
        OverlayAnchor::BottomLeft => "Bottom left",
        OverlayAnchor::BottomCenter => "Bottom center",
        OverlayAnchor::BottomRight => "Bottom right",
    }
}

fn input_unavailable() -> &'static str {
    if cfg!(target_os = "linux") {
        "Unavailable on this session. Wayland does not expose global input events."
    } else {
        "Unavailable in this recording backend."
    }
}

/// Real recording-settings renderer used by the deterministic harness.
#[derive(Debug, Default)]
pub struct RecordingSettingsScene;

impl Scene for RecordingSettingsScene {
    fn name(&self) -> &str {
        "recording-settings"
    }

    fn setup(&self, ctx: &egui::Context) {
        crate::recording_controls::install_scene_theme(ctx);
    }

    fn ui(&self, ui: &mut Ui, ctx: &SceneCtx<'_>) {
        let mut settings = RecordingSettings::shipped();
        settings.clicks.enabled = true;
        settings.keystrokes.enabled = true;
        settings.keystrokes.scope = KeystrokeScope::All;
        let theme = crate::recording_controls::scene_theme(ctx.theme);
        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
            ui.add_space(Space::XL);
            ui.allocate_ui_with_layout(
                egui::vec2(540.0, ui.available_height()),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    RecordingSettingsPanel::new(settings, EngineCapabilities::ALL, &theme).show(ui);
                },
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_keys_warning_names_content_risk_and_secure_suppression() {
        let warning = all_keys_warning();
        assert!(warning.contains("messages"));
        assert!(warning.contains("secrets"));
        assert!(warning.contains("Secure"));
    }

    #[test]
    fn shipped_panel_defaults_require_no_input_monitoring() {
        let settings = RecordingSettings::shipped();
        assert!(settings.shows_cursor());
        assert!(!settings.clicks.enabled);
        assert!(!settings.keystrokes.enabled);
        assert_eq!(settings.keystrokes.scope, KeystrokeScope::ModifiersOnly);
        assert!(!settings.needs_input_monitoring());
    }

    #[test]
    fn translucent_color_round_trip_does_not_premultiply_repeatedly() {
        let original = Rgba8::rgba(120, 80, 240, 96);
        let picker =
            Color32::from_rgba_unmultiplied(original.r, original.g, original.b, original.a);
        let once = rgba8_from_picker(picker);
        let twice = rgba8_from_picker(Color32::from_rgba_unmultiplied(
            once.r, once.g, once.b, once.a,
        ));
        assert_eq!(once, twice);
        assert_eq!(once.a, original.a);
        assert!(once.r.abs_diff(original.r) <= 1);
        assert!(once.g.abs_diff(original.g) <= 1);
        assert!(once.b.abs_diff(original.b) <= 1);
    }
}
