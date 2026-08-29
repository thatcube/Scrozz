//! The settings schema and small, forward-compatible preferences file.
//!
//! # Naming
//!
//! `area.key-name`: a dotted area, hyphenated words. It matches the action slugs
//! the hotkey commands already use, and it survives being a TOML table, a JSON
//! object path and a command-line argument without being quoted.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use scrozz_annotate::SmartFramePreset;
use scrozz_core::{Error as CoreError, Result as CoreResult};
use scrozz_store::atomic_write;
use serde::{Deserialize, Serialize};

use crate::{
    fault::{CliError, CliResult},
    hotkey_config::Accelerator,
    json::Json,
};

const SETTINGS_FILE: &str = "settings.json";
const SETTINGS_VERSION: u32 = 1;
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;
const MAX_SMART_FRAME_PRESETS: usize = 128;
pub const SETTINGS_PATH_ENV: &str = "SCROZZ_SETTINGS_PATH";

/// Ordered consumers for the GUI's After Capture workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AfterCapturePolicy {
    /// Resolve one Smart Frame revision before any consumer runs.
    pub apply_smart_frame: bool,
    /// Copy the derived revision.
    pub copy: bool,
    /// Save the derived revision.
    pub save: bool,
    /// Upload the derived revision when an upload provider exists.
    pub upload: bool,
    /// Show the derived revision in the Quick Access overlay.
    pub overlay: bool,
    /// Open the derived editable document.
    pub open_editor: bool,
    /// Pin the derived revision.
    pub pin: bool,
}

impl Default for AfterCapturePolicy {
    fn default() -> Self {
        Self {
            apply_smart_frame: false,
            copy: false,
            save: false,
            upload: false,
            overlay: true,
            open_editor: false,
            pin: false,
        }
    }
}

/// All durable user preferences.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserSettings {
    /// File format version.
    pub version: u32,
    /// Scalar settings keyed by their public dotted names.
    pub values: BTreeMap<String, String>,
    /// User-created Smart Frame presets. These never contain capture pixels.
    pub smart_frame_presets: Vec<SmartFramePreset>,
    /// Unknown top-level fields survive read-modify-write cycles.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            values: BTreeMap::new(),
            smart_frame_presets: Vec::new(),
            extensions: BTreeMap::new(),
        }
    }
}

impl UserSettings {
    /// Current or default value of one known setting.
    #[must_use]
    pub fn value(&self, setting: &Setting) -> &str {
        self.values
            .get(setting.key)
            .map_or(setting.default, String::as_str)
    }

