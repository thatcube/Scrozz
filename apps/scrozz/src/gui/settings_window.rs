//! Native settings and onboarding window driven from eframe's existing loop.
//!
//! The secondary viewport owns no persistence or platform behavior. Its
//! callback renders `scrozz-ui` view models and queues intents; the parent
//! viewport services those intents on the main thread, where native folder
//! pickers, hotkey registration, and launch-at-login belong.

use std::{
    mem,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use eframe::egui::{self, ViewportBuilder, ViewportClass, ViewportId};
use scrozz_core::Error as CoreError;
use scrozz_shell::{Accelerator, Conflict, FolderPicker, FolderPickerRequest, RegistrationStatus};
use scrozz_ui::{
    form::{Row, RowChange, RowId, RowKind, SettingsForm, ShortcutChord, ShortcutStatus},
    onboarding_view::{
        self, OnboardingAction, OnboardingContent, OnboardingOutcome, OnboardingState,
    },
    paint::{Mod, Surface},
    settings_view::{self, SettingsAction},
};

use crate::{
    fault::{CliError, CliResult},
    gui::app::App,
    onboarding,
    settings::{self, Kind, SETTINGS, Section, Setting, ValueSource},
    settings_hotkeys, settings_runtime,
    settings_store::SettingsStore,
    settings_support::{SettingsSupport, Support},
};

const EXTERNAL_CHANGE_ERROR: RowId = "window.external-change";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Settings,
    Onboarding,
}

#[derive(Debug, Clone)]
enum Intent {
    Settings(SettingsAction),
    Onboarding(OnboardingAction),
    Close,
}

struct State {
    form: SettingsForm,
    onboarding: OnboardingState,
    onboarding_content: OnboardingContent,
    page: Page,
    open: bool,
    onboarding_required: bool,
    return_to_settings: bool,
    intents: Vec<Intent>,
    icons: scrozz_ui::icons::IconStore,
}

impl State {
    fn new(
        ctx: &egui::Context,
        store: &SettingsStore,
        support: SettingsSupport,
        drag_out_available: bool,
    ) -> CliResult<Self> {
        let flow = onboarding::Flow::from_store(store)?;
        let onboarding_required = flow.is_visible();
        let mut form = form_from_store(store, support)?;
        validate_form(&mut form);
        Ok(Self {
            form,
            onboarding: OnboardingState::start(),
            onboarding_content: onboarding_content(store, support, drag_out_available)?,
            page: if onboarding_required {
                Page::Onboarding
            } else {
                Page::Settings
            },
            open: onboarding_required,
            onboarding_required,
            return_to_settings: false,
            intents: Vec::new(),
            icons: scrozz_ui::icons::IconStore::new(ctx),
        })
    }
}

/// Coordinates the secondary viewport and its main-thread intents.
pub struct SettingsWindow {
    store: SettingsStore,
    picker: Box<dyn FolderPicker>,
    state: Arc<Mutex<State>>,
    seen_settings_revision: u64,
    support: SettingsSupport,
    drag_out_available: bool,
}

impl SettingsWindow {
    /// Loads the settings view model and native folder picker.
    pub fn new(ctx: &egui::Context, app: &App, drag_out_available: bool) -> CliResult<Self> {
        let mut store = SettingsStore::load()?;
        let autostart = settings_runtime::reconcile_autostart(&mut store);
        let picker = Box::new(scrozz_shell::picker::native_folder_picker()?);
        let support = SettingsSupport::from_actions(|action| app.action_available(action));
        let mut initial = State::new(ctx, &store, support, drag_out_available)?;
        match autostart {
            Ok(RegistrationStatus::Drifted) => initial.form.set_notice(Some(
                "Launch-at-login registration changed outside Scrozz. Toggle and save it to repair the registration."
                    .to_owned(),
            )),
            Ok(_) => {}
            Err(error) => initial.form.set_notice(Some(format!(
                "Couldn't inspect launch-at-login registration: {error}"
            ))),
        }
        let state = Arc::new(Mutex::new(initial));
        Ok(Self {
            store,
            picker,
            state,
            seen_settings_revision: app.settings_revision(),
            support,
            drag_out_available,
        })
    }

