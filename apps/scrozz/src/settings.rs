//! The settings schema.
//!
//! # Why the schema lives here and not in the store
//!
//! A key's name, type and default are what the GUI renders, what `--json`
//! reports, and what a user's dotfiles refer to. The values live in the
//! versioned, atomically replaced document owned by [`crate::after_capture`];
//! shortcuts retain their dedicated store because applying one can fail at the
//! operating-system registration boundary.
//!
//! Recording interaction options are durable for the same reason: the settings
//! surface and the recording engine need one authoritative policy. Unknown or
//! malformed state is an error rather than a reason to silently enable an
//! input-monitoring feature.
//!
//! There is exactly **one** settings document. Private sharing does not add a
//! second one: [`SettingsStore`] is a view over the same file, so a cloud
//! write preserves the Recording, Camera, Recent Captures and After Capture
//! sections beside it — and any section a future version adds. That document
//! stores only validated, **non-secret** values; a credential-bearing key is
//! refused rather than written, because secrets belong in the platform vault.
//!
//! # Naming
//!
//! `area.key-name`: a dotted area, hyphenated words. It matches the action slugs
//! the hotkey commands already use, and it survives being a TOML table, a JSON
//! object path and a command-line argument without being quoted.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use crate::{
    after_capture::{AfterCaptureAction, AfterCaptureSettings, AfterCaptureStore, MediaKind},
    fault::{CliError, CliResult},
    hotkey_config::Accelerator,
    json::Json,
    shortcuts::{ShortcutAction, ShortcutStore, Shortcuts},
};
use scrozz_core::CursorMode;
use scrozz_record::{
    CameraDeviceId, RecordingSettings,
    settings::{
        CameraPlacement, CameraShape, ClickStyle, KeystrokeScope, OverlayAnchor, OverlaySize,
        OverlayTheme, Rgba8,
    },
};
use scrozz_shell::ScreenshotSound;

/// Legacy opt-in transform, superseded by `scenes.default`.
///
/// Retained only so an existing document can be migrated; nothing reads it to
/// decide behaviour any more.
pub const APPLY_SMART_FRAME_AFTER_CAPTURE_KEY: &str = "after-capture.apply-smart-frame";
/// The Scene applied wherever a capture type does not name its own.
pub const SCENES_DEFAULT_KEY: &str = "scenes.default";
/// Window drop shadow fidelity for window captures.
pub const WINDOW_SHADOW_KEY: &str = "capture.window-shadow";
pub const RECENT_CAPTURES_OVERLAY_PLACEMENT_KEY: &str = "recent-captures-overlay.placement";
pub const RECENT_CAPTURES_OVERLAY_FOLLOW_ACTIVE_DISPLAY_KEY: &str =
    "recent-captures-overlay.follow-active-display";
pub const RECENT_CAPTURES_OVERLAY_CARD_WIDTH_KEY: &str = "recent-captures-overlay.card-width";
pub const RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_ENABLED_KEY: &str =
    "recent-captures-overlay.auto-close-enabled";
pub const RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_ACTION_KEY: &str =
    "recent-captures-overlay.auto-close-action";
pub const RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_SECONDS_KEY: &str =
    "recent-captures-overlay.auto-close-seconds";
pub const RECENT_CAPTURES_OVERLAY_CLOSE_AFTER_DRAG_KEY: &str =
    "recent-captures-overlay.close-after-drag";
pub const RECENT_CAPTURES_OVERLAY_CLOSE_AFTER_UPLOAD_KEY: &str =
    "recent-captures-overlay.close-after-upload";
pub const RECENT_CAPTURES_OVERLAY_SAVE_BUTTON_KEY: &str = "recent-captures-overlay.save-button";

const RECORDING_CURSOR_KEY: &str = "record.cursor";
const RECORDING_CURSOR_SMOOTHING_KEY: &str = "record.cursor-smoothing";
const RECORDING_CLICKS_KEY: &str = "record.highlight-clicks";
const RECORDING_CLICK_COLOR_KEY: &str = "record.click-color";
const RECORDING_CLICK_SIZE_KEY: &str = "record.click-size";
const RECORDING_CLICK_STYLE_KEY: &str = "record.click-style";
const RECORDING_CLICK_ANIMATION_KEY: &str = "record.click-animation";
const RECORDING_KEYS_KEY: &str = "record.show-keystrokes";
const RECORDING_KEY_SCOPE_KEY: &str = "record.keystroke-scope";
const RECORDING_KEY_POSITION_KEY: &str = "record.keystroke-position";
const RECORDING_KEY_SIZE_KEY: &str = "record.keystroke-size";
const RECORDING_KEY_THEME_KEY: &str = "record.keystroke-theme";
const CAMERA_ENABLED_KEY: &str = "record.camera";
const CAMERA_DEVICE_KEY: &str = "record.camera-device";
const CAMERA_POSITION_KEY: &str = "record.camera-position";
const CAMERA_PLACEMENT_X_KEY: &str = "record.camera-placement-x";
const CAMERA_PLACEMENT_Y_KEY: &str = "record.camera-placement-y";
const CAMERA_SIZE_KEY: &str = "record.camera-size";
const CAMERA_SHAPE_KEY: &str = "record.camera-shape";
const CAMERA_PRESENTER_KEY: &str = "record.camera-presenter";
const CAMERA_PRESENTER_SCREEN_KEY: &str = "record.camera-presenter-screen";
const CAMERA_MIRROR_KEY: &str = "record.camera-mirror";
const CAMERA_BORDER_KEY: &str = "record.camera-border";
const CAMERA_SHADOW_KEY: &str = "record.camera-shadow";

/// What a setting accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `true` or `false`.
    Bool,
    /// A bounded integer.
    Int {
        /// Smallest accepted value.
        min: i64,
        /// Largest accepted value.
        max: i64,
    },
    /// A filesystem path. Not required to exist: a default folder on a
    /// not-yet-mounted volume is a legitimate thing to configure.
    Path,
    /// A filesystem path that may be empty while its companion feature is off.
    OptionalPath,
    /// One of a fixed set of strings.
    Choice(&'static [&'static str]),
    /// A key combination, validated by the same parser the hotkey commands use.
    Accelerator,
    /// An RGB or RGBA hexadecimal color.
    Color,
    /// Free text. Empty is permitted where it means "not configured".
    Text {
        /// Whether an empty value is meaningful.
        allow_empty: bool,
    },
}

impl Kind {
    /// The type name reported in JSON.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int { .. } => "int",
            Self::Path => "path",
            Self::OptionalPath => "optional-path",
            Self::Choice(_) => "choice",
            Self::Accelerator => "accelerator",
            Self::Color => "color",
            Self::Text { .. } => "text",
        }
    }
}

/// One setting.
#[derive(Debug, Clone, Copy)]
pub struct Setting {
    /// The dotted key.
    pub key: &'static str,
    /// What it accepts.
    pub kind: Kind,
    /// The value in force when nothing has been set.
    pub default: &'static str,
    /// One line, as a GUI would show under the control.
    pub description: &'static str,
}

impl Setting {
    /// The JSON representation of the schema default.
    #[must_use]
    pub fn to_json(self) -> Json {
        self.to_json_valued(self.default, "default")
    }

