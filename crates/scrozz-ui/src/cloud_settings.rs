//! Platform-adaptive cloud sharing Settings viewport.
//!
//! The surface reports user intent and never touches disk, credentials, or the
//! network. Those operations belong to the host, which returns a secret-free
//! model on the next frame.

use egui::{Align, Layout, RichText, TextEdit, Vec2};
use zeroize::Zeroize as _;

use crate::theme::{Appearance, Space, Text, Theme};

const VIEWPORT_ID: &str = "scrozz-cloud-settings";
const WINDOW_SIZE: Vec2 = Vec2::new(760.0, 650.0);
const MIN_WINDOW_SIZE: Vec2 = Vec2::new(620.0, 520.0);

/// Desktop whose native Settings navigation convention should be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPlatform {
    /// Top category navigation.
    MacOs,
    /// Left navigation matching Windows Settings.
    Windows,
    /// Desktop-neutral left navigation.
    Linux,
}

/// Stable pane used by generated cloud-settings previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudSettingsPreview {
    /// Provider setup, link policy, naming, and tags.
    Provider,
    /// Native credential-vault state and controls.
    Credentials,
}

impl SettingsPlatform {
    /// Current compilation target.
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

/// Persistent non-secret values edited by this viewport.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct CloudSettingsDraft {
    pub provider: String,
    pub bucket: String,
    pub region: String,
    pub endpoint: String,
    pub account_id: String,
    pub prefix: String,
    pub public_base_url: String,
    pub url_policy: String,
    pub expiry_seconds: u32,
    pub naming_template: String,
    pub tags: String,
    pub protection_mode: String,
    pub viewer_title: String,
    pub viewer_accent: String,
}

/// Native vault state safe to show or log.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct CloudCredentialView {
    pub backend: String,
    pub stored: bool,
    pub problem: Option<String>,
}

/// Result of the most recent read-only connection probe.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(missing_docs)]
pub enum CloudConnectionState {
    #[default]
    Idle,
    Testing,
    Passed,
    Failed(String),
}

/// Complete secret-free Settings view model.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct CloudSettingsModel {
    pub config: CloudSettingsDraft,
    pub credentials: CloudCredentialView,
    pub upload_enabled: bool,
    pub unavailable_reason: Option<String>,
    pub connection: CloudConnectionState,
}

/// Credential values entered for one native-vault update.
#[derive(Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct CredentialDraft {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
    pub share_password: String,
}

impl std::fmt::Debug for CredentialDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialDraft")
            .field("access_key_id", &"[REDACTED]")
            .field("secret_access_key", &"[REDACTED]")
            .field("session_token", &"[REDACTED]")
            .field("share_password", &"[REDACTED]")
            .finish()
    }
}

impl Drop for CredentialDraft {
    fn drop(&mut self) {
        self.access_key_id.zeroize();
        self.secret_access_key.zeroize();
        self.session_token.zeroize();
        self.share_password.zeroize();
    }
}

/// Intent emitted to the application host.
#[allow(missing_docs)]
pub enum CloudSettingsEvent {
    Save(Box<CloudSettingsDraft>),
    StoreCredentials(CredentialDraft),
    RemoveCredentials,
    TestConnection,
}