    /// Services requests and keeps the deferred viewport alive while open.
    pub fn update(&mut self, ctx: &egui::Context, app: &mut App) {
        if app.take_settings_request() {
            self.open_settings();
        }
        let changed = self.sync_external_changes(app.settings_revision());

        let open = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open;
        if !open {
            return;
        }

        let state = Arc::clone(&self.state);
        ctx.show_viewport_deferred(
            settings_viewport_id(),
            ViewportBuilder::default()
                .with_title("Scrozz Settings")
                .with_inner_size([760.0, 700.0])
                .with_min_inner_size([640.0, 560.0])
                .with_resizable(true)
                .with_decorations(true),
            move |ctx, class| render_viewport(ctx, class, &state),
        );

        if changed || self.drain_intents(app) {
            ctx.request_repaint_of(settings_viewport_id());
        }
    }

    fn open_settings(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.onboarding_required {
            match onboarding::mark_complete(&mut self.store) {
                Ok(()) => state.onboarding_required = false,
                Err(error) => {
                    state.onboarding_content.error =
                        Some(format!("Couldn't skip onboarding: {error}"));
                    return;
                }
            }
        }
        state.page = Page::Settings;
        state.open = true;
        state.return_to_settings = false;
    }

    fn sync_external_changes(&mut self, revision: u64) -> bool {
        if revision == self.seen_settings_revision {
            return false;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.form.is_dirty() {
            let already_reported = state
                .form
                .errors()
                .iter()
                .any(|(id, _)| *id == EXTERNAL_CHANGE_ERROR);
            state.form.set_external_error(
                EXTERNAL_CHANGE_ERROR,
                Some("Settings changed elsewhere. Reset this form to reload them.".to_owned()),
            );
            return !already_reported;
        }
        match SettingsStore::load().and_then(|store| {
            let form = form_from_store(&store, self.support)?;
            let content = onboarding_content(&store, self.support, self.drag_out_available)?;
            Ok((store, form, content))
        }) {
            Ok((store, mut form, content)) => {
                validate_form(&mut form);
                self.store = store;
                state.form = form;
                state.onboarding_content = content;
                self.seen_settings_revision = revision;
                true
            }
            Err(error) => {
                state
                    .form
                    .set_notice(Some(format!("Couldn't reload settings: {error}")));
                self.seen_settings_revision = revision;
                true
            }
        }
    }

    fn drain_intents(&mut self, app: &mut App) -> bool {
        let intents = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            mem::take(&mut state.intents)
        };
        let handled = !intents.is_empty();
        for intent in intents {
            match intent {
                Intent::Settings(action) => self.apply_settings_action(action, app),
                Intent::Onboarding(action) => self.apply_onboarding_action(action),
                Intent::Close => self.close(),
            }
        }
        handled
    }

    fn apply_settings_action(&mut self, action: SettingsAction, app: &mut App) {
        match action {
            SettingsAction::RowChanged { row_id, change } => {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.form.set_notice(None);
                state.form.set_external_error(row_id, None);
                state.form.apply(row_id, change);
                validate_form(&mut state.form);
            }
            SettingsAction::BrowsePath { row_id } => self.browse(row_id),
            SettingsAction::StartRecordingShortcut { row_id } => {
                self.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .form
                    .set_shortcut_status(row_id, ShortcutStatus::Recording);
            }
            SettingsAction::StopRecordingShortcut { row_id } => {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.form.set_shortcut_status(row_id, ShortcutStatus::Idle);
                validate_form(&mut state.form);
            }
            SettingsAction::Save => self.save(app),
            SettingsAction::Reset => self.reset_form(app.settings_revision()),
            SettingsAction::ResetRow { row_id } => self.reset_persisted(Some(row_id), app),
            SettingsAction::ResetAll => self.reset_persisted(None, app),
            SettingsAction::RerunOnboarding => {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.onboarding = OnboardingState::start();
                state.onboarding_content.error = None;
                state.page = Page::Onboarding;
                state.return_to_settings = true;
            }
            _ => {}
        }
    }

