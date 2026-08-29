//! Persisted policy and deterministic orchestration for completed captures.
//!
//! A capture is final before this module sees it. The policy fans that immutable
//! artifact out in one fixed order, recording every result independently so one
//! failed destination cannot suppress the others.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use fs2::FileExt as _;
use scrozz_annotate::SmartFramePreset;
use scrozz_core::{Error, Frame, Result};
use serde_json::{Map, Value};

const SETTINGS_VERSION: u32 = 2;
const APP_DIR: &str = "Scrozz";
const FILE_NAME: &str = "settings.json";
const LOCK_FILE_NAME: &str = ".settings.lock";
const MAX_SMART_FRAME_PRESETS: usize = 128;
/// Overrides the settings file path for portable installs and isolated tests.
pub const SETTINGS_FILE_ENV: &str = "SCROZZ_SETTINGS_FILE";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The completed media type whose ambient GUI policy is being resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    /// A still image with native capture metadata.
    Screenshot,
    /// A final playable recording file.
    Recording,
}

impl MediaKind {
    const fn index(self) -> usize {
        match self {
            Self::Screenshot => 0,
            Self::Recording => 1,
        }
    }

    /// Stable on-disk and settings-schema name.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Screenshot => "screenshot",
            Self::Recording => "recording",
        }
    }
}

/// One action available after a completed capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AfterCaptureAction {
    /// Present the capture in the bottom-corner recent-captures surface.
    ShowRecentCapturesOverlay,
    /// Put image pixels or a retained recording file reference on the clipboard.
    CopyToClipboard,
    /// Export to the configured location using collision-safe naming.
    SaveAutomatically,
    /// Upload through the configured provider and copy the resulting link.
    UploadAndCopyLink,
    /// Open the appropriate non-destructive editor.
    OpenEditor,
    /// Keep a still image in a floating always-above window.
    PinToScreen,
}

impl AfterCaptureAction {
    /// Settings-row order. Presentation order does not control execution order.
    pub const UI_ORDER: [Self; 6] = [
        Self::ShowRecentCapturesOverlay,
        Self::CopyToClipboard,
        Self::SaveAutomatically,
        Self::UploadAndCopyLink,
        Self::OpenEditor,
        Self::PinToScreen,
    ];

    /// Execution order after an immutable artifact has been finalized.
    pub const EXECUTION_ORDER: [Self; 6] = [
        Self::CopyToClipboard,
        Self::SaveAutomatically,
        Self::UploadAndCopyLink,
        Self::ShowRecentCapturesOverlay,
        Self::OpenEditor,
        Self::PinToScreen,
    ];

    const fn index(self) -> usize {
        match self {
            Self::ShowRecentCapturesOverlay => 0,
            Self::CopyToClipboard => 1,
            Self::SaveAutomatically => 2,
            Self::UploadAndCopyLink => 3,
            Self::OpenEditor => 4,
            Self::PinToScreen => 5,
        }
    }

    /// Stable action name inside the persisted document.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::ShowRecentCapturesOverlay => "show-recent-captures-overlay",
            Self::CopyToClipboard => "copy-to-clipboard",
            Self::SaveAutomatically => "save-automatically",
            Self::UploadAndCopyLink => "upload-and-copy-link",
            Self::OpenEditor => "open-editor",
            Self::PinToScreen => "pin-to-screen",
        }
    }

    /// User-facing row label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ShowRecentCapturesOverlay => "Show Recent Captures Overlay",
            Self::CopyToClipboard => "Copy to clipboard",
            Self::SaveAutomatically => "Save automatically",
            Self::UploadAndCopyLink => "Upload and copy link",
            Self::OpenEditor => "Open Editor",
            Self::PinToScreen => "Pin to Screen",
        }
    }

    /// User-facing explanation of the action.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::ShowRecentCapturesOverlay => {
                "Keep the completed capture in the nonactivating recent-captures corner surface."
            }
            Self::CopyToClipboard => {
                "Copy screenshot pixels immediately, or a retained recording file reference where supported."
            }
            Self::SaveAutomatically => {
                "Write once to the configured export location with collision-safe naming."
            }
            Self::UploadAndCopyLink => {
                "Use the configured cloud provider, then copy its shareable link."
            }
            Self::OpenEditor => {
                "Open the annotation editor for screenshots or the video editor for recordings."
            }
            Self::PinToScreen => "Keep a screenshot visible in a floating window.",
        }
    }

    /// Dotted settings key for this media/action pair.
    #[must_use]
    pub fn setting_key(self, media: MediaKind) -> String {
        format!("{}.{}", media.setting_area(), self.slug())
    }

    fn from_slug(slug: &str, media: MediaKind) -> Option<Self> {
        Self::UI_ORDER
            .into_iter()
            .find(|action| action.slug() == slug)
            .or(match (slug, media) {
                ("show-quick-access-overlay", _) | ("show-overlay", _) => {
                    Some(Self::ShowRecentCapturesOverlay)
                }
                ("copy-file-to-clipboard", _) | ("copy", _) => Some(Self::CopyToClipboard),
                ("save", _) => Some(Self::SaveAutomatically),
                ("upload-to-cloud-and-copy-link", _) | ("upload", _) => {
                    Some(Self::UploadAndCopyLink)
                }
                ("open-annotate-tool", MediaKind::Screenshot)
                | ("open-video-editor", MediaKind::Recording) => Some(Self::OpenEditor),
                ("pin-to-the-screen", MediaKind::Screenshot) => Some(Self::PinToScreen),
                _ => None,
            })
    }

    /// Whether this action exists in the product contract for this media type.
    #[must_use]
    pub const fn is_contract_available(self, media: MediaKind) -> bool {
        !(matches!(self, Self::PinToScreen) && matches!(media, MediaKind::Recording))
    }
}

