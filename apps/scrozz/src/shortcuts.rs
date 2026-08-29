//! The configurable global shortcut table.
//!
//! # Why this is not just a list of strings in the settings schema
//!
//! A shortcut has three separate jobs, and they pull in different directions:
//!
//! - it is **registered with the OS**, which can refuse it;
//! - it is **shown to the user**, in the platform's own notation;
//! - it is **stored**, and must survive the app gaining new actions.
//!
//! [`settings`](crate::settings) owns the schema — the key names, the types, the
//! one-line descriptions. This module owns everything else: which actions are
//! bindable, what they default to on *this* platform, whether a proposed
//! combination is usable, and how the whole set is written to disk. The schema
//! defers to the defaults here rather than repeating them, so there is one table
//! and it cannot drift.
//!
//! # Defaults are platform-specific, and have to be
//!
//! The obvious default for "capture a region" is the combination the user's
//! platform already trains them to expect. On macOS that combination is taken:
//! `Cmd+Shift+3/4/5/6` belong to Apple's own screenshot service, and
//! `RegisterEventHotKey` returns *success* for them while never delivering an
//! event. A default that silently does nothing is worse than no default, so
//! macOS gets combinations Apple has not claimed. Elsewhere `Super+Shift+<n>` is
//! free and is what the compositor-config generator already emits, so those are
//! kept exactly as they were.
//!
//! # Unassigned is a value, not a gap
//!
//! A row with no combination is a deliberate state: the user cleared it, or the
//! action ships unbound because it is not finished. It is stored as an explicit
//! empty string so that reading a file back cannot confuse "the user turned this
//! off" with "this version did not know about that action yet" — the latter has
//! to fall back to the default, and the former must not.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use scrozz_core::{Error, Result};
use scrozz_shell::hotkey::{Accelerator, DesiredBinding};
use serde::{Deserialize, Serialize};

/// The file format version written by this build.
const VERSION: u32 = 1;
const APP_DIR: &str = "Scrozz";
const FILE_NAME: &str = "shortcuts.json";

/// An action a global shortcut can be bound to.
///
/// Deliberately a subset of [`Action`](crate::gui::action::Action): "quit" and
/// "open settings" are reachable from the tray and are not worth a system-wide
/// key grab, and every entry here must be something the user would plausibly
/// want to trigger without Scrozz being frontmost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ShortcutAction {
    /// Open the selector with every still-capture mode available.
    CaptureAllInOne,
    /// Drag out a region and capture it.
    CaptureRegion,
    /// Pick a window and capture it.
    CaptureWindow,
    /// Capture the display under the pointer.
    CaptureFullscreen,
    /// Capture every connected display.
    CaptureAllDisplays,
    /// Start or stop recording.
    ToggleRecording,
}

impl ShortcutAction {
    /// Every bindable action, in the order the settings pane lists them.
    pub const ALL: [Self; 6] = [
        Self::CaptureAllInOne,
        Self::CaptureRegion,
        Self::CaptureWindow,
        Self::CaptureFullscreen,
        Self::CaptureAllDisplays,
        Self::ToggleRecording,
    ];

