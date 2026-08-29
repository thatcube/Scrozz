//! The macOS Screen & System Audio Recording preflight surface.
//!
//! This module only draws intent. The app owns TCC, Settings, cooldowns and the
//! Apple picker, so a button cannot make the UI claim that access changed before
//! the operating system confirms it.

use egui::{Align, Layout, RichText, Vec2};

use crate::theme::{Appearance, Space, Text, Theme};

const VIEWPORT: &str = "scrozz-screen-capture-permission";
const WINDOW_SIZE: Vec2 = Vec2::new(660.0, 470.0);

/// Which permission explanation is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStage {
    /// Scrozz's explanation before the system prompt.
    Preflight,
    /// Direct access was not granted.
    Denied,
    /// The user is expected to return from System Settings.
    WaitingForSettings,
    /// A policy may control the grant.
    Restricted,
    /// The direct-capture API is unavailable.
    Unavailable,
}

/// What can be said about Apple's privacy-preserving picker for this action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerFallback {
    /// The button can present Apple's picker.
    Available {
        /// The limitations that remain after selecting content.
        limitations: String,
    },
    /// No picker button can honestly complete the requested action.
    Unavailable {
        /// The exact reason.
        reason: String,
    },
}

/// One frame of permission-window content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPrompt {
    /// Which copy and controls to show.
    pub stage: PermissionStage,
    /// The action that is waiting, e.g. `Capture Window`.
    pub action: String,
    /// Whether Apple's fallback can complete that action.
    pub picker: PickerFallback,
}

impl PermissionPrompt {
    /// A deterministic fixture for the golden-image harness.
    #[must_use]
    pub fn preflight_fixture() -> Self {
        Self {
            stage: PermissionStage::Preflight,
            action: "Capture Window".to_owned(),
            picker: PickerFallback::Available {
                limitations: "Apple's picker replaces Scrozz's custom selection. \
                              Capture Area, All Displays, unattended global capture, \
                              and system-audio recording remain unavailable."
                    .to_owned(),
            },
        }
    }
}

/// A permission-window button or close request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionResponse {
    /// Allow the app to invoke macOS's prompt.
    Continue,
    /// Use Apple's limited picker for this action.
    UseApplePicker,
    /// Open the exact privacy pane.
    OpenSystemSettings,
    /// Dismiss without changing access.
    NotNow,
}

/// Persistent state for the focusable permission viewport.
#[derive(Debug, Default)]
pub struct PermissionWindow {
    showing: bool,
}

impl PermissionWindow {
    /// Closes the native viewport immediately.
    ///
    /// Used before Apple's display picker is presented so Scrozz's explanation
    /// cannot appear in the selected pixels.
    pub fn close(&mut self, ctx: &egui::Context) {
        self.showing = false;
        ctx.send_viewport_cmd_to(
            egui::ViewportId::from_hash_of(VIEWPORT),
            egui::ViewportCommand::Close,
        );
    }

    /// Draws the window if `prompt` exists.
    ///
    /// Returning `NotNow` for the close button makes closing the title-bar button
    /// obey the same cooldown as the explicit button.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        prompt: Option<&PermissionPrompt>,
    ) -> Option<PermissionResponse> {
        let Some(prompt) = prompt else {
            self.showing = false;
            return None;
        };

        let request_focus = !std::mem::replace(&mut self.showing, true);
        let builder = viewport_builder(request_focus);

        let mut response = None;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(VIEWPORT),
            builder,
            |permission_ui, _class| {
                if permission_ui
                    .ctx()
                    .input(|input| input.viewport().close_requested())
                {
                    response = Some(PermissionResponse::NotNow);
                    return;
                }
                if request_focus {
                    permission_ui
                        .ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                let theme = theme_for(permission_ui);
                permission_ui.painter().rect_filled(
                    permission_ui.max_rect(),
                    0.0,
                    theme.palette.canvas(),
                );
                response = draw_prompt(permission_ui, prompt, &theme);
            },
        );
        response
    }
}

fn viewport_builder(active: bool) -> egui::ViewportBuilder {
    let builder = egui::ViewportBuilder::default()
        .with_title("Scrozz Screen Recording Access")
        .with_app_id("com.thatcube.Scrozz.permission")
        .with_inner_size(WINDOW_SIZE)
        .with_min_inner_size(WINDOW_SIZE)
        .with_max_inner_size(WINDOW_SIZE)
        .with_resizable(false)
        .with_window_level(egui::WindowLevel::Normal);
    if active {
        builder.with_active(true)
    } else {
        builder
    }
}

