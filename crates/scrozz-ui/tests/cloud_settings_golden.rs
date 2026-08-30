//! Generated cloud Settings quality checks.

use scrozz_ui::{
    CloudConnectionState, CloudCredentialView, CloudSettingsDraft, CloudSettingsModel,
    CloudSettingsPreview, SettingsPlatform,
    cloud_settings::render_preview,
    harness::{
        Background, GoldenStore, Profile, RenderSpec, Scenario, Scene, SceneCtx, SceneRegistry,
        SoftwareRenderer, VirtualClock, default_snapshot_dir,
    },
    theme,
};

const SURFACE: (f32, f32) = (760.0, 650.0);

struct SettingsScene {
    platform: SettingsPlatform,
    pane: CloudSettingsPreview,
    model: CloudSettingsModel,
}

impl Scene for SettingsScene {
    fn name(&self) -> &str {
        "cloud-settings"
    }

    fn setup(&self, ctx: &egui::Context) {
        theme::install_fonts(ctx);
    }

    fn ui(&self, ui: &mut egui::Ui, _ctx: &SceneCtx<'_>) {
        render_preview(ui, self.platform, self.pane, &self.model);
    }
}

fn model(configured: bool) -> CloudSettingsModel {
    CloudSettingsModel {
        config: CloudSettingsDraft {
            provider: "minio".to_owned(),
            bucket: if configured { "team-captures" } else { "" }.to_owned(),
            region: "us-east-1".to_owned(),
            endpoint: "https://screenshots.example.net".to_owned(),
            account_id: String::new(),
            prefix: "scrozz/{kind}".to_owned(),
            public_base_url: "https://share.example.net".to_owned(),
            url_policy: "private-expiring".to_owned(),
            expiry_seconds: 86_400,
            naming_template: "{kind}-{timestamp}".to_owned(),
            tags: "team=design,source=scrozz".to_owned(),
            protection_mode: "vault".to_owned(),
            viewer_title: "Design review".to_owned(),
            viewer_accent: "#7c3aed".to_owned(),
        },
        credentials: CloudCredentialView {
            backend: if configured {
                "macOS Keychain"
            } else {
                "Linux Secret Service"
            }
            .to_owned(),
            stored: configured,
            problem: (!configured).then(|| "No Secret Service session is available.".to_owned()),
        },
        upload_enabled: configured,
        unavailable_reason: (!configured)
            .then(|| "Add a bucket and credentials to enable Upload.".to_owned()),
        connection: if configured {
            CloudConnectionState::Passed
        } else {
            CloudConnectionState::Idle
        },
    }
}

fn render(scene: SettingsScene, theme: egui::Theme) -> scrozz_ui::harness::Image {
    let mut registry = SceneRegistry::empty();
    registry.register(Scenario::EditorAnnotating, Box::new(scene));
    let renderer = SoftwareRenderer::new(registry);
    let mut spec = RenderSpec::golden(Scenario::EditorAnnotating, VirtualClock::ZERO);
    spec.profile = Profile::Golden;
    spec.size_pt = Some(SURFACE);
    spec.pixels_per_point = 1.0;
    spec.theme = theme;
    spec.background = Background::Transparent;
    renderer.render(&spec).expect("render cloud Settings")
}

#[test]
fn cloud_settings_match_generated_goldens() {
    let cases = [
        (
            "settings-cloud-provider--light",
            SettingsScene {
                platform: SettingsPlatform::MacOs,
                pane: CloudSettingsPreview::Provider,
                model: model(true),
            },
            egui::Theme::Light,
        ),
        (
            "settings-cloud-credentials-unavailable--dark",
            SettingsScene {
                platform: SettingsPlatform::Linux,
                pane: CloudSettingsPreview::Credentials,
                model: model(false),
            },
            egui::Theme::Dark,
        ),
    ];
    let store = GoldenStore::new(default_snapshot_dir().join("golden"));
    for (name, scene, theme) in cases {
        let outcome = store.compare(name, &render(scene, theme)).expect("compare");
        assert!(!outcome.is_failure(), "{name} changed: {outcome:?}");
    }
}
