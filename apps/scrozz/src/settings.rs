//! The settings schema.
//!
//! # Why the schema lives here and not in the store
//!
//! The schema and versioned JSON document deliberately live together: a key's
//! name, type and default are what the GUI renders, what `--json` reports, and
//! what a user's dotfiles refer to. The document stores only validated,
//! non-secret values and preserves sections owned by aggregate features.
//!
//! # Naming
//!
//! `area.key-name`: a dotted area, hyphenated words. It matches the action slugs
//! the hotkey commands already use, and it survives being a TOML table, a JSON
//! object path and a command-line argument without being quoted.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use fs2::FileExt as _;
use serde_json::{Map, Value};

use crate::{
    fault::{CliError, CliResult},
    hotkey_config::Accelerator,
    json::Json,
};

const SETTINGS_VERSION: u32 = 2;
const APP_DIR: &str = "Scrozz";
const FILE_NAME: &str = "settings.json";
const LOCK_FILE_NAME: &str = ".settings.lock";
/// Overrides the settings file path for portable installs and isolated tests.
pub const SETTINGS_FILE_ENV: &str = "SCROZZ_SETTINGS_FILE";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
            Self::Choice(_) => "choice",
            Self::Accelerator => "accelerator",
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
    /// The JSON representation, including the current (always default) value.
    #[must_use]
    pub fn to_json(self) -> Json {
        self.to_json_with(self.default, "default")
    }

    /// JSON representation with an effective value and its source.
    #[must_use]
    pub fn to_json_with(self, value: &str, source: &str) -> Json {
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
        default: "#7c3aed",
        description: "Six-digit CSS accent color for encrypted share viewers.",
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
        .map(|setting| {
            format!(
                "{:width$}  {}",
                setting.key,
                settings.value(setting.key).unwrap_or(setting.default)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Effective settings loaded from the versioned, non-secret settings document.
///
/// Unknown keys and root fields are retained byte-for-byte as JSON values when
/// this version updates a known setting. That is the compatibility seam used by
/// aggregate builds that add independent settings sections.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSettings {
    values: BTreeMap<String, String>,
    unknown_values: BTreeMap<String, Value>,
    unknown_root: BTreeMap<String, Value>,
    document_version: u32,
}

impl Default for StoredSettings {
    fn default() -> Self {
        Self {
            values: BTreeMap::new(),
            unknown_values: BTreeMap::new(),
            unknown_root: BTreeMap::new(),
            document_version: SETTINGS_VERSION,
        }
    }
}

impl StoredSettings {
    /// Effective value, falling back to the schema default.
    #[must_use]
    pub fn value(&self, key: &str) -> Option<&str> {
        let setting = SETTINGS.iter().find(|setting| setting.key == key)?;
        Some(
            self.values
                .get(key)
                .map(String::as_str)
                .unwrap_or(setting.default),
        )
    }

    /// Whether this document overrides a schema default.
    #[must_use]
    pub fn is_user_set(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    /// Validates and changes one known, non-secret value.
    pub fn set(&mut self, key: &str, value: &str) -> CliResult<()> {
        let setting = lookup(key)?;
        setting.validate(value)?;
        self.values.insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    /// Removes one override so the schema default applies again.
    pub fn reset(&mut self, key: &str) -> CliResult<()> {
        lookup(key)?;
        self.values.remove(key);
        Ok(())
    }

    fn from_json(text: &str) -> CliResult<Self> {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| storage_error(format!("the settings file is unreadable: {error}")))?;
        reject_credential_fields(&value)?;
        let mut root = value
            .as_object()
            .cloned()
            .ok_or_else(|| storage_error("the settings file must contain a JSON object"))?;
        let version = root
            .remove("version")
            .and_then(|value| value.as_u64())
            .map_or(0, |version| u32::try_from(version).unwrap_or(u32::MAX));
        let mut settings = Self {
            document_version: version.max(SETTINGS_VERSION),
            ..Self::default()
        };
        if let Some(values) = root.remove("values") {
            let values = values
                .as_object()
                .ok_or_else(|| storage_error("settings `values` must contain a JSON object"))?;
            for (key, value) in values {
                if let Some(setting) = SETTINGS.iter().find(|setting| setting.key == key) {
                    let value = scalar_setting(value).ok_or_else(|| {
                        storage_error(format!("stored setting {key:?} must be a scalar value"))
                    })?;
                    setting.validate(&value)?;
                    settings.values.insert(key.clone(), value);
                } else {
                    settings.unknown_values.insert(key.clone(), value.clone());
                }
            }
        }
        settings.unknown_root = root.into_iter().collect();
        Ok(settings)
    }

    fn to_json(&self) -> CliResult<String> {
        let mut root: Map<String, Value> = self
            .unknown_root
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        root.insert(
            "version".to_owned(),
            Value::Number(self.document_version.into()),
        );
        root.insert(
            "values".to_owned(),
            Value::Object(
                self.unknown_values
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .chain(
                        self.values
                            .iter()
                            .map(|(key, value)| (key.clone(), Value::String(value.clone()))),
                    )
                    .collect(),
            ),
        );
        serde_json::to_string_pretty(&Value::Object(root))
            .map_err(|error| storage_error(format!("could not render settings: {error}")))
    }
}

fn scalar_setting(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn reject_credential_fields(value: &Value) -> CliResult<()> {
    fn walk(value: &Value) -> Option<&str> {
        match value {
            Value::Object(fields) => fields.iter().find_map(|(key, value)| {
                credential_bearing_key(key)
                    .then_some(key.as_str())
                    .or_else(|| walk(value))
            }),
            Value::Array(values) => values.iter().find_map(walk),
            _ => None,
        }
    }
    if let Some(key) = walk(value) {
        return Err(storage_error(format!(
            "credential-bearing field {key:?} is forbidden in settings; use the native vault"
        )));
    }
    Ok(())
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

fn storage_error(message: impl Into<String>) -> CliError {
    CliError::Core(scrozz_core::Error::Storage(message.into()))
}

/// Atomic storage for the aggregate-compatible settings document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// Uses an explicit path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Resolves the platform configuration path.
    pub fn default_location() -> CliResult<Self> {
        if let Ok(path) = std::env::var(SETTINGS_FILE_ENV)
            && !path.trim().is_empty()
        {
            return Ok(Self::new(path));
        }
        let base = dirs::config_dir()
            .or_else(dirs::data_dir)
            .ok_or_else(|| storage_error("no platform config directory is available"))?;
        Ok(Self::new(base.join(APP_DIR).join(FILE_NAME)))
    }

    /// The file this store owns.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads the latest document. An absent file means schema defaults.
    pub fn load(&self) -> CliResult<StoredSettings> {
        self.with_lock(|| self.load_unlocked())
    }

    /// Atomically updates the latest document under a cross-process lock.
    pub fn update(
        &self,
        change: impl FnOnce(&mut StoredSettings) -> CliResult<()>,
    ) -> CliResult<StoredSettings> {
        self.with_lock(|| {
            let mut settings = self.load_unlocked()?;
            change(&mut settings)?;
            self.save_unlocked(&settings)?;
            Ok(settings)
        })
    }

    fn load_unlocked(&self) -> CliResult<StoredSettings> {
        match fs::read_to_string(&self.path) {
            Ok(text) => StoredSettings::from_json(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(StoredSettings::default())
            }
            Err(error) => Err(storage_error(format!(
                "could not read {}: {error}",
                self.path.display()
            ))),
        }
    }

    fn save_unlocked(&self, settings: &StoredSettings) -> CliResult<()> {
        let text = settings.to_json()?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| storage_error(format!("{} has no parent", self.path.display())))?;
        fs::create_dir_all(parent).map_err(|error| {
            storage_error(format!("could not create {}: {error}", parent.display()))
        })?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{FILE_NAME}.{}.{sequence}.tmp",
            std::process::id()
        ));
        let write = || -> std::io::Result<()> {
            let mut file = File::create(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()
        };
        write().map_err(|error| {
            let _ = fs::remove_file(&temporary);
            storage_error(format!("could not write {}: {error}", temporary.display()))
        })?;
        scrozz_shell::replace_file(&temporary, &self.path)
            .inspect_err(|_| {
                let _ = fs::remove_file(&temporary);
            })
            .map_err(CliError::Core)
    }

    fn with_lock<T>(&self, operation: impl FnOnce() -> CliResult<T>) -> CliResult<T> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| storage_error(format!("{} has no parent", self.path.display())))?;
        fs::create_dir_all(parent).map_err(|error| {
            storage_error(format!("could not create {}: {error}", parent.display()))
        })?;
        let lock_path = parent.join(LOCK_FILE_NAME);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| {
                storage_error(format!("could not open {}: {error}", lock_path.display()))
            })?;
        lock.lock_exclusive().map_err(|error| {
            storage_error(format!("could not lock {}: {error}", lock_path.display()))
        })?;
        let result = operation();
        let unlock = fs2::FileExt::unlock(&lock).map_err(|error| {
            storage_error(format!("could not unlock {}: {error}", lock_path.display()))
        });
        match (result, unlock) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }
}

