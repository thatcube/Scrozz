//! Versioned persistence for [`crate::settings`].
//!
//! The document contains only overrides. Defaults remain in the schema, so a
//! new release can change an untouched default without rewriting every user's
//! file. Writes use a temporary file in the same directory followed by rename;
//! readers therefore see either the old complete document or the new one.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use scrozz_core::Error as CoreError;
use serde::{Deserialize, Serialize};

use crate::{
    fault::{CliError, CliResult},
    json::Json,
    settings::{self, Setting, ValueSource},
};

/// Moves settings persistence into an isolated directory.
///
/// Tests and portable packages use this instead of writing to the user's real
/// configuration directory.
pub const CONFIG_DIR_ENV: &str = "SCROZZ_CONFIG_DIR";

/// The settings document format this build writes.
pub const CURRENT_VERSION: u32 = 3;

const FILE_NAME: &str = "settings.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    #[serde(default)]
    values: BTreeMap<String, String>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            values: BTreeMap::new(),
        }
    }
}

/// The loaded settings document.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
    document: Document,
}

impl SettingsStore {
    /// Loads the default per-user settings path.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the configuration directory is unavailable
    /// or an existing document cannot be read, migrated or validated.
    pub fn load() -> CliResult<Self> {
        Self::open(settings_path()?)
    }

    /// Compatibility spelling used by the system-integration layer.
    ///
    /// # Errors
    ///
    /// As [`Self::load`].
    pub fn open_default() -> CliResult<Self> {
        Self::load()
    }