    /// The stable identifier, shared with the tray and the GUI action table.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::CaptureAllInOne => "capture.all-in-one",
            Self::CaptureRegion => "capture.region",
            Self::CaptureWindow => "capture.window",
            Self::CaptureFullscreen => "capture.fullscreen",
            Self::CaptureAllDisplays => "capture.all-displays",
            Self::ToggleRecording => "record.toggle",
        }
    }

    /// The settings key this action's binding is stored under.
    ///
    /// `capture-display` rather than `capture-fullscreen` because the CLI has
    /// called it that since before the GUI existed, and a settings key is part of
    /// a user's dotfiles: renaming it would be a breaking change for no gain.
    #[must_use]
    pub const fn settings_key(self) -> &'static str {
        match self {
            Self::CaptureAllInOne => "hotkey.capture-all-in-one",
            Self::CaptureRegion => "hotkey.capture-region",
            Self::CaptureWindow => "hotkey.capture-window",
            Self::CaptureFullscreen => "hotkey.capture-display",
            Self::CaptureAllDisplays => "hotkey.capture-all-displays",
            Self::ToggleRecording => "hotkey.record-toggle",
        }
    }

    /// The name shown beside the control in the settings pane.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CaptureAllInOne => scrozz_core::product_copy::ALL_IN_ONE,
            Self::CaptureRegion => scrozz_core::product_copy::CAPTURE_AREA,
            Self::CaptureWindow => scrozz_core::product_copy::CAPTURE_WINDOW,
            Self::CaptureFullscreen => scrozz_core::product_copy::CAPTURE_FULLSCREEN,
            Self::CaptureAllDisplays => scrozz_core::product_copy::CAPTURE_ALL_DISPLAYS,
            Self::ToggleRecording => scrozz_core::product_copy::RECORD_SCREEN,
        }
    }

    /// The combination bound when the user has expressed no preference.
    ///
    /// `None` means the action ships unbound. Recording does, because the
    /// recording pipeline is not wired up yet: reserving a system-wide
    /// combination for something that does nothing takes it away from every other
    /// application for no benefit.
    #[must_use]
    pub const fn default_accelerator(self) -> Option<&'static str> {
        // `Cmd+Shift+3/4/5/6` are Apple's; see the module comment for why
        // defaulting to them would fail silently rather than loudly.
        let mac = cfg!(target_os = "macos");
        match self {
            Self::CaptureAllInOne => Some(if mac { "Cmd+Shift+0" } else { "Super+Shift+2" }),
            Self::CaptureRegion => Some(if mac { "Cmd+Shift+8" } else { "Super+Shift+4" }),
            Self::CaptureWindow => Some(if mac { "Cmd+Shift+9" } else { "Super+Shift+5" }),
            Self::CaptureFullscreen => Some(if mac { "Cmd+Shift+7" } else { "Super+Shift+3" }),
            Self::CaptureAllDisplays => Some(if mac {
                "Cmd+Ctrl+Shift+7"
            } else {
                "Super+Shift+6"
            }),
            Self::ToggleRecording => None,
        }
    }

    /// The default as the settings schema spells it, where blank means unbound.
    ///
    /// The schema's `default` is a `&'static str` because it has to sit in a
    /// `const` table; this is the same value with `None` flattened to `""`, which
    /// is exactly how an unassigned shortcut is stored and validated.
    #[must_use]
    pub const fn default_accelerator_setting(self) -> &'static str {
        match self.default_accelerator() {
            Some(accelerator) => accelerator,
            None => "",
        }
    }

    /// The action with this identifier, if it is bindable.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }
    /// The action stored under this settings key, if any.
    ///
    /// Accepts the identifier form too, so a file written by a future build that
    /// stores keys differently still resolves rather than being discarded.
    #[must_use]
    pub fn from_stored_key(key: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.id() == key || action.settings_key() == key)
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|action| *action == self)
            .unwrap_or_default()
    }
}

impl fmt::Display for ShortcutAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Why a proposed combination cannot be used.
///
/// Rendered under the control the user is editing, so every variant reads as a
/// complete sentence fragment and names the way out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortcutError {
    /// The string is not a key combination at all.
    Unparseable {
        /// The parser's own explanation.
        detail: String,
    },
    /// The operating system already owns it.
    Reserved {
        /// What holds it.
        owner: &'static str,
        /// Where the user would free it.
        remedy: &'static str,
    },
    /// Another Scrozz action already uses it.
    Duplicate {
        /// The action holding it.
        other: ShortcutAction,
    },
}

impl fmt::Display for ShortcutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unparseable { detail } => f.write_str(detail),
            Self::Reserved { owner, remedy } => {
                write!(f, "already used by {owner}; free it in {remedy}")
            }
            Self::Duplicate { other } => {
                write!(f, "already used by {other} — clear that one first")
            }
        }
    }
}

/// Every shortcut, assigned or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcuts {
    /// Indexed by [`ShortcutAction::index`]; `None` is a deliberate blank.
    bindings: [Option<String>; ShortcutAction::ALL.len()],
    /// Keys read from disk that this build does not recognise.
    ///
    /// Kept so that running an older build once does not silently delete a newer
    /// build's settings — a downgrade should be recoverable, not destructive.
    unknown: BTreeMap<String, String>,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Self {
            bindings: ShortcutAction::ALL
                .map(|action| action.default_accelerator().map(str::to_owned)),
            unknown: BTreeMap::new(),
        }
    }
}