impl MediaKind {
    const fn setting_area(self) -> &'static str {
        match self {
            Self::Screenshot => "capture",
            Self::Recording => "record",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ActionSet {
    enabled: [bool; AfterCaptureAction::UI_ORDER.len()],
    unknown: BTreeMap<String, Value>,
}

impl ActionSet {
    fn empty() -> Self {
        Self {
            enabled: [false; AfterCaptureAction::UI_ORDER.len()],
            unknown: BTreeMap::new(),
        }
    }

    fn overlay_only() -> Self {
        let mut set = Self::empty();
        set.set(AfterCaptureAction::ShowRecentCapturesOverlay, true);
        set
    }

    fn fresh_screenshot() -> Self {
        let mut set = Self::overlay_only();
        set.set(AfterCaptureAction::CopyToClipboard, true);
        set
    }

    const fn is_enabled(&self, action: AfterCaptureAction) -> bool {
        self.enabled[action.index()]
    }

    fn set(&mut self, action: AfterCaptureAction, enabled: bool) {
        self.enabled[action.index()] = enabled;
    }

    fn read_map(&mut self, media: MediaKind, map: &Map<String, Value>) -> Result<()> {
        for (slug, value) in map {
            if let Some(action) = AfterCaptureAction::from_slug(slug, media) {
                let enabled = value.as_bool().ok_or_else(|| {
                    Error::Storage(format!(
                        "after_capture.{}.{slug} must be true or false",
                        media.slug()
                    ))
                })?;
                self.set(action, enabled);
            } else {
                self.unknown.insert(slug.clone(), value.clone());
            }
        }
        Ok(())
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map: Map<String, Value> = self
            .unknown
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        for action in AfterCaptureAction::UI_ORDER {
            map.insert(
                action.slug().to_owned(),
                Value::Bool(self.is_enabled(action)),
            );
        }
        map
    }
}

/// Independent persisted action sets for screenshots and recordings.
#[derive(Debug, Clone, PartialEq)]
pub struct AfterCaptureSettings {
    sets: [ActionSet; 2],
    values: BTreeMap<String, String>,
    unknown_values: BTreeMap<String, Value>,
    smart_frame_presets: Vec<SmartFramePreset>,
    document_version: u32,
    unknown_root: BTreeMap<String, Value>,
    unknown_after_capture: BTreeMap<String, Value>,
}

impl Default for AfterCaptureSettings {
    fn default() -> Self {
        Self::fresh()
    }
}

impl AfterCaptureSettings {
    /// Defaults for a profile created by this version.
    #[must_use]
    pub fn fresh() -> Self {
        Self {
            sets: [ActionSet::fresh_screenshot(), ActionSet::overlay_only()],
            values: BTreeMap::new(),
            unknown_values: BTreeMap::new(),
            smart_frame_presets: Vec::new(),
            document_version: SETTINGS_VERSION,
            unknown_root: BTreeMap::new(),
            unknown_after_capture: BTreeMap::new(),
        }
    }

    /// Defaults for a document written before After Capture settings existed.
    ///
    /// Scrozz is unreleased, so an absent value takes the confirmed product
    /// default. Explicit legacy values are applied over this seed while parsing.
    #[must_use]
    pub fn legacy() -> Self {
        Self {
            sets: [ActionSet::fresh_screenshot(), ActionSet::overlay_only()],
            values: BTreeMap::new(),
            unknown_values: BTreeMap::new(),
            smart_frame_presets: Vec::new(),
            document_version: SETTINGS_VERSION,
            unknown_root: BTreeMap::new(),
            unknown_after_capture: BTreeMap::new(),
        }
    }

    /// Whether `action` is enabled for `media`.
    #[must_use]
    pub const fn is_enabled(&self, media: MediaKind, action: AfterCaptureAction) -> bool {
        self.sets[media.index()].is_enabled(action)
    }

    /// Enables or disables one action without changing any other cell.
    pub fn set(&mut self, media: MediaKind, action: AfterCaptureAction, enabled: bool) {
        self.sets[media.index()].set(action, enabled);
    }

    /// Resolves a dotted schema key into its media/action pair.
    #[must_use]
    pub fn resolve_key(key: &str) -> Option<(MediaKind, AfterCaptureAction)> {
        for media in [MediaKind::Screenshot, MediaKind::Recording] {
            let prefix = format!("{}.", media.setting_area());
            let Some(slug) = key.strip_prefix(&prefix) else {
                continue;
            };
            if let Some(action) = AfterCaptureAction::from_slug(slug, media) {
                return Some((media, action));
            }
        }
        None
    }

    /// Reads a value from a dotted settings key.
    #[must_use]
    pub fn value_for_key(&self, key: &str) -> Option<bool> {
        let (media, action) = Self::resolve_key(key)?;
        Some(self.is_enabled(media, action))
    }

    /// Writes a value through a dotted settings key.
    pub fn set_key(&mut self, key: &str, enabled: bool) -> bool {
        let Some((media, action)) = Self::resolve_key(key) else {
            return false;
        };
        self.set(media, action, enabled);
        true
    }

    /// A persisted non-action setting value.
    #[must_use]
    pub fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Stores a non-action setting value.
    pub fn set_value(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.values.insert(key.into(), value.into());
    }

    /// User-created Smart Frame presets in stable display order.
    #[must_use]
    pub fn smart_frame_presets(&self) -> &[SmartFramePreset] {
        &self.smart_frame_presets
    }

    /// Adds or replaces one validated Smart Frame preset.
    pub fn upsert_smart_frame_preset(&mut self, preset: SmartFramePreset) -> Result<()> {
        preset.validate()?;
        if let Some(existing) = self
            .smart_frame_presets
            .iter_mut()
            .find(|existing| existing.id == preset.id)
        {
            *existing = preset;
        } else {
            if self.smart_frame_presets.len() >= MAX_SMART_FRAME_PRESETS {
                return Err(Error::Storage(format!(
                    "settings already contain the limit of {MAX_SMART_FRAME_PRESETS} Smart Frame presets"
                )));
            }
            self.smart_frame_presets.push(preset);
        }
        self.smart_frame_presets
            .sort_by_key(|preset| preset.name.to_lowercase());
        self.validate_smart_frame_presets()
    }

    /// Removes one custom Smart Frame preset.
    pub fn delete_smart_frame_preset(&mut self, id: &str) -> Result<()> {
        let before = self.smart_frame_presets.len();
        self.smart_frame_presets.retain(|preset| preset.id != id);
        if self.smart_frame_presets.len() == before {
            return Err(Error::InvalidRequest(format!(
                "Smart Frame preset {id:?} does not exist"
            )));
        }
        Ok(())
    }

    fn validate_smart_frame_presets(&self) -> Result<()> {
        if self.smart_frame_presets.len() > MAX_SMART_FRAME_PRESETS {
            return Err(Error::Storage(format!(
                "settings contain {} Smart Frame presets; the limit is {MAX_SMART_FRAME_PRESETS}",
                self.smart_frame_presets.len()
            )));
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
            return Err(Error::Storage(
                "Smart Frame preset identifiers must be unique".to_owned(),
            ));
        }
        Ok(())
    }

    fn from_json(text: &str) -> Result<(Self, u32)> {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| Error::Storage(format!("the settings file is unreadable: {error}")))?;
        let mut root = value.as_object().cloned().ok_or_else(|| {
            Error::Storage("the settings file must contain a JSON object".to_owned())
        })?;
        let version = root
            .remove("version")
            .and_then(|value| value.as_u64())
            .map_or(0, |version| u32::try_from(version).unwrap_or(u32::MAX));

        let mut settings = if version < SETTINGS_VERSION {
            Self::legacy()
        } else {
            Self::fresh()
        };

        if let Some(value) = root.remove("after_capture") {
            let mut after_capture = value.as_object().cloned().ok_or_else(|| {
                Error::Storage("after_capture must contain a JSON object".to_owned())
            })?;
            for media in [MediaKind::Screenshot, MediaKind::Recording] {
                if let Some(value) = after_capture.remove(media.slug()) {
                    let map = value.as_object().ok_or_else(|| {
                        Error::Storage(format!(
                            "after_capture.{} must contain a JSON object",
                            media.slug()
                        ))
                    })?;
                    settings.sets[media.index()].read_map(media, map)?;
                }
            }
            settings.unknown_after_capture = after_capture.into_iter().collect();
        }

        // Early prototypes stored every schema value in one flat object. Keep
        // supporting it: action keys migrate into their typed sets and ordinary
        // settings remain available to the CLI and output pipeline.
        if let Some(values) = root.remove("values") {
            let values = values
                .as_object()
                .ok_or_else(|| Error::Storage("values must contain a JSON object".to_owned()))?;
            for (key, value) in values {
                if AfterCaptureSettings::resolve_key(key).is_some() {
                    let enabled = json_bool(value).ok_or_else(|| {
                        Error::Storage(format!(
                            "stored After Capture setting {key:?} must be true or false"
                        ))
                    })?;
                    let _ = settings.set_key(key, enabled);
                    continue;
                }
                if crate::settings::SETTINGS
                    .iter()
                    .any(|setting| setting.key == key)
                {
                    let value = scalar_setting(value).ok_or_else(|| {
                        Error::Storage(format!("stored setting {key:?} must be a scalar value"))
                    })?;
                    settings.values.insert(key.clone(), value);
                } else {
                    settings.unknown_values.insert(key.clone(), value.clone());
                }
            }
        }
        if let Some(value) = root.remove("smart_frame_presets") {
            settings.smart_frame_presets = serde_json::from_value(value).map_err(|error| {
                Error::Storage(format!(
                    "the Smart Frame preset library is unreadable: {error}"
                ))
            })?;
        }
        if version < SETTINGS_VERSION {
            for media in [MediaKind::Screenshot, MediaKind::Recording] {
                for action in AfterCaptureAction::UI_ORDER {
                    let key = action.setting_key(media);
                    if let Some(value) = root.remove(&key) {
                        let enabled = json_bool(&value).ok_or_else(|| {
                            Error::Storage(format!(
                                "stored After Capture setting {key:?} must be true or false"
                            ))
                        })?;
                        settings.set(media, action, enabled);
                    }
                }
            }
        }

        settings.document_version = version.max(SETTINGS_VERSION);
        settings.unknown_root = root.into_iter().collect();
        settings.validate_smart_frame_presets()?;
        Ok((settings, version))
    }

    fn to_json(&self) -> Result<String> {
        self.validate_smart_frame_presets()?;
        let mut root: Map<String, Value> = self
            .unknown_root
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let mut after_capture: Map<String, Value> = self
            .unknown_after_capture
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        after_capture.insert(
            MediaKind::Screenshot.slug().to_owned(),
            Value::Object(self.sets[MediaKind::Screenshot.index()].to_map()),
        );
        after_capture.insert(
            MediaKind::Recording.slug().to_owned(),
            Value::Object(self.sets[MediaKind::Recording.index()].to_map()),
        );
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
        root.insert(
            "smart_frame_presets".to_owned(),
            serde_json::to_value(&self.smart_frame_presets).map_err(|error| {
                Error::Storage(format!(
                    "could not encode the Smart Frame preset library: {error}"
                ))
            })?,
        );
        root.insert("after_capture".to_owned(), Value::Object(after_capture));
        serde_json::to_string_pretty(&Value::Object(root))
            .map_err(|error| Error::Storage(format!("could not render settings: {error}")))
    }
}

fn json_bool(value: &Value) -> Option<bool> {
    value.as_bool().or_else(|| match value.as_str()? {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    })
}

fn scalar_setting(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

/// Whether a settings cell has a real implementation in this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionAvailability {
    /// A toggle may be enabled and will run.
    pub available: bool,
    /// Accessible explanation shown instead of an inert control.
    pub reason: Option<&'static str>,
}

impl ActionAvailability {
    const AVAILABLE: Self = Self {
        available: true,
        reason: None,
    };