impl std::fmt::Debug for CloudSettingsEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Save(config) => f.debug_tuple("Save").field(config).finish(),
            Self::StoreCredentials(_) => f.write_str("StoreCredentials([REDACTED])"),
            Self::RemoveCredentials => f.write_str("RemoveCredentials"),
            Self::TestConnection => f.write_str("TestConnection"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Pane {
    #[default]
    Provider,
    Credentials,
}

/// Stateful Settings viewport.
pub struct CloudSettingsWindow {
    open: bool,
    focus_requested: bool,
    platform: SettingsPlatform,
    pane: Pane,
    draft: Option<CloudSettingsDraft>,
    dirty: bool,
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
    share_password: String,
    local_error: Option<String>,
}

impl Default for CloudSettingsWindow {
    fn default() -> Self {
        Self {
            open: false,
            focus_requested: false,
            platform: SettingsPlatform::current(),
            pane: Pane::default(),
            draft: None,
            dirty: false,
            access_key_id: String::new(),
            secret_access_key: String::new(),
            session_token: String::new(),
            share_password: String::new(),
            local_error: None,
        }
    }
}

impl Drop for CloudSettingsWindow {
    fn drop(&mut self) {
        self.clear_credential_fields();
    }
}

impl CloudSettingsWindow {
    /// Opens or focuses the viewport.
    pub fn open(&mut self, model: &CloudSettingsModel) {
        if !self.open || !self.dirty {
            self.draft = Some(model.config.clone());
        }

        self.open = true;
        self.focus_requested = true;
    }

    /// Whether the viewport is open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Draws the viewport and returns requested operations.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        model: &CloudSettingsModel,
    ) -> Vec<CloudSettingsEvent> {
        if !self.open {
            return Vec::new();
        }
        if !self.dirty {
            self.draft = Some(model.config.clone());
        }
        let mut events = Vec::new();
        let mut open = true;
        let focus = std::mem::take(&mut self.focus_requested);
        let builder = egui::ViewportBuilder::default()
            .with_title("Scrozz Settings - Sharing")
            .with_app_id("com.thatcube.Scrozz.settings")
            .with_inner_size(WINDOW_SIZE)
            .with_min_inner_size(MIN_WINDOW_SIZE)
            .with_resizable(true)
            .with_window_level(egui::WindowLevel::Normal)
            .with_active(focus);
        let appearance = match ctx.system_theme() {
            Some(egui::Theme::Dark) => Appearance::Dark,
            Some(egui::Theme::Light) | None => Appearance::Light,
        };
        let theme = Theme::for_appearance(appearance);
        let platform = self.platform;
        let pane = &mut self.pane;
        let draft = self.draft.as_mut().expect("open settings has a draft");
        let dirty = &mut self.dirty;
        let local_error = &mut self.local_error;
        let access_key_id = &mut self.access_key_id;
        let secret_access_key = &mut self.secret_access_key;
        let session_token = &mut self.session_token;
        let share_password = &mut self.share_password;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(VIEWPORT_ID),
            builder,
            |viewport, _| {
                if viewport
                    .ctx()
                    .input(|input| input.viewport().close_requested())
                {
                    open = false;
                    return;
                }
                if focus {
                    viewport
                        .ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                crate::theme::install_style(viewport.ctx(), &theme);
                viewport
                    .painter()
                    .rect_filled(viewport.max_rect(), 0.0, theme.palette.canvas());
                match platform {
                    SettingsPlatform::MacOs => {
                        draw_top_navigation(viewport, &theme, pane);
                        viewport.add_space(Space::MD);
                        draw_body(
                            viewport,
                            &theme,
                            *pane,
                            draft,
                            dirty,
                            local_error,
                            access_key_id,
                            secret_access_key,
                            session_token,
                            share_password,
                            model,
                            &mut events,
                        );
                    }
                    SettingsPlatform::Windows | SettingsPlatform::Linux => {
                        viewport.horizontal(|ui| {
                            ui.vertical(|ui| draw_side_navigation(ui, &theme, pane));
                            ui.separator();
                            ui.vertical(|ui| {
                                ui.set_min_width(430.0);
                                draw_body(
                                    ui,
                                    &theme,
                                    *pane,
                                    draft,
                                    dirty,
                                    local_error,
                                    access_key_id,
                                    secret_access_key,
                                    session_token,
                                    share_password,
                                    model,
                                    &mut events,
                                );
                            });
                        });
                    }
                }
            },
        );
        self.open = open;
        if !open {
            self.clear_credential_fields();
            self.local_error = None;
        }
        events
    }

    fn clear_credential_fields(&mut self) {
        self.access_key_id.zeroize();
        self.secret_access_key.zeroize();
        self.session_token.zeroize();
        self.share_password.zeroize();
    }
}