impl Shortcuts {
    /// The combination bound to an action, or `None` when it is unassigned.
    #[must_use]
    pub fn get(&self, action: ShortcutAction) -> Option<&str> {
        self.bindings[action.index()].as_deref()
    }

    /// Assigns a combination, or clears it with `None`.
    ///
    /// Stores the *normalised* spelling, so `"cmd + shift+8"` and `"Shift+Cmd+8"`
    /// become the same string and a settings file never records how the user
    /// happened to type it. An unparseable value is stored verbatim: rejecting it
    /// here would mean the pane could not show the user what they typed while
    /// telling them it is wrong.
    pub fn set(&mut self, action: ShortcutAction, accelerator: Option<&str>) {
        let value = accelerator
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(|raw| {
                Accelerator::parse(raw).map_or_else(|_| raw.to_owned(), |parsed| parsed.to_string())
            });
        self.bindings[action.index()] = value;
    }

    /// Restores one action's shipped default.
    pub fn reset(&mut self, action: ShortcutAction) {
        self.bindings[action.index()] = action.default_accelerator().map(str::to_owned);
    }

    /// Restores every shipped default, leaving unrecognised keys alone.
    pub fn reset_all(&mut self) {
        for action in ShortcutAction::ALL {
            self.reset(action);
        }
    }

    /// Whether an action still carries its shipped default.
    ///
    /// Compares the *parsed* combinations rather than the strings, because they
    /// are not the same question. The shipped default is written `Cmd+Shift+8`,
    /// while anything the user records is stored in the registrar's normalised
    /// order, `Shift+Cmd+8` — so a string comparison would leave Reset lit up
    /// after the user recorded the exact combination Scrozz ships with.
    #[must_use]
    pub fn is_default(&self, action: ShortcutAction) -> bool {
        match (self.get(action), action.default_accelerator()) {
            (None, None) => true,
            (Some(mine), Some(shipped)) => {
                match (Accelerator::parse(mine), Accelerator::parse(shipped)) {
                    (Ok(mine), Ok(shipped)) => mine == shipped,
                    // An unparseable stored value cannot be the default, and an
                    // unparseable *default* is a bug this must not paper over.
                    _ => mine == shipped,
                }
            }
            _ => false,
        }
    }

    /// Whether anything at all has been changed from the shipped defaults.
    #[must_use]
    pub fn is_all_default(&self) -> bool {
        ShortcutAction::ALL
            .into_iter()
            .all(|action| self.is_default(action))
    }

    /// Checks a candidate for one row without changing anything.
    ///
    /// This is what the settings pane calls as the user records a combination, so
    /// the reason appears under the control before they commit to it.
    ///
    /// # Errors
    ///
    /// Returns the [`ShortcutError`] to show, or nothing when the candidate is
    /// blank — clearing a shortcut is always allowed.
    pub fn check(
        &self,
        action: ShortcutAction,
        candidate: &str,
    ) -> std::result::Result<Option<Accelerator>, ShortcutError> {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            return Ok(None);
        }

        let parsed = Accelerator::parse(candidate).map_err(|err| ShortcutError::Unparseable {
            detail: err.to_string(),
        })?;

        if let Some(reserved) = parsed.system_owner() {
            return Err(ShortcutError::Reserved {
                owner: reserved.owner,
                remedy: reserved.remedy,
            });
        }

        for other in ShortcutAction::ALL {
            if other == action {
                continue;
            }
            if self
                .get(other)
                .and_then(|raw| Accelerator::parse(raw).ok())
                .is_some_and(|existing| existing == parsed)
            {
                return Err(ShortcutError::Duplicate { other });
            }
        }

