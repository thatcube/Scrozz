//! The settings form: a UI-only view model, owned end to end.
//!
//! [`SettingsForm`] is a plain value — no lifetime, no handle back into the
//! app's own settings store, no persistence of its own. The app builds one from
//! whatever it actually persists (a config file, a `scrozz-core` type, an OS
//! preference store — this crate neither knows nor cares), hands it to
//! [`crate::settings_view::render`] every frame, and folds the
//! [`crate::settings_view::SettingsAction`]s that come back into its own model.
//! That is the entire contract, and it is what keeps this crate free of a
//! dependency on `scrozz-shell` or on any particular persistence mechanism.
//!
//! # Rows are a closed, described set
//!
//! A [`Row`] is one line of the form: stable `id`, end-user label, and a
//! [`RowKind`] carrying whatever value that kind of control needs. The eight
//! kinds are exactly what the settings surface draws: a section header, a
//! toggle, a free-text field, a dropdown, an integer slider, a filesystem path
//! (edited by typing or by an app-owned browse dialog), a keyboard shortcut
//! (with its own conflict/validation state — the signature control, see
//! [`ShortcutStatus`]), and a filename template, which is a text field with
//! validation attached because a bad token in it silently corrupts every future
//! capture's filename.
//!
//! # Dirty and reset without persistence
//!
//! A [`SettingsForm`] remembers the rows it started with. [`SettingsForm::is_dirty`]
//! compares against that baseline; [`SettingsForm::reset`] restores it;
//! [`SettingsForm::commit`] moves the baseline forward after a successful save.
//! None of that touches disk — it is exactly the bookkeeping a settings surface
//! needs and nothing a persistence layer would also have to duplicate.

use std::{collections::BTreeMap, fmt};

use crate::paint::Mod;

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// A row's stable identity.
///
/// The app matches these against its own settings schema (a field name, an
/// enum discriminant — whatever it already has); this crate never interprets
/// them beyond equality. Never reuse an id for a different meaning: it is the
/// only handle the app has for routing a change back to the right setting.
pub type RowId = &'static str;

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Whether a validated value is acceptable, and why not when it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validation {
    /// The value may be saved.
    Valid,
    /// The value may not be saved, with a plain-English reason to show under
    /// the row.
    Invalid(String),
}

impl Validation {
    /// Whether this is [`Validation::Valid`].
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }

    /// The message to show under the row, if any.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Valid => None,
            Self::Invalid(reason) => Some(reason),
        }
    }
}

/// Placeholder tokens a filename template may reference.
///
/// Closed on purpose: an unrecognised `{token}` in a filename template would
/// otherwise silently become a literal, undiscovered until someone reads a
/// disk full of files literally named `{oops}.png`.
pub const TEMPLATE_TOKENS: &[&str] = &[
    "year", "month", "day", "hour", "minute", "second", "date", "time", "app", "title", "seq",
    "width", "height",
];

/// Validates a filename template.
///
/// This mirrors the app's export-backed parser: `{{` and `}}` are literal
/// braces, unmatched braces are rejected, and every field is drawn from
/// [`TEMPLATE_TOKENS`]. Pure and total: the same string always produces the
/// same verdict, which is what makes it testable without a running form and
/// what makes [`SettingsForm::apply`] able to call it as a plain function.
#[must_use]
pub fn validate_template(value: &str) -> Validation {
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
            }
            '{' => {
                let mut token = String::new();
                let mut closed = false;
                for character in chars.by_ref() {
                    if character == '}' {
                        closed = true;
                        break;
                    }
                    token.push(character);
                }
                if !closed {
                    return Validation::Invalid(format!(
                        "The filename template has an unclosed '{{' before {token:?}."
                    ));
                }
                if !TEMPLATE_TOKENS.contains(&token.as_str()) {
                    return Validation::Invalid(format!(
                        "Unknown placeholder {{{token}}}. Use one of: {}.",
                        TEMPLATE_TOKENS.join(", ")
                    ));
                }
            }
            '}' => {
                return Validation::Invalid(
                    "The filename template has an unmatched '}'. Write '}}' for a literal brace."
                        .to_owned(),
                );
            }
            _ => {}
        }
    }
    Validation::Valid
}