    /// The JSON representation with a resolved value and its provenance.
    ///
    /// Split from [`Setting::to_json`] rather than replacing it because the
    /// schema and the current state are two different questions: `--defaults`
    /// wants the former, and a script asking what is in force wants the latter.
    #[must_use]
    pub fn to_json_valued(self, value: &str, source: &str) -> Json {
        let mut fields = vec![
            ("key", Json::str(self.key)),
            ("type", Json::str(self.kind.slug())),
            ("value", Json::str(value)),
            ("default", Json::str(self.default)),
            // Honest about where the value came from: "user" once something has
            // been stored, so a script can tell a deliberate choice from a
            // default that may move in a later release.
            ("source", Json::str(source)),
            ("description", Json::str(self.description)),
        ];
        if let Kind::Choice(options) = self.kind {
            fields.push(("choices", Json::arr(options.iter().map(|o| Json::str(*o)))));
        }
        if let Kind::Int { min, max } = self.kind {
            fields.push(("minimum", Json::Int(min)));
            fields.push(("maximum", Json::Int(max)));
        }
        Json::Obj(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    /// Checks a candidate value.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] describing what was expected. The message
    /// names the accepted values rather than merely saying "invalid", because a
    /// user who mistyped a setting name has no other way to discover them.
    pub fn validate(&self, value: &str) -> CliResult<()> {
        match self.kind {
            Kind::Bool => match value {
                "true" | "false" => Ok(()),
                other => Err(CliError::usage(format!(
                    "{} takes `true` or `false`, not {other:?}",
                    self.key
                ))),
            },
            Kind::Int { min, max } => {
                let parsed: i64 = value.parse().map_err(|_| {
                    CliError::usage(format!(
                        "{} takes a whole number between {min} and {max}, not {value:?}",
                        self.key
                    ))
                })?;
                if (min..=max).contains(&parsed) {
                    Ok(())
                } else {
                    Err(CliError::usage(format!(
                        "{} must be between {min} and {max}; {parsed} is outside that",
                        self.key
                    )))
                }
            }
            Kind::Path => {
                if value.trim().is_empty() {
                    Err(CliError::usage(format!("{} cannot be empty", self.key)))
                } else {
                    Ok(())
                }
            }
            Kind::OptionalPath => Ok(()),
            Kind::Choice(options) => {
                if options.contains(&value) {
                    Ok(())
                } else {
                    Err(CliError::usage(format!(
                        "{} takes one of {}, not {value:?}",
                        self.key,
                        options.join(", ")
                    )))
                }
            }
            Kind::Accelerator => {
                // An empty accelerator is the recorded decision to have no
                // shortcut for something, which is a real answer and not a
                // missing one. Rejecting it would leave a user who wants an
                // action unbound with nothing to type.
                if value.trim().is_empty() {
                    Ok(())
                } else {
                    Accelerator::parse(value).map(|_| ())
                }
            }
            Kind::Color => Rgba8::parse(value).map(|_| ()).map_err(CliError::Core),
            Kind::Text { allow_empty } => {
                if !allow_empty && value.trim().is_empty() {
                    Err(CliError::usage(format!("{} cannot be empty", self.key)))
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// Loads every persisted recording preference.
///
/// Interaction defaults are deliberately permission-free: pointer visible,
/// click/key capture disabled, and modifiers-only if the key display is enabled.
///
/// # Errors
///
/// Returns a storage error when the settings document is unreadable, or a usage
/// error when a persisted interaction value is not one this build declares.
pub fn load_recording_settings() -> CliResult<RecordingSettings> {
    let store = AfterCaptureStore::default_location().map_err(CliError::Core)?;
    let profile = store.inferred_profile();
    let persisted = store.load(profile).map_err(CliError::Core)?;
    recording_settings_from(&persisted)
}

/// Resolves recording settings from an already-loaded settings document.
///
/// # Errors
///
/// As [`load_recording_settings`]. A persisted value that this build cannot
/// parse is an error rather than a silent fall back to a default, because
/// falling back could quietly turn an input-monitoring feature on or off.
pub fn recording_settings_from(persisted: &AfterCaptureSettings) -> CliResult<RecordingSettings> {
    let shortcuts = Shortcuts::default();
    let value =
        |key: &str| -> CliResult<String> { Ok(resolve(lookup(key)?, &shortcuts, persisted).0) };
    let mut settings = RecordingSettings::shipped();
    settings.after_capture = persisted.recording_policy();
    settings.cursor = if parse_bool(RECORDING_CURSOR_KEY, &value(RECORDING_CURSOR_KEY)?)? {
        CursorMode::Visible
    } else {
        CursorMode::Hidden
    };
    settings.cursor_smoothing = parse_bool(
        RECORDING_CURSOR_SMOOTHING_KEY,
        &value(RECORDING_CURSOR_SMOOTHING_KEY)?,
    )?;
    settings.clicks.enabled = parse_bool(RECORDING_CLICKS_KEY, &value(RECORDING_CLICKS_KEY)?)?;
    settings.clicks.color =
        Rgba8::parse(&value(RECORDING_CLICK_COLOR_KEY)?).map_err(CliError::Core)?;
    settings.clicks.size =
        OverlaySize::from_slug(&value(RECORDING_CLICK_SIZE_KEY)?).map_err(CliError::Core)?;
    settings.clicks.style =
        ClickStyle::from_slug(&value(RECORDING_CLICK_STYLE_KEY)?).map_err(CliError::Core)?;
    settings.clicks.animate = parse_bool(
        RECORDING_CLICK_ANIMATION_KEY,
        &value(RECORDING_CLICK_ANIMATION_KEY)?,
    )?;
    settings.keystrokes.enabled = parse_bool(RECORDING_KEYS_KEY, &value(RECORDING_KEYS_KEY)?)?;
    settings.keystrokes.scope =
        KeystrokeScope::from_slug(&value(RECORDING_KEY_SCOPE_KEY)?).map_err(CliError::Core)?;
    settings.keystrokes.position =
        OverlayAnchor::from_slug(&value(RECORDING_KEY_POSITION_KEY)?).map_err(CliError::Core)?;
    settings.keystrokes.size =
        OverlaySize::from_slug(&value(RECORDING_KEY_SIZE_KEY)?).map_err(CliError::Core)?;
    settings.keystrokes.theme =
        OverlayTheme::from_slug(&value(RECORDING_KEY_THEME_KEY)?).map_err(CliError::Core)?;
    settings.camera.enabled = parse_bool(CAMERA_ENABLED_KEY, &value(CAMERA_ENABLED_KEY)?)?;
    settings.camera.position =
        OverlayAnchor::from_slug(&value(CAMERA_POSITION_KEY)?).map_err(CliError::Core)?;
    settings.camera.placement = camera_placement(
        parse_i64(CAMERA_PLACEMENT_X_KEY, &value(CAMERA_PLACEMENT_X_KEY)?)?,
        parse_i64(CAMERA_PLACEMENT_Y_KEY, &value(CAMERA_PLACEMENT_Y_KEY)?)?,
    )?;
    settings.camera.size = parse_i64(CAMERA_SIZE_KEY, &value(CAMERA_SIZE_KEY)?)? as f32 / 100.0;
    settings.camera.shape =
        CameraShape::from_slug(&value(CAMERA_SHAPE_KEY)?).map_err(CliError::Core)?;
    settings.camera.presenter = parse_bool(CAMERA_PRESENTER_KEY, &value(CAMERA_PRESENTER_KEY)?)?;
    settings.camera.presenter_screen = parse_bool(
        CAMERA_PRESENTER_SCREEN_KEY,
        &value(CAMERA_PRESENTER_SCREEN_KEY)?,
    )?;
    settings.camera.mirror = parse_bool(CAMERA_MIRROR_KEY, &value(CAMERA_MIRROR_KEY)?)?;
    settings.camera.border = parse_bool(CAMERA_BORDER_KEY, &value(CAMERA_BORDER_KEY)?)?;
    settings.camera.shadow = parse_bool(CAMERA_SHADOW_KEY, &value(CAMERA_SHADOW_KEY)?)?;
    settings.validate().map_err(CliError::Core)
}

/// Reads the stable camera device preference from a loaded settings document.
///
/// `None` means "whatever the platform calls the default camera": a persisted
/// id is a platform identifier that survives reboots, never a native handle.
///
/// # Errors
///
/// Returns a usage error when the persisted identifier is malformed.
pub fn camera_device_from(persisted: &AfterCaptureSettings) -> CliResult<Option<CameraDeviceId>> {
    let shortcuts = Shortcuts::default();
    let value = resolve(lookup(CAMERA_DEVICE_KEY)?, &shortcuts, persisted).0;
    if value == "default" {
        return Ok(None);
    }
    CameraDeviceId::new(value).map(Some).map_err(CliError::Core)
}

/// Writes the stable camera device preference into a settings document.
pub fn apply_camera_device(persisted: &mut AfterCaptureSettings, device: Option<&CameraDeviceId>) {
    persisted.set_value(
        CAMERA_DEVICE_KEY,
        device.map_or("default", CameraDeviceId::as_str),
    );
}

/// Rebuilds free camera placement from its two persisted thousandths.
///
/// Both coordinates are `-1` when no drag has happened, so a half-written pair
/// is a real error rather than a silently re-anchored camera.
fn camera_placement(x: i64, y: i64) -> CliResult<Option<CameraPlacement>> {
    match (x, y) {
        (-1, -1) => Ok(None),
        (x @ 0..=1_000, y @ 0..=1_000) => {
            CameraPlacement::new(x as f32 / 1_000.0, y as f32 / 1_000.0)
                .map(Some)
                .map_err(CliError::Core)
        }
        _ => Err(CliError::usage(format!(
            "{CAMERA_PLACEMENT_X_KEY} and {CAMERA_PLACEMENT_Y_KEY} must both be -1 or both be 0..=1000"
        ))),
    }
}

/// Atomically persists every recording setting represented by the settings UI.
///
/// Only the keys this surface owns are written; every other value, including
/// state written by a newer Scrozz, survives the update untouched. The updated
/// document is returned so a caller holding one can refresh it without a second
/// read that another process could win.
///
/// The store is passed in rather than resolved here so the GUI writes to the
/// same document it reads its After Capture policy from, even in a run
/// configured with a non-default location.
///
/// # Errors
///
/// Returns a usage error for settings that fail validation, or a storage error
/// when the document cannot be read or atomically replaced.
pub fn save_recording_settings(
    store: &AfterCaptureStore,
    settings: RecordingSettings,
) -> CliResult<AfterCaptureSettings> {
    settings.validate().map_err(CliError::Core)?;
    let profile = store.inferred_profile();
    store
        .update(profile, |persisted| {
            apply_recording_settings(persisted, settings);
            Ok(())
        })
        .map_err(CliError::Core)
}

/// Atomically persists the camera composition and its stable device choice.
///
/// One update rather than two so a crash between them cannot leave a device
/// selected for a composition that was never written.
///
/// # Errors
///
/// Returns a usage error for settings that fail validation, or a storage error
/// when the document cannot be read or atomically replaced.
pub fn save_camera_preferences(
    store: &AfterCaptureStore,
    settings: RecordingSettings,
    device: Option<&CameraDeviceId>,
) -> CliResult<AfterCaptureSettings> {
    settings.validate().map_err(CliError::Core)?;
    let profile = store.inferred_profile();
    store
        .update(profile, |persisted| {
            apply_recording_settings(persisted, settings);
            apply_camera_device(persisted, device);
            Ok(())
        })
        .map_err(CliError::Core)
}

/// Writes every owned recording key into a settings document.
fn apply_recording_settings(persisted: &mut AfterCaptureSettings, settings: RecordingSettings) {
    persisted.set(
        MediaKind::Recording,
        AfterCaptureAction::ShowRecentCapturesOverlay,
        settings.after_capture.recent_captures_overlay,
    );
    persisted.set(
        MediaKind::Recording,
        AfterCaptureAction::OpenEditor,
        settings.after_capture.open_editor,
    );
    for (key, value) in [
        (RECORDING_CURSOR_KEY, settings.shows_cursor().to_string()),
        (
            RECORDING_CURSOR_SMOOTHING_KEY,
            settings.cursor_smoothing.to_string(),
        ),
        (RECORDING_CLICKS_KEY, settings.clicks.enabled.to_string()),
        (RECORDING_CLICK_COLOR_KEY, settings.clicks.color.to_hex()),
        (
            RECORDING_CLICK_SIZE_KEY,
            settings.clicks.size.slug().to_owned(),
        ),
        (
            RECORDING_CLICK_STYLE_KEY,
            settings.clicks.style.slug().to_owned(),
        ),
        (
            RECORDING_CLICK_ANIMATION_KEY,
            settings.clicks.animate.to_string(),
        ),
        (RECORDING_KEYS_KEY, settings.keystrokes.enabled.to_string()),
        (
            RECORDING_KEY_SCOPE_KEY,
            settings.keystrokes.scope.slug().to_owned(),
        ),
        (
            RECORDING_KEY_POSITION_KEY,
            settings.keystrokes.position.slug().to_owned(),
        ),
        (
            RECORDING_KEY_SIZE_KEY,
            settings.keystrokes.size.slug().to_owned(),
        ),
        (
            RECORDING_KEY_THEME_KEY,
            settings.keystrokes.theme.slug().to_owned(),
        ),
        (CAMERA_ENABLED_KEY, settings.camera.enabled.to_string()),
        (
            CAMERA_POSITION_KEY,
            settings.camera.position.slug().to_owned(),
        ),
        (
            CAMERA_PLACEMENT_X_KEY,
            settings
                .camera
                .placement
                .map_or(-1, |placement| (placement.x * 1_000.0).round() as i64)
                .to_string(),
        ),
        (
            CAMERA_PLACEMENT_Y_KEY,
            settings
                .camera
                .placement
                .map_or(-1, |placement| (placement.y * 1_000.0).round() as i64)
                .to_string(),
        ),
        (
            CAMERA_SIZE_KEY,
            ((settings.camera.size * 100.0).round() as i64).to_string(),
        ),
        (CAMERA_SHAPE_KEY, settings.camera.shape.slug().to_owned()),
        (CAMERA_PRESENTER_KEY, settings.camera.presenter.to_string()),
        (
            CAMERA_PRESENTER_SCREEN_KEY,
            settings.camera.presenter_screen.to_string(),
        ),
        (CAMERA_MIRROR_KEY, settings.camera.mirror.to_string()),
        (CAMERA_BORDER_KEY, settings.camera.border.to_string()),
        (CAMERA_SHADOW_KEY, settings.camera.shadow.to_string()),
    ] {
        persisted.set_value(key, value);
    }
}

fn parse_i64(key: &str, value: &str) -> CliResult<i64> {
    value
        .parse()
        .map_err(|_| CliError::usage(format!("{key} must be a whole number, not {value:?}")))
}

fn parse_bool(key: &str, value: &str) -> CliResult<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(CliError::usage(format!(
            "{key} must be `true` or `false`, not {other:?}"
        ))),
    }
}

/// Every setting, in the order `settings get` reports them.
///
/// Grouped by area and stable: a script that diffs this output should see a
/// change only when the schema really changed.
pub const SETTINGS: &[Setting] = &[
    Setting {
        key: "capture.folder",
        kind: Kind::Path,
        default: "~/Pictures/Scrozz",
        description: "Where captures are saved when no output path is given.",
    },
    Setting {
        key: "capture.format",
        kind: Kind::Choice(&["png", "jpeg", "webp"]),
        default: "png",
        description: "Default image format.",
    },
    Setting {
        key: "capture.quality",
        kind: Kind::Int { min: 1, max: 100 },
        default: "90",
        description: "Encoder quality for lossy formats. Ignored for PNG.",
    },
    Setting {
        key: "capture.cursor",
        kind: Kind::Bool,
        default: "false",
        description: "Include the mouse pointer in captures.",
    },
    Setting {
        key: "capture.crosshair-mode",
        kind: Kind::Choice(&["off", "modifier", "always"]),
        default: "off",
        description: "Show guides and a pixel loupe never, while holding the primary modifier, or always.",
    },
    Setting {
        key: "capture.freeze-screen",
        kind: Kind::Bool,
        default: "false",
        description: "Freeze screen contents while choosing a region or display.",
    },
    Setting {
        key: "capture.dimension-label",
        kind: Kind::Choice(&["logical", "output-pixels", "both"]),
        default: "logical",
        description: "Show logical 1x dimensions, output pixels, or both while selecting.",
    },
    Setting {
        key: "capture.retina-output",
        kind: Kind::Choice(&["native", "1x"]),
        default: "native",
        description: "Keep native Retina resolution or scale still images to logical 1x output.",
    },
    Setting {
        key: "capture.screenshot-sound",
        kind: Kind::Choice(&[
            "8-bit",
            "shutter",
            "soft-shutter",
            "camera",
            "custom",
            "off",
        ]),
        default: "8-bit",
        description: "Sound played after every successful screenshot.",
    },
    Setting {
        key: "capture.custom-sound-file",
        kind: Kind::OptionalPath,
        default: "",
        description: "Custom screenshot sound file used when screenshot sound is custom.",
    },
    Setting {
        key: "capture.copy-to-clipboard",
        kind: Kind::Bool,
        default: "true",
        description: "Copy screenshot pixels immediately after capture.",
    },
    Setting {
        key: "capture.show-recent-captures-overlay",
        kind: Kind::Bool,
        default: "true",
        description: "Show screenshots in Recent Captures Overlay.",
    },
    Setting {
        key: "capture.save-automatically",
        kind: Kind::Bool,
        default: "false",
        description: "Save screenshots automatically to the configured folder.",
    },
    Setting {
        key: "capture.upload-and-copy-link",
        kind: Kind::Bool,
        default: "false",
        description: "Upload screenshots with the configured provider and copy the link.",
    },
    Setting {
        key: "capture.open-editor",
        kind: Kind::Bool,
        default: "false",
        description: "Open screenshots in the annotation editor.",
    },
    Setting {
        key: "capture.pin-to-screen",
        kind: Kind::Bool,
        default: "false",
        description: "Pin screenshots in a floating always-above window.",
    },
    Setting {
        key: APPLY_SMART_FRAME_AFTER_CAPTURE_KEY,
        kind: Kind::Bool,
        default: "false",
        description: "Deprecated: superseded by `scenes.default`. Migrated on first load.",
    },
    // Scenes: `none`, `auto`, or `preset:<id>` naming a saved Smart Frame
    // preset. Free text rather than a Choice because the set of presets is
    // owned by the user, not by this table.
    Setting {
        key: SCENES_DEFAULT_KEY,
        kind: Kind::Text { allow_empty: false },
        default: "auto",
        description: "Scene applied to captures that do not name their own: \
                      `none`, `auto`, or `preset:<id>`.",
    },
    Setting {
        key: "scenes.region",
        kind: Kind::Text { allow_empty: false },
        default: "default",
        description: "Scene for region captures: `default`, `none`, `auto`, or `preset:<id>`.",
    },
    Setting {
        key: "scenes.window",
        kind: Kind::Text { allow_empty: false },
        default: "default",
        description: "Scene for window captures: `default`, `none`, `auto`, or `preset:<id>`.",
    },
    Setting {
        key: "scenes.full-screen",
        kind: Kind::Text { allow_empty: false },
        default: "default",
        description: "Scene for full-screen captures: `default`, `none`, `auto`, or \
                      `preset:<id>`.",
    },
    Setting {
        key: "scenes.all-displays",
        kind: Kind::Text { allow_empty: false },
        default: "default",
        description: "Scene for all-displays captures: `default`, `none`, `auto`, or \
                      `preset:<id>`.",
    },
    Setting {
        key: "scenes.scrolling",
        kind: Kind::Text { allow_empty: false },
        default: "default",
        description: "Scene for scrolling captures: `default`, `none`, `auto`, or `preset:<id>`.",
    },
    Setting {
        key: WINDOW_SHADOW_KEY,
        kind: Kind::Bool,
        default: "true",
        description: "Include the window's drop shadow in window captures.",
    },
    Setting {
        key: "record.fps",
        kind: Kind::Int { min: 1, max: 240 },
        default: "30",
        description: "Recording frame rate.",
    },
    Setting {
        key: "record.microphone",
        kind: Kind::Bool,
        default: "false",
        description: "Record microphone input.",
    },
    Setting {
        key: "record.system-audio",
        kind: Kind::Bool,
        default: "false",
        description: "Record system audio output.",
    },
    Setting {
        key: CAMERA_ENABLED_KEY,
        kind: Kind::Bool,
        default: "false",
        description: "Capture a camera and composite it into screen recordings.",
    },
    Setting {
        key: CAMERA_DEVICE_KEY,
        kind: Kind::Path,
        default: "default",
        description: "Stable camera device identifier, or `default`.",
    },
    Setting {
        key: CAMERA_POSITION_KEY,
        kind: Kind::Choice(&[
            "top-left",
            "top-center",
            "top-right",
            "bottom-left",
            "bottom-center",
            "bottom-right",
        ]),
        default: "bottom-right",
        description: "Safe-area camera anchor.",
    },
    Setting {
        key: CAMERA_SIZE_KEY,
        kind: Kind::Int { min: 8, max: 50 },
        default: "22",
        description: "Camera height as a percentage of the shorter output edge.",
    },
    Setting {
        key: CAMERA_PLACEMENT_X_KEY,
        kind: Kind::Int {
            min: -1,
            max: 1_000,
        },
        default: "-1",
        description: "Normalized dragged camera x position in thousandths; -1 uses the anchor.",
    },
    Setting {
        key: CAMERA_PLACEMENT_Y_KEY,
        kind: Kind::Int {
            min: -1,
            max: 1_000,
        },
        default: "-1",
        description: "Normalized dragged camera y position in thousandths; -1 uses the anchor.",
    },
    Setting {
        key: CAMERA_SHAPE_KEY,
        kind: Kind::Choice(&["circle", "rounded", "square", "rectangle"]),
        default: "circle",
        description: "Camera mask shape.",
    },
    Setting {
        key: CAMERA_PRESENTER_KEY,
        kind: Kind::Bool,
        default: "false",
        description: "Use the camera as the primary presenter canvas.",
    },
    Setting {
        key: CAMERA_PRESENTER_SCREEN_KEY,
        kind: Kind::Bool,
        default: "true",
        description: "Show the shared screen as an inset in presenter mode.",
    },
    Setting {
        key: CAMERA_MIRROR_KEY,
        kind: Kind::Bool,
        default: "true",
        description: "Mirror camera preview and recorded composition.",
    },
    Setting {
        key: CAMERA_BORDER_KEY,
        kind: Kind::Bool,
        default: "true",
        description: "Draw a high-contrast camera border.",
    },
    Setting {
        key: CAMERA_SHADOW_KEY,
        kind: Kind::Bool,
        default: "true",
        description: "Draw a restrained camera shadow.",
    },
    Setting {
        key: RECORDING_CURSOR_KEY,
        kind: Kind::Bool,
        default: "true",
        description: "Show the pointer in recordings.",
    },
    Setting {
        key: RECORDING_CURSOR_SMOOTHING_KEY,
        kind: Kind::Bool,
        default: "false",
        description: "Smooth pointer movement without changing captured event timing.",
    },
    Setting {
        key: RECORDING_CLICKS_KEY,
        kind: Kind::Bool,
        default: "false",
        description: "Render click highlights while recording.",
    },
    Setting {
        key: RECORDING_CLICK_COLOR_KEY,
        kind: Kind::Color,
        default: "#7c6cf6",
        description: "Click highlight color as #RRGGBB or #RRGGBBAA.",
    },
    Setting {
        key: RECORDING_CLICK_SIZE_KEY,
        kind: Kind::Choice(&["small", "medium", "large"]),
        default: "medium",
        description: "Click highlight size.",
    },
    Setting {
        key: RECORDING_CLICK_STYLE_KEY,
        kind: Kind::Choice(&["outline", "filled"]),
        default: "outline",
        description: "Click highlight drawing style.",
    },
    Setting {
        key: RECORDING_CLICK_ANIMATION_KEY,
        kind: Kind::Bool,
        default: "true",
        description: "Animate click highlights instead of showing a brief static mark.",
    },
    Setting {
        key: RECORDING_KEYS_KEY,
        kind: Kind::Bool,
        default: "false",
        description: "Show filtered keystrokes while recording.",
    },
    Setting {
        key: RECORDING_KEY_SCOPE_KEY,
        kind: Kind::Choice(&["modifiers-only", "all"]),
        default: "modifiers-only",
        description: "Choose shortcut-only display or privacy-sensitive all-keys display.",
    },
    Setting {
        key: RECORDING_KEY_POSITION_KEY,
        kind: Kind::Choice(&[
            "top-left",
            "top-center",
            "top-right",
            "bottom-left",
            "bottom-center",
            "bottom-right",
        ]),
        default: "bottom-center",
        description: "Position of the keystroke display.",
    },
    Setting {
        key: RECORDING_KEY_SIZE_KEY,
        kind: Kind::Choice(&["small", "medium", "large"]),
        default: "medium",
        description: "Keystroke display size.",
    },
    Setting {
        key: RECORDING_KEY_THEME_KEY,
        kind: Kind::Choice(&["adaptive", "dark", "light"]),
        default: "adaptive",
        description: "Keystroke display contrast style.",
    },
    Setting {
        key: "record.show-recent-captures-overlay",
        kind: Kind::Bool,
        default: "true",
        description: "Show completed recordings in Recent Captures Overlay.",
    },
    Setting {
        key: "record.copy-to-clipboard",
        kind: Kind::Bool,
        default: "false",
        description: "Copy a retained recording file reference where the platform supports it.",
    },
    Setting {
        key: "record.save-automatically",
        kind: Kind::Bool,
        default: "false",
        description: "Save completed recordings automatically to the configured folder.",
    },
    Setting {
        key: "record.upload-and-copy-link",
        kind: Kind::Bool,
        default: "false",
        description: "Upload recordings with the configured provider and copy the link.",
    },
    Setting {
        key: "record.open-editor",
        kind: Kind::Bool,
        default: "false",
        description: "Open completed recordings in the video editor.",
    },
    Setting {
        key: "history.max-image-bytes",
        kind: Kind::Int {
            min: 0,
            // A little under i64::MAX so the bound is expressible in JSON
            // without a consumer needing arbitrary-precision integers.
            max: 1 << 53,
        },
        default: "10737418240",
        description: "Disk budget for stored source images. Pinned captures are never evicted.",
    },
    Setting {
        key: "history.max-image-age",
        kind: Kind::Choice(&["forever", "1-month", "1-week", "3-days", "1-day"]),
        default: "forever",
        description: "Maximum age of unpinned source images. Capture documents and edits are kept.",
    },
    Setting {
        key: RECENT_CAPTURES_OVERLAY_PLACEMENT_KEY,
        kind: Kind::Choice(&["left", "right"]),
        default: "left",
        description: "Screen edge used by the Recent Captures Overlay.",
    },
    Setting {
        key: RECENT_CAPTURES_OVERLAY_FOLLOW_ACTIVE_DISPLAY_KEY,
        kind: Kind::Bool,
        default: "false",
        description: "Move Recent Captures to the display containing the active pointer or capture.",
    },
    Setting {
        key: RECENT_CAPTURES_OVERLAY_CARD_WIDTH_KEY,
        kind: Kind::Int { min: 224, max: 320 },
        default: "288",
        description: "Preferred Recent Captures card width in logical points.",
    },
    Setting {
        key: RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_ENABLED_KEY,
        kind: Kind::Bool,
        default: "false",
        description: "Clean up Recent Captures cards after an elapsed interval.",
    },
    Setting {
        key: RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_ACTION_KEY,
        kind: Kind::Choice(&["hide", "save-then-hide"]),
        default: "hide",
        description: "Safe action performed when Recent Captures cleanup runs.",
    },
    Setting {
        key: RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_SECONDS_KEY,
        kind: Kind::Int { min: 5, max: 3_600 },
        default: "30",
        description: "Elapsed seconds before Recent Captures cleanup.",
    },
    Setting {
        key: RECENT_CAPTURES_OVERLAY_CLOSE_AFTER_DRAG_KEY,
        kind: Kind::Bool,
        default: "true",
        description: "Hide a card after an accepted external drag unless Option or Alt is held.",
    },
    Setting {
        key: RECENT_CAPTURES_OVERLAY_CLOSE_AFTER_UPLOAD_KEY,
        kind: Kind::Bool,
        default: "false",
        description: "Hide a card only after a cloud upload is confirmed successful.",
    },
    Setting {
        key: RECENT_CAPTURES_OVERLAY_SAVE_BUTTON_KEY,
        kind: Kind::Choice(&["export-location", "choose-destination"]),
        default: "export-location",
        description: "Default destination behavior of the Recent Captures Save button.",
    },
    Setting {
        key: "hotkey.capture-history",
        kind: Kind::Accelerator,
        default: ShortcutAction::OpenHistory.default_accelerator_setting(),
        description: "Uses this shortcut to open Capture History.",
    },
    Setting {
        key: "hotkey.capture-all-in-one",
        kind: Kind::Accelerator,
        default: ShortcutAction::CaptureAllInOne.default_accelerator_setting(),
        description: scrozz_core::product_copy::SHORTCUT_ALL_IN_ONE,
    },
    Setting {
        key: "cloud.provider",
        kind: Kind::Choice(&["aws", "r2", "b2", "minio"]),
        default: "aws",
        description: "S3-compatible provider preset used for private sharing.",
    },
    Setting {
        key: "cloud.bucket",
        kind: Kind::Text { allow_empty: true },
        default: "",
        description: "Bucket name. Empty leaves sharing unconfigured.",
    },
    Setting {
        key: "cloud.region",
        kind: Kind::Text { allow_empty: false },
        default: "us-east-1",
        description: "Signature region. R2 always signs with auto; B2 requires its bucket region.",
    },
    Setting {
        key: "cloud.endpoint",
        kind: Kind::Text { allow_empty: true },
        default: "",
        description: "Optional S3 API endpoint override; required for MinIO.",
    },
    Setting {
        key: "cloud.account-id",
        kind: Kind::Text { allow_empty: true },
        default: "",
        description: "Cloudflare account id used to form the default R2 endpoint.",
    },
    Setting {
        key: "cloud.prefix",
        kind: Kind::Text { allow_empty: true },
        default: "captures",
        description: "Object-key prefix for uploaded captures.",
    },
    Setting {
        key: "cloud.public-base-url",
        kind: Kind::Text { allow_empty: true },
        default: "",
        description: "Custom public origin or path used only for non-expiring links.",
    },
    Setting {
        key: "cloud.url-policy",
        kind: Kind::Choice(&["private-expiring", "public-base"]),
        default: "private-expiring",
        description: "Use provider-enforced expiring links or the configured public base URL.",
    },
    Setting {
        key: "cloud.expiry-seconds",
        kind: Kind::Int {
            min: 0,
            max: 604_800,
        },
        default: "86400",
        description: "Presigned-link lifetime; zero means an already-public bucket or CDN.",
    },
    Setting {
        key: "cloud.naming-template",
        kind: Kind::Text { allow_empty: false },
        default: "Screenshot-{timestamp}",
        description: "Object name template; supports {timestamp}, {card}, and {kind}.",
    },
    Setting {
        key: "cloud.tags",
        kind: Kind::Text { allow_empty: true },
        default: "",
        description: "Comma-separated key=value tags added to each uploaded object.",
    },
    Setting {
        key: "cloud.protection-mode",
        kind: Kind::Choice(&["none", "vault"]),
        default: "none",
        description: "Encrypt shares with the default password kept in the native credential vault.",
    },
    Setting {
        key: "cloud.credential-command",
        kind: Kind::Text { allow_empty: true },
        default: "",
        description: "Optional program whose stdout supplies the secret access key.",
    },
    Setting {
        key: "cloud.viewer-title",
        kind: Kind::Text { allow_empty: false },
        default: "Scrozz share",
        description: "Heading and browser title for encrypted share viewers.",
    },
    Setting {
        key: "cloud.viewer-accent",
        kind: Kind::Text { allow_empty: false },
        default: "#f05a28",
        description: "Six-digit CSS accent color for encrypted share viewers.",
    },
    Setting {
        key: "hotkey.capture-region",
        kind: Kind::Accelerator,
        default: ShortcutAction::CaptureRegion.default_accelerator_setting(),
        description: scrozz_core::product_copy::SHORTCUT_CAPTURE_AREA,
    },
    Setting {
        key: "hotkey.capture-window",
        kind: Kind::Accelerator,
        default: ShortcutAction::CaptureWindow.default_accelerator_setting(),
        description: scrozz_core::product_copy::SHORTCUT_CAPTURE_WINDOW,
    },
    Setting {
        key: "hotkey.capture-display",
        kind: Kind::Accelerator,
        default: ShortcutAction::CaptureFullscreen.default_accelerator_setting(),
        description: scrozz_core::product_copy::SHORTCUT_CAPTURE_FULLSCREEN,
    },
    Setting {
        key: "hotkey.capture-all-displays",
        kind: Kind::Accelerator,
        default: ShortcutAction::CaptureAllDisplays.default_accelerator_setting(),
        description: scrozz_core::product_copy::SHORTCUT_CAPTURE_ALL_DISPLAYS,
    },
    Setting {
        key: "hotkey.capture-scrolling",
        kind: Kind::Accelerator,
        default: ShortcutAction::CaptureScrolling.default_accelerator_setting(),
        description: scrozz_core::product_copy::SHORTCUT_SCROLLING_CAPTURE,
    },
    Setting {
        key: "hotkey.record-toggle",
        kind: Kind::Accelerator,
        default: ShortcutAction::ToggleRecording.default_accelerator_setting(),
        description: scrozz_core::product_copy::SHORTCUT_RECORD_SCREEN,
    },
    Setting {
        key: "hotkey.record-start",
        kind: Kind::Accelerator,
        default: "Super+Shift+R",
        description: scrozz_core::product_copy::SHORTCUT_RECORD_SCREEN,
    },
    Setting {
        key: "hotkey.record-stop",
        kind: Kind::Accelerator,
        default: "Super+Shift+Escape",
        description: "Hotkey for stopping a recording.",
    },
];

/// Looks up a setting.
///
/// # Errors
///
/// Returns [`CliError::Usage`] naming the closest match, if there is one. A bare
/// "unknown key" is useless when the user is one character away.
pub fn lookup(key: &str) -> CliResult<&'static Setting> {
    if let Some(setting) = SETTINGS.iter().find(|s| s.key == key) {
        return Ok(setting);
    }
    let suggestion = closest(key);
    Err(CliError::usage(match suggestion {
        Some(near) => format!("unknown setting {key:?}; did you mean {near:?}?"),
        None => format!("unknown setting {key:?}; run `scrozz settings get` to list every key"),
    }))
}

/// The nearest known key, when one is close enough to be worth suggesting.
fn closest(key: &str) -> Option<&'static str> {
    let lower = key.to_ascii_lowercase();
    SETTINGS
        .iter()
        .map(|s| (s.key, distance(&lower, s.key)))
        // Two edits catches a typo and a missing hyphen without suggesting
        // something unrelated.
        .filter(|(_, d)| *d <= 2)
        .min_by_key(|(_, d)| *d)
        .map(|(key, _)| key)
}

/// Levenshtein distance, iterative with a single row.
fn distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b_chars.len()).collect();
    for (i, ac) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, bc) in b_chars.iter().enumerate() {
            let insert_or_delete = row[j + 1].min(row[j]) + 1;
            let substitute = previous + usize::from(ac != *bc);
            previous = row[j + 1];
            row[j + 1] = insert_or_delete.min(substitute);
        }
    }
    row[b_chars.len()]
}

