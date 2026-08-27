//! Minimal, isolated persistence for command-line settings.
//!
//! This overlay exists so security-sensitive system toggles are durable before
//! the full settings UI lands. It owns no schema and can be deleted cleanly:
//! every key and validation rule remains in [`crate::settings`].

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use scrozz_core::Error as CoreError;
use serde::{Deserialize, Serialize};

use crate::{
    fault::{CliError, CliResult},
    json::Json,
    settings::{self, Setting},
};

const SCHEMA: u32 = 1;
pub const CONFIG_DIR_ENV: &str = "SCROZZ_CONFIG_DIR";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema: u32,
    values: BTreeMap<String, String>,
}

impl Document {
    fn empty() -> Self {
        Self {
            schema: SCHEMA,
            values: BTreeMap::new(),
        }
    }
}

/// Where a resolved value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The schema default.
    Default,
    /// The user's settings document.
    User,
}

impl Source {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::User => "user",
        }
    }
}

/// One schema entry resolved against the persisted overlay.
#[derive(Debug, Clone)]
pub struct ResolvedSetting {
    setting: Setting,
    value: String,
    source: Source,
}

impl ResolvedSetting {
    /// The schema entry.
    #[must_use]
    pub const fn setting(&self) -> Setting {
        self.setting
    }

    /// The value currently in force.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Whether the value is a default or user override.
    #[must_use]
    pub const fn source(&self) -> Source {
        self.source
    }

    /// Stable command JSON.
    #[must_use]
    pub fn to_json(&self) -> Json {
        self.setting
            .to_json_with(self.value(), self.source().slug())
    }
}

/// The versioned settings document.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// Resolves the per-user settings path.
    ///
    /// `SCROZZ_CONFIG_DIR` names the Scrozz-specific directory directly and is
    /// intended for tests, portable installs, and sandboxes.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the platform has no usable config directory.
    pub fn open_default() -> CliResult<Self> {
        let directory = match std::env::var_os(CONFIG_DIR_ENV) {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => dirs::config_dir()
                .map(|base| base.join("scrozz"))
                .ok_or_else(|| storage("this platform has no user configuration directory"))?,
        };
        Ok(Self::in_directory(directory))
    }

    /// Creates a store rooted in a caller-selected directory.
    #[must_use]
    pub fn in_directory(directory: impl Into<PathBuf>) -> Self {
        Self {
            path: directory.into().join("settings.json"),
        }
    }

    /// The settings document path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolves one setting.
    ///
    /// # Errors
    ///
    /// Returns a storage error for unreadable, malformed, or invalid persisted
    /// data rather than silently falling back.
    pub fn get(&self, setting: &Setting) -> CliResult<ResolvedSetting> {
        let document = self.load()?;
        Ok(resolve(setting, &document))
    }

    /// Resolves every schema entry in stable order.
    ///
    /// # Errors
    ///
    /// As [`Self::get`].
    pub fn all(&self) -> CliResult<Vec<ResolvedSetting>> {
        let document = self.load()?;
        Ok(settings::SETTINGS
            .iter()
            .map(|setting| resolve(setting, &document))
            .collect())
    }

    /// Validates and persists one user override.
    ///
    /// # Errors
    ///
    /// Returns a usage error for an invalid value and a storage error if the
    /// versioned document cannot be read or atomically replaced.
    pub fn set(&self, setting: &Setting, value: &str) -> CliResult<ResolvedSetting> {
        setting.validate(value)?;
        let mut document = self.load()?;
        document
            .values
            .insert(setting.key.to_owned(), value.to_owned());
        self.persist(&document)?;
        Ok(resolve(setting, &document))
    }

    /// Resolves a Boolean key.
    ///
    /// # Errors
    ///
    /// Returns a usage error if `key` is unknown or not Boolean, and a storage
    /// error for corrupt persisted data.
    pub fn boolean(&self, key: &str) -> CliResult<bool> {
        let setting = settings::lookup(key)?;
        if setting.kind != settings::Kind::Bool {
            return Err(CliError::usage(format!("{key} is not a Boolean setting")));
        }
        match self.get(setting)?.value() {
            "true" => Ok(true),
            "false" => Ok(false),
            value => Err(storage(format!(
                "{key} contains invalid persisted Boolean {value:?}"
            ))),
        }
    }

    fn load(&self) -> CliResult<Document> {
        recover_atomic_replace(&self.path).map_err(|error| {
            storage(format!(
                "could not recover {}: {error}",
                self.path.display()
            ))
        })?;
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Document::empty());
            }
            Err(error) => {
                return Err(storage(format!(
                    "could not read {}: {error}",
                    self.path.display()
                )));
            }
        };
        let document: Document = serde_json::from_slice(&bytes).map_err(|error| {
            storage(format!(
                "{} is not valid settings JSON: {error}",
                self.path.display()
            ))
        })?;
        if document.schema != SCHEMA {
            return Err(storage(format!(
                "{} uses settings schema {}, but this build supports {SCHEMA}",
                self.path.display(),
                document.schema
            )));
        }
        for (key, value) in &document.values {
            if let Ok(setting) = settings::lookup(key)
                && let Err(error) = setting.validate(value)
            {
                return Err(storage(format!(
                    "{} contains an invalid value for {key}: {error}",
                    self.path.display()
                )));
            }
        }
        Ok(document)
    }

    fn persist(&self, document: &Document) -> CliResult<()> {
        let mut bytes = serde_json::to_vec_pretty(document)
            .map_err(|error| storage(format!("could not encode settings: {error}")))?;
        bytes.push(b'\n');
        atomic_replace(&self.path, &bytes).map_err(|error| {
            storage(format!(
                "could not replace {} atomically: {error}",
                self.path.display()
            ))
        })
    }
}