    const fn unavailable(reason: &'static str) -> Self {
        Self {
            available: false,
            reason: Some(reason),
        }
    }
}

/// Actual capability of one After Capture cell in this build.
#[must_use]
pub const fn current_availability(
    media: MediaKind,
    action: AfterCaptureAction,
) -> ActionAvailability {
    if matches!(media, MediaKind::Recording) {
        return ActionAvailability::unavailable(
            "Screen recording is not implemented in this build, so recording actions cannot run yet.",
        );
    }
    match action {
        AfterCaptureAction::ShowRecentCapturesOverlay
        | AfterCaptureAction::CopyToClipboard
        | AfterCaptureAction::SaveAutomatically
        | AfterCaptureAction::OpenEditor => ActionAvailability::AVAILABLE,
        AfterCaptureAction::UploadAndCopyLink => ActionAvailability::unavailable(
            "No cloud upload provider is implemented or configured in this build.",
        ),
        AfterCaptureAction::PinToScreen => {
            ActionAvailability::unavailable("Pin to Screen is not implemented in this build.")
        }
    }
}

/// Which defaults apply when no settings document exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallProfile {
    /// No prior Scrozz configuration can be inferred.
    Fresh,
    /// A Scrozz config directory already exists and may need legacy migration.
    Existing,
}

impl InstallProfile {
    fn defaults(self) -> AfterCaptureSettings {
        match self {
            Self::Fresh => AfterCaptureSettings::fresh(),
            Self::Existing => AfterCaptureSettings::legacy(),
        }
    }
}

/// Atomic storage for the versioned settings document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfterCaptureStore {
    path: PathBuf,
    infer_global_legacy: bool,
}