// ---------------------------------------------------------------------------
// The shortcut recorder's value
// ---------------------------------------------------------------------------

/// A recorded keyboard shortcut, owned so it can travel with the form.
///
/// [`crate::paint::Shortcut`] is the read-only, `'static` menu-item glyph and
/// cannot represent a chord the user just recorded, so this is its owned
/// counterpart. It reuses [`Mod`] rather than re-deriving platform glyphs a
/// second time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShortcutChord {
    /// Modifiers held, in the platform's conventional order.
    pub mods: Vec<Mod>,
    /// The key itself, already in display form (`"4"`, `"Space"`).
    pub key: String,
}

impl ShortcutChord {
    /// A chord with no modifiers.
    #[must_use]
    pub fn bare(key: impl Into<String>) -> Self {
        Self {
            mods: Vec::new(),
            key: key.into(),
        }
    }

    /// A chord with modifiers.
    #[must_use]
    pub fn with_mods(mods: Vec<Mod>, key: impl Into<String>) -> Self {
        Self {
            mods,
            key: key.into(),
        }
    }

    /// The glyph run shown in the row, e.g. `⇧⌘4`.
    #[must_use]
    pub fn glyphs(&self) -> String {
        let mut s = String::new();
        for m in &self.mods {
            s.push_str(m.glyph());
        }
        s.push_str(&self.key);
        s
    }

    /// The spoken form, e.g. `Shift Command 4` (D13).
    #[must_use]
    pub fn spoken(&self) -> String {
        let mut parts: Vec<&str> = self.mods.iter().map(|m| m.spoken()).collect();
        parts.push(&self.key);
        parts.join(" ")
    }
}

impl fmt::Display for ShortcutChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.glyphs())
    }
}

/// The live state of a shortcut row.
///
/// This is the signature control of the settings surface: a shortcut is either
/// sitting quietly, actively being recorded, or presenting a problem the user
/// must resolve before it can be saved. The three states are drawn distinctly
/// so a conflict cannot be mistaken for a live recording session.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShortcutStatus {
    /// Showing the current chord (or "not set"), waiting to be clicked.
    Idle,
    /// The row has focus and is capturing key events into a new chord.
    Recording,
    /// The recorded chord collides with another shortcut or a reserved OS
    /// combination.
    ///
    /// This crate never computes the collision itself — it has no view of the
    /// rest of the app's bindings or the OS's reserved set — so the app sets
    /// this via [`SettingsForm::set_shortcut_status`] once it has checked.
    Conflict {
        /// What it collides with, in words the user already recognises (an
        /// action name, not an internal id).
        with: String,
    },
    /// The recorded chord is not acceptable on its own terms (a bare modifier,
    /// an OS-reserved combination with no override).
    Invalid {
        /// Why, in plain language.
        reason: String,
    },
}