/// Draws the real cloud Settings body without creating a native viewport.
///
/// The deterministic harness uses this exact path for committed goldens.
pub fn render_preview(
    ui: &mut egui::Ui,
    platform: SettingsPlatform,
    pane: CloudSettingsPreview,
    model: &CloudSettingsModel,
) {
    let mut preview = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(ui.ctx().content_rect())
            .layout(Layout::top_down(Align::LEFT)),
    );
    preview.set_clip_rect(ui.ctx().content_rect());
    let ui = &mut preview;
    let theme = Theme::for_appearance(match ui.ctx().theme() {
        egui::Theme::Dark => Appearance::Dark,
        egui::Theme::Light => Appearance::Light,
    });
    crate::theme::install_style(ui.ctx(), &theme);
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, theme.palette.canvas());
    let mut pane = match pane {
        CloudSettingsPreview::Provider => Pane::Provider,
        CloudSettingsPreview::Credentials => Pane::Credentials,
    };
    let mut draft = model.config.clone();
    let mut dirty = false;
    let mut local_error = None;
    let mut access_key_id = String::new();
    let mut secret_access_key = String::new();
    let mut session_token = String::new();
    let mut share_password = String::new();
    let mut events = Vec::new();
    match platform {
        SettingsPlatform::MacOs => {
            let bounds = ui.max_rect();
            let navigation_rect =
                egui::Rect::from_min_size(bounds.min, egui::vec2(bounds.width(), 58.0));
            let body_rect = egui::Rect::from_min_max(
                egui::pos2(bounds.left(), navigation_rect.bottom()),
                bounds.max,
            );
            let mut navigation = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(navigation_rect)
                    .layout(Layout::top_down(Align::LEFT)),
            );
            navigation.set_clip_rect(navigation_rect);
            draw_top_navigation(&mut navigation, &theme, &mut pane);
            let mut body = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(body_rect)
                    .layout(Layout::top_down(Align::LEFT)),
            );
            body.set_clip_rect(body_rect);
            draw_body(
                &mut body,
                &theme,
                pane,
                &mut draft,
                &mut dirty,
                &mut local_error,
                &mut access_key_id,
                &mut secret_access_key,
                &mut session_token,
                &mut share_password,
                model,
                &mut events,
            );
        }
        SettingsPlatform::Windows | SettingsPlatform::Linux => {
            ui.horizontal(|ui| {
                ui.vertical(|ui| draw_side_navigation(ui, &theme, &mut pane));
                ui.separator();
                ui.vertical(|ui| {
                    ui.set_min_width(430.0);
                    draw_body(
                        ui,
                        &theme,
                        pane,
                        &mut draft,
                        &mut dirty,
                        &mut local_error,
                        &mut access_key_id,
                        &mut secret_access_key,
                        &mut session_token,
                        &mut share_password,
                        model,
                        &mut events,
                    );
                });
            });
        }
    }
}

fn draw_top_navigation(ui: &mut egui::Ui, theme: &Theme, pane: &mut Pane) {
    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
        ui.heading(
            RichText::new("Sharing")
                .font(theme.font(Text::Title))
                .color(theme.palette.text),
        );
        ui.add_space(Space::XL);
        ui.selectable_value(pane, Pane::Provider, "Provider");
        ui.selectable_value(pane, Pane::Credentials, "Credentials");
    });
    ui.separator();
}

fn draw_side_navigation(ui: &mut egui::Ui, theme: &Theme, pane: &mut Pane) {
    ui.set_min_width(155.0);
    ui.add_space(Space::MD);
    ui.label(
        RichText::new("Sharing")
            .font(theme.font(Text::Title))
            .color(theme.palette.text),
    );
    ui.add_space(Space::LG);
    ui.selectable_value(pane, Pane::Provider, "Provider");
    ui.selectable_value(pane, Pane::Credentials, "Credentials");
}