/// Every setting as JSON.
#[must_use]
pub fn all_json() -> Json {
    Json::arr(SETTINGS.iter().copied().map(Setting::to_json))
}

/// Every setting as aligned text.
#[must_use]
pub fn all_human() -> String {
    let width = SETTINGS.iter().map(|s| s.key.len()).max().unwrap_or(0);
    SETTINGS
        .iter()
        .map(|s| format!("{:width$}  {}", s.key, s.default))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The shortcuts stored on this machine, or the defaults.
///
/// Never fails: a settings listing that errors because a config file is
/// unreadable is less useful than one that shows what the app will actually do,
/// which without a readable file is the defaults.
#[must_use]
pub fn stored_shortcuts() -> Shortcuts {
    ShortcutStore::default_location().map_or_else(|_| Shortcuts::default(), |store| store.load())
}

/// The versioned settings stored on this machine.
///
/// # Errors
///
/// Returns a storage error if an existing document cannot be read or migrated.
pub fn stored_settings() -> CliResult<(AfterCaptureSettings, AfterCaptureStore)> {
    let store = AfterCaptureStore::default_location().map_err(CliError::Core)?;
    let profile = store.inferred_profile();
    // Schema migration, including folding the retired Smart Frame flag into
    // Scenes, belongs to the store's versioned load. Doing it here would miss
    // every other caller that loads the document directly, the GUI included.
    let settings = store.load(profile).map_err(CliError::Core)?;
    Ok((settings, store))
}

/// The value in force for a setting, and whether the user chose it.
///
/// Shortcuts resolve through their registration-aware store; every other key
/// resolves through the versioned settings document.
#[must_use]
pub fn resolve(
    setting: &Setting,
    shortcuts: &Shortcuts,
    persisted: &AfterCaptureSettings,
) -> (String, &'static str) {
    match ShortcutAction::from_stored_key(setting.key) {
        Some(action) => {
            let value = shortcuts.get(action).unwrap_or_default().to_owned();
            let source = if shortcuts.is_default(action) {
                "default"
            } else {
                "user"
            };
            (value, source)
        }
        None => {
            let value = persisted
                .value_for_key(setting.key)
                .map(|value| value.to_string())
                .or_else(|| persisted.value(setting.key).map(str::to_owned))
                .unwrap_or_else(|| setting.default.to_owned());
            let source = if value == setting.default {
                "default"
            } else {
                "user"
            };
            (value, source)
        }
    }
}

/// Every setting as JSON, with anything the user has stored applied.
#[must_use]
pub fn all_json_resolved(shortcuts: &Shortcuts, persisted: &AfterCaptureSettings) -> Json {
    Json::arr(SETTINGS.iter().map(|setting| {
        let (value, source) = resolve(setting, shortcuts, persisted);
        setting.to_json_valued(&value, source)
    }))
}

/// Every setting as aligned text, with anything the user has stored applied.
///
/// An unassigned shortcut prints as `(unassigned)` rather than as nothing at
/// all, because a blank column in a listing reads as a bug.
#[must_use]
pub fn all_human_resolved(shortcuts: &Shortcuts, persisted: &AfterCaptureSettings) -> String {
    let width = SETTINGS.iter().map(|s| s.key.len()).max().unwrap_or(0);
    SETTINGS
        .iter()
        .map(|setting| {
            let (value, _) = resolve(setting, shortcuts, persisted);
            let shown = if value.is_empty() {
                "(unassigned)".to_owned()
            } else {
                value
            };
            format!("{:width$}  {}", setting.key, shown)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resolves the screenshot sound from schema values.
///
/// Stored and default values pass through this same parser.
pub fn screenshot_sound(persisted: &AfterCaptureSettings) -> CliResult<ScreenshotSound> {
    let shortcuts = Shortcuts::default();
    let selected = resolve(lookup("capture.screenshot-sound")?, &shortcuts, persisted).0;
    let custom = resolve(lookup("capture.custom-sound-file")?, &shortcuts, persisted).0;
    screenshot_sound_from(&selected, &custom)
}

/// Resolves the source-image retention policy without renaming persisted keys.
pub fn retention_policy(
    persisted: &AfterCaptureSettings,
) -> CliResult<scrozz_store::RetentionPolicy> {
    let shortcuts = Shortcuts::default();
    let bytes = resolve(lookup("history.max-image-bytes")?, &shortcuts, persisted)
        .0
        .parse::<u64>()
        .map_err(|_| {
            CliError::usage("history.max-image-bytes must be a non-negative whole number")
        })?;
    let age = resolve(lookup("history.max-image-age")?, &shortcuts, persisted).0;
    Ok(scrozz_store::RetentionPolicy {
        max_image_bytes: bytes,
        max_image_age: scrozz_store::RetentionWindow::from_token(&age)?,
    })
}

/// Whether window captures keep the window's own drop shadow.
///
/// # Errors
///
/// Returns [`CliError::Usage`] if the stored value is not a boolean.
pub fn window_shadow(persisted: &AfterCaptureSettings) -> CliResult<bool> {
    let shortcuts = Shortcuts::default();
    let value = resolve(lookup(WINDOW_SHADOW_KEY)?, &shortcuts, persisted).0;
    value
        .parse::<bool>()
        .map_err(|_| CliError::usage(format!("{WINDOW_SHADOW_KEY} must be `true` or `false`")))
}

/// Folds the retired `after-capture.apply-smart-frame` flag into `scenes.default`.
///
/// Call this only for a document that predates Scenes — the settings store
/// decides that from the document version, which is the one place that can tell
/// an existing install from a new one.
///
/// The distinction matters because the two have opposite defaults. The retired
/// flag defaulted to `false`, so an existing user who never touched it had
/// framing off and must keep it off; a new install has no history to preserve
/// and takes the schema default of `auto`. Folding forward therefore maps an
/// explicit `true` to `auto`, and both an explicit `false` and an absent flag to
/// `none`.
///
/// Returns whether anything changed, so the caller can skip a pointless write.
pub fn migrate_scenes(persisted: &mut AfterCaptureSettings) -> bool {
    if persisted.value(SCENES_DEFAULT_KEY).is_some() {
        return false;
    }
    let legacy = persisted
        .value(APPLY_SMART_FRAME_AFTER_CAPTURE_KEY)
        .map(str::to_owned)
        .or_else(|| {
            persisted
                .value_for_key(APPLY_SMART_FRAME_AFTER_CAPTURE_KEY)
                .map(|value| value.to_string())
        });
    let resolved = if legacy.is_some_and(|value| value.trim() == "true") {
        "auto"
    } else {
        "none"
    };
    persisted.set_value(SCENES_DEFAULT_KEY, resolved);
    true
}

/// Deletes a Scene preset and every assignment that named it.
///
/// The one place a preset may be removed. A preset can be deleted from Settings
/// or from the editor's preset list, and both leave the same wreckage if they
/// only touch the preset store: `scenes.default` and the per-capture rows keep
/// pointing at an id that no longer resolves, so the row still reads as
/// configured while applying nothing. Deleting and re-pointing together — inside
/// one store update — means no reader ever observes a dangling reference.
///
/// The default falls back to `auto` and per-capture rows to `default`, matching
/// what the pane offers when nothing has been chosen.
///
/// # Errors
///
/// Returns whatever [`AfterCaptureSettings::delete_smart_frame_preset`] returns,
/// leaving assignments untouched if the delete itself was refused.
pub fn forget_scene_preset(
    persisted: &mut AfterCaptureSettings,
    id: &str,
) -> scrozz_core::Result<()> {
    persisted.delete_smart_frame_preset(id)?;
    let token = format!("preset:{id}");
    if persisted.value(SCENES_DEFAULT_KEY) == Some(token.as_str()) {
        persisted.set_value(SCENES_DEFAULT_KEY, "auto");
    }
    for (_, key) in SCENE_CAPTURE_KEYS {
        if persisted.value(key) == Some(token.as_str()) {
            persisted.set_value(*key, "default");
        }
    }
    Ok(())
}

/// Every Scene assignment in force, as raw tokens the UI layer parses.
///
/// Returned as `(slug, token)` pairs rather than a typed enum so this module
/// keeps knowing only about strings; `scrozz_ui::settings` owns the grammar.
///
/// # Errors
///
/// Returns [`CliError::Usage`] if a Scenes key is missing from the schema.
pub fn scene_assignments(
    persisted: &AfterCaptureSettings,
) -> CliResult<Vec<(&'static str, String)>> {
    let shortcuts = Shortcuts::default();
    SCENE_CAPTURE_KEYS
        .iter()
        .map(|(slug, key)| {
            let value = resolve(lookup(key)?, &shortcuts, persisted).0;
            Ok((*slug, value))
        })
        .collect()
}

/// The default Scene token.
///
/// # Errors
///
/// Returns [`CliError::Usage`] if the key is missing from the schema.
pub fn scene_default(persisted: &AfterCaptureSettings) -> CliResult<String> {
    let shortcuts = Shortcuts::default();
    Ok(resolve(lookup(SCENES_DEFAULT_KEY)?, &shortcuts, persisted).0)
}

/// The Scene token in force for one capture type, with `default` resolved.
///
/// Returns `None` when no Scene applies, `Some("auto")` for the adaptive
/// built-in, or `Some("preset:<id>")` for a saved preset.
///
/// # Errors
///
/// Returns [`CliError::Usage`] if `slug` is not a known capture type or a
/// Scenes key is missing from the schema.
pub fn scene_for_capture(
    persisted: &AfterCaptureSettings,
    slug: &str,
) -> CliResult<Option<String>> {
    let key = SCENE_CAPTURE_KEYS
        .iter()
        .find(|(candidate, _)| *candidate == slug)
        .map(|(_, key)| *key)
        .ok_or_else(|| CliError::usage(format!("unknown capture type {slug:?}")))?;
    let shortcuts = Shortcuts::default();
    let token = resolve(lookup(key)?, &shortcuts, persisted).0;
    let resolved = if token == "default" {
        scene_default(persisted)?
    } else {
        token
    };
    Ok((resolved != "none").then_some(resolved))
}

/// Capture-type slug to settings key, in the order the pane lists them.
pub const SCENE_CAPTURE_KEYS: &[(&str, &str)] = &[
    ("region", "scenes.region"),
    ("window", "scenes.window"),
    ("full-screen", "scenes.full-screen"),
    ("all-displays", "scenes.all-displays"),
    ("scrolling", "scenes.scrolling"),
];

/// Resolves the complete Recent Captures Overlay behavior contract.
pub fn recent_captures_overlay_settings(
    persisted: &AfterCaptureSettings,
) -> CliResult<scrozz_ui::RecentCapturesOverlaySettings> {
    use scrozz_ui::recent_captures_overlay::{
        RecentCapturesAutoCloseAction, RecentCapturesSaveBehavior,
    };

    let shortcuts = Shortcuts::default();
    let value = |key| -> CliResult<String> { Ok(resolve(lookup(key)?, &shortcuts, persisted).0) };
    let parse_bool = |key| -> CliResult<bool> {
        value(key)?
            .parse::<bool>()
            .map_err(|_| CliError::usage(format!("stored setting {key:?} must be true or false")))
    };
    let placement = scrozz_ui::RecentCapturesPlacement::from_slug(&value(
        RECENT_CAPTURES_OVERLAY_PLACEMENT_KEY,
    )?)
    .ok_or_else(|| CliError::usage("stored Recent Captures placement is invalid"))?;
    let auto_close_action = RecentCapturesAutoCloseAction::from_slug(&value(
        RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_ACTION_KEY,
    )?)
    .ok_or_else(|| CliError::usage("stored Recent Captures auto-close action is invalid"))?;
    let save_behavior =
        RecentCapturesSaveBehavior::from_slug(&value(RECENT_CAPTURES_OVERLAY_SAVE_BUTTON_KEY)?)
            .ok_or_else(|| {
                CliError::usage("stored Recent Captures Save button behavior is invalid")
            })?;

    Ok(scrozz_ui::RecentCapturesOverlaySettings {
        placement,
        follow_active_display: parse_bool(RECENT_CAPTURES_OVERLAY_FOLLOW_ACTIVE_DISPLAY_KEY)?,
        card_width: value(RECENT_CAPTURES_OVERLAY_CARD_WIDTH_KEY)?
            .parse::<f32>()
            .map_err(|_| CliError::usage("stored Recent Captures card width must be a number"))?,
        auto_close_enabled: parse_bool(RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_ENABLED_KEY)?,
        auto_close_action,
        auto_close_seconds: value(RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_SECONDS_KEY)?
            .parse::<u32>()
            .map_err(|_| {
                CliError::usage("stored Recent Captures auto-close interval must be an integer")
            })?,
        close_after_drag: parse_bool(RECENT_CAPTURES_OVERLAY_CLOSE_AFTER_DRAG_KEY)?,
        close_after_upload: parse_bool(RECENT_CAPTURES_OVERLAY_CLOSE_AFTER_UPLOAD_KEY)?,
        save_behavior,
    }
    .normalized())
}

fn screenshot_sound_from(selected: &str, custom: &str) -> CliResult<ScreenshotSound> {
    match selected {
        "8-bit" => Ok(ScreenshotSound::EightBit),
        "shutter" => Ok(ScreenshotSound::Shutter),
        "soft-shutter" => Ok(ScreenshotSound::SoftShutter),
        "camera" => Ok(ScreenshotSound::Camera),
        "off" => Ok(ScreenshotSound::Off),
        "custom" if custom.trim().is_empty() => Err(CliError::usage(
            "capture.custom-sound-file must be set when capture.screenshot-sound is custom",
        )),
        "custom" => Ok(ScreenshotSound::Custom(PathBuf::from(custom))),
        other => Err(CliError::usage(format!(
            "unknown screenshot sound {other:?}"
        ))),
    }
}

/// Effective settings as one read-only snapshot, plus the edits made to it.
///
/// A *view* over the single versioned document [`crate::after_capture`] owns,
/// not a second file. That is the whole point: private sharing has to write
/// `cloud.*` keys without disturbing the Recording, Camera, Recent Captures and
/// After Capture state stored beside them, and re-serialising one authoritative
/// document is the only way to guarantee that. Unknown keys and unknown root
/// sections survive untouched because the underlying document preserves them.
///
/// Nothing secret is ever held here. [`StoredSettings::set`] refuses a
/// credential-bearing key outright, and such a key is never read back, so a
/// stray secret cannot be echoed by `scrozz settings get`.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSettings {
    effective: BTreeMap<String, String>,
    user_set: BTreeSet<String>,
    pending: BTreeMap<String, String>,
}

impl Default for StoredSettings {
    fn default() -> Self {
        Self::from_resolved(&Shortcuts::default(), &AfterCaptureSettings::default())
    }
}

impl StoredSettings {
    /// Resolves every schema key against stored shortcuts and stored values.
    fn from_resolved(shortcuts: &Shortcuts, persisted: &AfterCaptureSettings) -> Self {
        let mut effective = BTreeMap::new();
        let mut user_set = BTreeSet::new();
        for setting in SETTINGS {
            if credential_bearing_key(setting.key) {
                continue;
            }
            let (value, _) = resolve(setting, shortcuts, persisted);
            if user_chose(setting, shortcuts, persisted) {
                user_set.insert(setting.key.to_owned());
            }
            effective.insert(setting.key.to_owned(), value);
        }
        Self {
            effective,
            user_set,
            pending: BTreeMap::new(),
        }
    }

    /// Effective value, falling back to the schema default.
    #[must_use]
    pub fn value(&self, key: &str) -> Option<&str> {
        self.effective.get(key).map(String::as_str)
    }

    /// Whether the user, rather than the schema, chose this key's value.
    #[must_use]
    pub fn is_user_set(&self, key: &str) -> bool {
        self.user_set.contains(key)
    }

    /// Validates and changes one known, non-secret value.
    ///
    /// # Errors
    ///
    /// Returns a usage error for an unknown key, a value the schema rejects, or
    /// a credential-bearing key.
    pub fn set(&mut self, key: &str, value: &str) -> CliResult<()> {
        let setting = lookup(key)?;
        if credential_bearing_key(setting.key) {
            return Err(credential_refusal(setting.key));
        }
        setting.validate(value)?;
        self.effective.insert(key.to_owned(), value.to_owned());
        self.pending.insert(key.to_owned(), value.to_owned());
        if value == setting.default {
            self.user_set.remove(key);
        } else {
            self.user_set.insert(key.to_owned());
        }
        Ok(())
    }

    /// Restores one key to its schema default.
    ///
    /// # Errors
    ///
    /// Returns a usage error for an unknown key.
    pub fn reset(&mut self, key: &str) -> CliResult<()> {
        let setting = lookup(key)?;
        self.set(key, setting.default)
    }

    /// The edits not yet written back to the document.
    fn take_pending(&mut self) -> BTreeMap<String, String> {
        std::mem::take(&mut self.pending)
    }
}

/// Whether the user, rather than the schema, chose this key's current value.
fn user_chose(setting: &Setting, shortcuts: &Shortcuts, persisted: &AfterCaptureSettings) -> bool {
    match ShortcutAction::from_stored_key(setting.key) {
        Some(action) => !shortcuts.is_default(action),
        None => {
            persisted.value(setting.key).is_some()
                || persisted
                    .value_for_key(setting.key)
                    .is_some_and(|enabled| enabled.to_string() != setting.default)
        }
    }
}

/// Deliberately never echoes the value: a refusal that quotes the secret it
/// refused is worse than no refusal at all.
fn credential_refusal(key: &str) -> CliError {
    CliError::usage(format!(
        "credential-bearing field {key:?} is forbidden in settings; use the native vault"
    ))
}

fn credential_bearing_key(key: &str) -> bool {
    let key = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    ["accesskey", "secret", "sessiontoken", "password"]
        .iter()
        .any(|needle| key.contains(needle))
}

/// Reads and writes schema values inside the one versioned settings document.
///
/// Every write goes through [`AfterCaptureStore`], so it inherits that store's
/// cross-process lock, atomic replacement, and unknown-field preservation
/// rather than racing it from a second file.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    inner: AfterCaptureStore,
}

impl SettingsStore {
    /// Uses an explicit document path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            inner: AfterCaptureStore::new(path),
        }
    }

    /// Resolves the platform configuration path.
    ///
    /// # Errors
    ///
    /// Returns a storage error when no configuration directory exists.
    pub fn default_location() -> CliResult<Self> {
        AfterCaptureStore::default_location()
            .map(|inner| Self { inner })
            .map_err(CliError::Core)
    }

    /// The document this store reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    /// Loads the effective settings. An absent document means schema defaults.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the document cannot be read or migrated.
    pub fn load(&self) -> CliResult<StoredSettings> {
        let profile = self.inner.inferred_profile();
        let persisted = self.inner.load(profile).map_err(CliError::Core)?;
        Ok(StoredSettings::from_resolved(
            &stored_shortcuts(),
            &persisted,
        ))
    }

    /// Applies `change` and writes back only what it altered.
    ///
    /// # Errors
    ///
    /// Returns whatever `change` refuses, or a storage error if the document
    /// cannot be written.
    pub fn update(
        &self,
        change: impl FnOnce(&mut StoredSettings) -> CliResult<()>,
    ) -> CliResult<StoredSettings> {
        let profile = self.inner.inferred_profile();
        let mut view = self.load()?;
        change(&mut view)?;
        let pending = view.take_pending();
        if pending.is_empty() {
            return Ok(view);
        }
        // Only the deltas are applied, inside the store's own lock, so a
        // concurrent writer's untouched sections are never clobbered by the
        // stale snapshot read above.
        let persisted = self
            .inner
            .update(profile, |latest| {
                for (key, value) in &pending {
                    if let Some((media, action)) = AfterCaptureSettings::resolve_key(key) {
                        latest.set(media, action, value == "true");
                    } else {
                        latest.set_value(key.clone(), value.clone());
                    }
                }
                Ok(())
            })
            .map_err(CliError::Core)?;
        Ok(StoredSettings::from_resolved(
            &stored_shortcuts(),
            &persisted,
        ))
    }
}