impl ShortcutStatus {
    /// Whether this status blocks a save.
    #[must_use]
    pub const fn blocks_save(&self) -> bool {
        matches!(self, Self::Conflict { .. } | Self::Invalid { .. })
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// What a row shows and edits.
///
/// Closed in spirit, `#[non_exhaustive]` in fact: the eight kinds below are
/// every control the settings surface currently draws. Adding a ninth is a
/// deliberate design decision, not a drive-by addition, which is exactly what
/// `#[non_exhaustive]` is for.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RowKind {
    /// A section header: no value, just a label that begins a group.
    Section,
    /// A boolean on/off control.
    Toggle {
        /// Current value.
        value: bool,
    },
    /// Free text with no validation.
    TextField {
        /// Current value.
        value: String,
        /// Shown when `value` is empty.
        placeholder: &'static str,
    },
    /// One choice among a fixed, closed set.
    Dropdown {
        /// The choices, in display order.
        options: Vec<&'static str>,
        /// Index into `options`.
        selected: usize,
    },
    /// An integer within a range — capture delay, JPEG quality, and the like.
    Slider {
        /// Current value.
        value: i64,
        /// Inclusive lower bound.
        min: i64,
        /// Inclusive upper bound.
        max: i64,
        /// Step size a drag snaps to.
        step: i64,
        /// A short unit suffix shown after the number (`"s"`, `"%"`).
        unit: Option<&'static str>,
    },
    /// A filesystem path, edited by typing or by an app-owned browse dialog.
    ///
    /// This crate never opens a dialog itself — it has no filesystem access by
    /// design — so a click on the browse affordance is reported as
    /// [`crate::settings_view::SettingsAction::BrowsePath`] and the app is
    /// expected to open its own native picker and apply the result back.
    Path {
        /// Current value.
        value: String,
        /// Shown when `value` is empty.
        placeholder: &'static str,
        /// Label on the browse affordance, e.g. `"Choose Folder…"`.
        browse_label: &'static str,
    },
    /// A recordable keyboard shortcut. See [`ShortcutStatus`].
    Shortcut {
        /// The current chord, or `None` if unset.
        chord: Option<ShortcutChord>,
        /// The row's live state.
        status: ShortcutStatus,
    },
    /// A filename template: a text field whose content is validated by
    /// [`validate_template`] on every change.
    Template {
        /// Current value.
        value: String,
        /// The most recent validation of `value`.
        validation: Validation,
    },
}

impl RowKind {
    /// A stable slug for this kind, for logging and specimen renders.
    #[must_use]
    pub const fn slug(&self) -> &'static str {
        match self {
            Self::Section => "section",
            Self::Toggle { .. } => "toggle",
            Self::TextField { .. } => "text-field",
            Self::Dropdown { .. } => "dropdown",
            Self::Slider { .. } => "slider",
            Self::Path { .. } => "path",
            Self::Shortcut { .. } => "shortcut",
            Self::Template { .. } => "template",
        }
    }
}

/// One row of the settings form.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Stable identity. See [`RowId`].
    pub id: RowId,
    /// The label shown to the left of the control (or, for a section, the
    /// section's own title).
    pub label: &'static str,
    /// An optional line of explanatory copy shown under the label, in a
    /// quieter weight.
    pub help: Option<&'static str>,
    /// The control and its current value.
    pub kind: RowKind,
    /// Whether the row accepts input. A disabled row is still drawn — greyed,
    /// never hidden — so a setting that is temporarily unavailable does not
    /// look like it was removed.
    pub enabled: bool,
    /// Why a visible control cannot currently be changed.
    pub disabled_reason: Option<&'static str>,
    /// Whether the app can remove a persisted override for this row.
    pub resettable: bool,
}

impl Row {
    /// A section header.
    #[must_use]
    pub const fn section(id: RowId, label: &'static str) -> Self {
        Self {
            id,
            label,
            help: None,
            kind: RowKind::Section,
            enabled: true,
            disabled_reason: None,
            resettable: false,
        }
    }

    /// A section header with a subtitle.
    #[must_use]
    pub const fn section_with_help(id: RowId, label: &'static str, help: &'static str) -> Self {
        Self {
            id,
            label,
            help: Some(help),
            kind: RowKind::Section,
            enabled: true,
            disabled_reason: None,
            resettable: false,
        }
    }

    /// A toggle row.
    #[must_use]
    pub const fn toggle(
        id: RowId,
        label: &'static str,
        help: Option<&'static str>,
        value: bool,
    ) -> Self {
        Self {
            id,
            label,
            help,
            kind: RowKind::Toggle { value },
            enabled: true,
            disabled_reason: None,
            resettable: false,
        }
    }

    /// A free-text row.
    #[must_use]
    pub fn text(
        id: RowId,
        label: &'static str,
        help: Option<&'static str>,
        value: impl Into<String>,
        placeholder: &'static str,
    ) -> Self {
        Self {
            id,
            label,
            help,
            kind: RowKind::TextField {
                value: value.into(),
                placeholder,
            },
            enabled: true,
            disabled_reason: None,
            resettable: false,
        }
    }