    /// Whether a value came from durable user settings.
    #[must_use]
    pub fn is_overridden(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    /// Typed After Capture policy.
    #[must_use]
    pub fn after_capture_policy(&self) -> AfterCapturePolicy {
        let enabled = |key: &str, fallback: bool| {
            self.values
                .get(key)
                .and_then(|value| value.parse::<bool>().ok())
                .unwrap_or(fallback)
        };
        let defaults = AfterCapturePolicy::default();
        AfterCapturePolicy {
            apply_smart_frame: enabled(
                "after-capture.apply-smart-frame",
                defaults.apply_smart_frame,
            ),
            copy: enabled("after-capture.copy", defaults.copy)
                || enabled("capture.copy-to-clipboard", false),
            save: enabled("after-capture.save", defaults.save),
            upload: enabled("after-capture.upload", defaults.upload),
            overlay: enabled("after-capture.overlay", defaults.overlay),
            open_editor: enabled("after-capture.open-editor", defaults.open_editor),
            pin: enabled("after-capture.pin", defaults.pin),
        }
    }

    fn validate(&self) -> CoreResult<()> {
        if self.smart_frame_presets.len() > MAX_SMART_FRAME_PRESETS {
            return Err(CoreError::Storage(format!(
                "settings contain {} Smart Frame presets; the limit is {MAX_SMART_FRAME_PRESETS}",
                self.smart_frame_presets.len()
            )));
        }
        for (key, value) in &self.values {
            if let Ok(setting) = lookup(key) {
                setting
                    .validate(value)
                    .map_err(|error| CoreError::Storage(error.to_string()))?;
            }
        }
        for preset in &self.smart_frame_presets {
            preset.validate()?;
        }
        let mut ids: Vec<&str> = self
            .smart_frame_presets
            .iter()
            .map(|preset| preset.id.as_str())
            .collect();
        ids.sort_unstable();
        if ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(CoreError::Storage(
                "Smart Frame preset identifiers must be unique".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Atomic reader/writer for the preferences file.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// Default per-user settings location.
    pub fn open_default() -> CoreResult<Self> {
        if let Some(path) = std::env::var_os(SETTINGS_PATH_ENV) {
            return Ok(Self::at(path));
        }
        #[cfg(test)]
        {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("unnamed");
            let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
            });
            Ok(Self::at(std::env::temp_dir().join(format!(
                "scrozz-test-settings-{}-{hash}.json",
                std::process::id()
            ))))
        }
        #[cfg(not(test))]
        {
            let root = dirs::config_dir().ok_or_else(|| {
                CoreError::Storage("no platform configuration directory is available".to_owned())
            })?;
            Ok(Self::at(root.join("Scrozz").join(SETTINGS_FILE)))
        }
    }

    /// Explicit path, primarily for tests.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Reads and validates settings, returning defaults when no file exists.
    pub fn load(&self) -> CoreResult<UserSettings> {
        match fs::metadata(&self.path) {
            Ok(metadata) if metadata.len() > MAX_SETTINGS_BYTES => {
                return Err(CoreError::Storage(format!(
                    "settings file is {} bytes; the limit is {MAX_SETTINGS_BYTES}",
                    metadata.len()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(UserSettings::default());
            }
            Err(error) => return Err(CoreError::Io(error)),
        }
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) => return Err(CoreError::Io(error)),
        };
        let settings: UserSettings = serde_json::from_slice(&bytes)
            .map_err(|error| CoreError::Storage(format!("cannot decode settings: {error}")))?;
        settings.validate()?;
        Ok(settings)
    }

    /// Persists one scalar setting without dropping unknown fields or presets.
    pub fn set(&self, key: &str, value: &str) -> CliResult<UserSettings> {
        let setting = lookup(key)?;
        setting.validate(value)?;
        let mut settings = self.load()?;
        settings.values.insert(key.to_owned(), value.to_owned());
        self.save(&settings)?;
        Ok(settings)
    }

    /// Adds or replaces one custom preset.
    pub fn upsert_preset(&self, preset: SmartFramePreset) -> CoreResult<UserSettings> {
        preset.validate()?;
        let mut settings = self.load()?;
        if let Some(existing) = settings
            .smart_frame_presets
            .iter_mut()
            .find(|existing| existing.id == preset.id)
        {
            *existing = preset;
        } else {
            settings.smart_frame_presets.push(preset);
        }
        settings
            .smart_frame_presets
            .sort_by_key(|preset| preset.name.to_lowercase());
        self.save(&settings)?;
        Ok(settings)
    }

    /// Removes one custom preset.
    pub fn delete_preset(&self, id: &str) -> CoreResult<UserSettings> {
        let mut settings = self.load()?;
        let before = settings.smart_frame_presets.len();
        settings
            .smart_frame_presets
            .retain(|preset| preset.id != id);
        if settings.smart_frame_presets.len() == before {
            return Err(CoreError::InvalidRequest(format!(
                "Smart Frame preset {id:?} does not exist"
            )));
        }
        self.save(&settings)?;
        Ok(settings)
    }

    /// Writes a complete validated settings file atomically.
    pub fn save(&self, settings: &UserSettings) -> CoreResult<()> {
        settings.validate()?;
        let mut current = settings.clone();
        current.version = current.version.max(SETTINGS_VERSION);
        let bytes = serde_json::to_vec_pretty(&current)
            .map_err(|error| CoreError::Storage(format!("cannot encode settings: {error}")))?;
        if bytes.len() as u64 > MAX_SETTINGS_BYTES {
            return Err(CoreError::Storage(format!(
                "settings file would be {} bytes; the limit is {MAX_SETTINGS_BYTES}",
                bytes.len()
            )));
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&self.path, &bytes)
    }

    /// Storage path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

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
    /// One of a fixed set of strings.
    Choice(&'static [&'static str]),
    /// A key combination, validated by the same parser the hotkey commands use.
    Accelerator,
}

impl Kind {
    /// The type name reported in JSON.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int { .. } => "int",
            Self::Path => "path",
            Self::Choice(_) => "choice",
            Self::Accelerator => "accelerator",
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
    /// The JSON representation, including the current (always default) value.
    #[must_use]
    pub fn to_json(self) -> Json {
        self.to_json_value(self.default, "default")
    }

    /// JSON representation with a resolved value and provenance.
    #[must_use]
    pub fn to_json_value(self, value: &str, source: &str) -> Json {
        let mut fields = vec![
            ("key", Json::str(self.key)),
            ("type", Json::str(self.kind.slug())),
            ("value", Json::str(value)),
            ("default", Json::str(self.default)),
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
            Kind::Accelerator => Accelerator::parse(value).map(|_| ()),
        }
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
        key: "capture.copy-to-clipboard",
        kind: Kind::Bool,
        default: "false",
        description: "Also copy every capture to the clipboard.",
    },
    Setting {
        key: "capture.window-shadow",
        kind: Kind::Bool,
        default: "true",
        description: "Include the window's drop shadow in window captures.",
    },
    Setting {
        key: "after-capture.apply-smart-frame",
        kind: Kind::Bool,
        default: "false",
        description: "Create one Smart Frame revision before enabled After Capture actions.",
    },
    Setting {
        key: "after-capture.copy",
        kind: Kind::Bool,
        default: "false",
        description: "Copy the shared After Capture revision.",
    },
    Setting {
        key: "after-capture.save",
        kind: Kind::Bool,
        default: "false",
        description: "Save the shared After Capture revision.",
    },
    Setting {
        key: "after-capture.upload",
        kind: Kind::Bool,
        default: "false",
        description: "Upload the shared After Capture revision when a provider is configured.",
    },
    Setting {
        key: "after-capture.overlay",
        kind: Kind::Bool,
        default: "true",
        description: "Show the shared After Capture revision in Quick Access.",
    },
    Setting {
        key: "after-capture.open-editor",
        kind: Kind::Bool,
        default: "false",
        description: "Open the shared After Capture revision in the editor.",
    },
    Setting {
        key: "after-capture.pin",
        kind: Kind::Bool,
        default: "false",
        description: "Pin the shared After Capture revision.",
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
        key: "hotkey.capture-region",
        kind: Kind::Accelerator,
        default: "Super+Shift+4",
        description: "Hotkey for an interactive region capture.",
    },
    Setting {
        key: "hotkey.capture-window",
        kind: Kind::Accelerator,
        default: "Super+Shift+5",
        description: "Hotkey for an interactive window capture.",
    },
    Setting {
        key: "hotkey.capture-display",
        kind: Kind::Accelerator,
        default: "Super+Shift+3",
        description: "Hotkey for capturing the active display.",
    },
    Setting {
        key: "hotkey.capture-all-displays",
        kind: Kind::Accelerator,
        default: "Super+Shift+6",
        description: "Hotkey for capturing every display.",
    },
    Setting {
        key: "hotkey.record-start",
        kind: Kind::Accelerator,
        default: "Super+Shift+R",
        description: "Hotkey for starting a recording.",
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

/// Every setting resolved against durable preferences.
#[must_use]
pub fn all_json_for(settings: &UserSettings) -> Json {
    Json::arr(SETTINGS.iter().copied().map(|setting| {
        setting.to_json_value(
            settings.value(&setting),
            if settings.is_overridden(setting.key) {
                "user"
            } else {
                "default"
            },
        )
    }))
}

/// Human-readable settings resolved against durable preferences.
#[must_use]
pub fn all_human_for(settings: &UserSettings) -> String {
    let width = SETTINGS
        .iter()
        .map(|setting| setting.key.len())
        .max()
        .unwrap_or(0);
    SETTINGS
        .iter()
        .map(|setting| {
            format!(
                "{:width$}  {}",
                setting.key,
                settings.value(setting),
                width = width
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use scrozz_annotate::{SmartFramePreset, SmartFramePresetSettings};

    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "scrozz-settings-{label}-{}-{nonce}",
                std::process::id()
            ))
            .join("settings.json")
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
    fn every_setting_has_a_description() {
        for setting in SETTINGS {
            assert!(!setting.description.is_empty(), "{}", setting.key);
            assert!(
                setting.description.ends_with('.'),
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
        assert_eq!(
            setting.default.parse::<u64>().unwrap(),
            scrozz_store::RetentionPolicy::default().max_image_bytes
        );
    }

    #[test]
    fn smart_frame_after_capture_is_opt_in_and_overlay_stays_on() {
        let policy = UserSettings::default().after_capture_policy();
        assert!(!policy.apply_smart_frame);
        assert!(policy.overlay);
        assert!(!policy.copy);
        assert!(!policy.save);
    }

    #[test]
    fn settings_and_presets_persist_atomically_with_unknown_fields() {
        let path = scratch("round-trip");
        let root = path.parent().unwrap().to_path_buf();
        let store = SettingsStore::at(&path);
        let mut settings = UserSettings::default();
        settings
            .extensions
            .insert("future_root".to_owned(), serde_json::json!({"kept": true}));
        settings
            .values
            .insert("future.setting".to_owned(), "future-value".to_owned());
        settings.smart_frame_presets.push(
            SmartFramePreset::new(
                "quiet-frame",
                "Quiet Frame",
                SmartFramePresetSettings::default(),
            )
            .unwrap(),
        );
        store.save(&settings).unwrap();
        let updated = store
            .set("after-capture.apply-smart-frame", "true")
            .unwrap();

        assert!(updated.after_capture_policy().apply_smart_frame);
        assert_eq!(updated.extensions["future_root"]["kept"], true);
        assert_eq!(updated.values["future.setting"], "future-value");
        assert_eq!(updated.smart_frame_presets[0].name, "Quiet Frame");
        assert!(path.is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preset_update_and_delete_are_cross_capture_storage_operations() {
        let path = scratch("presets");
        let root = path.parent().unwrap().to_path_buf();
        let store = SettingsStore::at(&path);
        let preset = SmartFramePreset::new(
            "team-note",
            "Team Note",
            SmartFramePresetSettings::default(),
        )
        .unwrap();
        store.upsert_preset(preset.clone()).unwrap();
        let mut renamed = preset;
        renamed.name = "Team Note Updated".to_owned();
        let settings = store.upsert_preset(renamed).unwrap();
        assert_eq!(settings.smart_frame_presets.len(), 1);
        assert_eq!(settings.smart_frame_presets[0].name, "Team Note Updated");

        let settings = store.delete_preset("team-note").unwrap();
        assert!(settings.smart_frame_presets.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn newer_settings_keep_unknown_fields_when_a_known_value_changes() {
        let path = scratch("forward-compatible");
        let root = path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            &path,
            br#"{
                "version": 9,
                "values": {"future.setting": "kept"},
                "smart_frame_presets": [],
                "future_section": {"token": 17}
            }"#,
        )
        .unwrap();
        let store = SettingsStore::at(&path);
        let settings = store
            .set("after-capture.apply-smart-frame", "true")
            .unwrap();
        assert_eq!(settings.version, 9);
        assert_eq!(settings.values["future.setting"], "kept");
        assert_eq!(settings.extensions["future_section"]["token"], 17);

        let reloaded = store.load().unwrap();
        assert_eq!(reloaded.version, 9);
        assert_eq!(reloaded.extensions["future_section"]["token"], 17);
        std::fs::remove_dir_all(root).unwrap();
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
        // Until persistence exists, saying "user" would be a lie a script could
        // act on.
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
}