/// Every effective setting as JSON.
#[must_use]
pub fn all_json_from(settings: &StoredSettings) -> Json {
    Json::arr(SETTINGS.iter().filter_map(|setting| {
        let value = settings.value(setting.key)?;
        let source = if settings.is_user_set(setting.key) {
            "user"
        } else {
            "default"
        };
        Some(setting.to_json_valued(value, source))
    }))
}

/// Every effective setting as aligned text.
#[must_use]
pub fn all_human_from(settings: &StoredSettings) -> String {
    let width = SETTINGS
        .iter()
        .map(|setting| setting.key.len())
        .max()
        .unwrap_or(0);
    SETTINGS
        .iter()
        .filter_map(|setting| {
            let value = settings.value(setting.key)?;
            let shown = if value.is_empty() {
                "(unassigned)"
            } else {
                value
            };
            Some(format!("{:width$}  {}", setting.key, shown))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::after_capture::InstallProfile;

    fn scratch(name: &str) -> PathBuf {
        let sequence = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "scrozz-settings-{name}-{}-{}",
            std::process::id(),
            sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tuned_recording_settings() -> RecordingSettings {
        let mut expected = RecordingSettings::shipped();
        expected.cursor = CursorMode::Hidden;
        expected.cursor_smoothing = false;
        expected.clicks.enabled = true;
        expected.clicks.color = Rgba8::rgba(12, 34, 56, 200);
        expected.clicks.size = OverlaySize::Large;
        expected.clicks.style = ClickStyle::Filled;
        expected.clicks.animate = false;
        expected.keystrokes.enabled = true;
        expected.keystrokes.scope = KeystrokeScope::All;
        expected.keystrokes.position = OverlayAnchor::TopRight;
        expected.keystrokes.size = OverlaySize::Small;
        expected.keystrokes.theme = OverlayTheme::Light;
        expected
    }

    #[test]
    fn recording_interaction_defaults_need_no_input_monitoring() {
        let defaults = recording_settings_from(&AfterCaptureSettings::fresh()).unwrap();

        assert!(!defaults.clicks.enabled);
        assert!(!defaults.keystrokes.enabled);
        assert_eq!(defaults.keystrokes.scope, KeystrokeScope::ModifiersOnly);
        assert_eq!(defaults.cursor, CursorMode::Visible);
    }

    #[test]
    fn camera_preferences_round_trip_without_native_handles() {
        let root = scratch("camera");
        let _cleanup = Scratch(root.clone());
        let store = AfterCaptureStore::new(root.join("settings.json"));
        let camera = scrozz_record::settings::CameraSettings {
            enabled: true,
            position: OverlayAnchor::TopLeft,
            placement: Some(CameraPlacement::new(0.333, 0.667).expect("fixture placement")),
            size: 0.34,
            shape: CameraShape::Square,
            presenter: true,
            presenter_screen: false,
            mirror: false,
            border: false,
            shadow: true,
        };
        let mut expected = RecordingSettings::shipped();
        expected.camera = camera;

        let persisted = store
            .update(InstallProfile::Fresh, |persisted| {
                apply_recording_settings(persisted, expected);
                apply_camera_device(persisted, Some(&CameraDeviceId::new("stable-camera-id")?));
                Ok(())
            })
            .unwrap();

        let reloaded = recording_settings_from(&persisted).unwrap();
        assert_eq!(reloaded.camera, camera);
        assert_eq!(
            camera_device_from(&persisted)
                .unwrap()
                .as_ref()
                .map(CameraDeviceId::as_str),
            Some("stable-camera-id")
        );

        // A stable device id is a platform identifier, never a native handle.
        let encoded = std::fs::read_to_string(root.join("settings.json")).unwrap();
        assert!(encoded.contains("stable-camera-id"));
        assert!(!encoded.contains("AVCaptureDevice"));
    }

    #[test]
    fn missing_camera_preferences_never_enable_capture() {
        let defaults = recording_settings_from(&AfterCaptureSettings::fresh()).unwrap();
        assert!(!defaults.camera.enabled);
        assert!(
            camera_device_from(&AfterCaptureSettings::fresh())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn recording_interaction_options_round_trip_without_plaintext_events() {
        let root = scratch("recording-interactions");
        let _cleanup = Scratch(root.clone());
        let store = AfterCaptureStore::new(root.join("settings.json"));
        let expected = tuned_recording_settings();

        let persisted = store
            .update(InstallProfile::Fresh, |persisted| {
                apply_recording_settings(persisted, expected);
                Ok(())
            })
            .unwrap();
        assert_eq!(recording_settings_from(&persisted).unwrap(), expected);

        let reloaded = store.load(InstallProfile::Fresh).unwrap();
        assert_eq!(recording_settings_from(&reloaded).unwrap(), expected);

        let encoded = std::fs::read_to_string(root.join("settings.json")).unwrap();
        assert!(encoded.contains(RECORDING_CLICKS_KEY));
        assert!(encoded.contains(RECORDING_KEY_SCOPE_KEY));
        assert!(!encoded.contains("keycode"));
        assert!(!encoded.contains("events"));
    }

    #[test]
    fn saving_recording_settings_preserves_every_unowned_value() {
        let root = scratch("recording-preserve");
        let _cleanup = Scratch(root.clone());
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("settings.json");
        let store = AfterCaptureStore::new(&path);
        std::fs::write(
            &path,
            br#"{"version":2,"future-root":{"keep":7},"values":{"capture.folder":"/Volumes/Archive","future.key":"keep"},"after_capture":{"future-media":{"keep":true}}}"#,
        )
        .unwrap();

        let returned = save_recording_settings(&store, tuned_recording_settings()).unwrap();
        assert_eq!(
            recording_settings_from(&returned).unwrap(),
            tuned_recording_settings(),
            "the returned document is the one that was written"
        );

        let updated: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(updated["future-root"], serde_json::json!({"keep": 7}));
        assert_eq!(
            updated["after_capture"]["future-media"],
            serde_json::json!({"keep": true})
        );
        assert_eq!(updated["values"]["future.key"], "keep");
        assert_eq!(updated["values"]["capture.folder"], "/Volumes/Archive");
        assert_eq!(updated["values"][RECORDING_KEY_SCOPE_KEY], "all");
    }

    #[test]
    fn a_malformed_interaction_value_is_not_silently_ignored() {
        let mut persisted = AfterCaptureSettings::fresh();
        persisted.set_value(RECORDING_KEY_SCOPE_KEY, "everything");

        let error = recording_settings_from(&persisted).unwrap_err().to_string();

        assert!(error.contains("everything"), "{error}");
    }

    #[test]
    fn every_key_is_unique() {
        let mut keys: Vec<&str> = SETTINGS.iter().map(|s| s.key).collect();
        let count = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), count, "a settings key is declared twice");
    }

    #[test]
    fn every_key_follows_the_naming_rule() {
        for setting in SETTINGS {
            assert!(
                setting.key.contains('.'),
                "{} has no area prefix",
                setting.key
            );
            assert!(
                setting
                    .key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || matches!(c, '.' | '-')),
                "{} is not lowercase-dotted-hyphenated",
                setting.key
            );
            assert!(
                !setting.key.contains('_'),
                "{} uses an underscore",
                setting.key
            );
        }
    }

    #[test]
    fn every_default_validates_against_its_own_type() {
        // The failure this prevents is embarrassing and easy: shipping a default
        // the app itself would reject.
        for setting in SETTINGS {
            setting
                .validate(setting.default)
                .unwrap_or_else(|e| panic!("{} has an invalid default: {e}", setting.key));
        }
    }

    #[test]
    fn credential_values_have_no_plaintext_setting() {
        for forbidden in ["secret", "access-key", "session-token", "password"] {
            assert!(
                SETTINGS
                    .iter()
                    .all(|setting| !setting.key.contains(forbidden)),
                "a credential-bearing setting contains {forbidden:?}"
            );
        }
        assert!(lookup("cloud.credential-command").is_ok());
    }

    #[test]
    fn every_setting_has_a_description() {
        for setting in SETTINGS {
            assert!(!setting.description.is_empty(), "{}", setting.key);
            assert!(
                setting.description.ends_with('.') || setting.description.ends_with('…'),
                "{} reads as a fragment",
                setting.key
            );
        }
    }

    #[test]
    fn hotkey_defaults_match_the_generated_bindings() {
        // The settings schema and the compositor config generator must not drift
        // apart; a user who changes a hotkey and regenerates their config would
        // otherwise get two different keys for one action.
        for action in crate::cli::HotkeyAction::all() {
            let key = format!("hotkey.{}", action.slug());
            let setting = lookup(&key).unwrap_or_else(|_| panic!("{key} is missing"));
            assert_eq!(
                setting.default,
                action.default_accelerator(),
                "{key} disagrees with the hotkey action"
            );
        }
    }

    #[test]
    fn the_retention_default_matches_the_core_policy() {
        // D23 fixes the default at 10 GB in `scrozz-store`. Two numbers that
        // must agree should be checked, not hoped about.
        let setting = lookup("history.max-image-bytes").unwrap();
        let age = lookup("history.max-image-age").unwrap();
        assert_eq!(
            setting.default.parse::<u64>().unwrap(),
            scrozz_store::RetentionPolicy::default().max_image_bytes
        );
        assert_eq!(
            scrozz_store::RetentionWindow::from_token(age.default).unwrap(),
            scrozz_store::RetentionPolicy::default().max_image_age
        );
        let Kind::Choice(options) = age.kind else {
            panic!("history.max-image-age should be a choice")
        };
        assert_eq!(
            options,
            scrozz_store::RetentionWindow::all()
                .iter()
                .map(|window| window.as_token())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn persisted_retention_values_resolve_without_renaming_legacy_keys() {
        let mut persisted = AfterCaptureSettings::fresh();
        persisted.set_value("history.max-image-bytes", "4096");
        persisted.set_value("history.max-image-age", "1-week");

        assert_eq!(
            retention_policy(&persisted).unwrap(),
            scrozz_store::RetentionPolicy {
                max_image_bytes: 4096,
                max_image_age: scrozz_store::RetentionWindow::OneWeek,
            }
        );
    }

    #[test]
    fn the_format_choices_match_the_cli() {
        let setting = lookup("capture.format").unwrap();
        let Kind::Choice(options) = setting.kind else {
            panic!("capture.format should be a choice")
        };
        assert_eq!(options, ["png", "jpeg", "webp"]);
    }

    #[test]
    fn crosshair_mode_has_the_three_product_states_and_defaults_off() {
        let setting = lookup("capture.crosshair-mode").unwrap();
        let Kind::Choice(options) = setting.kind else {
            panic!("capture.crosshair-mode should be a choice")
        };
        assert_eq!(options, ["off", "modifier", "always"]);
        assert_eq!(setting.default, "off");
    }

    #[test]
    fn frozen_selection_defaults_off() {
        assert_eq!(lookup("capture.freeze-screen").unwrap().default, "false");
    }

    #[test]
    fn a_new_install_frames_captures_automatically() {
        let persisted = AfterCaptureSettings::fresh();
        assert_eq!(lookup(SCENES_DEFAULT_KEY).unwrap().default, "auto");
        assert_eq!(scene_default(&persisted).unwrap(), "auto");
        for (slug, _) in SCENE_CAPTURE_KEYS {
            assert_eq!(
                scene_for_capture(&persisted, slug).unwrap(),
                Some("auto".to_owned()),
                "{slug} should follow the default"
            );
        }
    }

    #[test]
    fn an_explicit_smart_frame_choice_survives_the_move_to_scenes() {
        // The whole point of the migration: a user who turned Smart Frame on
        // keeps framing, and a user who turned it off does not silently get it.
        for (legacy, expected) in [("true", "auto"), ("false", "none")] {
            let mut persisted = AfterCaptureSettings::fresh();
            persisted.set_value(APPLY_SMART_FRAME_AFTER_CAPTURE_KEY, legacy);
            assert!(migrate_scenes(&mut persisted));
            assert_eq!(scene_default(&persisted).unwrap(), expected);
            // Idempotent: a second load must not re-derive anything.
            assert!(!migrate_scenes(&mut persisted));
        }
    }

    #[test]
    fn an_existing_install_that_never_stored_the_legacy_flag_keeps_framing_off() {
        // The retired flag defaulted to `false`, so silence in an existing
        // document means the user had framing off — not that they want the new
        // install default. Only the store decides a document is "existing";
        // this asserts what happens once it has.
        let mut persisted = AfterCaptureSettings::fresh();
        assert!(migrate_scenes(&mut persisted));
        assert_eq!(scene_default(&persisted).unwrap(), "none");
        assert!(!migrate_scenes(&mut persisted));
    }

    #[test]
    fn a_per_type_override_wins_over_the_default() {
        let mut persisted = AfterCaptureSettings::fresh();
        persisted.set_value(SCENES_DEFAULT_KEY, "none");
        persisted.set_value("scenes.window", "preset:studio");
        assert_eq!(
            scene_for_capture(&persisted, "window").unwrap(),
            Some("preset:studio".to_owned())
        );
        assert_eq!(scene_for_capture(&persisted, "region").unwrap(), None);
        assert!(scene_for_capture(&persisted, "nonsense").is_err());
    }

    #[test]
    fn window_shadow_is_on_by_default_and_can_be_turned_off() {
        let mut persisted = AfterCaptureSettings::fresh();
        assert!(window_shadow(&persisted).unwrap());
        persisted.set_value(WINDOW_SHADOW_KEY, "false");
        assert!(!window_shadow(&persisted).unwrap());
    }

    #[test]
    fn recent_captures_overlay_defaults_preserve_existing_behavior() {
        let settings =
            recent_captures_overlay_settings(&AfterCaptureSettings::fresh()).expect("defaults");
        assert_eq!(
            settings,
            scrozz_ui::RecentCapturesOverlaySettings::default()
        );
        assert_eq!(settings.placement, scrozz_ui::RecentCapturesPlacement::Left);
        assert_eq!(settings.card_width, 288.0);
        assert!(!settings.follow_active_display);
        assert!(!settings.auto_close_enabled);
        assert!(settings.close_after_drag);
    }

    #[test]
    fn recent_captures_overlay_values_resolve_and_normalize() {
        let mut persisted = AfterCaptureSettings::fresh();
        persisted.set_value(RECENT_CAPTURES_OVERLAY_PLACEMENT_KEY, "right");
        persisted.set_value(RECENT_CAPTURES_OVERLAY_FOLLOW_ACTIVE_DISPLAY_KEY, "true");
        persisted.set_value(RECENT_CAPTURES_OVERLAY_CARD_WIDTH_KEY, "999");
        persisted.set_value(RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_ENABLED_KEY, "true");
        persisted.set_value(
            RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_ACTION_KEY,
            "save-then-hide",
        );
        persisted.set_value(RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_SECONDS_KEY, "2");
        persisted.set_value(RECENT_CAPTURES_OVERLAY_CLOSE_AFTER_DRAG_KEY, "false");
        persisted.set_value(RECENT_CAPTURES_OVERLAY_CLOSE_AFTER_UPLOAD_KEY, "true");
        persisted.set_value(
            RECENT_CAPTURES_OVERLAY_SAVE_BUTTON_KEY,
            "choose-destination",
        );

        let settings = recent_captures_overlay_settings(&persisted).expect("resolve");
        assert_eq!(
            settings.placement,
            scrozz_ui::RecentCapturesPlacement::Right
        );
        assert!(settings.follow_active_display);
        assert_eq!(settings.card_width, 320.0);
        assert!(settings.auto_close_enabled);
        assert_eq!(settings.auto_close_seconds, 5);
        assert!(!settings.close_after_drag);
        assert!(settings.close_after_upload);
        assert_eq!(
            settings.save_behavior,
            scrozz_ui::recent_captures_overlay::RecentCapturesSaveBehavior::ChooseDestination
        );
    }

    #[test]
    fn selector_dimension_labels_default_to_logical_1x() {
        let setting = lookup("capture.dimension-label").unwrap();
        let Kind::Choice(options) = setting.kind else {
            panic!("capture.dimension-label should be a choice")
        };
        assert_eq!(options, ["logical", "output-pixels", "both"]);
        assert_eq!(setting.default, "logical");
    }

    #[test]
    fn retina_output_defaults_to_native_resolution() {
        let setting = lookup("capture.retina-output").unwrap();
        let Kind::Choice(options) = setting.kind else {
            panic!("capture.retina-output should be a choice")
        };
        assert_eq!(options, ["native", "1x"]);
        assert_eq!(setting.default, "native");
    }

    #[test]
    fn screenshot_sound_defaults_on_and_supports_custom_or_off() {
        let setting = lookup("capture.screenshot-sound").unwrap();
        let Kind::Choice(options) = setting.kind else {
            panic!("capture.screenshot-sound should be a choice")
        };
        assert_eq!(
            options,
            [
                "8-bit",
                "shutter",
                "soft-shutter",
                "camera",
                "custom",
                "off"
            ]
        );
        assert_eq!(setting.default, "8-bit");
        assert_eq!(
            screenshot_sound(&AfterCaptureSettings::fresh()).unwrap(),
            ScreenshotSound::EightBit
        );
        assert_eq!(
            screenshot_sound_from("custom", "/tmp/shutter.wav").unwrap(),
            ScreenshotSound::Custom(PathBuf::from("/tmp/shutter.wav"))
        );
        assert!(screenshot_sound_from("custom", "").is_err());
    }

    #[test]
    fn booleans_accept_only_true_and_false() {
        let setting = lookup("capture.cursor").unwrap();
        assert!(setting.validate("true").is_ok());
        assert!(setting.validate("false").is_ok());
        for bad in ["yes", "1", "TRUE", "True", ""] {
            assert!(setting.validate(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn integers_are_bounds_checked() {
        let setting = lookup("capture.quality").unwrap();
        assert!(setting.validate("1").is_ok());
        assert!(setting.validate("100").is_ok());
        assert!(setting.validate("0").is_err());
        assert!(setting.validate("101").is_err());
        assert!(setting.validate("-5").is_err());
        assert!(setting.validate("90.5").is_err());
        assert!(setting.validate("lots").is_err());
    }

    #[test]
    fn an_out_of_range_number_says_what_the_range_is() {
        let err = lookup("record.fps").unwrap().validate("500").unwrap_err();
        let message = err.to_string();
        assert!(message.contains('1'), "{message}");
        assert!(message.contains("240"), "{message}");
    }

    #[test]
    fn a_bad_choice_lists_the_good_ones() {
        let err = lookup("capture.format")
            .unwrap()
            .validate("gif")
            .unwrap_err();
        let message = err.to_string();
        for option in ["png", "jpeg", "webp"] {
            assert!(message.contains(option), "{message}");
        }
    }

    #[test]
    fn accelerator_settings_use_the_real_parser() {
        let setting = lookup("hotkey.capture-region").unwrap();
        assert!(setting.validate("Ctrl+Alt+P").is_ok());
        assert!(setting.validate("Print").is_ok());
        assert!(setting.validate("Super+Nonsense").is_err());
        assert!(setting.validate("Super").is_err());
    }

    #[test]
    fn a_path_setting_rejects_only_emptiness() {
        let setting = lookup("capture.folder").unwrap();
        assert!(setting.validate("/anywhere/at/all").is_ok());
        // A folder on a volume that is not mounted right now is a perfectly
        // reasonable thing to configure, so existence is not checked.
        assert!(setting.validate("/Volumes/Archive/Shots").is_ok());
        assert!(setting.validate("").is_err());
        assert!(setting.validate("   ").is_err());
    }

    #[test]
    fn an_unknown_key_suggests_the_near_miss() {
        let err = lookup("capture.formats").unwrap_err();
        assert!(err.to_string().contains("capture.format"), "{err}");

        let err = lookup("capture.qualtiy").unwrap_err();
        assert!(err.to_string().contains("capture.quality"), "{err}");
    }

    #[test]
    fn a_wildly_wrong_key_points_at_the_listing_instead() {
        let err = lookup("bananas").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("scrozz settings get"), "{message}");
        assert!(!message.contains("did you mean"), "{message}");
    }

    #[test]
    fn an_unknown_key_is_a_usage_error_not_a_crash() {
        assert_eq!(
            lookup("nope.nope").unwrap_err().exit(),
            crate::exit::Exit::Usage
        );
    }

    #[test]
    fn levenshtein_behaves() {
        assert_eq!(distance("", ""), 0);
        assert_eq!(distance("abc", "abc"), 0);
        assert_eq!(distance("abc", "abd"), 1);
        assert_eq!(distance("abc", "ab"), 1);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("kitten", "sitting"), 3);
    }

    #[test]
    fn the_json_listing_carries_the_type_and_constraints() {
        let Json::Arr(items) = all_json() else {
            panic!("expected an array")
        };
        assert_eq!(items.len(), SETTINGS.len());

        let rendered = lookup("capture.format")
            .unwrap()
            .to_json()
            .to_compact_string();
        assert!(rendered.contains(r#""type":"choice""#), "{rendered}");
        assert!(
            rendered.contains(r#""choices":["png","jpeg","webp"]"#),
            "{rendered}"
        );

        let rendered = lookup("record.fps").unwrap().to_json().to_compact_string();
        assert!(rendered.contains(r#""minimum":1"#), "{rendered}");
        assert!(rendered.contains(r#""maximum":240"#), "{rendered}");
    }

    #[test]
    fn the_json_shape_starts_with_the_same_six_keys_for_every_setting() {
        for setting in SETTINGS {
            let Json::Obj(pairs) = setting.to_json() else {
                panic!("expected an object")
            };
            let keys: Vec<&str> = pairs.iter().take(6).map(|(k, _)| k.as_str()).collect();
            assert_eq!(
                keys,
                ["key", "type", "value", "default", "source", "description"],
                "{}",
                setting.key
            );
        }
    }

    #[test]
    fn values_are_reported_as_sourced_from_defaults() {
        // Schema-only values have no persisted context, so they are defaults.
        for setting in SETTINGS {
            assert!(
                setting
                    .to_json()
                    .to_compact_string()
                    .contains(r#""source":"default""#),
                "{}",
                setting.key
            );
        }
    }

    #[test]
    fn the_human_listing_names_every_key() {
        let text = all_human();
        for setting in SETTINGS {
            assert!(text.contains(setting.key), "{}", setting.key);
        }
        assert_eq!(text.lines().count(), SETTINGS.len());
    }

    #[test]
    fn cloud_settings_round_trip_through_the_one_settings_document() {
        let root = scratch("cloud-round-trip");
        let _guard = Scratch(root.clone());
        let store = SettingsStore::new(root.join("settings.json"));

        store
            .update(|settings| settings.set("cloud.bucket", "screenshots"))
            .expect("write a cloud setting");

        let loaded = store.load().expect("read back");
        assert_eq!(loaded.value("cloud.bucket"), Some("screenshots"));
        assert!(loaded.is_user_set("cloud.bucket"));
        let json = all_json_from(&loaded).to_compact_string();
        assert!(json.contains(r#""source":"user""#), "{json}");
    }

    #[test]
    fn a_cloud_write_preserves_every_other_settings_section() {
        // The whole reason private sharing does not get its own file: writing a
        // cloud key must leave Recording, Camera, Recent Captures, After
        // Capture and any not-yet-known section exactly as they were.
        let root = scratch("cloud-composes");
        let _guard = Scratch(root.clone());
        let path = root.join("settings.json");
        std::fs::create_dir_all(&root).expect("scratch dir");
        std::fs::write(
            &path,
            r#"{
                "version": 7,
                "future_root": {"kept": true},
                "values": {
                    "future.setting": {"shape": [1, 2, 3]},
                    "record.camera": "true",
                    "recent-captures-overlay.placement": "right",
                    "cloud.bucket": "before"
                },
                "after_capture": {"screenshot": {"copy-to-clipboard": true}}
            }"#,
        )
        .expect("seed document");

        let store = SettingsStore::new(&path);
        store
            .update(|settings| settings.set("cloud.bucket", "after"))
            .expect("update one cloud key");

        let rendered = std::fs::read_to_string(&path).expect("read document");
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(value["future_root"]["kept"], true);
        assert_eq!(value["values"]["future.setting"]["shape"][2], 3);
        assert_eq!(value["values"]["record.camera"], "true");
        assert_eq!(
            value["values"]["recent-captures-overlay.placement"],
            "right"
        );
        assert_eq!(
            value["after_capture"]["screenshot"]["copy-to-clipboard"],
            true
        );
        assert_eq!(value["values"]["cloud.bucket"], "after");
    }

    #[test]
    fn the_schema_never_offers_a_credential_bearing_key() {
        // Secrets live in the platform vault. If a key like this were ever
        // added, `settings get` would print it and `settings set` would write
        // it to a world-readable file.
        for setting in SETTINGS {
            assert!(
                !credential_bearing_key(setting.key),
                "{} would persist a secret",
                setting.key
            );
        }
        for key in [
            "cloud.secret",
            "access_key_id",
            "AccessKeyId",
            "session_token",
            "session-token",
            "sharePassword",
        ] {
            assert!(credential_bearing_key(key), "{key}");
        }
    }

    #[test]
    fn a_credential_bearing_refusal_never_echoes_the_value() {
        let error = credential_refusal("cloud.secret-access-key");
        assert!(error.to_string().contains("native vault"), "{error}");
        assert!(!error.to_string().contains("never-echo-this"), "{error}");
    }
}