#[allow(clippy::too_many_arguments)]
fn draw_body(
    ui: &mut egui::Ui,
    theme: &Theme,
    pane: Pane,
    draft: &mut CloudSettingsDraft,
    dirty: &mut bool,
    local_error: &mut Option<String>,
    access_key_id: &mut String,
    secret_access_key: &mut String,
    session_token: &mut String,
    share_password: &mut String,
    model: &CloudSettingsModel,
    events: &mut Vec<CloudSettingsEvent>,
) {
    egui::Frame::new()
        .inner_margin(egui::Margin::same(Space::XL as i8))
        .show(ui, |ui| match pane {
            Pane::Provider => {
                draw_provider(ui, draft, dirty, local_error, model, events);
            }
            Pane::Credentials => draw_credentials(
                ui,
                *dirty,
                access_key_id,
                secret_access_key,
                session_token,
                share_password,
                local_error,
                model,
                events,
            ),
        });
    if let Some(error) = local_error {
        ui.add_space(Space::SM);
        ui.colored_label(egui::Color32::from_rgb(190, 90, 70), error.as_str());
    }
}

fn draw_provider(
    ui: &mut egui::Ui,
    draft: &mut CloudSettingsDraft,
    dirty: &mut bool,
    local_error: &mut Option<String>,
    model: &CloudSettingsModel,
    events: &mut Vec<CloudSettingsEvent>,
) {
    ui.heading("Provider");
    ui.label("Non-secret values are stored in Scrozz settings. Credentials never are.");
    ui.add_space(Space::MD);
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("scrozz-cloud-provider-grid")
            .num_columns(2)
            .spacing([Space::MD, Space::SM])
            .show(ui, |ui| {
                field_label(ui, "Provider");
                let before = draft.provider.clone();
                egui::ComboBox::from_id_salt("cloud-provider")
                    .selected_text(provider_label(&draft.provider))
                    .show_ui(ui, |ui| {
                        for (slug, label) in [
                            ("aws", "Amazon S3"),
                            ("r2", "Cloudflare R2"),
                            ("b2", "Backblaze B2"),
                            ("minio", "MinIO / S3-compatible"),
                        ] {
                            ui.selectable_value(&mut draft.provider, slug.to_owned(), label);
                        }
                    });
                *dirty |= before != draft.provider;
                ui.end_row();

                text_row(ui, "Bucket / container", &mut draft.bucket, dirty, false);
                text_row(ui, "Region", &mut draft.region, dirty, false);
                text_row(ui, "Endpoint", &mut draft.endpoint, dirty, false);
                if draft.provider == "r2" {
                    text_row(ui, "R2 account id", &mut draft.account_id, dirty, false);
                }
                text_row(ui, "Object prefix", &mut draft.prefix, dirty, false);
                text_row(
                    ui,
                    "Public / base URL",
                    &mut draft.public_base_url,
                    dirty,
                    false,
                );

                field_label(ui, "Link policy");
                let before = draft.url_policy.clone();
                egui::ComboBox::from_id_salt("cloud-url-policy")
                    .selected_text(if draft.url_policy == "public-base" {
                        "Public base URL"
                    } else {
                        "Private expiring link"
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut draft.url_policy,
                            "private-expiring".to_owned(),
                            "Private expiring link",
                        );
                        ui.selectable_value(
                            &mut draft.url_policy,
                            "public-base".to_owned(),
                            "Public base URL",
                        );
                    });
                *dirty |= before != draft.url_policy;
                ui.end_row();

                if draft.url_policy == "private-expiring" {
                    field_label(ui, "Default expiry (seconds)");
                    *dirty |= ui
                        .add(
                            egui::DragValue::new(&mut draft.expiry_seconds)
                                .range(1..=604_800)
                                .speed(60),
                        )
                        .changed();
                    ui.end_row();
                }
                text_row(
                    ui,
                    "Naming template",
                    &mut draft.naming_template,
                    dirty,
                    false,
                );
                text_row(ui, "Default tags", &mut draft.tags, dirty, false);

                field_label(ui, "Password protection");
                let before = draft.protection_mode.clone();
                egui::ComboBox::from_id_salt("cloud-protection")
                    .selected_text(if draft.protection_mode == "vault" {
                        "Use vault password"
                    } else {
                        "Off"
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut draft.protection_mode, "none".to_owned(), "Off");
                        ui.selectable_value(
                            &mut draft.protection_mode,
                            "vault".to_owned(),
                            "Use vault password",
                        );
                    });
                *dirty |= before != draft.protection_mode;
                ui.end_row();

                text_row(ui, "Viewer title", &mut draft.viewer_title, dirty, false);
                text_row(ui, "Viewer accent", &mut draft.viewer_accent, dirty, false);
            });
    });
    ui.add_space(Space::MD);
    if let Some(reason) = &model.unavailable_reason {
        ui.colored_label(egui::Color32::from_rgb(190, 90, 70), reason);
    }
    ui.horizontal(|ui| {
        if ui.add_enabled(*dirty, egui::Button::new("Save")).clicked() {
            *local_error = validate_draft(draft).err();
            if local_error.is_none() {
                events.push(CloudSettingsEvent::Save(Box::new(draft.clone())));
                *dirty = false;
            }
        }
        if ui
            .add_enabled(
                connection_test_enabled(*dirty, model.upload_enabled, &model.connection),
                egui::Button::new("Test connection"),
            )
            .clicked()
        {
            events.push(CloudSettingsEvent::TestConnection);
        }

        if *dirty {
            ui.label("Save changes before testing.");
        } else {
            draw_connection(ui, &model.connection);
        }
    });
}