impl AfterCaptureStore {
    /// Uses an explicit path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            infer_global_legacy: false,
        }
    }

    /// Resolves the platform configuration path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] when no platform config directory exists.
    pub fn default_location() -> Result<Self> {
        if let Ok(path) = std::env::var(SETTINGS_FILE_ENV)
            && !path.trim().is_empty()
        {
            return Ok(Self::new(path));
        }
        let base = dirs::config_dir().or_else(dirs::data_dir).ok_or_else(|| {
            Error::Storage("no platform config directory is available for settings".to_owned())
        })?;
        Ok(Self {
            path: base.join(APP_DIR).join(FILE_NAME),
            infer_global_legacy: true,
        })
    }

    /// The file this store owns.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Infers whether an absent file belongs to a fresh or existing profile.
    #[must_use]
    pub fn inferred_profile(&self) -> InstallProfile {
        let has_shortcuts = self
            .path
            .parent()
            .is_some_and(|parent| parent.join("shortcuts.json").is_file());
        let has_history = self.infer_global_legacy
            && scrozz_store::StoreLayout::default_location()
                .is_ok_and(|layout| layout.index_path().is_file());
        let has_remembered_region = self.infer_global_legacy
            && crate::selection_store::RememberedRegionStore::default_location()
                .is_ok_and(|store| store.path().is_file());
        if has_shortcuts || has_history || has_remembered_region {
            InstallProfile::Existing
        } else {
            InstallProfile::Fresh
        }
    }

    /// Loads settings and rewrites older documents in the current schema.
    ///
    /// # Errors
    ///
    /// Returns a storage error for unreadable, invalid, or unmigratable files.
    pub fn load(&self, profile: InstallProfile) -> Result<AfterCaptureSettings> {
        self.with_lock(|| self.load_unlocked(profile))
    }

    fn load_unlocked(&self, profile: InstallProfile) -> Result<AfterCaptureSettings> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let settings = profile.defaults();
                self.save_unlocked(&settings)?;
                return Ok(settings);
            }
            Err(error) => {
                return Err(Error::Storage(format!(
                    "could not read {}: {error}",
                    self.path.display()
                )));
            }
        };
        let (settings, version) = AfterCaptureSettings::from_json(&text)?;
        if version < SETTINGS_VERSION {
            self.save_unlocked(&settings)?;
        }
        Ok(settings)
    }

    /// Atomically replaces the settings document.
    ///
    /// # Errors
    ///
    /// Returns a storage error when its directory or replacement cannot be
    /// created.
    pub fn save(&self, settings: &AfterCaptureSettings) -> Result<()> {
        self.with_lock(|| self.save_unlocked(settings))
    }

    /// Atomically updates the latest document under one cross-process lock.
    ///
    /// # Errors
    ///
    /// Returns a storage error if loading, applying, or replacing the document
    /// fails. No stale caller snapshot is written.
    pub fn update(
        &self,
        profile: InstallProfile,
        change: impl FnOnce(&mut AfterCaptureSettings) -> Result<()>,
    ) -> Result<AfterCaptureSettings> {
        self.with_lock(|| {
            let mut settings = self.load_unlocked(profile)?;
            change(&mut settings)?;
            self.save_unlocked(&settings)?;
            Ok(settings)
        })
    }

    fn save_unlocked(&self, settings: &AfterCaptureSettings) -> Result<()> {
        let text = settings.to_json()?;
        let parent = self.path.parent().ok_or_else(|| {
            Error::Storage(format!("{} has no parent directory", self.path.display()))
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            Error::Storage(format!("could not create {}: {error}", parent.display()))
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
            Error::Storage(format!("could not write {}: {error}", temporary.display()))
        })?;
        scrozz_shell::replace_file(&temporary, &self.path).inspect_err(|_error| {
            let _ = fs::remove_file(&temporary);
        })
    }

    fn with_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let parent = self.path.parent().ok_or_else(|| {
            Error::Storage(format!("{} has no parent directory", self.path.display()))
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            Error::Storage(format!("could not create {}: {error}", parent.display()))
        })?;
        let lock_path = parent.join(LOCK_FILE_NAME);
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
                "could not lock settings at {}: {error}",
                lock_path.display()
            ))
        })?;
        let result = operation();
        let unlock = fs2::FileExt::unlock(&lock).map_err(|error| {
            Error::Storage(format!(
                "could not unlock settings at {}: {error}",
                lock_path.display()
            ))
        });
        match (result, unlock) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }
}

/// An immutable screenshot handed to every enabled action.
#[derive(Debug, Clone, Copy)]
pub struct FinalizedScreenshot<'a> {
    /// Native pixels and capture metadata, used directly for the initial copy.
    pub frame: &'a Frame,
    /// The exact encoded artifact retained for save, upload, overlay, and editor.
    pub png: &'a [u8],
}

/// A recording copied out of recorder-owned temporary storage.
///
/// The retained file has no automatic drop cleanup. Clipboard consumers may
/// dereference a file URL after Scrozz's action pass has returned, so process
/// startup retention—not object lifetime—must eventually sweep these files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedRecording {
    path: PathBuf,
}

impl RetainedRecording {
    /// Copies a finalized non-empty recording into stable retained storage.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the source is not a regular non-empty file or
    /// no collision-free retained file can be created.
    pub fn retain(source: &Path, directory: &Path) -> Result<Self> {
        let metadata = fs::metadata(source).map_err(|error| {
            Error::Storage(format!(
                "could not inspect finalized recording {}: {error}",
                source.display()
            ))
        })?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(Error::Storage(format!(
                "finalized recording {} is not a non-empty regular file",
                source.display()
            )));
        }
        fs::create_dir_all(directory).map_err(|error| {
            Error::Storage(format!(
                "could not create retained recording directory {}: {error}",
                directory.display()
            ))
        })?;

