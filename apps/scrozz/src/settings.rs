//! The settings schema and the small durable after-capture policy.
//!
//! # Why the schema lives here and not in the store
//!
//! Most settings are still schema-only. The independent Recording after-capture
//! actions are persisted now because both the GUI startup path and the CLI need
//! one authoritative policy. Unknown or malformed state is an error rather than
//! a reason to silently open a window the user disabled.
//!
//! # Naming
//!
//! `area.key-name`: a dotted area, hyphenated words. It matches the action slugs
//! the hotkey commands already use, and it survives being a TOML table, a JSON
//! object path and a command-line argument without being quoted.

use crate::{
    fault::{CliError, CliResult},
    hotkey_config::Accelerator,
    json::Json,
};
use fs2::FileExt as _;
use scrozz_core::{Error, Result};
use scrozz_record::AfterCaptureSettings;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const SETTINGS_VERSION: u64 = 2;
const LEGACY_SETTINGS_VERSION: u64 = 1;
const APP_DIR: &str = "Scrozz";
const SETTINGS_FILE: &str = "settings.json";
const SETTINGS_LOCK_FILE: &str = ".settings.lock";
const SETTINGS_FILE_ENV: &str = "SCROZZ_SETTINGS_FILE";
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
const RECORDING_OVERLAY_KEY: &str = "record.show-recent-captures-overlay";
const RECORDING_EDITOR_KEY: &str = "record.open-editor";
const RECORDING_OVERLAY_FIELD: &str = "show-recent-captures-overlay";
const RECORDING_EDITOR_FIELD: &str = "open-editor";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct LoadedSettings {
    actions: AfterCaptureSettings,
    persisted: bool,
    document: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AfterCaptureStore {
    path: PathBuf,
}

impl AfterCaptureStore {
    fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn default_location() -> Result<Self> {
        if let Ok(path) = std::env::var(SETTINGS_FILE_ENV)
            && !path.trim().is_empty()
        {
            return Ok(Self::new(path));
        }
        let base = dirs::config_dir().or_else(dirs::data_dir).ok_or_else(|| {
            Error::Storage(
                "no platform configuration directory is available for after-capture settings"
                    .to_owned(),
            )
        })?;
        Ok(Self::new(base.join(APP_DIR).join(SETTINGS_FILE)))
    }

    fn load(&self) -> Result<(AfterCaptureSettings, bool)> {
        self.with_lock(|| {
            let loaded = self.load_unlocked()?;
            Ok((loaded.actions, loaded.persisted))
        })
    }

    fn load_unlocked(&self) -> Result<LoadedSettings> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadedSettings {
                    actions: AfterCaptureSettings::default(),
                    persisted: false,
                    document: default_settings_document(),
                });
            }
            Err(error) => {
                return Err(Error::Storage(format!(
                    "could not open after-capture settings {}: {error}",
                    self.path.display()
                )));
            }
        };
        let mut bytes = Vec::new();
        file.take(MAX_SETTINGS_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                Error::Storage(format!(
                    "could not read after-capture settings {}: {error}",
                    self.path.display()
                ))
            })?;
        if bytes.len() as u64 > MAX_SETTINGS_BYTES {
            return Err(Error::Storage(format!(
                "after-capture settings {} exceed the {} byte limit",
                self.path.display(),
                MAX_SETTINGS_BYTES
            )));
        }
        let document: Value = serde_json::from_slice(&bytes).map_err(|error| {
            Error::Storage(format!(
                "could not decode after-capture settings {}: {error}",
                self.path.display()
            ))
        })?;
        let version = document
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                Error::Storage(format!(
                    "after-capture settings {} have no numeric version",
                    self.path.display()
                ))
            })?;
        let actions = match version {
            SETTINGS_VERSION => read_modern_actions(&document, &self.path)?,
            LEGACY_SETTINGS_VERSION => read_legacy_actions(&document, &self.path)?,
            version => {
                return Err(Error::Storage(format!(
                    "after-capture settings {} use unsupported version {version}",
                    self.path.display()
                )));
            }
        };
        Ok(LoadedSettings {
            actions,
            persisted: true,
            document: if version == SETTINGS_VERSION {
                document
            } else {
                default_settings_document()
            },
        })
    }

    fn update(
        &self,
        change: impl FnOnce(&mut AfterCaptureSettings),
    ) -> Result<AfterCaptureSettings> {
        self.with_lock(|| {
            let mut loaded = self.load_unlocked()?;
            change(&mut loaded.actions);
            write_actions(&mut loaded.document, loaded.actions)?;
            self.save_document_unlocked(&loaded.document)?;
            Ok(loaded.actions)
        })
    }

    fn save_document_unlocked(&self, document: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(document).map_err(|error| {
            Error::Storage(format!("could not encode after-capture settings: {error}"))
        })?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_SETTINGS_BYTES {
            return Err(Error::Storage(format!(
                "updated after-capture settings would exceed the {MAX_SETTINGS_BYTES} byte limit"
            )));
        }

        let parent = self.path.parent().ok_or_else(|| {
            Error::Storage(format!(
                "after-capture settings path {} has no parent directory",
                self.path.display()
            ))
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            Error::Storage(format!(
                "could not create settings directory {}: {error}",
                parent.display()
            ))
        })?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".{SETTINGS_FILE}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let result = write_and_replace(&temp, &self.path, &bytes);
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn with_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let parent = self.path.parent().ok_or_else(|| {
            Error::Storage(format!(
                "after-capture settings path {} has no parent directory",
                self.path.display()
            ))
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            Error::Storage(format!(
                "could not create settings directory {}: {error}",
                parent.display()
            ))
        })?;
        let lock_path = parent.join(SETTINGS_LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| {
                Error::Storage(format!(
                    "could not open settings lock {}: {error}",
                    lock_path.display()
                ))
            })?;
        lock.lock_exclusive().map_err(|error| {
            Error::Storage(format!(
                "could not lock after-capture settings {}: {error}",
                lock_path.display()
            ))
        })?;
        let result = operation();
        let unlock = lock.unlock().map_err(|error| {
            Error::Storage(format!(
                "could not unlock after-capture settings {}: {error}",
                lock_path.display()
            ))
        });
        match (result, unlock) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(primary), Err(unlock)) => {
                Err(Error::Storage(format!("{primary}; additionally, {unlock}")))
            }
        }
    }
}