fn connection_test_enabled(
    dirty: bool,
    upload_enabled: bool,
    state: &CloudConnectionState,
) -> bool {
    !dirty && upload_enabled && !matches!(state, CloudConnectionState::Testing)
}

#[allow(clippy::too_many_arguments)]
fn draw_credentials(
    ui: &mut egui::Ui,
    provider_dirty: bool,
    access_key_id: &mut String,
    secret_access_key: &mut String,
    session_token: &mut String,
    share_password: &mut String,
    local_error: &mut Option<String>,
    model: &CloudSettingsModel,
    events: &mut Vec<CloudSettingsEvent>,
) {
    ui.heading("Credentials");
    if provider_dirty {
        ui.colored_label(
            egui::Color32::from_rgb(190, 90, 70),
            "Save Provider changes before editing that provider's credentials.",
        );
        return;
    }
    ui.label(format!(
        "{}: {}",
        model.credentials.backend,
        if model.credentials.stored {
            "credential entry stored"
        } else {
            "no credential entry stored"
        }
    ));
    if let Some(problem) = &model.credentials.problem {
        ui.colored_label(egui::Color32::from_rgb(190, 90, 70), problem);
    }
    ui.add_space(Space::MD);
    egui::Grid::new("scrozz-cloud-credentials-grid")
        .num_columns(2)
        .spacing([Space::MD, Space::SM])
        .show(ui, |ui| {
            text_row(ui, "Access key id", access_key_id, &mut false, true);
            text_row(ui, "Secret access key", secret_access_key, &mut false, true);
            text_row(ui, "Session token", session_token, &mut false, true);
            text_row(
                ui,
                "Default share password",
                share_password,
                &mut false,
                true,
            );
        });
    ui.label(
        "Leave session token and share password empty when unused. Updating requires the complete access key and secret.",
    );
    ui.add_space(Space::MD);
    ui.horizontal(|ui| {
        if ui.button("Store / update").clicked() {
            if access_key_id.trim().is_empty() || secret_access_key.is_empty() {
                *local_error =
                    Some("Access key id and secret access key are both required.".to_owned());
            } else {
                events.push(CloudSettingsEvent::StoreCredentials(CredentialDraft {
                    access_key_id: std::mem::take(access_key_id),
                    secret_access_key: std::mem::take(secret_access_key),
                    session_token: std::mem::take(session_token),
                    share_password: std::mem::take(share_password),
                }));
                *local_error = None;
            }
        }
        if ui
            .add_enabled(
                model.credentials.stored,
                egui::Button::new("Remove from vault"),
            )
            .clicked()
        {
            events.push(CloudSettingsEvent::RemoveCredentials);
        }
    });
}

fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String, dirty: &mut bool, password: bool) {
    ui.label(label);
    let mut edit = TextEdit::singleline(value).desired_width(300.0);
    if password {
        edit = edit.password(true);
    }
    *dirty |= ui.add(edit).changed();
    ui.end_row();
}

fn field_label(ui: &mut egui::Ui, label: &str) {
    ui.label(label);
}