        static NEXT: AtomicU64 = AtomicU64::new(1);
        let extension = source
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| !extension.is_empty())
            .unwrap_or("mp4");
        for _ in 0..64 {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(
                "Scrozz Recording {}-{sequence}.{extension}",
                std::process::id()
            ));
            let mut destination = match OpenOptions::new().write(true).create_new(true).open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(Error::Storage(format!(
                        "could not retain recording at {}: {error}",
                        path.display()
                    )));
                }
            };
            let copy = (|| -> std::io::Result<()> {
                let mut source_file = File::open(source)?;
                let copied = std::io::copy(&mut source_file, &mut destination)?;
                if copied == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "the finalized recording became empty",
                    ));
                }
                destination.sync_all()
            })();
            if let Err(error) = copy {
                let _ = fs::remove_file(&path);
                return Err(Error::Storage(format!(
                    "could not retain recording at {}: {error}",
                    path.display()
                )));
            }
            return Ok(Self { path });
        }
        Err(Error::Storage(
            "could not allocate a collision-free retained recording path".to_owned(),
        ))
    }

    /// Stable path safe for clipboard and overlay references.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the retained bytes, primarily for validation and upload.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the retained artifact disappeared.
    pub fn read(&self) -> Result<Vec<u8>> {
        let mut file = File::open(&self.path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// What one successful action produced for the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionEffect {
    /// The action completed with no host follow-up.
    Completed,
    /// A collision-safe file was written.
    Saved(PathBuf),
    /// An upload returned a shareable URL.
    Uploaded(String),
    /// Present the same artifact in Recent Captures Overlay.
    ShowRecentCapturesOverlay,
    /// Open the matching editor on the same artifact.
    OpenEditor,
    /// Pin the same screenshot.
    PinToScreen,
}

/// The outcome of one enabled action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// The action completed.
    Succeeded(ActionEffect),
    /// The action failed independently.
    Failed(String),
}

/// One ordered action result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionStep {
    /// Which action ran.
    pub action: AfterCaptureAction,
    /// Its isolated result.
    pub outcome: ActionOutcome,
}

/// Complete, non-short-circuiting result of an After Capture pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecutionReport {
    /// Enabled actions in deterministic execution order.
    pub steps: Vec<ActionStep>,
}

impl ExecutionReport {
    /// Whether this action completed with the named host effect.
    #[must_use]
    pub fn has_effect(&self, expected: &ActionEffect) -> bool {
        self.steps.iter().any(
            |step| matches!(&step.outcome, ActionOutcome::Succeeded(effect) if effect == expected),
        )
    }

    /// Every failure, retaining action identity.
    pub fn failures(&self) -> impl Iterator<Item = (AfterCaptureAction, &str)> {
        self.steps.iter().filter_map(|step| match &step.outcome {
            ActionOutcome::Failed(error) => Some((step.action, error.as_str())),
            ActionOutcome::Succeeded(_) => None,
        })
    }
}

/// Performs one action against one finalized artifact type.
pub trait ActionExecutor<Artifact> {
    /// Executes `action` once.
    fn execute(
        &mut self,
        action: AfterCaptureAction,
        artifact: &Artifact,
    ) -> std::result::Result<ActionEffect, String>;
}