    fn browse(&mut self, row_id: RowId) {
        let starting_directory = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.form.row(row_id).and_then(|row| match &row.kind {
                RowKind::Path { value, .. } => picker_starting_directory(value),
                _ => None,
            })
        };
        let request = FolderPickerRequest {
            title: Some("Choose where Scrozz saves captures".to_owned()),
            prompt: Some("Choose a capture folder".to_owned()),
            starting_directory,
        };
        match self.picker.pick_folder(&request) {
            Ok(path) => {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.form.set_notice(None);
                state
                    .form
                    .apply(row_id, RowChange::Path(path.to_string_lossy().into_owned()));
                validate_form(&mut state.form);
            }
            Err(CoreError::Cancelled) => {}
            Err(error) => self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .form
                .set_notice(Some(format!("Couldn't open the folder picker: {error}"))),
        }
    }

    fn save(&mut self, app: &mut App) {
        let edits = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            validate_form(&mut state.form);
            if state.form.has_errors() {
                return;
            }
            edits_from_form(&state.form)
        };
        if edits.is_empty() {
            return;
        }

        match settings_runtime::apply(&mut self.store, &edits) {
            Ok(()) => {
                let reload = app.reload_settings();
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.form.commit();
                match onboarding_content(&self.store, self.support, self.drag_out_available) {
                    Ok(content) => state.onboarding_content = content,
                    Err(error) => state.form.set_notice(Some(format!(
                        "Settings saved, but onboarding couldn't reload: {error}"
                    ))),
                }
                if let Err(error) = reload {
                    state.form.set_notice(Some(format!(
                        "Settings saved, but the running app couldn't reload them: {error}"
                    )));
                }
                self.seen_settings_revision = app.settings_revision();
            }
            Err(error) => self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .form
                .set_notice(Some(format!("Couldn't save settings: {error}"))),
        }
    }

    fn reset_form(&mut self, revision: u64) {
        if revision != self.seen_settings_revision {
            self.sync_external_changes(revision);
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.form.is_dirty() {
                state.form.reset();
                state.form.set_external_error(EXTERNAL_CHANGE_ERROR, None);
                drop(state);
                self.sync_external_changes(revision);
            }
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.form.reset();
        validate_form(&mut state.form);
    }

    fn reset_persisted(&mut self, row_id: Option<RowId>, app: &mut App) {
        if self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .form
            .is_dirty()
        {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .form
                .set_notice(Some(
                    "Save or discard unsaved changes before resetting persisted settings."
                        .to_owned(),
                ));
            return;
        }

        let result = match row_id {
            Some(key) => settings_runtime::reset(&mut self.store, key).map(|_| ()),
            None => settings_runtime::reset_all(&mut self.store).map(|_| ()),
        };
        if let Err(error) = result {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .form
                .set_notice(Some(format!("Couldn't reset settings: {error}")));
            return;
        }

        let reload = app.reload_settings();
        match form_from_store(&self.store, self.support) {
            Ok(mut form) => {
                validate_form(&mut form);
                if let Err(error) = reload {
                    form.set_notice(Some(format!(
                        "Settings reset, but the running app couldn't reload them: {error}"
                    )));
                }
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.form = form;
                if let Ok(content) =
                    onboarding_content(&self.store, self.support, self.drag_out_available)
                {
                    state.onboarding_content = content;
                }
                self.seen_settings_revision = app.settings_revision();
            }
            Err(error) => self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .form
                .set_notice(Some(format!(
                    "Settings reset, but the form couldn't reload: {error}"
                ))),
        }
    }

    fn apply_onboarding_action(&mut self, action: OnboardingAction) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.onboarding_content.error = None;
        match state.onboarding.apply(action) {
            OnboardingOutcome::Continue(next) => state.onboarding = next,
            OnboardingOutcome::Dismissed { .. } => match onboarding::mark_complete(&mut self.store)
            {
                Ok(()) => {
                    state.onboarding_required = false;
                    if state.return_to_settings {
                        state.page = Page::Settings;
                        state.return_to_settings = false;
                    } else {
                        state.open = false;
                    }
                }
                Err(error) => {
                    state.onboarding_content.error =
                        Some(format!("Couldn't save onboarding progress: {error}"));
                }
            },
            _ => {}
        }
    }

    fn close(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.page == Page::Onboarding && state.onboarding_required {
            if let Err(error) = onboarding::mark_complete(&mut self.store) {
                state.onboarding_content.error = Some(format!("Couldn't skip onboarding: {error}"));
                return;
            }
            state.onboarding_required = false;
        }
        state.open = false;
    }
}