fn read_modern_actions(document: &Value, path: &Path) -> Result<AfterCaptureSettings> {
    let recording = document
        .get("after_capture")
        .and_then(|value| value.get("recording"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            Error::Storage(format!(
                "after-capture settings {} have no after_capture.recording object",
                path.display()
            ))
        })?;
    Ok(AfterCaptureSettings {
        recent_captures_overlay: bool_field(recording, RECORDING_OVERLAY_FIELD, path)?,
        open_editor: bool_field(recording, RECORDING_EDITOR_FIELD, path)?,
    })
}

fn read_legacy_actions(document: &Value, path: &Path) -> Result<AfterCaptureSettings> {
    let actions = document
        .get("after_capture")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            Error::Storage(format!(
                "legacy after-capture settings {} have no after_capture object",
                path.display()
            ))
        })?;
    Ok(AfterCaptureSettings {
        recent_captures_overlay: bool_field(actions, "recording_overlay", path)?,
        open_editor: bool_field(actions, "recording_open_editor", path)?,
    })
}

fn bool_field(object: &serde_json::Map<String, Value>, field: &str, path: &Path) -> Result<bool> {
    object.get(field).and_then(Value::as_bool).ok_or_else(|| {
        Error::Storage(format!(
            "after-capture settings {} require boolean field {field:?}",
            path.display()
        ))
    })
}

fn write_actions(document: &mut Value, actions: AfterCaptureSettings) -> Result<()> {
    let recording = document
        .get_mut("after_capture")
        .and_then(|value| value.get_mut("recording"))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            Error::Storage(
                "settings document has no writable after_capture.recording object".to_owned(),
            )
        })?;
    recording.insert(
        RECORDING_OVERLAY_FIELD.to_owned(),
        Value::Bool(actions.recent_captures_overlay),
    );
    recording.insert(
        RECORDING_EDITOR_FIELD.to_owned(),
        Value::Bool(actions.open_editor),
    );
    Ok(())
}

fn default_settings_document() -> Value {
    json!({
        "after_capture": {
            "recording": {
                "copy-to-clipboard": false,
                "open-editor": false,
                "pin-to-screen": false,
                "save-automatically": false,
                "show-recent-captures-overlay": true,
                "upload-and-copy-link": false
            },
            "screenshot": {
                "copy-to-clipboard": true,
                "open-editor": true,
                "pin-to-screen": false,
                "save-automatically": true,
                "show-recent-captures-overlay": true,
                "upload-and-copy-link": false
            }
        },
        "values": {},
        "version": SETTINGS_VERSION
    })
}