    /// A dropdown row.
    ///
    /// # Panics
    ///
    /// Never at runtime from this constructor, but a `selected` index outside
    /// `options` is a bug in the caller; [`SettingsForm::apply`] refuses to set
    /// one out of range, so a form built this way stays valid by construction
    /// as long as the caller passes a sane starting index.
    #[must_use]
    pub fn dropdown(
        id: RowId,
        label: &'static str,
        help: Option<&'static str>,
        options: Vec<&'static str>,
        selected: usize,
    ) -> Self {
        Self {
            id,
            label,
            help,
            kind: RowKind::Dropdown {
                selected: selected.min(options.len().saturating_sub(1)),
                options,
            },
            enabled: true,
            disabled_reason: None,
            resettable: false,
        }
    }

    /// A slider row.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn slider(
        id: RowId,
        label: &'static str,
        help: Option<&'static str>,
        value: i64,
        min: i64,
        max: i64,
        step: i64,
        unit: Option<&'static str>,
    ) -> Self {
        Self {
            id,
            label,
            help,
            kind: RowKind::Slider {
                value,
                min,
                max,
                step,
                unit,
            },
            enabled: true,
            disabled_reason: None,
            resettable: false,
        }
    }

    /// A path row.
    #[must_use]
    pub fn path(
        id: RowId,
        label: &'static str,
        help: Option<&'static str>,
        value: impl Into<String>,
        placeholder: &'static str,
        browse_label: &'static str,
    ) -> Self {
        Self {
            id,
            label,
            help,
            kind: RowKind::Path {
                value: value.into(),
                placeholder,
                browse_label,
            },
            enabled: true,
            disabled_reason: None,
            resettable: false,
        }
    }

    /// A shortcut row.
    #[must_use]
    pub const fn shortcut(
        id: RowId,
        label: &'static str,
        help: Option<&'static str>,
        chord: Option<ShortcutChord>,
    ) -> Self {
        Self {
            id,
            label,
            help,
            kind: RowKind::Shortcut {
                chord,
                status: ShortcutStatus::Idle,
            },
            enabled: true,
            disabled_reason: None,
            resettable: false,
        }
    }

    /// A filename-template row, validated on construction.
    #[must_use]
    pub fn template(
        id: RowId,
        label: &'static str,
        help: Option<&'static str>,
        value: impl Into<String>,
    ) -> Self {
        let value = value.into();
        let validation = validate_template(&value);
        Self {
            id,
            label,
            help,
            kind: RowKind::Template { value, validation },
            enabled: true,
            disabled_reason: None,
            resettable: false,
        }
    }

    /// This row with `enabled` overridden.
    #[must_use]
    pub const fn enabled(mut self, yes: bool) -> Self {
        self.enabled = yes;
        if yes {
            self.disabled_reason = None;
        }
        self
    }

    /// This row disabled with a visible explanation.
    #[must_use]
    pub const fn disabled(mut self, reason: &'static str) -> Self {
        self.enabled = false;
        self.disabled_reason = Some(reason);
        self
    }

    /// This row with a persisted-override reset affordance.
    #[must_use]
    pub const fn resettable(mut self, yes: bool) -> Self {
        self.resettable = yes;
        self
    }
}

// ---------------------------------------------------------------------------
// Row-level changes
// ---------------------------------------------------------------------------

/// A change proposed to one row, distinguished from the row's *current* value
/// so [`SettingsForm::apply`] can validate before committing it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RowChange {
    /// A new toggle value.
    Toggle(bool),
    /// A new text value.
    Text(String),
    /// A new dropdown selection, by index.
    Dropdown(usize),
    /// A new slider value. Clamped to the row's range and snapped to its step.
    Slider(i64),
    /// A new path value.
    Path(String),
    /// A new filename template.
    Template(String),
    /// A chord finished recording.
    ShortcutRecorded(ShortcutChord),
    /// The shortcut was cleared.
    ShortcutCleared,
}