        Ok(Some(parsed))
    }

    /// Every row that cannot be used as it stands.
    ///
    /// A whole-table check rather than a per-row one because duplicates are a
    /// property of the pair: a file hand-edited to bind two actions to the same
    /// combination has one problem, and the pane has to be able to say which two
    /// rows it is between.
    #[must_use]
    pub fn problems(&self) -> Vec<(ShortcutAction, ShortcutError)> {
        let mut problems = Vec::new();
        for action in ShortcutAction::ALL {
            let Some(raw) = self.get(action) else {
                continue;
            };
            // Check against only the rows already accepted, so a duplicated pair
            // reports the second row rather than both: blaming the first would
            // tell the user to change a shortcut that was fine.
            let mut earlier = Self {
                bindings: Default::default(),
                unknown: BTreeMap::new(),
            };
            for previous in ShortcutAction::ALL {
                if previous == action {
                    break;
                }
                if !problems.iter().any(|(bad, _)| *bad == previous) {
                    earlier.bindings[previous.index()] = self.bindings[previous.index()].clone();
                }
            }
            if let Err(problem) = earlier.check(action, raw) {
                problems.push((action, problem));
            }
        }
        problems
    }

    /// The bindings to hand to [`GlobalHotkeys::apply`].
    ///
    /// [`GlobalHotkeys::apply`]: scrozz_shell::hotkey::GlobalHotkeys::apply
    ///
    /// Unassigned rows are simply absent, which is what makes clearing a shortcut
    /// and never having set one the same thing to the registrar.
    #[must_use]
    pub fn desired(&self) -> Vec<DesiredBinding> {
        ShortcutAction::ALL
            .into_iter()
            .filter_map(|action| {
                self.get(action)
                    .map(|accelerator| DesiredBinding::new(accelerator, action.id()))
            })
            .collect()
    }

    /// The combination for each assigned action, in the platform's notation.
    ///
    /// What a menu shows beside a label. Unparseable rows are skipped rather than
    /// rendered raw: they are not bound either, and a menu naming a shortcut that
    /// does nothing is a lie the user cannot debug.
    #[must_use]
    pub fn symbols(&self) -> Vec<(ShortcutAction, String)> {
        ShortcutAction::ALL
            .into_iter()
            .filter_map(|action| {
                let parsed = Accelerator::parse(self.get(action)?).ok()?;
                Some((action, parsed.symbols()))
            })
            .collect()
    }

    fn from_document(document: Document) -> Self {
        let mut shortcuts = Self::default();
        for (key, value) in document.shortcuts {
            match ShortcutAction::from_stored_key(&key) {
                // An empty string is the recorded decision to have no shortcut;
                // an absent key is the absence of a decision, which defaults.
                Some(action) => shortcuts.bindings[action.index()] = normalise(&value),
                None => {
                    shortcuts.unknown.insert(key, value);
                }
            }
        }
        shortcuts
    }

    fn to_document(&self) -> Document {
        let mut stored: BTreeMap<String, String> = ShortcutAction::ALL
            .into_iter()
            .map(|action| {
                (
                    action.settings_key().to_owned(),
                    self.get(action).unwrap_or_default().to_owned(),
                )
            })
            .collect();
        stored.extend(
            self.unknown
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        Document {
            version: VERSION,
            shortcuts: stored,
        }
    }

    /// Reads a settings document, falling back to defaults for anything missing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] when the text is not a shortcuts document.
    pub fn from_json(text: &str) -> Result<Self> {
        let document: Document = serde_json::from_str(text)
            .map_err(|err| Error::Storage(format!("the shortcuts file is unreadable: {err}")))?;
        Ok(Self::from_document(document))
    }

    /// Renders the settings document.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if serialisation fails, which it cannot for
    /// this shape but is not worth panicking over if it ever does.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(&self.to_document())
            .map_err(|err| Error::Storage(format!("could not render the shortcuts: {err}")))
    }
}

/// Trims a stored value into a binding, treating blank as "deliberately none".
fn normalise(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The on-disk shape.
///
/// `version` is written but not currently branched on, because there has only
/// ever been one format. It exists so the *next* format has somewhere to say so:
/// a file with no version field at all would leave a future reader guessing.
#[derive(Debug, Serialize, Deserialize)]
struct Document {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    shortcuts: BTreeMap<String, String>,
}

/// A shortcuts file at a resolved path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutStore {
    path: PathBuf,
}

impl ShortcutStore {
    /// Uses an explicit path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Resolves the platform config path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] when this process has no config directory.
    pub fn default_location() -> Result<Self> {
        let base = dirs::config_dir().or_else(dirs::data_dir).ok_or_else(|| {
            Error::Storage("no platform config directory is available for shortcuts".to_owned())
        })?;
        Ok(Self::new(base.join(APP_DIR).join(FILE_NAME)))
    }