/// Every effective setting as JSON.
#[must_use]
pub fn all_json_from(settings: &StoredSettings) -> Json {
    Json::arr(SETTINGS.iter().map(|setting| {
        let value = settings.value(setting.key).unwrap_or(setting.default);
        let source = if settings.is_user_set(setting.key) {
            "user"
        } else {
            "default"
        };
        setting.to_json_with(value, source)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn settings_round_trip_atomically_and_report_user_source() {
        let root =
            std::env::temp_dir().join(format!("scrozz-settings-round-trip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = SettingsStore::new(root.join("settings.json"));
        store
            .update(|settings| settings.set("cloud.bucket", "screenshots"))
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.value("cloud.bucket"), Some("screenshots"));
        assert!(loaded.is_user_set("cloud.bucket"));
        let json = all_json_from(&loaded).to_compact_string();
        assert!(json.contains(r#""source":"user""#), "{json}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_aggregate_fields_survive_a_known_setting_update() {
        let input = r#"{
            "version": 7,
            "future_root": {"kept": true},
            "values": {
                "future.setting": {"shape": [1, 2, 3]},
                "cloud.bucket": "before"
            },
            "after_capture": {"screenshot": {"copy-to-clipboard": true}}
        }"#;
        let mut settings = StoredSettings::from_json(input).unwrap();
        settings.set("cloud.bucket", "after").unwrap();
        let rendered = settings.to_json().unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["version"], 7);
        assert_eq!(value["future_root"]["kept"], true);
        assert_eq!(value["values"]["future.setting"]["shape"][2], 3);
        assert_eq!(
            value["after_capture"]["screenshot"]["copy-to-clipboard"],
            true
        );
        assert_eq!(value["values"]["cloud.bucket"], "after");
    }

    #[test]
    fn credential_bearing_fields_are_rejected_without_echoing_values() {
        for key in [
            "cloud.secret",
            "access_key_id",
            "AccessKeyId",
            "session_token",
            "session-token",
            "sharePassword",
        ] {
            let input = format!(r#"{{"version":2,"values":{{"{key}":"never-echo-this"}}}}"#);
            let error = StoredSettings::from_json(&input).unwrap_err();
            assert!(error.to_string().contains("native vault"), "{error}");
            assert!(!error.to_string().contains("never-echo-this"), "{error}");
        }
    }
}