fn draw_prompt(
    ui: &mut egui::Ui,
    prompt: &PermissionPrompt,
    theme: &Theme,
) -> Option<PermissionResponse> {
    let palette = theme.palette;
    let mut response = None;

    egui::Frame::new()
        .inner_margin(egui::Margin::same(Space::HUGE as i8))
        .show(ui, |ui| {
            ui.set_min_size(WINDOW_SIZE - Vec2::splat(Space::HUGE * 2.0));
            ui.label(
                RichText::new(title(prompt.stage))
                    .font(theme.font(Text::Title))
                    .color(palette.text),
            );
            ui.add_space(Space::SM);
            ui.label(
                RichText::new(format!("{} is waiting.", prompt.action))
                    .font(theme.font(Text::Subtitle))
                    .color(palette.text_muted),
            );
            ui.add_space(Space::XL);

            egui::Frame::new()
                .fill(palette.card_fill_raised)
                .stroke(egui::Stroke::new(1.0, palette.hairline))
                .corner_radius(14)
                .inner_margin(egui::Margin::same(Space::XL as i8))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(body(prompt.stage))
                            .font(theme.font(Text::Body))
                            .color(palette.text),
                    );

                    if !matches!(prompt.stage, PermissionStage::Preflight) {
                        ui.add_space(Space::LG);
                        let detail = match &prompt.picker {
                            PickerFallback::Available { limitations } => limitations,
                            PickerFallback::Unavailable { reason } => reason,
                        };
                        ui.label(
                            RichText::new(detail)
                                .font(theme.font(Text::Caption))
                                .color(palette.text_muted),
                        );
                    }
                });

            ui.add_space(Space::XL);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Not Now").clicked() {
                    response = Some(PermissionResponse::NotNow);
                }
                if can_open_settings(prompt.stage) && ui.button("Open System Settings").clicked() {
                    response = Some(PermissionResponse::OpenSystemSettings);
                }
                if matches!(prompt.picker, PickerFallback::Available { .. })
                    && !matches!(prompt.stage, PermissionStage::Preflight)
                    && ui.button("Use Apple Picker").clicked()
                {
                    response = Some(PermissionResponse::UseApplePicker);
                }
                if matches!(prompt.stage, PermissionStage::Preflight)
                    && ui.button("Continue").clicked()
                {
                    response = Some(PermissionResponse::Continue);
                }
            });
        });

    response
}

const fn title(stage: PermissionStage) -> &'static str {
    match stage {
        PermissionStage::Preflight => "Before macOS asks",
        PermissionStage::Denied => "Direct access is not granted",
        PermissionStage::WaitingForSettings => "Return after changing access",
        PermissionStage::Restricted => "Access may be managed",
        PermissionStage::Unavailable => "Direct capture is unavailable",
    }
}

const fn body(stage: PermissionStage) -> &'static str {
    match stage {
        PermissionStage::Preflight => {
            "Scrozz needs direct screen access for its custom Capture Area and Capture Window \
             selection, global shortcuts, and recording. macOS controls the grant. Sensitive \
             content may be visible or audible. Scrozz does not upload it unless you explicitly \
             enable an upload action."
        }
        PermissionStage::Denied => {
            "macOS did not grant direct Screen & System Audio Recording access. You can grant it \
             in System Settings or use Apple's picker for this one supported capture. If the \
             setting is locked, an organization or parental controls may manage it."
        }
        PermissionStage::WaitingForSettings => {
            "Change Scrozz's Screen & System Audio Recording access in System Settings, then \
             return here. Scrozz will retry this action once only if macOS reports the grant."
        }
        PermissionStage::Restricted => {
            "macOS is withholding direct capture. If this Mac is managed by an organization or \
             parental controls, only an administrator can change that policy."
        }
        PermissionStage::Unavailable => {
            "This macOS version does not provide the direct capture API this action requires. \
             Scrozz will not substitute another target or pretend the capture can run."
        }
    }
}

const fn can_open_settings(stage: PermissionStage) -> bool {
    !matches!(
        stage,
        PermissionStage::Restricted | PermissionStage::Unavailable
    )
}

fn theme_for(ui: &egui::Ui) -> Theme {
    let appearance = if ui.visuals().dark_mode {
        Appearance::Dark
    } else {
        Appearance::Light
    };
    Theme::for_appearance(appearance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_and_unavailable_surfaces_do_not_offer_a_fake_settings_fix() {
        assert!(!can_open_settings(PermissionStage::Restricted));
        assert!(!can_open_settings(PermissionStage::Unavailable));
        assert!(can_open_settings(PermissionStage::Denied));
    }

    #[test]
    fn preflight_copy_names_sensitive_screen_and_audio_access() {
        let copy = body(PermissionStage::Preflight);
        assert!(copy.contains("Sensitive content"));
        assert!(copy.contains("visible or audible"));
        assert!(copy.contains("does not upload"));
        assert!(copy.contains("macOS controls"));
    }

    #[test]
    fn permission_ui_is_a_focusable_normal_window_only_when_opened() {
        let opened = viewport_builder(true);
        assert_eq!(opened.window_level, Some(egui::WindowLevel::Normal));
        assert_eq!(opened.active, Some(true));
        assert_eq!(viewport_builder(false).active, None);
    }
}