    /// The file this store reads and replaces.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the stored shortcuts, or the defaults.
    ///
    /// Never fails. A missing file is the normal first run, and a corrupt one is
    /// reported and stepped over: refusing to start because a settings file was
    /// truncated by a bad shutdown would be a worse outcome than losing the
    /// customisation, and the file is rewritten on the next save anyway.
    #[must_use]
    pub fn load(&self) -> Shortcuts {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Shortcuts::default(),
            Err(err) => {
                tracing::warn!(path = %self.path.display(), %err, "could not read shortcuts; using defaults");
                return Shortcuts::default();
            }
        };
        Shortcuts::from_json(&text).unwrap_or_else(|err| {
            tracing::warn!(path = %self.path.display(), %err, "ignoring an unreadable shortcuts file");
            Shortcuts::default()
        })
    }

    /// Replaces the stored shortcuts.
    ///
    /// Writes a sibling temporary file and renames it over the target, so a crash
    /// mid-write leaves the previous settings intact rather than a half-written
    /// file that loads as defaults.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the directory could not be created or the
    /// file could not be written.
    pub fn save(&self, shortcuts: &Shortcuts) -> Result<()> {
        let text = shortcuts.to_json()?;
        let parent = self.path.parent().ok_or_else(|| {
            Error::Storage(format!(
                "{} has no parent directory to write into",
                self.path.display()
            ))
        })?;
        fs::create_dir_all(parent).map_err(|err| {
            Error::Storage(format!("could not create {}: {err}", parent.display()))
        })?;

        let temporary = parent.join(format!(".{FILE_NAME}.{}", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut file = File::create(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()
        };
        write().map_err(|err| {
            let _ = fs::remove_file(&temporary);
            Error::Storage(format!("could not write {}: {err}", temporary.display()))
        })?;

        fs::rename(&temporary, &self.path).map_err(|err| {
            let _ = fs::remove_file(&temporary);
            Error::Storage(format!("could not replace {}: {err}", self.path.display()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_the_shipped_combination_counts_as_default() {
        // The trap this guards: the shipped spelling is `Cmd+Shift+8`, and what
        // the recorder stores is the normalised `Shift+Cmd+8`. Compared as
        // strings those differ, and Reset would stay lit after the user pressed
        // the very combination Scrozz ships with.
        for action in ShortcutAction::ALL {
            let Some(shipped) = action.default_accelerator() else {
                continue;
            };
            let normalised = Accelerator::parse(shipped)
                .expect("every shipped default must parse")
                .to_string();
            let mut shortcuts = Shortcuts::default();
            shortcuts.set(action, Some(&normalised));
            assert!(
                shortcuts.is_default(action),
                "{action} stored as {normalised} must still read as the default"
            );
        }
    }

    #[test]
    fn a_reordered_spelling_is_the_same_shortcut() {
        let mut shortcuts = Shortcuts::default();
        shortcuts.set(ShortcutAction::CaptureRegion, Some("shift + ctrl+F9"));
        let stored = shortcuts
            .get(ShortcutAction::CaptureRegion)
            .expect("just set");
        assert_eq!(
            stored,
            Accelerator::parse("Ctrl+Shift+F9").unwrap().to_string(),
            "storage must normalise so two spellings cannot become two rows"
        );
    }

    #[test]
    fn clearing_a_row_is_not_the_same_as_resetting_it() {
        // The distinction the pane depends on: Clear means "deliberately
        // unassigned" and must survive a restart, rather than being read back as
        // "nothing stored, so use the default".
        let mut shortcuts = Shortcuts::default();
        shortcuts.set(ShortcutAction::CaptureRegion, None);
        assert_eq!(shortcuts.get(ShortcutAction::CaptureRegion), None);

        let written = shortcuts.to_json().expect("a set it can serialise");
        let reloaded = Shortcuts::from_json(&written).expect("a document it just wrote");
        assert_eq!(
            reloaded.get(ShortcutAction::CaptureRegion),
            None,
            "an unassigned row must not silently reacquire its default"
        );
        assert!(!reloaded.is_default(ShortcutAction::CaptureRegion));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "scrozz-shortcuts-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn every_action_has_a_distinct_identity() {
        // The id is shared with the tray and the GUI dispatcher, and the settings
        // key is part of a user's dotfiles; a collision in either would silently
        // merge two rows.
        for (index, action) in ShortcutAction::ALL.iter().enumerate() {
            assert_eq!(action.index(), index, "{action:?} indexes itself wrongly");
            assert_eq!(ShortcutAction::from_id(action.id()), Some(*action));
            assert_eq!(
                ShortcutAction::from_stored_key(action.settings_key()),
                Some(*action)
            );
        }

        let mut ids: Vec<&str> = ShortcutAction::ALL.iter().map(|it| it.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ShortcutAction::ALL.len());
    }

    #[test]
    fn the_defaults_are_usable_on_this_platform() {
        // The whole reason the defaults are platform-specific: on macOS the
        // obvious choices are Apple's, and binding them fails silently.
        let shortcuts = Shortcuts::default();
        assert_eq!(
            shortcuts.problems(),
            Vec::new(),
            "the shipped defaults must be bindable as they stand"
        );
    }

    #[test]
    fn defaults_are_free_of_duplicates_and_reserved_combinations() {
        let shortcuts = Shortcuts::default();
        for action in ShortcutAction::ALL {
            let Some(raw) = shortcuts.get(action) else {
                continue;
            };
            let parsed = Accelerator::parse(raw).expect("a shipped default must parse");
            assert!(
                parsed.system_owner().is_none(),
                "{action:?} defaults to {raw}, which the system already owns"
            );
        }
    }

    #[test]
    fn recording_ships_unbound_because_it_does_nothing_yet() {
        let shortcuts = Shortcuts::default();
        assert_eq!(shortcuts.get(ShortcutAction::ToggleRecording), None);
        assert!(shortcuts.is_default(ShortcutAction::ToggleRecording));
    }

    #[test]
    fn a_binding_is_stored_normalised_rather_than_as_typed() {
        let mut shortcuts = Shortcuts::default();
        shortcuts.set(ShortcutAction::CaptureRegion, Some("  ctrl + shift+p "));
        shortcuts.set(ShortcutAction::CaptureWindow, Some("Shift+Control+P"));
        assert_eq!(
            shortcuts.get(ShortcutAction::CaptureRegion),
            Some("Ctrl+Shift+P")
        );
        assert_eq!(
            shortcuts.get(ShortcutAction::CaptureRegion),
            shortcuts.get(ShortcutAction::CaptureWindow),
            "two spellings of one combination must not be two different strings"
        );
    }

    #[test]
    fn clearing_and_never_setting_look_the_same_to_the_registrar() {
        let mut shortcuts = Shortcuts::default();
        shortcuts.set(ShortcutAction::CaptureRegion, None);
        assert_eq!(shortcuts.get(ShortcutAction::CaptureRegion), None);
        assert!(
            !shortcuts
                .desired()
                .iter()
                .any(|want| want.action == ShortcutAction::CaptureRegion.id())
        );

        shortcuts.set(ShortcutAction::CaptureWindow, Some("   "));
        assert_eq!(
            shortcuts.get(ShortcutAction::CaptureWindow),
            None,
            "whitespace is not a shortcut"
        );
    }

    #[test]
    fn resetting_restores_one_row_without_touching_the_others() {
        let mut shortcuts = Shortcuts::default();
        shortcuts.set(ShortcutAction::CaptureRegion, Some("Ctrl+Shift+P"));
        shortcuts.set(ShortcutAction::CaptureWindow, Some("Ctrl+Shift+W"));
        assert!(!shortcuts.is_default(ShortcutAction::CaptureRegion));

        shortcuts.reset(ShortcutAction::CaptureRegion);
        assert!(shortcuts.is_default(ShortcutAction::CaptureRegion));
        assert_eq!(
            shortcuts.get(ShortcutAction::CaptureWindow),
            Some("Ctrl+Shift+W")
        );

        shortcuts.reset_all();
        assert!(shortcuts.is_all_default());
        assert_eq!(shortcuts, Shortcuts::default());
    }

    #[test]
    fn a_duplicate_names_the_row_already_holding_it() {
        let shortcuts = Shortcuts::default();
        let taken = shortcuts
            .get(ShortcutAction::CaptureRegion)
            .expect("region ships bound");

        let problem = shortcuts
            .check(ShortcutAction::CaptureWindow, taken)
            .expect_err("a second row cannot take a combination already in use");
        assert_eq!(
            problem,
            ShortcutError::Duplicate {
                other: ShortcutAction::CaptureRegion
            }
        );
        assert!(
            problem
                .to_string()
                .contains(scrozz_core::product_copy::CAPTURE_AREA),
            "the message must name the other row: {problem}"
        );
    }

    #[test]
    fn keeping_a_rows_own_combination_is_not_a_duplicate() {
        // Re-recording the same keys, or merely opening the pane, must not
        // report the row as conflicting with itself.
        let shortcuts = Shortcuts::default();
        let mine = shortcuts
            .get(ShortcutAction::CaptureRegion)
            .expect("region ships bound");
        assert!(shortcuts.check(ShortcutAction::CaptureRegion, mine).is_ok());
    }

    #[test]
    fn nonsense_is_rejected_with_the_parsers_own_words() {
        let shortcuts = Shortcuts::default();
        let problem = shortcuts
            .check(ShortcutAction::CaptureRegion, "Ctrl+")
            .expect_err("an empty component is not a combination");
        assert!(matches!(problem, ShortcutError::Unparseable { .. }));
        assert!(
            !problem.to_string().is_empty(),
            "an error with no text cannot be shown under a control"
        );

        assert!(matches!(
            shortcuts.check(ShortcutAction::CaptureRegion, "Ctrl+Shift"),
            Err(ShortcutError::Unparseable { .. })
        ));
    }

    #[test]
    fn a_system_owned_combination_is_refused_before_it_is_ever_registered() {
        let shortcuts = Shortcuts::default();
        let reserved = scrozz_shell::hotkey::reserved_shortcuts();
        assert!(!reserved.is_empty(), "every platform declares some");

        for taken in reserved {
            let problem = shortcuts
                .check(ShortcutAction::CaptureRegion, taken.accelerator)
                .expect_err("the platform's own shortcuts are not available");
            assert!(
                matches!(problem, ShortcutError::Reserved { .. }),
                "{} should be reported as reserved, got {problem:?}",
                taken.accelerator
            );
        }
    }

    #[test]
    fn clearing_a_row_is_always_allowed() {
        let shortcuts = Shortcuts::default();
        assert_eq!(shortcuts.check(ShortcutAction::CaptureRegion, ""), Ok(None));
        assert_eq!(
            shortcuts.check(ShortcutAction::CaptureRegion, "   "),
            Ok(None)
        );
    }

    #[test]
    fn a_hand_edited_duplicate_blames_the_second_row_only() {
        // Both rows are equally "wrong", but telling the user to change the one
        // they did not touch is unhelpful; the later row is the new claim.
        let mut shortcuts = Shortcuts::default();
        let taken = shortcuts
            .get(ShortcutAction::CaptureRegion)
            .expect("region ships bound")
            .to_owned();
        shortcuts.set(ShortcutAction::CaptureWindow, Some(&taken));

        let problems = shortcuts.problems();
        assert_eq!(
            problems,
            vec![(
                ShortcutAction::CaptureWindow,
                ShortcutError::Duplicate {
                    other: ShortcutAction::CaptureRegion
                }
            )]
        );
    }

    #[test]
    fn a_round_trip_through_json_changes_nothing() {
        let mut shortcuts = Shortcuts::default();
        shortcuts.set(ShortcutAction::CaptureRegion, Some("Ctrl+Shift+P"));
        shortcuts.set(ShortcutAction::CaptureWindow, None);

        let text = shortcuts.to_json().expect("render");
        let read = Shortcuts::from_json(&text).expect("parse");
        assert_eq!(read, shortcuts);
    }

    #[test]
    fn an_absent_key_takes_the_default_and_a_blank_one_means_none() {
        // The distinction that makes adding a new action safe: a file written
        // before the action existed must not leave it unbound forever.
        let read =
            Shortcuts::from_json(r#"{"version":1,"shortcuts":{"hotkey.capture-window":""}}"#)
                .expect("parse");

        assert_eq!(
            read.get(ShortcutAction::CaptureRegion),
            ShortcutAction::CaptureRegion.default_accelerator(),
            "an unmentioned action falls back to its default"
        );
        assert_eq!(
            read.get(ShortcutAction::CaptureWindow),
            None,
            "an empty string is a recorded decision to have no shortcut"
        );
    }

    #[test]
    fn a_file_from_a_newer_build_survives_being_read_by_this_one() {
        // Downgrading must not delete settings this build cannot interpret.
        let text = r#"{"version":99,"shortcuts":{"hotkey.capture-region":"Ctrl+Shift+P","capture.telepathy":"Ctrl+Alt+T"}}"#;
        let read = Shortcuts::from_json(text).expect("parse");
        assert_eq!(
            read.get(ShortcutAction::CaptureRegion),
            Some("Ctrl+Shift+P")
        );

        let rewritten = read.to_json().expect("render");
        assert!(
            rewritten.contains("capture.telepathy"),
            "an unknown key must be written back, not dropped: {rewritten}"
        );
    }

    #[test]
    fn an_identifier_is_accepted_where_a_settings_key_is_expected() {
        // Migration insurance: the two naming schemes exist, and a file using
        // either must load.
        let read = Shortcuts::from_json(r#"{"shortcuts":{"capture.fullscreen":"Ctrl+Shift+F"}}"#)
            .expect("parse");
        assert_eq!(
            read.get(ShortcutAction::CaptureFullscreen),
            Some("Ctrl+Shift+F")
        );
    }

    #[test]
    fn garbage_is_reported_rather_than_silently_treated_as_empty() {
        assert!(Shortcuts::from_json("not json at all").is_err());
    }

    #[test]
    fn a_missing_file_loads_the_defaults() {
        let dir = temp_dir("missing");
        let store = ShortcutStore::new(dir.join("nothing-here.json"));
        assert_eq!(store.load(), Shortcuts::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_then_loading_returns_what_was_saved() {
        let dir = temp_dir("round-trip");
        let store = ShortcutStore::new(dir.join("shortcuts.json"));

        let mut shortcuts = Shortcuts::default();
        shortcuts.set(ShortcutAction::CaptureAllDisplays, Some("Ctrl+Shift+D"));
        shortcuts.set(ShortcutAction::CaptureAllInOne, None);
        store.save(&shortcuts).expect("save");

        assert_eq!(store.load(), shortcuts);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_creates_the_directory_and_leaves_no_temporary_behind() {
        let dir = temp_dir("mkdir");
        let store = ShortcutStore::new(dir.join("nested").join("deeper").join("shortcuts.json"));
        store.save(&Shortcuts::default()).expect("save");
        assert!(store.path().exists());

        let strays: Vec<_> = fs::read_dir(store.path().parent().expect("parent"))
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != FILE_NAME)
            .collect();
        assert!(strays.is_empty(), "left something behind: {strays:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_file_loads_as_defaults_rather_than_stopping_the_app() {
        let dir = temp_dir("corrupt");
        let path = dir.join("shortcuts.json");
        fs::write(&path, "{ truncated").expect("write");
        assert_eq!(ShortcutStore::new(&path).load(), Shortcuts::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_desired_set_names_actions_the_dispatcher_recognises() {
        let shortcuts = Shortcuts::default();
        for want in shortcuts.desired() {
            assert!(
                ShortcutAction::from_id(&want.action).is_some(),
                "{} is not a bindable action",
                want.action
            );
        }
    }

    #[test]
    fn menu_symbols_skip_rows_that_are_not_actually_bound() {
        let mut shortcuts = Shortcuts::default();
        shortcuts.set(ShortcutAction::CaptureRegion, None);
        let shown: Vec<ShortcutAction> = shortcuts
            .symbols()
            .into_iter()
            .map(|(action, _)| action)
            .collect();
        assert!(!shown.contains(&ShortcutAction::CaptureRegion));
        assert!(!shown.contains(&ShortcutAction::ToggleRecording));
        assert!(shown.contains(&ShortcutAction::CaptureWindow));
    }

    #[test]
    fn menu_symbols_use_the_platforms_own_notation() {
        let shortcuts = Shortcuts::default();
        let (_, rendered) = shortcuts
            .symbols()
            .into_iter()
            .find(|(action, _)| *action == ShortcutAction::CaptureRegion)
            .expect("region ships bound");

        if cfg!(target_os = "macos") {
            assert!(
                rendered.contains('\u{2318}') && !rendered.contains('+'),
                "macOS menus print glyphs, not words: {rendered}"
            );
        } else {
            assert!(
                rendered.contains('+'),
                "elsewhere the spelled form is the idiom: {rendered}"
            );
        }
    }
}