fn write_and_replace(temp: &Path, destination: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create_private_file(temp)?;
    file.write_all(bytes).map_err(|error| {
        Error::Storage(format!(
            "could not write settings staging file {}: {error}",
            temp.display()
        ))
    })?;
    file.sync_all().map_err(|error| {
        Error::Storage(format!(
            "could not sync settings staging file {}: {error}",
            temp.display()
        ))
    })?;
    drop(file);
    replace_file(temp, destination)?;
    Ok(())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| {
            Error::Storage(format!(
                "could not create settings staging file {}: {error}",
                path.display()
            ))
        })
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            Error::Storage(format!(
                "could not create settings staging file {}: {error}",
                path.display()
            ))
        })
}

#[cfg(not(target_os = "windows"))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).map_err(|error| {
        Error::Storage(format!(
            "could not publish after-capture settings {}: {error}",
            destination.display()
        ))
    })?;
    let parent = destination.parent().ok_or_else(|| {
        Error::Storage(format!(
            "after-capture settings path {} has no parent directory",
            destination.display()
        ))
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            Error::Storage(format!(
                "could not sync settings directory {}: {error}",
                parent.display()
            ))
        })
}

#[cfg(target_os = "windows")]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::{
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::PCWSTR,
    };

    let destination_label = destination.display().to_string();
    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: both buffers are NUL-terminated and live for the duration of the call.
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| {
        Error::Storage(format!(
            "could not publish after-capture settings {}: {error}",
            destination_label
        ))
    })
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
    /// The JSON representation using the shipped default.
    #[must_use]
    pub fn to_json(self) -> Json {
        self.to_json_with(self.default, "default")
    }

    fn to_json_with(self, value: &str, source: &str) -> Json {
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

/// Loads the persisted Recording after-capture action matrix.
///
/// # Errors
///
/// Returns a storage error when the settings file is unreadable, oversized,
/// malformed, or from an unsupported future schema.
pub fn load_recording_after_capture() -> Result<AfterCaptureSettings> {
    AfterCaptureStore::default_location()?
        .load()
        .map(|(value, _)| value)
}

/// Persists one setting when this narrow store owns it.
///
/// Returns `false` for settings that remain schema-only.
///
/// # Errors
///
/// Returns a usage error for an invalid value or a storage error when the
/// current policy cannot be read or atomically replaced.
pub fn persist(setting: &Setting, value: &str) -> CliResult<bool> {
    persist_to(&AfterCaptureStore::default_location()?, setting, value)
}

fn persist_to(store: &AfterCaptureStore, setting: &Setting, value: &str) -> CliResult<bool> {
    if !matches!(setting.key, RECORDING_OVERLAY_KEY | RECORDING_EDITOR_KEY) {
        return Ok(false);
    }
    setting.validate(value)?;
    let enabled = value == "true";
    store.update(|actions| match setting.key {
        RECORDING_OVERLAY_KEY => actions.recent_captures_overlay = enabled,
        RECORDING_EDITOR_KEY => actions.open_editor = enabled,
        _ => unreachable!("persisted settings were filtered above"),
    })?;
    Ok(true)
}

fn resolved_value(setting: &Setting) -> CliResult<(String, &'static str)> {
    if !matches!(setting.key, RECORDING_OVERLAY_KEY | RECORDING_EDITOR_KEY) {
        return Ok((setting.default.to_owned(), "default"));
    }
    let store = AfterCaptureStore::default_location()?;
    resolved_value_from(&store, setting)
}

fn resolved_value_from(
    store: &AfterCaptureStore,
    setting: &Setting,
) -> CliResult<(String, &'static str)> {
    if !matches!(setting.key, RECORDING_OVERLAY_KEY | RECORDING_EDITOR_KEY) {
        return Ok((setting.default.to_owned(), "default"));
    }
    let (actions, persisted) = store.load()?;
    let value = match setting.key {
        RECORDING_OVERLAY_KEY => actions.recent_captures_overlay,
        RECORDING_EDITOR_KEY => actions.open_editor,
        _ => unreachable!("persisted settings were filtered above"),
    };
    Ok((
        value.to_string(),
        if persisted { "user" } else { "default" },
    ))
}

/// One setting as JSON with its current persisted value.
pub fn resolved_json(setting: Setting) -> CliResult<Json> {
    let (value, source) = resolved_value(&setting)?;
    Ok(setting.to_json_with(&value, source))
}

/// One setting's current value.
pub fn value(setting: &Setting) -> CliResult<String> {
    resolved_value(setting).map(|(value, _)| value)
}

/// Every setting as JSON with persisted values resolved.
pub fn resolved_all_json() -> CliResult<Json> {
    SETTINGS
        .iter()
        .copied()
        .map(resolved_json)
        .collect::<CliResult<Vec<_>>>()
        .map(Json::arr)
}

/// Every setting as aligned text with persisted values resolved.
pub fn resolved_all_human() -> CliResult<String> {
    let width = SETTINGS.iter().map(|s| s.key.len()).max().unwrap_or(0);
    SETTINGS
        .iter()
        .map(|setting| value(setting).map(|value| format!("{:width$}  {value}", setting.key)))
        .collect::<CliResult<Vec<_>>>()
        .map(|lines| lines.join("\n"))
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
        key: RECORDING_OVERLAY_KEY,
        kind: Kind::Bool,
        default: "true",
        description: "Add completed recordings to the Recent Captures Overlay.",
    },
    Setting {
        key: RECORDING_EDITOR_KEY,
        kind: Kind::Bool,
        default: "false",
        description: "Open the Video Editor after a recording completes.",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "scrozz-after-capture-{name}-{}-{sequence}",
            std::process::id()
        ))
    }

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_after_capture_settings_use_overlay_only_defaults() {
        let root = scratch("missing");
        let _cleanup = Scratch(root.clone());
        let store = AfterCaptureStore::new(root.join(SETTINGS_FILE));

        let (actions, persisted) = store.load().unwrap();

        assert_eq!(actions, AfterCaptureSettings::default());
        assert!(actions.recent_captures_overlay);
        assert!(!actions.open_editor);
        assert!(!persisted);
    }

    #[test]
    fn recording_after_capture_actions_persist_independently() {
        let root = scratch("matrix");
        let _cleanup = Scratch(root.clone());
        let store = AfterCaptureStore::new(root.join(SETTINGS_FILE));
        let overlay = lookup(RECORDING_OVERLAY_KEY).unwrap();
        let editor = lookup(RECORDING_EDITOR_KEY).unwrap();

        assert!(persist_to(&store, editor, "true").unwrap());
        let (actions, persisted) = store.load().unwrap();
        assert!(persisted);
        assert!(actions.recent_captures_overlay);
        assert!(actions.open_editor);

        assert!(persist_to(&store, overlay, "false").unwrap());
        let (actions, _) = store.load().unwrap();
        assert!(!actions.recent_captures_overlay);
        assert!(actions.open_editor);
        assert_eq!(
            resolved_value_from(&store, overlay).unwrap(),
            ("false".to_owned(), "user")
        );
    }

    #[test]
    fn modern_settings_updates_preserve_every_unowned_value() {
        let root = scratch("preserve-modern");
        let _cleanup = Scratch(root.clone());
        fs::create_dir_all(&root).unwrap();
        let path = root.join(SETTINGS_FILE);
        let store = AfterCaptureStore::new(&path);
        let mut document = default_settings_document();
        document["future-root"] = json!({"keep": 7});
        document["after_capture"]["future-media"] = json!({"keep": true});
        document["after_capture"]["recording"]["future-action"] = json!("keep");
        document["values"] = json!({"capture.folder": "/Volumes/Archive"});
        let expected_screenshot = document["after_capture"]["screenshot"].clone();
        fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        persist_to(&store, lookup(RECORDING_EDITOR_KEY).unwrap(), "true").unwrap();

        let updated: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(updated["future-root"], json!({"keep": 7}));
        assert_eq!(
            updated["after_capture"]["future-media"],
            json!({"keep": true})
        );
        assert_eq!(
            updated["after_capture"]["recording"]["future-action"],
            json!("keep")
        );
        assert_eq!(
            updated["values"],
            json!({"capture.folder": "/Volumes/Archive"})
        );
        assert_eq!(updated["after_capture"]["screenshot"], expected_screenshot);
        assert_eq!(
            updated["after_capture"]["recording"][RECORDING_EDITOR_FIELD],
            Value::Bool(true)
        );
    }

    #[test]
    fn concurrent_updates_do_not_lose_an_independent_action() {
        let root = scratch("concurrent");
        let _cleanup = Scratch(root.clone());
        let store = AfterCaptureStore::new(root.join(SETTINGS_FILE));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let overlay_store = store.clone();
        let overlay_barrier = std::sync::Arc::clone(&barrier);
        let overlay = std::thread::spawn(move || {
            overlay_barrier.wait();
            persist_to(
                &overlay_store,
                lookup(RECORDING_OVERLAY_KEY).unwrap(),
                "false",
            )
            .unwrap();
        });
        let editor_store = store.clone();
        let editor_barrier = std::sync::Arc::clone(&barrier);
        let editor = std::thread::spawn(move || {
            editor_barrier.wait();
            persist_to(&editor_store, lookup(RECORDING_EDITOR_KEY).unwrap(), "true").unwrap();
        });
        barrier.wait();
        overlay.join().unwrap();
        editor.join().unwrap();

        let (actions, _) = store.load().unwrap();
        assert!(!actions.recent_captures_overlay);
        assert!(actions.open_editor);
    }

    #[test]
    fn expanding_unknown_json_cannot_publish_an_unreadable_document() {
        let root = scratch("serialized-limit");
        let _cleanup = Scratch(root.clone());
        fs::create_dir_all(&root).unwrap();
        let path = root.join(SETTINGS_FILE);
        let store = AfterCaptureStore::new(&path);
        let mut document = default_settings_document();
        document["future"] = Value::Array(vec![Value::Bool(false); 8_000]);
        let compact = serde_json::to_vec(&document).unwrap();
        assert!(compact.len() as u64 <= MAX_SETTINGS_BYTES);
        fs::write(&path, &compact).unwrap();

        let error = persist_to(&store, lookup(RECORDING_EDITOR_KEY).unwrap(), "true")
            .unwrap_err()
            .to_string();

        assert!(error.contains("byte limit"), "{error}");
        let (actions, _) = store.load().unwrap();
        assert!(!actions.open_editor);
        assert_eq!(fs::read(path).unwrap(), compact);
    }

    #[test]
    fn temporary_version_one_state_migrates_into_the_modern_matrix() {
        let root = scratch("migrate-v1");
        let _cleanup = Scratch(root.clone());
        fs::create_dir_all(&root).unwrap();
        let path = root.join(SETTINGS_FILE);
        let store = AfterCaptureStore::new(&path);
        fs::write(
            &path,
            br#"{"version":1,"after_capture":{"recording_overlay":false,"recording_open_editor":true}}"#,
        )
        .unwrap();

        let (actions, persisted) = store.load().unwrap();
        assert!(persisted);
        assert!(!actions.recent_captures_overlay);
        assert!(actions.open_editor);
        persist_to(&store, lookup(RECORDING_OVERLAY_KEY).unwrap(), "true").unwrap();

        let migrated: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(migrated["version"], SETTINGS_VERSION);
        assert_eq!(
            migrated["after_capture"]["recording"][RECORDING_OVERLAY_FIELD],
            Value::Bool(true)
        );
        assert_eq!(
            migrated["after_capture"]["recording"][RECORDING_EDITOR_FIELD],
            Value::Bool(true)
        );
    }

    #[test]
    fn malformed_or_oversized_after_capture_state_is_not_silently_accepted() {
        let root = scratch("invalid");
        let _cleanup = Scratch(root.clone());
        fs::create_dir_all(&root).unwrap();
        let path = root.join(SETTINGS_FILE);
        let store = AfterCaptureStore::new(&path);

        fs::write(&path, br#"{"version":1,"after_capture":"truncated"#).unwrap();
        assert!(matches!(store.load(), Err(Error::Storage(_))));

        fs::write(&path, vec![b'x'; MAX_SETTINGS_BYTES as usize + 1]).unwrap();
        let error = store.load().unwrap_err().to_string();
        assert!(error.contains("byte limit"), "{error}");
    }

    #[test]
    fn failed_publish_removes_owned_settings_staging_file() {
        let root = scratch("cleanup");
        let _cleanup = Scratch(root.clone());
        let destination = root.join(SETTINGS_FILE);
        fs::create_dir_all(&destination).unwrap();
        let store = AfterCaptureStore::new(&destination);

        assert!(
            store
                .save_document_unlocked(&default_settings_document())
                .is_err()
        );
        let entries = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(entries, [SETTINGS_FILE]);
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