    /// Loads an explicit path.
    ///
    /// This is public so embedding code and tests can keep persistence isolated.
    ///
    /// # Errors
    ///
    /// Returns a storage error for an unreadable or invalid document.
    pub fn open(path: impl Into<PathBuf>) -> CliResult<Self> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self {
                path,
                document: Document::default(),
            });
        }

        let bytes = fs::read(&path).map_err(|error| {
            storage(format!(
                "cannot read settings from {}: {error}",
                path.display()
            ))
        })?;
        let mut document: Document = serde_json::from_slice(&bytes).map_err(|error| {
            storage(format!(
                "cannot decode settings from {}: {error}",
                path.display()
            ))
        })?;
        let migrated = migrate(&mut document)?;
        validate_document(&document, &path)?;

        let store = Self { path, document };
        if migrated {
            store.write_document(&store.document)?;
        }
        Ok(store)
    }

    /// The file this store reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolves a schema entry to its current value and provenance.
    #[must_use]
    pub fn resolve<'a>(&'a self, setting: &'static Setting) -> (&'a str, ValueSource) {
        self.document
            .values
            .get(setting.key)
            .map_or((setting.default, ValueSource::Default), |value| {
                (value.as_str(), ValueSource::User)
            })
    }

    /// Looks up and resolves one setting.
    ///
    /// # Errors
    ///
    /// Returns a usage error for an unknown key.
    pub fn get(&self, key: &str) -> CliResult<(&'static Setting, &str, ValueSource)> {
        let setting = settings::lookup(key)?;
        let (value, source) = self.resolve(setting);
        Ok((setting, value, source))
    }

    /// Reads a boolean setting.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown or non-boolean key.
    pub fn boolean(&self, key: &str) -> CliResult<bool> {
        let (setting, value, _) = self.get(key)?;
        if setting.kind != settings::Kind::Bool {
            return Err(CliError::usage(format!("{key} is not a boolean setting")));
        }
        Ok(value == "true")
    }

    /// Resolves the persisted OCR rows into the runtime adapter.
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::InvalidRequest`] for contradictory or
    /// malformed language settings.
    pub fn ocr_config(&self) -> CliResult<scrozz_ocr::RuntimeConfig> {
        ocr_config(&self.document)
    }

    /// The stable provenance token for one key.
    ///
    /// # Errors
    ///
    /// Returns a usage error for an unknown key.
    pub fn source(&self, key: &str) -> CliResult<&'static str> {
        self.get(key).map(|(_, _, source)| source.slug())
    }

    /// Persists one user override.
    ///
    /// Validation happens before memory or disk changes.
    ///
    /// # Errors
    ///
    /// Returns a usage error for an unknown key or invalid value, and a storage
    /// error if the atomic write fails.
    pub fn set(&mut self, key: &str, value: &str) -> CliResult<ValueSource> {
        let setting = settings::lookup(key)?;
        setting.validate(value)?;

        let mut next = self.document.clone();
        next.values.insert(key.to_owned(), value.to_owned());
        ocr_config(&next)?;
        self.write_document(&next)?;
        self.document = next;
        Ok(ValueSource::User)
    }

    /// Resets one key to its schema default.
    ///
    /// # Errors
    ///
    /// Returns a usage error for an unknown key and a storage error if the
    /// atomic write fails.
    pub fn reset(&mut self, key: &str) -> CliResult<bool> {
        settings::lookup(key)?;
        if !self.document.values.contains_key(key) {
            return Ok(false);
        }

        let mut next = self.document.clone();
        next.values.remove(key);
        self.write_document(&next)?;
        self.document = next;
        Ok(true)
    }

    /// Resets every override.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the atomic write fails.
    pub fn reset_all(&mut self) -> CliResult<usize> {
        let count = self.document.values.len();
        if count == 0 {
            return Ok(0);
        }

        let next = Document::default();
        self.write_document(&next)?;
        self.document = next;
        Ok(count)
    }

    /// Applies a complete set of validated edits in one atomic write.
    ///
    /// # Errors
    ///
    /// Returns before writing when any key or value is invalid.
    pub fn apply(&mut self, edits: &[(String, String)]) -> CliResult<()> {
        let mut next = self.document.clone();
        for (key, value) in edits {
            let setting = settings::lookup(key)?;
            setting.validate(value)?;
            next.values.insert(key.clone(), value.clone());
        }
        ocr_config(&next)?;
        self.write_document(&next)?;
        self.document = next;
        Ok(())
    }

    /// Every resolved setting in stable CLI JSON order.
    #[must_use]
    pub fn all_json(&self) -> Json {
        Json::arr(settings::SETTINGS.iter().map(|setting| {
            let (value, source) = self.resolve(setting);
            setting.to_json(value, source)
        }))
    }

    /// Every resolved setting as aligned text.
    #[must_use]
    pub fn all_human(&self) -> String {
        let width = settings::SETTINGS
            .iter()
            .map(|setting| setting.key.len())
            .max()
            .unwrap_or(0);
        settings::SETTINGS
            .iter()
            .map(|setting| {
                let (value, _) = self.resolve(setting);
                format!("{:width$}  {value}", setting.key)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn write_document(&self, document: &Document) -> CliResult<()> {
        let parent = self.path.parent().ok_or_else(|| {
            storage(format!(
                "settings path {} has no parent directory",
                self.path.display()
            ))
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            storage(format!(
                "cannot create settings directory {}: {error}",
                parent.display()
            ))
        })?;

        let mut bytes = serde_json::to_vec_pretty(document)
            .map_err(|error| storage(format!("cannot encode settings: {error}")))?;
        bytes.push(b'\n');
        atomic_write(&self.path, &bytes)
    }
}

/// Resolves the per-user settings file path.
///
/// # Errors
///
/// Returns a storage error when the platform has no configuration directory.
pub fn settings_path() -> CliResult<PathBuf> {
    if let Some(directory) = std::env::var_os(CONFIG_DIR_ENV)
        && !directory.is_empty()
    {
        return Ok(PathBuf::from(directory).join(FILE_NAME));
    }
    dirs::config_dir()
        .map(|directory| directory.join("scrozz").join(FILE_NAME))
        .ok_or_else(|| storage("this system does not expose a user configuration directory"))
}

fn migrate(document: &mut Document) -> CliResult<bool> {
    if document.version == 0 {
        return Err(storage("settings document version 0 is invalid"));
    }
    if document.version > CURRENT_VERSION {
        return Err(storage(format!(
            "settings document version {} is newer than this build supports ({CURRENT_VERSION})",
            document.version
        )));
    }

    let original = document.version;
    while document.version < CURRENT_VERSION {
        match document.version {
            1 => document.version = 2,
            2 => {
                if let Some(value) = document.values.remove("system.url-scheme") {
                    document
                        .values
                        .entry("system.url-scheme-enabled".to_owned())
                        .or_insert(value);
                }
                document.values.remove("update.check-automatically");
                document.values.remove("update.channel");
                document.version = 3;
            }
            version => {
                return Err(storage(format!(
                    "no migration exists for settings document version {version}"
                )));
            }
        }
    }
    Ok(document.version != original)
}

fn validate_document(document: &Document, path: &Path) -> CliResult<()> {
    for (key, value) in &document.values {
        let setting = settings::lookup(key).map_err(|_| {
            storage(format!(
                "{} contains unknown setting {key:?}",
                path.display()
            ))
        })?;
        setting.validate(value).map_err(|error| {
            storage(format!(
                "{} contains invalid value for {key}: {error}",
                path.display()
            ))
        })?;
    }
    ocr_config(document).map_err(|error| {
        storage(format!(
            "{} contains invalid OCR settings: {error}",
            path.display()
        ))
    })?;
    Ok(())
}

fn ocr_config(document: &Document) -> CliResult<scrozz_ocr::RuntimeConfig> {
    let value = |key| -> CliResult<&str> {
        let setting = settings::lookup(key)?;
        Ok(document
            .values
            .get(key)
            .map_or(setting.default, String::as_str))
    };
    scrozz_ocr::RuntimeConfig::from_settings(
        value(scrozz_ocr::LANGUAGES_KEY)?,
        value(scrozz_ocr::AUTO_DETECT_LANGUAGE_KEY)? == "true",
        value(scrozz_ocr::KEEP_LINE_BREAKS_KEY)? == "true",
        value(scrozz_ocr::DETECT_LINKS_KEY)? == "true",
    )
    .map_err(Into::into)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> CliResult<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .ok_or_else(|| storage(format!("{} has no parent directory", path.display())))?;
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(FILE_NAME);

    for _ in 0..32 {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".{stem}.{}.{}.tmp", std::process::id(), sequence));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }

        let mut file = match options.open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(storage(format!(
                    "cannot create temporary settings file {}: {error}",
                    temp.display()
                )));
            }
        };

        let result = (|| -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            replace_file(&temp, path)
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temp);
            return Err(storage(format!(
                "cannot replace settings file {}: {error}",
                path.display()
            )));
        }
        return Ok(());
    }

    Err(storage(format!(
        "cannot allocate a temporary file beside {}",
        path.display()
    )))
}

#[cfg(not(target_os = "windows"))]
fn replace_file(temp: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(temp, path)
}

#[cfg(target_os = "windows")]
fn replace_file(temp: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows::{
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::PCWSTR,
    };

    let mut temp = temp.as_os_str().encode_wide().collect::<Vec<_>>();
    temp.push(0);
    let mut path = path.as_os_str().encode_wide().collect::<Vec<_>>();
    path.push(0);

    // SAFETY: both buffers are NUL-terminated and remain alive for the call.
    unsafe {
        MoveFileExW(
            PCWSTR(temp.as_ptr()),
            PCWSTR(path.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(std::io::Error::other)
}

fn storage(message: impl Into<String>) -> CliError {
    CliError::Core(CoreError::Storage(message.into()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "scrozz-settings-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn settings(&self) -> PathBuf {
            self.path.join(FILE_NAME)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn a_missing_document_uses_defaults_without_creating_a_file() {
        let scratch = Scratch::new("defaults");
        let store = SettingsStore::open(scratch.settings()).unwrap();
        let (_, value, source) = store.get("capture.format").unwrap();
        assert_eq!(value, "png");
        assert_eq!(source, ValueSource::Default);
        assert_eq!(store.source("capture.format").unwrap(), "default");
        assert!(!scratch.settings().exists());
    }

    #[test]
    fn set_is_atomic_persistent_and_reports_user_source() {
        let scratch = Scratch::new("set");
        let mut store = SettingsStore::open(scratch.settings()).unwrap();
        store.set("capture.format", "webp").unwrap();

        let reloaded = SettingsStore::open(scratch.settings()).unwrap();
        let (setting, value, source) = reloaded.get("capture.format").unwrap();
        assert_eq!(value, "webp");
        assert_eq!(source, ValueSource::User);
        assert_eq!(reloaded.source("capture.format").unwrap(), "user");
        assert!(
            setting
                .to_json(value, source)
                .to_compact_string()
                .contains(r#""source":"user""#)
        );

        let leftovers: Vec<_> = fs::read_dir(&scratch.path)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn reset_one_and_all_restore_defaults() {
        let scratch = Scratch::new("reset");
        let mut store = SettingsStore::open(scratch.settings()).unwrap();
        store.set("capture.format", "jpeg").unwrap();
        store.set("capture.cursor", "true").unwrap();

        assert!(store.reset("capture.format").unwrap());
        assert!(!store.reset("capture.format").unwrap());
        assert_eq!(store.get("capture.format").unwrap().1, "png");
        assert_eq!(store.reset_all().unwrap(), 1);
        assert_eq!(store.get("capture.cursor").unwrap().2, ValueSource::Default);
    }

    #[test]
    fn an_invalid_edit_changes_neither_memory_nor_disk() {
        let scratch = Scratch::new("invalid");
        let mut store = SettingsStore::open(scratch.settings()).unwrap();
        assert!(store.set("capture.quality", "101").is_err());
        assert_eq!(store.get("capture.quality").unwrap().1, "90");
        assert!(!scratch.settings().exists());
    }

    #[test]
    fn ocr_settings_are_validated_together_before_an_atomic_write() {
        let scratch = Scratch::new("ocr-config");
        let mut store = SettingsStore::open(scratch.settings()).unwrap();

        let error = store
            .apply(&[
                ("ocr.languages".to_owned(), "en-US".to_owned()),
                ("ocr.auto-detect-language".to_owned(), "true".to_owned()),
            ])
            .unwrap_err();
        assert_eq!(error.exit(), crate::exit::Exit::InvalidRequest);
        assert!(!scratch.settings().exists());

        store
            .apply(&[
                ("ocr.languages".to_owned(), "de-DE,en-US".to_owned()),
                ("ocr.keep-line-breaks".to_owned(), "false".to_owned()),
                ("ocr.detect-links".to_owned(), "false".to_owned()),
            ])
            .unwrap();
        let config = store.ocr_config().unwrap();
        assert_eq!(config.language_mode(), scrozz_ocr::LanguageMode::Configured);
        assert_eq!(config.options().languages, ["de-DE", "en-US"]);
        assert_eq!(
            config.options().line_breaks,
            scrozz_ocr::LineBreaks::Collapse
        );
        assert!(!config.detects_links());
    }

    #[test]
    fn malformed_ocr_language_tags_never_reach_disk() {
        let scratch = Scratch::new("ocr-malformed");
        let mut store = SettingsStore::open(scratch.settings()).unwrap();

        let error = store.set("ocr.languages", "en--US").unwrap_err();
        assert_eq!(error.exit(), crate::exit::Exit::InvalidRequest);
        assert!(error.to_string().contains("ocr.languages"), "{error}");
        assert!(!scratch.settings().exists());
    }

    #[test]
    fn old_documents_are_migrated_and_rewritten() {
        let scratch = Scratch::new("migration");
        fs::write(
            scratch.settings(),
            br#"{"version":1,"values":{"capture.format":"jpeg","system.url-scheme":"true","update.channel":"preview"}}"#,
        )
        .unwrap();

        let store = SettingsStore::open(scratch.settings()).unwrap();
        assert_eq!(store.get("capture.format").unwrap().1, "jpeg");
        assert!(store.boolean("system.url-scheme-enabled").unwrap());
        let document: Document =
            serde_json::from_slice(&fs::read(scratch.settings()).unwrap()).unwrap();
        assert_eq!(document.version, CURRENT_VERSION);
        assert!(!document.values.contains_key("system.url-scheme"));
        assert!(!document.values.contains_key("update.channel"));
    }

    #[test]
    fn future_invalid_and_unknown_documents_fail_loudly() {
        for (name, contents) in [
            ("future", r#"{"version":99,"values":{}}"#),
            (
                "invalid",
                r#"{"version":3,"values":{"capture.quality":"101"}}"#,
            ),
            (
                "unknown",
                r#"{"version":3,"values":{"capture.typo":"png"}}"#,
            ),
        ] {
            let scratch = Scratch::new(name);
            fs::write(scratch.settings(), contents).unwrap();
            let error = SettingsStore::open(scratch.settings()).unwrap_err();
            assert_eq!(error.exit(), crate::exit::Exit::Storage, "{error}");
        }
    }

    #[test]
    fn the_environment_override_is_a_directory() {
        let _env = crate::test_env::lock();
        let scratch = Scratch::new("override");
        crate::test_env::set(CONFIG_DIR_ENV, scratch.path.to_str().unwrap());
        assert_eq!(settings_path().unwrap(), scratch.settings());
    }
}