/// The result of one [`SettingsForm::apply`] call.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplyOutcome {
    /// Which row this outcome is about.
    pub row_id: RowId,
    /// Whether the change was accepted onto the row.
    ///
    /// A rejected change is a no-op: the row's value is unchanged. This is
    /// deliberately different from an *invalid* value, which the row accepts
    /// and displays with its error — a template row keeps whatever the user
    /// typed even while it is invalid, because clearing it out from under them
    /// mid-sentence is worse than showing a red message.
    pub accepted: bool,
    /// A message to show, when the change was rejected or is invalid.
    pub message: Option<String>,
}

impl ApplyOutcome {
    const fn applied(row_id: RowId) -> Self {
        Self {
            row_id,
            accepted: true,
            message: None,
        }
    }

    fn invalid(row_id: RowId, message: String) -> Self {
        Self {
            row_id,
            accepted: true,
            message: Some(message),
        }
    }

    fn rejected(row_id: RowId, message: &str) -> Self {
        Self {
            row_id,
            accepted: false,
            message: Some(message.to_owned()),
        }
    }
}

/// Rounds `raw` to the nearest multiple of `step` within `[min, max]`.
#[must_use]
fn clamp_to_step(raw: i64, min: i64, max: i64, step: i64) -> i64 {
    let step = step.max(1);
    let clamped = raw.clamp(min, max);
    let snapped = min + ((clamped - min) as f64 / step as f64).round() as i64 * step;
    snapped.clamp(min, max)
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

/// The settings form: every row, plus the baseline it was loaded from.
///
/// A plain owned value, safe to clone, compare and hold across frames. The app
/// constructs one at load time from its own persisted settings and this crate
/// never reads or writes storage itself.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsForm {
    rows: Vec<Row>,
    baseline: Vec<Row>,
    external_errors: BTreeMap<RowId, String>,
    notice: Option<String>,
}

impl SettingsForm {
    /// A form over `rows`, with `rows` itself as the reset baseline.
    #[must_use]
    pub fn new(rows: Vec<Row>) -> Self {
        Self {
            baseline: rows.clone(),
            rows,
            external_errors: BTreeMap::new(),
            notice: None,
        }
    }

    /// Every row, in display order.
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// One row by id.
    #[must_use]
    pub fn row(&self, id: RowId) -> Option<&Row> {
        self.rows.iter().find(|r| r.id == id)
    }