/// Runs every enabled action in deterministic order without short-circuiting.
#[must_use]
pub fn orchestrate<Artifact>(
    media: MediaKind,
    settings: &AfterCaptureSettings,
    artifact: &Artifact,
    executor: &mut impl ActionExecutor<Artifact>,
) -> ExecutionReport {
    let mut report = ExecutionReport::default();
    for action in AfterCaptureAction::EXECUTION_ORDER {
        if !settings.is_enabled(media, action) {
            continue;
        }
        let outcome = if action.is_contract_available(media) {
            executor
                .execute(action, artifact)
                .map_or_else(ActionOutcome::Failed, ActionOutcome::Succeeded)
        } else {
            ActionOutcome::Failed(format!(
                "{} is unavailable for {}s",
                action.label(),
                media.slug()
            ))
        };
        report.steps.push(ActionStep { action, outcome });
    }
    report
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use scrozz_core::{ColorSpace, PhysicalSize, PixelFormat, ScaleFactor};

    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "scrozz-after-capture-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn frame(space: ColorSpace) -> Frame {
        Frame {
            data: vec![11, 22, 33, 255],
            size: PhysicalSize::new(1.0, 1.0),
            stride: 4,
            format: PixelFormat::Rgba8,
            color_space: space,
            scale: ScaleFactor::IDENTITY,
        }
    }

    #[test]
    fn every_unset_profile_uses_the_confirmed_screenshot_default() {
        let fresh = AfterCaptureSettings::fresh();
        assert!(fresh.is_enabled(MediaKind::Screenshot, AfterCaptureAction::CopyToClipboard));
        assert!(fresh.is_enabled(
            MediaKind::Screenshot,
            AfterCaptureAction::ShowRecentCapturesOverlay
        ));
        assert!(fresh.is_enabled(
            MediaKind::Recording,
            AfterCaptureAction::ShowRecentCapturesOverlay
        ));
        assert_eq!(
            AfterCaptureAction::UI_ORDER
                .into_iter()
                .filter(|action| fresh.is_enabled(MediaKind::Screenshot, *action))
                .count(),
            2
        );

        let existing = AfterCaptureSettings::legacy();
        assert!(existing.is_enabled(MediaKind::Screenshot, AfterCaptureAction::CopyToClipboard));
        assert!(existing.is_enabled(
            MediaKind::Screenshot,
            AfterCaptureAction::ShowRecentCapturesOverlay
        ));
        assert!(existing.is_enabled(
            MediaKind::Recording,
            AfterCaptureAction::ShowRecentCapturesOverlay
        ));
        assert!(!existing.is_enabled(MediaKind::Recording, AfterCaptureAction::CopyToClipboard));
    }

    #[test]
    fn version_one_values_migrate_and_are_rewritten_as_version_two() {
        let root = scratch("migration");
        let path = root.join("settings.json");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &path,
            r#"{
  "version": 1,
  "values": {
    "capture.copy-to-clipboard": "true",
    "capture.save-automatically": true,
    "record.show-quick-access-overlay": false
  }
}"#,
        )
        .unwrap();
        let store = AfterCaptureStore::new(&path);
        let migrated = store.load(InstallProfile::Existing).expect("migrates");

        assert!(migrated.is_enabled(MediaKind::Screenshot, AfterCaptureAction::CopyToClipboard));
        assert!(migrated.is_enabled(MediaKind::Screenshot, AfterCaptureAction::SaveAutomatically));
        assert!(!migrated.is_enabled(
            MediaKind::Recording,
            AfterCaptureAction::ShowRecentCapturesOverlay
        ));
        let rewritten = fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("\"version\": 2"), "{rewritten}");
        assert!(
            rewritten.contains("\"show-recent-captures-overlay\""),
            "{rewritten}"
        );
        assert!(!rewritten.contains("quick-access"), "{rewritten}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persistence_survives_restart_and_keeps_media_independent() {
        let root = scratch("restart");
        let store = AfterCaptureStore::new(root.join("nested/settings.json"));
        let mut settings = AfterCaptureSettings::fresh();
        store
            .save(&AfterCaptureSettings::legacy())
            .expect("initial save");
        settings.set(
            MediaKind::Screenshot,
            AfterCaptureAction::SaveAutomatically,
            true,
        );
        settings.set(
            MediaKind::Recording,
            AfterCaptureAction::ShowRecentCapturesOverlay,
            false,
        );
        settings.set(MediaKind::Recording, AfterCaptureAction::OpenEditor, true);
        store.save(&settings).expect("save");

        let restarted = store.load(InstallProfile::Fresh).expect("restart");
        assert_eq!(restarted, settings);
        assert!(restarted.is_enabled(MediaKind::Screenshot, AfterCaptureAction::SaveAutomatically));
        assert!(!restarted.is_enabled(
            MediaKind::Recording,
            AfterCaptureAction::ShowRecentCapturesOverlay
        ));
        assert!(restarted.is_enabled(MediaKind::Recording, AfterCaptureAction::OpenEditor));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_transactions_preserve_independent_edits() {
        use std::sync::{Arc, Barrier};

        let root = scratch("concurrent");
        let store = Arc::new(AfterCaptureStore::new(root.join("settings.json")));
        store.save(&AfterCaptureSettings::fresh()).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let workers: Vec<_> = [
            AfterCaptureAction::SaveAutomatically,
            AfterCaptureAction::OpenEditor,
        ]
        .into_iter()
        .map(|action| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .update(InstallProfile::Fresh, |latest| {
                        latest.set(MediaKind::Screenshot, action, true);
                        Ok(())
                    })
                    .unwrap();
            })
        })
        .collect();
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }

        let settings = store.load(InstallProfile::Fresh).unwrap();
        assert!(settings.is_enabled(MediaKind::Screenshot, AfterCaptureAction::SaveAutomatically));
        assert!(settings.is_enabled(MediaKind::Screenshot, AfterCaptureAction::OpenEditor));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn absent_store_uses_the_requested_install_profile() {
        let root = scratch("profiles");
        let fresh_store = AfterCaptureStore::new(root.join("fresh/settings.json"));
        let existing_store = AfterCaptureStore::new(root.join("existing/settings.json"));
        let fresh = fresh_store.load(InstallProfile::Fresh).unwrap();
        let existing = existing_store.load(InstallProfile::Existing).unwrap();
        assert!(fresh.is_enabled(MediaKind::Screenshot, AfterCaptureAction::CopyToClipboard));
        assert!(existing.is_enabled(MediaKind::Screenshot, AfterCaptureAction::CopyToClipboard));
        assert!(fresh_store.path().is_file());
        assert!(existing_store.path().is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_launch_defaults_do_not_flip_after_other_state_creates_the_directory() {
        let root = scratch("stable-first-launch");
        let store = AfterCaptureStore::new(root.join("settings.json"));
        let first = store.load(InstallProfile::Fresh).unwrap();
        fs::write(root.join("shortcuts.json"), "{}").unwrap();
        assert_eq!(store.inferred_profile(), InstallProfile::Existing);

        let restarted = store.load(store.inferred_profile()).unwrap();
        assert_eq!(restarted, first);
        assert!(restarted.is_enabled(MediaKind::Screenshot, AfterCaptureAction::CopyToClipboard));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unrelated_existing_state_still_gets_copy_and_overlay_when_policy_is_absent() {
        let root = scratch("inferred-existing");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("shortcuts.json"), "{}").unwrap();
        let store = AfterCaptureStore::new(root.join("settings.json"));
        assert_eq!(store.inferred_profile(), InstallProfile::Existing);
        let settings = store.load(store.inferred_profile()).unwrap();
        assert!(settings.is_enabled(
            MediaKind::Screenshot,
            AfterCaptureAction::ShowRecentCapturesOverlay
        ));
        assert!(settings.is_enabled(MediaKind::Screenshot, AfterCaptureAction::CopyToClipboard));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_document_with_only_unrelated_settings_gets_copy_and_overlay() {
        let root = scratch("legacy-unrelated-settings");
        let path = root.join("settings.json");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &path,
            r#"{
  "version": 1,
  "values": {
    "capture.format": "webp",
    "capture.cursor": true
  }
}"#,
        )
        .unwrap();

        let store = AfterCaptureStore::new(&path);
        let migrated = store.load(InstallProfile::Existing).unwrap();
        assert_eq!(migrated.value("capture.format"), Some("webp"));
        assert_eq!(migrated.value("capture.cursor"), Some("true"));
        assert!(migrated.is_enabled(MediaKind::Screenshot, AfterCaptureAction::CopyToClipboard));
        assert!(migrated.is_enabled(
            MediaKind::Screenshot,
            AfterCaptureAction::ShowRecentCapturesOverlay
        ));
        assert!(!migrated.is_enabled(MediaKind::Recording, AfterCaptureAction::CopyToClipboard));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_explicit_legacy_copy_false_remains_false() {
        let root = scratch("legacy-explicit-false");
        let path = root.join("settings.json");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &path,
            r#"{
  "version": 1,
  "values": {
    "capture.copy-to-clipboard": false
  }
}"#,
        )
        .unwrap();

        let store = AfterCaptureStore::new(&path);
        let migrated = store.load(InstallProfile::Existing).unwrap();
        assert!(!migrated.is_enabled(MediaKind::Screenshot, AfterCaptureAction::CopyToClipboard));
        assert!(migrated.is_enabled(
            MediaKind::Screenshot,
            AfterCaptureAction::ShowRecentCapturesOverlay
        ));

        let restarted = store.load(InstallProfile::Existing).unwrap();
        assert!(!restarted.is_enabled(MediaKind::Screenshot, AfterCaptureAction::CopyToClipboard));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn smart_frame_presets_round_trip_with_unknown_fields() {
        let root = scratch("smart-frame-presets");
        let path = root.join("settings.json");
        let store = AfterCaptureStore::new(&path);
        let mut preset = SmartFramePreset::new(
            "quiet-frame",
            "Quiet Frame",
            scrozz_annotate::SmartFramePresetSettings::default(),
        )
        .unwrap();
        preset
            .extensions
            .insert("future-preset-field".to_owned(), serde_json::json!(true));

        let updated = store
            .update(InstallProfile::Fresh, |settings| {
                settings.upsert_smart_frame_preset(preset)?;
                settings.set_value("after-capture.apply-smart-frame", "true");
                settings
                    .unknown_root
                    .insert("future-root".to_owned(), serde_json::json!({"kept": true}));
                Ok(())
            })
            .unwrap();
        assert_eq!(updated.smart_frame_presets().len(), 1);

        let restarted = store.load(InstallProfile::Fresh).unwrap();
        assert_eq!(restarted.smart_frame_presets()[0].name, "Quiet Frame");
        assert_eq!(
            restarted.smart_frame_presets()[0]
                .extensions
                .get("future-preset-field"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            restarted
                .unknown_root
                .get("future-root")
                .and_then(|value| value.get("kept"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            restarted.value("after-capture.apply-smart-frame"),
            Some("true")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn smart_frame_presets_load_without_a_values_object() {
        let root = scratch("smart-frame-presets-without-values");
        let path = root.join("settings.json");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &path,
            r#"{
  "version": 2,
  "smart_frame_presets": [
    {
      "version": 1,
      "id": "quiet-frame",
      "name": "Quiet Frame",
      "settings": {}
    }
  ]
}"#,
        )
        .unwrap();

        let store = AfterCaptureStore::new(&path);
        let settings = store.load(InstallProfile::Fresh).unwrap();
        assert_eq!(settings.smart_frame_presets()[0].id, "quiet-frame");
        store.save(&settings).unwrap();
        assert_eq!(
            store
                .load(InstallProfile::Fresh)
                .unwrap()
                .smart_frame_presets()[0]
                .id,
            "quiet-frame"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn smart_frame_preset_upsert_and_delete_are_atomic() {
        let root = scratch("smart-frame-preset-mutations");
        let store = AfterCaptureStore::new(root.join("settings.json"));
        let first = SmartFramePreset::new(
            "team-note",
            "Team Note",
            scrozz_annotate::SmartFramePresetSettings::default(),
        )
        .unwrap();
        let mut replacement = first.clone();
        replacement.name = "Team Note Updated".to_owned();

        store
            .update(InstallProfile::Fresh, |settings| {
                settings.upsert_smart_frame_preset(first)
            })
            .unwrap();
        let updated = store
            .update(InstallProfile::Fresh, |settings| {
                settings.upsert_smart_frame_preset(replacement)
            })
            .unwrap();
        assert_eq!(updated.smart_frame_presets().len(), 1);
        assert_eq!(updated.smart_frame_presets()[0].name, "Team Note Updated");

        let empty = store
            .update(InstallProfile::Fresh, |settings| {
                settings.delete_smart_frame_preset("team-note")
            })
            .unwrap();
        assert!(empty.smart_frame_presets().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[derive(Default)]
    struct Trace {
        called: Vec<AfterCaptureAction>,
        fail: Option<AfterCaptureAction>,
        observed_png: Vec<u8>,
        observed_space: Option<ColorSpace>,
    }

    impl ActionExecutor<FinalizedScreenshot<'_>> for Trace {
        fn execute(
            &mut self,
            action: AfterCaptureAction,
            artifact: &FinalizedScreenshot<'_>,
        ) -> std::result::Result<ActionEffect, String> {
            self.called.push(action);
            self.observed_png = artifact.png.to_vec();
            self.observed_space = Some(artifact.frame.color_space);
            if self.fail == Some(action) {
                return Err(format!("{} refused", action.label()));
            }
            Ok(match action {
                AfterCaptureAction::ShowRecentCapturesOverlay => {
                    ActionEffect::ShowRecentCapturesOverlay
                }
                AfterCaptureAction::OpenEditor => ActionEffect::OpenEditor,
                AfterCaptureAction::PinToScreen => ActionEffect::PinToScreen,
                AfterCaptureAction::SaveAutomatically => {
                    ActionEffect::Saved(PathBuf::from("/captures/shot.png"))
                }
                AfterCaptureAction::UploadAndCopyLink => {
                    ActionEffect::Uploaded("https://example.invalid/shot".to_owned())
                }
                AfterCaptureAction::CopyToClipboard => ActionEffect::Completed,
            })
        }
    }

    #[test]
    fn every_screenshot_action_runs_alone() {
        let native = frame(ColorSpace::DisplayP3);
        let artifact = FinalizedScreenshot {
            frame: &native,
            png: b"exact encoded bytes",
        };
        for action in AfterCaptureAction::UI_ORDER {
            let mut settings = AfterCaptureSettings::fresh();
            for other in AfterCaptureAction::UI_ORDER {
                settings.set(MediaKind::Screenshot, other, other == action);
            }

            let mut trace = Trace::default();
            let report = orchestrate(MediaKind::Screenshot, &settings, &artifact, &mut trace);
            assert_eq!(trace.called, [action]);
            assert_eq!(report.steps.len(), 1);
            assert!(matches!(
                report.steps[0].outcome,
                ActionOutcome::Succeeded(_)
            ));
        }
    }

    #[test]
    fn every_recording_action_has_an_explicit_single_action_outcome() {
        #[derive(Default)]
        struct RecordingTrace(Vec<AfterCaptureAction>);
        impl ActionExecutor<RetainedRecording> for RecordingTrace {
            fn execute(
                &mut self,
                action: AfterCaptureAction,
                _artifact: &RetainedRecording,
            ) -> std::result::Result<ActionEffect, String> {
                self.0.push(action);
                Ok(ActionEffect::Completed)
            }
        }

        let root = scratch("recording-actions");
        let source = root.join("source.mov");
        fs::create_dir_all(&root).unwrap();
        fs::write(&source, b"final media").unwrap();
        let retained = RetainedRecording::retain(&source, &root.join("retained")).unwrap();
        for action in AfterCaptureAction::UI_ORDER {
            let mut settings = AfterCaptureSettings::fresh();
            for other in AfterCaptureAction::UI_ORDER {
                settings.set(MediaKind::Recording, other, other == action);
            }
            let mut trace = RecordingTrace::default();
            let report = orchestrate(MediaKind::Recording, &settings, &retained, &mut trace);
            assert_eq!(report.steps.len(), 1);
            if action == AfterCaptureAction::PinToScreen {
                assert!(trace.0.is_empty());
                assert_eq!(report.failures().count(), 1);
            } else {
                assert_eq!(trace.0, [action]);
                assert!(matches!(
                    report.steps[0].outcome,
                    ActionOutcome::Succeeded(_)
                ));
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn combinations_use_fixed_order_and_isolate_failures() {
        let native = frame(ColorSpace::Srgb);
        let artifact = FinalizedScreenshot {
            frame: &native,
            png: b"png",
        };
        let mut settings = AfterCaptureSettings::fresh();
        for action in AfterCaptureAction::UI_ORDER {
            settings.set(MediaKind::Screenshot, action, true);
        }
        let mut trace = Trace {
            fail: Some(AfterCaptureAction::SaveAutomatically),
            ..Trace::default()
        };
        let report = orchestrate(MediaKind::Screenshot, &settings, &artifact, &mut trace);

        assert_eq!(trace.called, AfterCaptureAction::EXECUTION_ORDER);
        assert_eq!(report.steps.len(), AfterCaptureAction::UI_ORDER.len());
        assert_eq!(report.failures().count(), 1);
        assert!(report.has_effect(&ActionEffect::ShowRecentCapturesOverlay));
        assert!(report.has_effect(&ActionEffect::OpenEditor));
        assert!(report.has_effect(&ActionEffect::PinToScreen));
    }

    #[test]
    fn an_action_set_cannot_schedule_duplicate_writes_or_uploads() {
        let native = frame(ColorSpace::Srgb);
        let artifact = FinalizedScreenshot {
            frame: &native,
            png: b"png",
        };
        let mut settings = AfterCaptureSettings::fresh();
        for action in AfterCaptureAction::UI_ORDER {
            settings.set(MediaKind::Screenshot, action, false);
        }
        for _ in 0..3 {
            settings.set(
                MediaKind::Screenshot,
                AfterCaptureAction::SaveAutomatically,
                true,
            );
            settings.set(
                MediaKind::Screenshot,
                AfterCaptureAction::UploadAndCopyLink,
                true,
            );
        }
        let mut trace = Trace::default();
        let report = orchestrate(MediaKind::Screenshot, &settings, &artifact, &mut trace);
        assert_eq!(
            trace.called,
            [
                AfterCaptureAction::SaveAutomatically,
                AfterCaptureAction::UploadAndCopyLink
            ]
        );
        assert_eq!(report.steps.len(), 2);
    }

    #[test]
    fn every_action_observes_the_same_exact_bytes_and_color_profile() {
        let native = frame(ColorSpace::DisplayP3);
        let artifact = FinalizedScreenshot {
            frame: &native,
            png: b"\x89PNG\r\n\x1a\nsame artifact",
        };
        let mut settings = AfterCaptureSettings::fresh();
        for action in AfterCaptureAction::UI_ORDER {
            settings.set(MediaKind::Screenshot, action, true);
        }
        let mut trace = Trace::default();
        let _ = orchestrate(MediaKind::Screenshot, &settings, &artifact, &mut trace);
        assert_eq!(trace.observed_png, artifact.png);
        assert_eq!(trace.observed_space, Some(ColorSpace::DisplayP3));
    }

    #[test]
    fn recording_pin_is_reported_unavailable_without_suppressing_later_actions() {
        #[derive(Default)]
        struct RecordingTrace(Vec<AfterCaptureAction>);
        impl ActionExecutor<()> for RecordingTrace {
            fn execute(
                &mut self,
                action: AfterCaptureAction,
                _artifact: &(),
            ) -> std::result::Result<ActionEffect, String> {
                self.0.push(action);
                Ok(ActionEffect::Completed)
            }
        }

        let mut settings = AfterCaptureSettings::fresh();
        for action in AfterCaptureAction::UI_ORDER {
            settings.set(MediaKind::Recording, action, false);
        }
        settings.set(MediaKind::Recording, AfterCaptureAction::PinToScreen, true);
        settings.set(MediaKind::Recording, AfterCaptureAction::OpenEditor, true);
        let mut trace = RecordingTrace::default();
        let report = orchestrate(MediaKind::Recording, &settings, &(), &mut trace);
        assert_eq!(trace.0, [AfterCaptureAction::OpenEditor]);
        assert_eq!(report.steps.len(), 2);
        assert_eq!(report.failures().count(), 1);
    }

    #[test]
    fn unavailable_cells_are_exhaustive_and_explain_themselves() {
        for action in AfterCaptureAction::UI_ORDER {
            let screenshot = current_availability(MediaKind::Screenshot, action);
            assert_eq!(
                screenshot.available,
                matches!(
                    action,
                    AfterCaptureAction::ShowRecentCapturesOverlay
                        | AfterCaptureAction::CopyToClipboard
                        | AfterCaptureAction::SaveAutomatically
                        | AfterCaptureAction::OpenEditor
                )
            );
            assert_eq!(screenshot.available, screenshot.reason.is_none());

            let recording = current_availability(MediaKind::Recording, action);
            assert!(!recording.available);
            assert!(
                recording
                    .reason
                    .is_some_and(|reason| !reason.trim().is_empty()),
                "{action:?} has a mystery unavailable recording cell"
            );
        }
    }

    #[test]
    fn retained_recording_outlives_recorder_temp_cleanup() {
        let root = scratch("recording");
        let temp = root.join("recorder/session.tmp.mov");
        let retained_dir = root.join("retained");
        fs::create_dir_all(temp.parent().unwrap()).unwrap();
        fs::write(&temp, b"playable-finalized-media").unwrap();

        let retained = RetainedRecording::retain(&temp, &retained_dir).expect("retain");
        fs::remove_file(&temp).expect("recorder cleanup");

        assert!(retained.path().starts_with(&retained_dir));
        assert_ne!(retained.path(), temp);
        assert_eq!(retained.read().unwrap(), b"playable-finalized-media");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn future_unknown_fields_survive_a_round_trip() {
        let text = r#"{
  "version": 99,
  "future-root": {"kept": true},
  "values": {
    "future-bool": true,
    "future-object": {"nested": [1, 2, 3]}
  },
  "after_capture": {
    "future-media": {"kept": true},
    "screenshot": {
      "copy-to-clipboard": false,
      "teleport": {"kept": true}
    },
    "recording": {}
  }
}"#;
        let (settings, _) = AfterCaptureSettings::from_json(text).expect("read future");
        let rendered = settings.to_json().expect("rewrite");
        assert!(rendered.contains("\"version\": 99"), "{rendered}");
        assert!(rendered.contains("\"future-root\""), "{rendered}");
        assert!(rendered.contains("\"future-media\""), "{rendered}");
        assert!(rendered.contains("\"teleport\""), "{rendered}");
        assert!(rendered.contains("\"future-bool\": true"), "{rendered}");
        assert!(rendered.contains("\"future-object\""), "{rendered}");
        assert!(rendered.contains("\"nested\""), "{rendered}");
    }

    #[test]
    fn malformed_action_values_are_reported_not_silently_defaulted() {
        let error = AfterCaptureSettings::from_json(
            r#"{"version":2,"after_capture":{"screenshot":{"copy-to-clipboard":"yes"}}}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("true or false"), "{error}");
    }
}