fn expand_picker_path(value: &str) -> PathBuf {
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
        .and_then(|relative| dirs::home_dir().map(|home| home.join(relative)))
        .unwrap_or_else(|| PathBuf::from(value))
}

fn picker_starting_directory(value: &str) -> Option<PathBuf> {
    let mut candidate = expand_picker_path(value);
    loop {
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !candidate.pop() {
            return None;
        }
    }
}

fn render_viewport(ui: &mut egui::Ui, _class: ViewportClass, shared: &Arc<Mutex<State>>) {
    let mut state = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ctx = ui.ctx().clone();
    if ctx.input(|input| input.viewport().close_requested()) {
        state.intents.push(Intent::Close);
        ctx.request_repaint_of(ViewportId::ROOT);
        return;
    }

    let appearance = match ctx.theme() {
        egui::Theme::Dark => scrozz_ui::theme::Appearance::Dark,
        egui::Theme::Light => scrozz_ui::theme::Appearance::Light,
    };
    let theme = scrozz_ui::theme::Theme::for_appearance(appearance);
    let surface = Surface::still(&theme, &state.icons, scrozz_ui::motion::Motion::at_ms(0));
    let page = state.page;
    let response = match page {
        Page::Settings => Rendered::Settings(settings_view::render(ui, &surface, &state.form)),
        Page::Onboarding => Rendered::Onboarding(onboarding_view::render(
            ui,
            &surface,
            state.onboarding,
            &state.onboarding_content,
        )),
    };
    match response {
        Rendered::Settings(response) => state
            .intents
            .extend(response.actions.into_iter().map(Intent::Settings)),
        Rendered::Onboarding(response) => {
            if let Some(action) = response.action {
                state.intents.push(Intent::Onboarding(action));
            }
        }
    }
    if !state.intents.is_empty() {
        ctx.request_repaint_of(ViewportId::ROOT);
    }
}

fn settings_viewport_id() -> ViewportId {
    ViewportId::from_hash_of("scrozz.settings")
}

enum Rendered {
    Settings(settings_view::SettingsResponse),
    Onboarding(onboarding_view::OnboardingResponse),
}

fn form_from_store(store: &SettingsStore, support: SettingsSupport) -> CliResult<SettingsForm> {
    let mut rows = Vec::with_capacity(SETTINGS.len() + 10);
    let mut section = None;
    for setting in SETTINGS
        .iter()
        .filter(|setting| setting.section != Section::Onboarding)
    {
        if section != Some(setting.section) {
            section = Some(setting.section);
            rows.push(Row::section(
                section_id(setting.section),
                setting.section.title(),
            ));
        }
        let (value, source) = store.resolve(setting);
        let mut row = row_from_setting(setting, value)?.resettable(source == ValueSource::User);
        if let Support::Disabled(reason) = support.setting(setting.key) {
            row = row.disabled(reason);
        }
        rows.push(row);
    }
    Ok(SettingsForm::new(rows))
}