fn resolve(setting: &Setting, document: &Document) -> ResolvedSetting {
    match document.values.get(setting.key) {
        Some(value) => ResolvedSetting {
            setting: *setting,
            value: value.clone(),
            source: Source::User,
        },
        None => ResolvedSetting {
            setting: *setting,
            value: setting.default.to_owned(),
            source: Source::Default,
        },
    }
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("settings path has no parent"))?;
    fs::create_dir_all(parent)?;
    recover_atomic_replace(path)?;

    let temporary = loop {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".settings-{}-{sequence}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(bytes)?;
                file.sync_all()?;
                drop(file);
                break candidate;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    };

    let backup = backup_path(path)?;
    let result = (|| {
        if file_exists(&backup)? {
            remove_regular_file(&backup)?;
            sync_directory(parent)?;
        }
        if file_exists(path)? {
            require_regular_file(path)?;
            fs::rename(path, &backup)?;
            sync_directory(parent)?;
        }
        fs::rename(&temporary, path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn recover_atomic_replace(path: &Path) -> std::io::Result<()> {
    let backup = backup_path(path)?;
    match (file_exists(path)?, file_exists(&backup)?) {
        (false, true) => {
            require_regular_file(&backup)?;
            fs::rename(&backup, path)?;
            sync_directory(
                path.parent()
                    .ok_or_else(|| std::io::Error::other("settings path has no parent"))?,
            )
        }
        (true, backup_exists) => {
            require_regular_file(path)?;
            if backup_exists {
                require_regular_file(&backup)?;
            }
            Ok(())
        }
        (false, false) => Ok(()),
    }
}

fn backup_path(path: &Path) -> std::io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("settings path has no parent"))?;
    let mut name = OsString::from(
        path.file_name()
            .ok_or_else(|| std::io::Error::other("settings path has no file name"))?,
    );
    name.push(".previous");
    Ok(parent.join(name))
}

fn file_exists(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn require_regular_file(path: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_file() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{} is not a regular settings file",
            path.display()
        )))
    }
}

fn remove_regular_file(path: &Path) -> std::io::Result<()> {
    require_regular_file(path)?;
    fs::remove_file(path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?
        .sync_all()
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}
fn storage(message: impl Into<String>) -> CliError {
    CliError::Core(CoreError::Storage(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!(
            "scrozz-settings-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn defaults_do_not_create_a_file() {
        let directory = scratch();
        let store = SettingsStore::in_directory(&directory);
        let value = store
            .get(settings::lookup("capture.format").unwrap())
            .unwrap();
        assert_eq!(value.value(), "png");
        assert_eq!(value.source(), Source::Default);
        assert!(!store.path().exists());
    }

    #[test]
    fn a_user_value_survives_reopening() {
        let directory = scratch();
        let setting = settings::lookup("system.url-scheme-enabled").unwrap();
        let store = SettingsStore::in_directory(&directory);
        store.set(setting, "true").unwrap();

        let reopened = SettingsStore::in_directory(&directory);
        let value = reopened.get(setting).unwrap();
        assert_eq!(value.value(), "true");
        assert_eq!(value.source(), Source::User);
        assert!(reopened.boolean(setting.key).unwrap());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn rejected_values_never_replace_the_document() {
        let directory = scratch();
        let setting = settings::lookup("capture.quality").unwrap();
        let store = SettingsStore::in_directory(&directory);
        store.set(setting, "80").unwrap();
        let before = fs::read(store.path()).unwrap();
        assert!(store.set(setting, "101").is_err());
        assert_eq!(fs::read(store.path()).unwrap(), before);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn malformed_or_future_documents_fail_loudly() {
        for bytes in [
            br#"not json"#.as_slice(),
            br#"{"schema":99,"values":{}}"#.as_slice(),
            br#"{"schema":1,"values":{"capture.quality":"101"}}"#.as_slice(),
        ] {
            let directory = scratch();
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("settings.json"), bytes).unwrap();
            let error = SettingsStore::in_directory(&directory).all().unwrap_err();
            assert_eq!(error.exit(), crate::exit::Exit::Storage);
            let _ = fs::remove_dir_all(directory);
        }
    }

    #[test]
    fn unknown_keys_are_preserved_for_forward_compatibility() {
        let directory = scratch();
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("settings.json"),
            br#"{"schema":1,"values":{"future.example":"kept"}}"#,
        )
        .unwrap();
        let store = SettingsStore::in_directory(&directory);
        store
            .set(settings::lookup("capture.format").unwrap(), "webp")
            .unwrap();
        let contents = fs::read_to_string(store.path()).unwrap();
        assert!(contents.contains("future.example"), "{contents}");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn resolved_json_reports_the_real_source() {
        let directory = scratch();
        let store = SettingsStore::in_directory(&directory);
        let setting = settings::lookup("system.url-scheme-enabled").unwrap();
        assert!(
            store
                .get(setting)
                .unwrap()
                .to_json()
                .to_compact_string()
                .contains(r#""source":"default""#)
        );
        store.set(setting, "true").unwrap();
        assert!(
            store
                .get(setting)
                .unwrap()
                .to_json()
                .to_compact_string()
                .contains(r#""source":"user""#)
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn interrupted_replacement_restores_the_previous_document() {
        let directory = scratch();
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("settings.json");
        let backup = backup_path(&path).unwrap();
        fs::write(
            &backup,
            br#"{"schema":1,"values":{"system.url-scheme-enabled":"true"}}"#,
        )
        .unwrap();

        let store = SettingsStore::in_directory(&directory);
        assert!(store.boolean("system.url-scheme-enabled").unwrap());
        assert!(path.is_file());
        assert!(!backup.exists());
        let _ = fs::remove_dir_all(directory);
    }
}