    fn row_mut(&mut self, id: RowId) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.id == id)
    }

    /// Whether any row differs from the baseline it was loaded or last saved
    /// with.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.rows != self.baseline
    }

    /// The ids of every row that differs from the baseline.
    #[must_use]
    pub fn dirty_rows(&self) -> Vec<RowId> {
        self.rows
            .iter()
            .zip(self.baseline.iter())
            .filter(|(a, b)| a != b)
            .map(|(a, _)| a.id)
            .collect()
    }

    /// Every row currently presenting a blocking error, with its message.
    ///
    /// A template row with invalid syntax and a shortcut row with a conflict or
    /// an invalid chord both surface here — this is what a save button checks
    /// before doing anything irreversible.
    #[must_use]
    pub fn errors(&self) -> Vec<(RowId, String)> {
        let mut errors: Vec<_> = self
            .rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::Template {
                    validation: Validation::Invalid(reason),
                    ..
                } => Some((r.id, reason.clone())),
                RowKind::Shortcut { status, .. } if status.blocks_save() => {
                    let message = match status {
                        ShortcutStatus::Conflict { with } => {
                            format!("Already used by {with}.")
                        }
                        ShortcutStatus::Invalid { reason } => reason.clone(),
                        ShortcutStatus::Idle | ShortcutStatus::Recording => {
                            unreachable!("blocks_save() only returns true for Conflict and Invalid")
                        }
                    };
                    Some((r.id, message))
                }
                _ => None,
            })
            .collect();
        for (id, message) in &self.external_errors {
            if !errors.iter().any(|(existing, _)| existing == id) {
                errors.push((*id, message.clone()));
            }
        }
        errors
    }

    /// Whether [`Self::errors`] is non-empty.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors().is_empty()
    }

    /// Sets or clears an app-owned validation error for a row.
    ///
    /// The app uses this for checks that deliberately live outside this crate,
    /// such as native shortcut conflicts and schema-backed path validation.
    pub fn set_external_error(&mut self, id: RowId, message: Option<String>) {
        if let Some(message) = message {
            self.external_errors.insert(id, message);
        } else {
            self.external_errors.remove(id);
        }
    }

    /// A non-blocking message shown in the footer, such as a failed disk write.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Replaces the non-blocking footer message.
    pub fn set_notice(&mut self, message: Option<String>) {
        self.notice = message;
    }

    /// Applies a proposed change to one row.
    ///
    /// Validates as it goes: a slider value is clamped and snapped to its step,
    /// a dropdown index outside the option list is rejected outright, and a
    /// template is re-validated on every keystroke so the error under it is
    /// never stale. Returns what happened rather than panicking, because a
    /// stray or late UI event naming an id that no longer exists (a row removed
    /// between frames, say) is an ordinary occurrence in immediate-mode UI, not
    /// a bug.
    pub fn apply(&mut self, id: RowId, change: RowChange) -> ApplyOutcome {
        let Some(row) = self.row_mut(id) else {
            return ApplyOutcome::rejected(id, "no such row");
        };
        if !row.enabled {
            return ApplyOutcome::rejected(id, "row is disabled");
        }
        match (&mut row.kind, change) {
            (RowKind::Toggle { value }, RowChange::Toggle(v)) => {
                *value = v;
                ApplyOutcome::applied(id)
            }
            (RowKind::TextField { value, .. }, RowChange::Text(v)) => {
                *value = v;
                ApplyOutcome::applied(id)
            }
            (RowKind::Dropdown { selected, options }, RowChange::Dropdown(i)) => {
                if i >= options.len() {
                    ApplyOutcome::rejected(id, "option index out of range")
                } else {
                    *selected = i;
                    ApplyOutcome::applied(id)
                }
            }
            (
                RowKind::Slider {
                    value,
                    min,
                    max,
                    step,
                    ..
                },
                RowChange::Slider(v),
            ) => {
                *value = clamp_to_step(v, *min, *max, *step);
                ApplyOutcome::applied(id)
            }
            (RowKind::Path { value, .. }, RowChange::Path(v)) => {
                *value = v;
                ApplyOutcome::applied(id)
            }
            (RowKind::Template { value, validation }, RowChange::Template(v)) => {
                *validation = validate_template(&v);
                *value = v;
                if let Validation::Invalid(reason) = validation {
                    ApplyOutcome::invalid(id, reason.clone())
                } else {
                    ApplyOutcome::applied(id)
                }
            }
            (RowKind::Shortcut { chord, status }, RowChange::ShortcutRecorded(c)) => {
                *chord = Some(c);
                // Conflict detection needs the rest of the app's bindings and
                // the OS's reserved set, neither of which this crate can see;
                // it resets to `Idle` and waits for the app to call
                // `set_shortcut_status` once it has checked.
                *status = ShortcutStatus::Idle;
                ApplyOutcome::applied(id)
            }
            (RowKind::Shortcut { chord, status }, RowChange::ShortcutCleared) => {
                *chord = None;
                *status = ShortcutStatus::Idle;
                ApplyOutcome::applied(id)
            }
            (kind, _) => ApplyOutcome::rejected(id, kind.slug()),
        }
    }

    /// Sets a shortcut row's live status, e.g. after the app has checked a
    /// freshly recorded chord for conflicts.
    ///
    /// Returns `false` and does nothing if `id` does not name a shortcut row.
    pub fn set_shortcut_status(&mut self, id: RowId, status: ShortcutStatus) -> bool {
        let Some(row) = self.row_mut(id) else {
            return false;
        };
        let RowKind::Shortcut {
            status: current, ..
        } = &mut row.kind
        else {
            return false;
        };
        *current = status;
        true
    }

    /// Discards every change, restoring the baseline.
    pub fn reset(&mut self) {
        self.rows = self.baseline.clone();
        self.external_errors.clear();
        self.notice = None;
    }

    /// Moves the baseline to the current rows, e.g. after a successful save.
    pub fn commit(&mut self) {
        self.baseline = self.rows.clone();
        self.external_errors.clear();
        self.notice = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SettingsForm {
        SettingsForm::new(vec![
            Row::section("s.capture", "Capture"),
            Row::toggle("capture.sound", "Play a sound when capturing", None, true),
            Row::dropdown(
                "capture.mode",
                "Default capture mode",
                None,
                vec!["Region", "Window", "Full Screen"],
                0,
            ),
            Row::slider(
                "capture.countdown",
                "Countdown before capture",
                None,
                0,
                0,
                5,
                1,
                Some("s"),
            ),
            Row::path(
                "output.directory",
                "Save captures to",
                None,
                "~/Pictures/Scrozz",
                "No folder chosen",
                "Choose Folder…",
            ),
            Row::template(
                "output.filename_template",
                "Filename template",
                None,
                "{app} {date} {time}",
            ),
            Row::shortcut(
                "shortcuts.capture_region",
                "Capture region",
                None,
                Some(ShortcutChord::with_mods(vec![Mod::Shift, Mod::Cmd], "4")),
            ),
        ])
    }

    #[test]
    fn fresh_form_is_not_dirty() {
        let form = sample();
        assert!(!form.is_dirty());
        assert!(form.dirty_rows().is_empty());
    }

    #[test]
    fn toggling_marks_the_form_dirty() {
        let mut form = sample();
        let outcome = form.apply("capture.sound", RowChange::Toggle(false));
        assert!(outcome.accepted);
        assert!(form.is_dirty());
        assert_eq!(form.dirty_rows(), vec!["capture.sound"]);
        let RowKind::Toggle { value } = form.row("capture.sound").unwrap().kind else {
            panic!("expected a toggle row");
        };
        assert!(!value);
    }

    #[test]
    fn reset_restores_the_baseline_and_clears_dirty() {
        let mut form = sample();
        form.apply("capture.sound", RowChange::Toggle(false));
        assert!(form.is_dirty());
        form.reset();
        assert!(!form.is_dirty());
        let RowKind::Toggle { value } = form.row("capture.sound").unwrap().kind else {
            panic!("expected a toggle row");
        };
        assert!(value);
    }

    #[test]
    fn commit_moves_the_baseline_forward() {
        let mut form = sample();
        form.apply("capture.sound", RowChange::Toggle(false));
        form.commit();
        assert!(!form.is_dirty());
        // A subsequent reset now restores to the *committed* value, not the
        // form's original construction value.
        form.apply("capture.sound", RowChange::Toggle(true));
        form.reset();
        let RowKind::Toggle { value } = form.row("capture.sound").unwrap().kind else {
            panic!("expected a toggle row");
        };
        assert!(!value);
    }

    #[test]
    fn dropdown_rejects_an_out_of_range_index() {
        let mut form = sample();
        let outcome = form.apply("capture.mode", RowChange::Dropdown(99));
        assert!(!outcome.accepted);
        assert!(!form.is_dirty());
    }

    #[test]
    fn slider_clamps_and_snaps_to_step() {
        let mut form = sample();
        form.apply("capture.countdown", RowChange::Slider(37));
        let RowKind::Slider { value, .. } = form.row("capture.countdown").unwrap().kind else {
            panic!("expected a slider row");
        };
        assert_eq!(value, 5, "clamped to the row's max");

        form.apply("capture.countdown", RowChange::Slider(-4));
        let RowKind::Slider { value, .. } = form.row("capture.countdown").unwrap().kind else {
            panic!("expected a slider row");
        };
        assert_eq!(value, 0, "clamped to the row's min");
    }

    #[test]
    fn slider_snaps_to_step() {
        let mut form = SettingsForm::new(vec![Row::slider(
            "quality",
            "Quality",
            None,
            0,
            0,
            100,
            10,
            Some("%"),
        )]);
        form.apply("quality", RowChange::Slider(53));
        let RowKind::Slider { value, .. } = form.row("quality").unwrap().kind else {
            panic!("expected a slider row");
        };
        assert_eq!(value, 50);
    }

    #[test]
    fn template_row_flags_an_unknown_placeholder() {
        let mut form = sample();
        let outcome = form.apply(
            "output.filename_template",
            RowChange::Template("{app}-{oops}".to_owned()),
        );
        assert!(outcome.accepted, "the row keeps the typed text");
        assert!(outcome.message.is_some());
        assert!(form.has_errors());
        let (row_id, _) = &form.errors()[0];
        assert_eq!(*row_id, "output.filename_template");
    }

    #[test]
    fn template_row_accepts_known_tokens() {
        assert_eq!(
            validate_template("{app}-{date}-{time}-{seq}-{width}x{height}"),
            Validation::Valid
        );
    }

    #[test]
    fn template_row_matches_export_brace_escaping() {
        assert!(validate_template("").is_valid());
        assert!(validate_template("{{literal}}-{year}").is_valid());
        assert!(!validate_template("literal}").is_valid());
    }

    #[test]
    fn template_row_rejects_unmatched_brace() {
        assert!(!validate_template("{app").is_valid());
    }

    #[test]
    fn shortcut_conflict_blocks_save_until_resolved() {
        let mut form = sample();
        assert!(!form.has_errors());
        form.set_shortcut_status(
            "shortcuts.capture_region",
            ShortcutStatus::Conflict {
                with: "Start Recording".to_owned(),
            },
        );
        assert!(form.has_errors());
        let (row_id, message) = &form.errors()[0];
        assert_eq!(*row_id, "shortcuts.capture_region");
        assert!(message.contains("Start Recording"));

        form.set_shortcut_status("shortcuts.capture_region", ShortcutStatus::Idle);
        assert!(!form.has_errors());
    }

    #[test]
    fn recording_a_shortcut_replaces_the_chord_and_resets_status() {
        let mut form = sample();
        form.set_shortcut_status(
            "shortcuts.capture_region",
            ShortcutStatus::Conflict {
                with: "Something Else".to_owned(),
            },
        );
        form.apply(
            "shortcuts.capture_region",
            RowChange::ShortcutRecorded(ShortcutChord::bare("5")),
        );
        let RowKind::Shortcut { chord, status } =
            &form.row("shortcuts.capture_region").unwrap().kind
        else {
            panic!("expected a shortcut row");
        };
        assert_eq!(chord.as_ref().unwrap().key, "5");
        assert_eq!(*status, ShortcutStatus::Idle);
    }

    #[test]
    fn clearing_a_shortcut_removes_the_chord() {
        let mut form = sample();
        form.apply("shortcuts.capture_region", RowChange::ShortcutCleared);
        let RowKind::Shortcut { chord, .. } = &form.row("shortcuts.capture_region").unwrap().kind
        else {
            panic!("expected a shortcut row");
        };
        assert!(chord.is_none());
    }

    #[test]
    fn applying_to_an_unknown_row_is_rejected_not_a_panic() {
        let mut form = sample();
        let outcome = form.apply("no.such.row", RowChange::Toggle(true));
        assert!(!outcome.accepted);
    }

    #[test]
    fn applying_to_a_disabled_row_is_rejected() {
        let mut form = SettingsForm::new(vec![Row::toggle("x", "X", None, false).enabled(false)]);
        let outcome = form.apply("x", RowChange::Toggle(true));
        assert!(!outcome.accepted);
    }

    #[test]
    fn mismatched_change_kind_is_rejected() {
        let mut form = sample();
        let outcome = form.apply("capture.sound", RowChange::Text("nope".to_owned()));
        assert!(!outcome.accepted);
    }

    #[test]
    fn shortcut_chord_glyphs_and_spoken_form() {
        let chord = ShortcutChord::with_mods(vec![Mod::Shift, Mod::Cmd], "4");
        // Exact glyphs are platform-dependent (see `Mod::glyph`); what matters
        // here is that both modifiers and the key appear, in order.
        let glyphs = chord.glyphs();
        assert!(glyphs.ends_with('4'));
        let spoken = chord.spoken();
        assert!(spoken.ends_with('4'));
        assert_eq!(spoken.matches(' ').count(), 2);
    }
}