fn provider_label(provider: &str) -> &str {
    match provider {
        "aws" => "Amazon S3",
        "r2" => "Cloudflare R2",
        "b2" => "Backblaze B2",
        "minio" => "MinIO / S3-compatible",
        _ => "Unknown provider",
    }
}

fn validate_draft(draft: &CloudSettingsDraft) -> Result<(), String> {
    if draft.bucket.trim().is_empty() {
        return Err("A bucket or container is required before sharing can be enabled.".to_owned());
    }
    if draft.provider == "minio" && draft.endpoint.trim().is_empty() {
        return Err("MinIO requires an explicit HTTPS endpoint.".to_owned());
    }
    if draft.provider == "b2" && draft.region.trim().is_empty() {
        return Err("Backblaze B2 requires its bucket region.".to_owned());
    }
    if draft.provider == "r2"
        && draft.account_id.trim().is_empty()
        && draft.endpoint.trim().is_empty()
    {
        return Err("Cloudflare R2 requires an account id or explicit endpoint.".to_owned());
    }
    if draft.url_policy == "public-base" && draft.public_base_url.trim().is_empty() {
        return Err("Public-link policy requires a public / base URL.".to_owned());
    }
    if !draft.viewer_accent.starts_with('#')
        || draft.viewer_accent.len() != 7
        || !draft.viewer_accent[1..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Viewer accent must be a six-digit color such as #f05a28.".to_owned());
    }
    Ok(())
}

fn draw_connection(ui: &mut egui::Ui, state: &CloudConnectionState) {
    match state {
        CloudConnectionState::Idle => {}
        CloudConnectionState::Testing => {
            ui.spinner();
            ui.label("Testing...");
        }
        CloudConnectionState::Passed => {
            ui.colored_label(egui::Color32::from_rgb(40, 150, 90), "Connected");
        }
        CloudConnectionState::Failed(reason) => {
            ui.colored_label(egui::Color32::from_rgb(190, 90, 70), reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> CloudSettingsDraft {
        CloudSettingsDraft {
            provider: "aws".to_owned(),
            bucket: "shots".to_owned(),
            region: "us-east-1".to_owned(),
            endpoint: String::new(),
            account_id: String::new(),
            prefix: "captures".to_owned(),
            public_base_url: String::new(),
            url_policy: "private-expiring".to_owned(),
            expiry_seconds: 86_400,
            naming_template: "Screenshot-{timestamp}".to_owned(),
            tags: String::new(),
            protection_mode: "none".to_owned(),
            viewer_title: "Scrozz share".to_owned(),
            viewer_accent: "#f05a28".to_owned(),
        }
    }

    #[test]
    fn platform_navigation_matches_desktop_conventions() {
        assert_eq!(SettingsPlatform::MacOs, SettingsPlatform::MacOs);
        assert_ne!(SettingsPlatform::Windows, SettingsPlatform::Linux);
    }

    #[test]
    fn provider_specific_requirements_are_clear() {
        let mut value = draft();
        value.provider = "minio".to_owned();
        value.endpoint.clear();
        assert!(validate_draft(&value).unwrap_err().contains("MinIO"));
    }

    #[test]
    fn credential_debug_is_redacted() {
        let value = CredentialDraft {
            access_key_id: "AKIA-SENTINEL-7F2".to_owned(),
            secret_access_key: "s3-value-sentinel-91A".to_owned(),
            session_token: "session-value-sentinel-44C".to_owned(),
            share_password: "viewer-value-sentinel-08D".to_owned(),
        };
        let rendered = format!("{value:?}");
        for secret in [
            "AKIA-SENTINEL-7F2",
            "s3-value-sentinel-91A",
            "session-value-sentinel-44C",
            "viewer-value-sentinel-08D",
        ] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }

    #[test]
    fn connection_test_never_uses_an_unsaved_draft() {
        assert!(!connection_test_enabled(
            true,
            true,
            &CloudConnectionState::Idle
        ));
        assert!(connection_test_enabled(
            false,
            true,
            &CloudConnectionState::Idle
        ));
    }
}