fn row_from_setting(setting: &Setting, value: &str) -> CliResult<Row> {
    setting.validate(value)?;
    let help = Some(setting.description);
    Ok(match setting.kind {
        Kind::Bool => Row::toggle(setting.key, setting.label, help, value == "true"),
        Kind::Int { min, max } => Row::slider(
            setting.key,
            setting.label,
            help,
            value.parse::<i64>().map_err(|error| {
                CliError::usage(format!(
                    "validated setting {} is unreadable: {error}",
                    setting.key
                ))
            })?,
            min,
            max,
            1,
            None,
        ),
        Kind::Path => Row::path(
            setting.key,
            setting.label,
            help,
            value,
            setting.default,
            "Choose folder...",
        ),
        Kind::Text => Row::text(setting.key, setting.label, help, value, ""),
        Kind::Template => Row::template(setting.key, setting.label, help, value),
        Kind::Choice(options) => Row::dropdown(
            setting.key,
            setting.label,
            help,
            options.to_vec(),
            options
                .iter()
                .position(|option| *option == value)
                .unwrap_or(0),
        ),
        Kind::Accelerator => Row::shortcut(setting.key, setting.label, help, shortcut_chord(value)),
    })
}

const fn section_id(section: Section) -> RowId {
    match section {
        Section::Capture => "section.capture",
        Section::Clipboard => "section.clipboard",
        Section::Recording => "section.recording",
        Section::Shortcuts => "section.shortcuts",
        Section::History => "section.history",
        Section::Ocr => "section.ocr",
        Section::QuickAccess => "section.quick-access",
        Section::Annotation => "section.annotation",
        Section::System => "section.system",
        Section::Updates => "section.updates",
        Section::Onboarding => "section.onboarding",
    }
}

fn row_value(row: &Row) -> Option<String> {
    match &row.kind {
        RowKind::Section => None,
        RowKind::Toggle { value } => Some(value.to_string()),
        RowKind::TextField { value, .. }
        | RowKind::Path { value, .. }
        | RowKind::Template { value, .. } => Some(value.clone()),
        RowKind::Dropdown { selected, options } => {
            options.get(*selected).map(|value| (*value).to_owned())
        }
        RowKind::Slider { value, .. } => Some(value.to_string()),
        RowKind::Shortcut { chord, .. } => {
            Some(chord.as_ref().map_or_else(String::new, chord_value))
        }
        _ => None,
    }
}

fn edits_from_form(form: &SettingsForm) -> Vec<(String, String)> {
    form.dirty_rows()
        .into_iter()
        .filter_map(|id| {
            form.row(id)
                .and_then(row_value)
                .map(|value| (id.to_owned(), value))
        })
        .collect()
}

fn validate_form(form: &mut SettingsForm) {
    form.set_external_error("window.shortcut-validation", None);
    form.set_external_error("window.ocr-validation", None);
    let mut shortcuts = Vec::new();
    for setting in SETTINGS
        .iter()
        .filter(|setting| setting.section != Section::Onboarding)
    {
        let Some((enabled, value)) = form
            .row(setting.key)
            .map(|row| (row.enabled, row_value(row)))
        else {
            continue;
        };
        if !enabled {
            form.set_external_error(setting.key, None);
            continue;
        }
        let Some(value) = value else {
            continue;
        };
        match setting.kind {
            Kind::Accelerator => match setting.validate(&value) {
                Ok(()) => {
                    form.set_shortcut_status(setting.key, ShortcutStatus::Idle);
                    shortcuts.push((setting.key.to_owned(), value));
                }
                Err(error) => {
                    form.set_shortcut_status(
                        setting.key,
                        ShortcutStatus::Invalid {
                            reason: error.to_string(),
                        },
                    );
                }
            },
            _ => form.set_external_error(
                setting.key,
                setting
                    .validate(&value)
                    .err()
                    .map(|error| error.to_string()),
            ),
        }
    }

    let ocr_values = (
        form.row(scrozz_ocr::LANGUAGES_KEY).and_then(row_value),
        form.row(scrozz_ocr::AUTO_DETECT_LANGUAGE_KEY)
            .and_then(row_value),
        form.row(scrozz_ocr::KEEP_LINE_BREAKS_KEY)
            .and_then(row_value),
        form.row(scrozz_ocr::DETECT_LINKS_KEY).and_then(row_value),
    );
    if let (Some(languages), Some(automatic), Some(line_breaks), Some(links)) = ocr_values
        && let Err(error) = scrozz_ocr::RuntimeConfig::from_settings(
            &languages,
            automatic == "true",
            line_breaks == "true",
            links == "true",
        )
    {
        form.set_external_error("window.ocr-validation", Some(error.to_string()));
    }

    match settings_hotkeys::check_all(&shortcuts) {
        Ok(conflicts) => {
            for (key, conflict) in conflicts {
                let status = match conflict {
                    Conflict::AlreadyBound { action } => ShortcutStatus::Conflict {
                        with: settings::lookup(&action)
                            .map_or(action, |setting| setting.label.to_owned()),
                    },
                    other => ShortcutStatus::Invalid {
                        reason: other.to_string(),
                    },
                };
                if let Ok(setting) = settings::lookup(&key) {
                    form.set_shortcut_status(setting.key, status);
                }
            }
        }
        Err(error) => form.set_external_error(
            "window.shortcut-validation",
            Some(format!("Shortcut validation failed: {error}")),
        ),
    }
}

fn shortcut_chord(value: &str) -> Option<ShortcutChord> {
    let accelerator = Accelerator::parse(value).ok()?;
    let mut mods = Vec::with_capacity(4);
    if accelerator.has_control() {
        mods.push(Mod::Ctrl);
    }
    if accelerator.has_alt() {
        mods.push(Mod::Opt);
    }
    if accelerator.has_shift() {
        mods.push(Mod::Shift);
    }
    if accelerator.has_super() {
        mods.push(Mod::Cmd);
    }
    Some(ShortcutChord::with_mods(mods, accelerator.key_name()))
}

fn chord_value(chord: &ShortcutChord) -> String {
    let mut parts = Vec::with_capacity(chord.mods.len() + 1);
    for (modifier, name) in [
        (
            Mod::Cmd,
            if cfg!(target_os = "macos") {
                "Cmd"
            } else {
                "Win"
            },
        ),
        (Mod::Ctrl, "Ctrl"),
        (Mod::Opt, "Alt"),
        (Mod::Shift, "Shift"),
    ] {
        if chord.mods.contains(&modifier) {
            parts.push(name);
        }
    }
    parts.push(&chord.key);
    parts.join("+")
}

fn onboarding_content(
    store: &SettingsStore,
    support: SettingsSupport,
    drag_out_available: bool,
) -> CliResult<OnboardingContent> {
    let shortcut = store.get("hotkey.capture-region")?.1;
    let capture_hotkey_available =
        matches!(support.setting("hotkey.capture-region"), Support::Enabled);
    Ok(OnboardingContent {
        capture_shortcut: shortcut_chord(shortcut)
            .map_or_else(|| shortcut.to_owned(), |chord| chord.glyphs()),
        capture_folder: store.get("capture.folder")?.1.to_owned(),
        drag_out_available,
        capture_hotkey_available,
        compositor_config: if capture_hotkey_available {
            onboarding::compositor_config(store)?
        } else {
            None
        },
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    fn store(name: &str) -> (PathBuf, SettingsStore) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "scrozz-settings-window-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let store = SettingsStore::open(directory.join("settings.json")).unwrap();
        (directory, store)
    }

    fn all_supported() -> SettingsSupport {
        SettingsSupport::from_actions(|_| true)
    }

    #[test]
    fn production_schema_round_trips_through_the_ui_form() {
        let (directory, store) = store("round-trip");
        let form = form_from_store(&store, all_supported()).unwrap();
        for setting in SETTINGS
            .iter()
            .filter(|setting| setting.section != Section::Onboarding)
        {
            let form_value = form.row(setting.key).and_then(row_value).unwrap();
            let stored_value = store.get(setting.key).unwrap().1;
            if setting.kind == Kind::Accelerator {
                assert_eq!(
                    Accelerator::parse(&form_value).unwrap(),
                    Accelerator::parse(stored_value).unwrap(),
                    "{}",
                    setting.key
                );
            } else {
                assert_eq!(form_value, stored_value, "{}", setting.key);
            }
        }
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn settings_without_runtime_consumers_are_truthfully_disabled() {
        let (directory, store) = store("support");
        let form = form_from_store(&store, all_supported()).unwrap();

        assert!(form.row("capture.folder").unwrap().enabled);
        let recording = form.row("record.fps").unwrap();
        assert!(!recording.enabled);
        assert!(
            recording
                .disabled_reason
                .is_some_and(|reason| reason.contains("recording runtime"))
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn persisted_overrides_get_a_reset_affordance_even_when_disabled() {
        let (directory, mut store) = store("resettable");
        store.set("record.fps", "60").unwrap();

        let form = form_from_store(&store, all_supported()).unwrap();

        let row = form.row("record.fps").unwrap();
        assert!(!row.enabled);
        assert!(row.resettable);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn unsaved_shortcut_duplicates_block_both_rows() {
        let (directory, store) = store("shortcut-conflict");
        let mut form = form_from_store(&store, all_supported()).unwrap();
        let chord = shortcut_chord("Cmd+Alt+P").unwrap();
        form.apply(
            "hotkey.capture-region",
            RowChange::ShortcutRecorded(chord.clone()),
        );
        form.apply("hotkey.capture-display", RowChange::ShortcutRecorded(chord));
        validate_form(&mut form);
        assert!(matches!(
            form.row("hotkey.capture-region").unwrap().kind,
            RowKind::Shortcut {
                status: ShortcutStatus::Conflict { .. },
                ..
            }
        ));
        assert!(matches!(
            form.row("hotkey.capture-display").unwrap().kind,
            RowKind::Shortcut {
                status: ShortcutStatus::Conflict { .. },
                ..
            }
        ));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn contradictory_ocr_edits_are_blocked_before_persistence() {
        let (directory, store) = store("ocr-cross-validation");
        let mut form = form_from_store(&store, all_supported()).unwrap();
        form.apply("ocr.languages", RowChange::Text("en-US".to_owned()));
        form.apply("ocr.auto-detect-language", RowChange::Toggle(true));

        validate_form(&mut form);

        assert!(form.has_errors());
        assert!(
            form.errors()
                .iter()
                .any(|(_, message)| message.contains("ocr.auto-detect-language"))
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn every_valid_modifier_alias_round_trips_through_the_ui_chord() {
        for value in [
            "CmdOrCtrl+Shift+4",
            "CmdOrControl+Shift+4",
            "CommandOrCtrl+Shift+4",
            "CommandOrControl+Shift+4",
            "Hyper+Shift+4",
            "Win+Shift+4",
            "Logo+Shift+4",
        ] {
            let accelerator = Accelerator::parse(value).unwrap();
            let chord = shortcut_chord(value).expect("the schema accepted this accelerator");
            assert_eq!(
                Accelerator::parse(&chord_value(&chord)).unwrap(),
                accelerator,
                "{value}"
            );
        }
    }

    #[test]
    fn picker_paths_expand_the_home_shorthand() {
        assert_eq!(
            expand_picker_path("/tmp/scrozz"),
            PathBuf::from("/tmp/scrozz")
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_picker_path("~"), home);
            assert_eq!(
                expand_picker_path("~/Pictures/Scrozz"),
                home.join("Pictures/Scrozz")
            );
        }

        let root = std::env::temp_dir().join(format!("scrozz-picker-start-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        assert_eq!(
            picker_starting_directory(&root.join("missing/child").to_string_lossy()),
            Some(root.clone())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn onboarding_uses_the_real_folder_and_shortcut() {
        let (directory, mut store) = store("onboarding-values");
        store.set("capture.folder", "/tmp/captures").unwrap();
        store.set("hotkey.capture-region", "Cmd+Alt+P").unwrap();
        let content = onboarding_content(&store, all_supported(), true).unwrap();
        assert_eq!(content.capture_folder, "/tmp/captures");
        assert!(content.capture_shortcut.ends_with('P'));
        let _ = fs::remove_dir_all(directory);
    }
}
