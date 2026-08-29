//! The coordinator: one state machine, driven from the main thread.
//!
//! # Why this is not a loop
//!
//! `tray-icon` and `muda` are `Rc`-based and `!Send`; `GlobalHotKeyManager` on
//! macOS installs a Carbon event handler on the thread that creates it; and
//! winit demands the main thread on every platform. All three need the *same*
//! main thread, with a live event loop, and — this is the part that costs
//! afternoons — all three fail **silently** without one. The tray appears and
//! never responds. The hotkey registers successfully and never fires.
//!
//! So [`App`] contains no loop at all. It exposes a single [`App::tick`] that
//! drains every source once and returns. Whoever owns the real main loop — an
//! `eframe` update callback, or [`crate::gui::host::Headless`] — calls it. That
//! inversion is what makes the whole coordinator testable without a display
//! server, and what stops a second event loop being invented later.
//!
//! # Why polling, not handlers
//!
//! Both `global-hotkey` and `muda` expose `set_event_handler`, and both store it
//! in a process-global `OnceCell`. The first caller wins and every other
//! consumer is starved — including any library we might later depend on. The
//! receivers are the supported concurrent path, so [`scrozz_shell::Tray::poll`]
//! and [`scrozz_shell::GlobalHotkeys::poll`] are what this uses.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use scrozz_annotate::SmartFramePreset;
use scrozz_export::Destination;
use scrozz_shell::{
    Accelerator, Capability, GlobalHotkeys, Hotkey, HotkeyManager, KeyState, Permissions,
    SystemPermissions, Tray, TrayAction,
};
use scrozz_store::CaptureId;
use scrozz_ui::editor::{EditorDestination, EditorEvent, EditorRequest, EditorStatus};

use crate::{
    cli::Cli,
    fault::{CliError, CliResult},
    gui::{
        action::{Action, CaptureKind},
        card::{CardEvent, CardSurface},
        pipeline::{Job, Outcome, Pipeline},
        server::Server,
    },
    json::Json,
    report::Report,
    settings::{AfterCapturePolicy, UserSettings},
};

/// What a hotkey is bound to unless the environment says otherwise.
///
/// Deliberately **not** `Cmd+Shift+3/4/5`: those belong to macOS's own capture
/// service, and `RegisterEventHotKey` returns success for them while never
/// delivering an event — a trap `scrozz-shell` already knows about and refuses
/// up front. These three are unclaimed on a default install.
pub const DEFAULT_BINDINGS: &[(&str, Action)] = &[
    ("Cmd+Shift+7", Action::Capture(CaptureKind::Fullscreen)),
    ("Cmd+Shift+8", Action::Capture(CaptureKind::Region)),
    ("Cmd+Shift+9", Action::Capture(CaptureKind::Window)),
];

/// Overrides the whole binding table, as `accelerator=action-id` pairs.
///
/// e.g. `SCROZZ_GUI_HOTKEYS="Ctrl+Alt+P=capture.fullscreen"`. Empty disables
/// registration entirely, which is how a test run avoids touching the keyboard
/// of whoever is using the machine.
pub const HOTKEYS_ENV: &str = "SCROZZ_GUI_HOTKEYS";

/// Set to `0` to run without a menu-bar item.
pub const TRAY_ENV: &str = "SCROZZ_GUI_TRAY";

/// Milliseconds after which the app quits on its own.
///
/// Exists so an automated run cannot leave a menu-bar item or a window on
/// someone's screen: the app is not trusted to be told to quit, it is given a
/// deadline it cannot miss.
pub const DEADLINE_ENV: &str = "SCROZZ_GUI_TIMEOUT_MS";

/// Set to `1` to take one capture immediately at startup.
///
/// The end-to-end path without touching the keyboard.
pub const AUTOCAPTURE_ENV: &str = "SCROZZ_GUI_CAPTURE_ON_START";

/// How the GUI was asked to run.
#[derive(Debug, Clone)]
pub struct Config {
    /// Accelerator and action, in registration order.
    pub bindings: Vec<(String, Action)>,
    /// Whether to put an item in the menu bar.
    pub tray: bool,
    /// Whether to listen for forwarded commands.
    pub ipc: bool,
    /// When to quit regardless of what anyone asked.
    pub deadline: Option<Duration>,
    /// Whether to capture once at startup.
    pub capture_on_start: Option<CaptureKind>,
    /// GUI-only transform and consumer ordering.
    pub after_capture: AfterCapturePolicy,
    /// User-created Smart Frame presets.
    pub smart_frame_presets: Vec<SmartFramePreset>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bindings: DEFAULT_BINDINGS
                .iter()
                .map(|(accel, action)| ((*accel).to_owned(), *action))
                .collect(),
            tray: true,
            ipc: true,
            deadline: None,
            capture_on_start: None,
            after_capture: AfterCapturePolicy::default(),
            smart_frame_presets: Vec::new(),
        }
    }
}

impl Config {
    /// Reads the configuration for this run.
    ///
    /// The CLI has no GUI flags — adding them would mean editing `cli.rs`, which
    /// belongs to the command-line surface — so the knobs are environment
    /// variables. That is also the right shape for them: they are for driving
    /// the app from a script, not for a person to type.
    #[must_use]
    pub fn from_cli(_cli: &Cli) -> Self {
        let mut config = Self::default();

        if let Ok(raw) = std::env::var(HOTKEYS_ENV) {
            config.bindings = parse_bindings(&raw);
        }
        if let Ok(raw) = std::env::var(TRAY_ENV) {
            config.tray = !matches!(raw.as_str(), "0" | "false" | "no");
        }
        if let Ok(raw) = std::env::var(DEADLINE_ENV)
            && let Ok(ms) = raw.parse::<u64>()
        {
            config.deadline = Some(Duration::from_millis(ms));
        }
        if let Ok(raw) = std::env::var(AUTOCAPTURE_ENV) {
            config.capture_on_start = match raw.as_str() {
                "0" | "false" | "no" | "" => None,
                "region" => Some(CaptureKind::Region),
                "window" => Some(CaptureKind::Window),
                _ => Some(CaptureKind::Fullscreen),
            };
        }

        config
    }

    /// Applies durable user preferences after environment-only launch options.
    #[must_use]
    pub fn with_user_settings(mut self, settings: &UserSettings) -> Self {
        self.after_capture = settings.after_capture_policy();
        self.smart_frame_presets = settings.smart_frame_presets.clone();
        self
    }

    /// A configuration that touches nothing outside this process.
    ///
    /// No menu-bar item, no keyboard registration, no socket. What tests use,
    /// and what makes them safe to run on a machine someone is working on.
    #[must_use]
    pub fn sealed() -> Self {
        Self {
            bindings: Vec::new(),
            tray: false,
            ipc: false,
            deadline: Some(Duration::from_millis(250)),
            capture_on_start: None,
            after_capture: AfterCapturePolicy::default(),
            smart_frame_presets: Vec::new(),
        }
    }
}

/// Parses `accel=action-id` pairs separated by commas or semicolons.
fn parse_bindings(raw: &str) -> Vec<(String, Action)> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let (accel, id) = entry.split_once('=')?;
            let action = Action::from_id(id.trim())?;
            Some((accel.trim().to_owned(), action))
        })
        .collect()
}

/// Whether the host should keep going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Still running.
    Continue,
    /// Quit was asked for, or the deadline passed.
    Stop,
}

/// The running application.
pub struct App {
    config: Config,
    surface: Box<dyn CardSurface>,
    pipeline: Pipeline,
    tray: Option<Tray>,
    hotkeys: GlobalHotkeys,
    server: Option<Server>,
    started: Instant,
    captures: u64,
    editors: HashMap<crate::gui::card::CardId, CaptureId>,
    pending_editors: HashSet<crate::gui::card::CardId>,
    pending_export: Option<crate::gui::card::CardId>,
    pending_persists: HashMap<crate::gui::card::CardId, PendingPersist>,
    export_failures: HashMap<crate::gui::card::CardId, String>,
    next_persist_revision: u64,
    notes: Vec<String>,
}

struct PendingPersist {
    revision: u64,
    data: scrozz_annotate::DocumentData,
    error: Option<String>,
}

impl App {
    /// Builds the application, connecting everything that will connect.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] only for failures that make the app pointless: a
    /// capture worker that will not start, or an endpoint another instance
    /// already holds. A tray that will not appear, or a hotkey the system
    /// refuses, are *recorded* and the app runs on — per D8 a missing capability
    /// is explained, not fatal.
    pub fn new(config: Config, surface: Box<dyn CardSurface>) -> CliResult<Self> {
        let pipeline = Pipeline::start_with_policy(config.after_capture.clone())?;
        let mut notes = Vec::new();

        let server = if config.ipc {
            // The one failure worth stopping for: another instance is live,
            // and a second menu-bar item is exactly what single-instance exists
            // to prevent.
            let server = Server::bind()?;
            notes.push(format!("listening at {}", server.path().display()));
            Some(server)
        } else {
            None
        };

        let tray = if config.tray {
            match Tray::with_tooltip("Scrozz") {
                Ok(tray) => {
                    notes.push("menu-bar item shown".to_owned());
                    Some(tray)
                }
                Err(err) => {
                    notes.push(format!("no menu-bar item: {err}"));
                    None
                }
            }
        } else {
            None
        };

        let mut hotkeys = if config.bindings.is_empty() {
            // Nothing to bind means nothing should touch the keyboard. The
            // detached backend does the bookkeeping without an OS registration.
            GlobalHotkeys::detached()
        } else {
            match GlobalHotkeys::new() {
                Ok(manager) => manager,
                Err(err) => {
                    notes.push(format!("hotkeys unavailable: {err}"));
                    GlobalHotkeys::detached()
                }
            }
        };
        hotkeys.set_command("scrozz");

        for (accelerator, action) in &config.bindings {
            let hotkey = Hotkey {
                accelerator: accelerator.clone(),
            };
            match hotkeys.register(&hotkey, action.id()) {
                Ok(()) => notes.push(format!("{accelerator} → {}", action.id())),
                // Wayland answers `Unsupported` carrying the compositor config
                // line to paste, which is the actual remedy (D11). Losing it in
                // a generic "hotkey failed" would be the wrong trade.
                Err(err) => notes.push(format!("{accelerator} not bound: {err}")),
            }
        }

        let mut app = Self {
            config,
            surface,
            pipeline,
            tray,
            hotkeys,
            server,
            started: Instant::now(),
            captures: 0,
            editors: HashMap::new(),
            pending_editors: HashSet::new(),
            pending_export: None,
            pending_persists: HashMap::new(),
            export_failures: HashMap::new(),
            next_persist_revision: 1,
            notes,
        };

        if let Some(kind) = app.config.capture_on_start {
            app.begin_capture(kind);
        }

        Ok(app)
    }

    /// Services every source once. Never blocks.
    ///
    /// Order matters slightly: input first, so a capture asked for on this tick
    /// is already in flight before outcomes are drained, and card events last,
    /// so a card presented on this tick can be acted on immediately.
    pub fn tick(&mut self) -> Tick {
        if self.expired() {
            self.note("the run deadline passed");
            return Tick::Stop;
        }

        if self.drain_tray() == Tick::Stop {
            return Tick::Stop;
        }
        self.drain_hotkeys();
        if self.drain_server() == Tick::Stop {
            return Tick::Stop;
        }
        self.drain_pipeline();
        self.drain_cards();
        self.drain_editors();

        Tick::Continue
    }

    fn expired(&self) -> bool {
        self.config
            .deadline
            .is_some_and(|limit| self.started.elapsed() >= limit)
    }

    fn drain_tray(&mut self) -> Tick {
        // Collected before dispatch: `Tray::poll` borrows the tray, and acting
        // on an entry may need `&mut self` to reach it again.
        let mut pending = Vec::new();
        if let Some(tray) = &self.tray {
            while let Some(entry) = tray.poll() {
                pending.push(entry);
            }
        }

        for entry in pending {
            if self.perform(Action::from_tray(entry)) == Tick::Stop {
                return Tick::Stop;
            }
        }
        Tick::Continue
    }

    fn drain_hotkeys(&mut self) {
        let mut pending = Vec::new();
        while let Some(event) = self.hotkeys.poll() {
            // Both edges arrive. Acting on the release as well would take two
            // captures per press.
            if event.state == KeyState::Pressed {
                pending.push(event.action);
            }
        }

        for id in pending {
            if let Some(action) = Action::from_id(&id) {
                self.perform(action);
            } else {
                tracing::warn!(action = %id, "a hotkey fired for an action this build does not know");
            }
        }
    }

    fn drain_server(&mut self) -> Tick {
        // Collected before any is served: serving needs `&mut self`, and the
        // listener is borrowed for as long as it is being polled.
        let mut pending = Vec::new();
        if let Some(server) = &self.server {
            while let Some(request) = server.poll() {
                pending.push(request);
            }
        }

        for request in pending {
            tracing::debug!(?request, "a forwarded command arrived");
            // The command runs to completion here, on the main thread, because
            // it must produce byte-identical output to a local run and the
            // command layer is synchronous. A capture is tens of milliseconds;
            // a recording returns as soon as it has started.
            if let Some(command) = request.serve() {
                self.captures += u64::from(matches!(command, crate::cli::Command::Capture(_)));
                if matches!(
                    command,
                    crate::cli::Command::Settings(crate::cli::SettingsArgs {
                        command: crate::cli::SettingsCommand::Set { .. }
                    })
                ) {
                    match crate::settings::SettingsStore::open_default()
                        .and_then(|store| store.load())
                    {
                        Ok(settings) => {
                            self.config.after_capture = settings.after_capture_policy();
                            self.config.smart_frame_presets = settings.smart_frame_presets;
                            if !self.pipeline.post(Job::ConfigureAfterCapture(
                                self.config.after_capture.clone(),
                            )) {
                                self.note(
                                    "settings were saved, but the capture worker could not reload \
                                     After Capture policy",
                                );
                            }
                        }
                        Err(error) => {
                            self.note(format!(
                                "settings were saved but could not be reloaded: {error}"
                            ));
                        }
                    }
                }
                if matches!(command, crate::cli::Command::Gui) {
                    // A second `scrozz gui` means "show yourself", not "start
                    // again". There is nothing to show yet, so it is a no-op
                    // that has at least been answered rather than ignored.
                    self.note("a second launch was answered by this instance");
                }
            }
        }

        // A forwarded command never ends this process. `scrozz quit` is not a
        // command; quitting is a menu entry, because the thing being quit is
        // the application the user can see, not a request that arrived on a
        // socket.
        Tick::Continue
    }

    fn drain_pipeline(&mut self) {
        while let Some(outcome) = self.pipeline.poll() {
            match outcome {
                Outcome::Ready(ready) => {
                    self.captures += 1;
                    let card = ready.card;
                    let summary = card.summary();
                    let card_id = card.id;
                    for consumer in ready.consumers {
                        match consumer.result {
                            Ok(detail) => self.note(format!("{card_id} {detail}")),
                            Err(error) => self.note(format!(
                                "{card_id} {} failed without blocking other actions: {error}",
                                consumer.consumer
                            )),
                        }
                    }
                    if ready.show_overlay {
                        if let Err(err) = self.surface.present(*card) {
                            self.note(format!("a card could not be shown: {err}"));
                        } else {
                            self.note(summary);
                        }
                    } else {
                        self.note(format!("{summary} (Quick Access disabled)"));
                    }
                    if ready.open_editor
                        && self.pending_editors.insert(card_id)
                        && !self.pipeline.post(Job::Open(card_id))
                    {
                        self.pending_editors.remove(&card_id);
                        self.note(format!("{card_id} could not queue the editor restore"));
                    }
                }
                Outcome::EditorReady {
                    card,
                    capture,
                    title,
                    document,
                } => {
                    self.pending_editors.remove(&card);
                    let document = *document;
                    let document = match self.pending_persists.get(&card) {
                        Some(pending) => scrozz_annotate::Document::from_data(
                            document.source,
                            pending.data.clone(),
                        ),
                        None => Ok(document),
                    };
                    match document {
                        Err(error) => {
                            self.note(format!(
                                "{card} could not restore pending editor changes: {error}"
                            ));
                        }
                        Ok(document) => {
                            self.editors.insert(card, capture);
                            if let Err(error) = self.surface.open_editor(
                                EditorRequest::new(card.0, title, document)
                                    .with_presets(self.config.smart_frame_presets.clone()),
                            ) {
                                self.editors.remove(&card);
                                self.note(format!("{card} could not open the editor: {error}"));
                            } else {
                                if let Some(error) = self
                                    .pending_persists
                                    .get(&card)
                                    .and_then(|pending| pending.error.as_ref())
                                {
                                    self.surface.editor_persist_status(
                                        card,
                                        EditorStatus::Failed(error.clone()),
                                    );
                                }
                                if let Some(error) = self.export_failures.get(&card) {
                                    self.surface.editor_export_status(
                                        card,
                                        EditorStatus::Failed(error.clone()),
                                    );
                                }
                                self.note(format!("{card} opened for editing"));
                            }
                        }
                    }
                }
                Outcome::EditorRefused { card, error } => {
                    self.pending_editors.remove(&card);
                    self.surface
                        .editor_status(card, EditorStatus::Failed(error.to_string()));
                    self.note(format!("{card} could not open the editor: {error}"));
                }
                Outcome::EditorExported { card, detail } => {
                    if self.pending_export == Some(card) {
                        self.pending_export = None;
                    }
                    self.export_failures.remove(&card);
                    self.surface
                        .editor_export_status(card, EditorStatus::Complete(detail.clone()));
                    self.note(format!("{card} {detail}"));
                }
                Outcome::EditorExportRefused { card, error } => {
                    if self.pending_export == Some(card) {
                        self.pending_export = None;
                    }
                    let error = error.to_string();
                    self.export_failures.insert(card, error.clone());
                    self.surface
                        .editor_export_status(card, EditorStatus::Failed(error.clone()));
                    self.note(format!("{card} refused editor export: {error}"));
                }
                Outcome::EditorPersisted { card, revision } => {
                    if self
                        .pending_persists
                        .get(&card)
                        .is_none_or(|pending| pending.revision != revision)
                    {
                        continue;
                    }
                    self.pending_persists.remove(&card);
                    self.surface.editor_persist_status(
                        card,
                        EditorStatus::Complete("saved changes".to_owned()),
                    );
                    self.note(format!("{card} saved changes"));
                }
                Outcome::EditorPersistRefused {
                    card,
                    revision,
                    error,
                } => {
                    let error = error.to_string();
                    let Some(pending) = self
                        .pending_persists
                        .get_mut(&card)
                        .filter(|pending| pending.revision == revision)
                    else {
                        continue;
                    };
                    pending.error = Some(error.clone());
                    self.surface
                        .editor_persist_status(card, EditorStatus::Failed(error.clone()));
                    self.note(format!("{card} could not save changes: {error}"));
                }
                Outcome::SmartFrameAnalyzed {
                    card,
                    revision,
                    result,
                } => {
                    self.surface
                        .editor_smart_frame_analyzed(card, revision, *result);
                }
                Outcome::Failed { card, error } => {
                    self.note(format!("{card} failed: {error}"));
                }
                Outcome::Done { card, detail } => {
                    self.surface
                        .editor_status(card, EditorStatus::Complete(detail.clone()));
                    self.note(format!("{card} {detail}"));
                }
                Outcome::Refused { card, error } => {
                    self.surface
                        .editor_status(card, EditorStatus::Failed(error.to_string()));
                    self.note(format!("{card} refused: {error}"));
                }
            }
        }
    }

    fn drain_cards(&mut self) {
        let mut pending = Vec::new();
        while let Some(event) = self.surface.poll() {
            pending.push(event);
        }

        for event in pending {
            match event {
                CardEvent::Copy(id) => {
                    self.route_card_delivery(id, Destination::Clipboard, Job::Copy(id));
                }
                CardEvent::Save(id) => {
                    self.route_card_delivery(
                        id,
                        Destination::Folder(crate::output::default_directory()),
                        Job::Save(id),
                    );
                }
                CardEvent::Open(id) => {
                    if self.editors.contains_key(&id) {
                        self.surface.focus_editor(id);
                    } else if self.pending_editors.insert(id) && !self.pipeline.post(Job::Open(id))
                    {
                        self.pending_editors.remove(&id);
                        self.note(format!("{id} could not queue the editor restore"));
                    }
                }

                CardEvent::Dismiss(id) => {
                    self.surface.dismiss(id);
                    // The bytes are only worth holding while a card can still
                    // ask for them.
                    self.pipeline.post(Job::Release(id));
                    self.note(format!("{id} dismissed"));
                }
                // Not yet routed. The drag payload is `scrozz-shell`'s
                // `DragSource`, and collapsing into the dock is the capture
                // stack's own animation — both belong to the surface that
                // raised the event, once there is one that can.
                CardEvent::Drag(id) | CardEvent::Collapse(id) => {
                    self.note(format!("{id}: {event:?} is not routed yet"));
                }
            }
        }
    }

    fn route_card_delivery(
        &mut self,
        card: crate::gui::card::CardId,
        destination: Destination,
        fallback: Job,
    ) {
        let pending = self
            .pending_persists
            .get(&card)
            .map(|pending| pending.data.clone());
        let capture = self.editors.get(&card).cloned();
        let job = match (pending, capture) {
            (Some(data), Some(capture)) => Job::Export {
                card,
                capture,
                destination,
                data,
            },
            _ => fallback,
        };
        if !self.pipeline.post(job) {
            self.note(format!("{card} could not queue delivery"));
        }
    }

    fn drain_editors(&mut self) {
        let mut pending = Vec::new();
        while let Some(event) = self.surface.poll_editor() {
            pending.push(event);
        }

        for event in pending {
            match event {
                EditorEvent::Persist { id, data } => {
                    let card = crate::gui::card::CardId(id);
                    self.persist_editor_data(card, data);
                }
                EditorEvent::Export {
                    id,
                    destination,
                    data,
                } => {
                    let card = crate::gui::card::CardId(id);
                    if let Some(active) = self.pending_export {
                        let error = format!("{active} is already exporting; wait for it to finish");
                        self.export_failures.insert(card, error.clone());
                        self.surface
                            .editor_export_status(card, EditorStatus::Failed(error.clone()));
                        self.note(format!("{card} could not export: {error}"));
                        continue;
                    }
                    let Some(capture) = self.editors.get(&card).cloned() else {
                        let error = "the capture history identity is no longer available";
                        self.export_failures.insert(card, error.to_owned());
                        self.surface
                            .editor_export_status(card, EditorStatus::Failed(error.to_owned()));
                        self.note(format!("{card} could not export: {error}"));
                        continue;
                    };
                    let destination = match destination {
                        EditorDestination::Clipboard => Destination::Clipboard,
                        EditorDestination::DefaultFolder => {
                            Destination::Folder(crate::output::default_directory())
                        }
                    };
                    if self.pipeline.post(Job::Export {
                        card,
                        capture,
                        destination,
                        data,
                    }) {
                        self.export_failures.remove(&card);
                        self.pending_export = Some(card);
                    } else {
                        let error = "the export worker is no longer available";
                        self.export_failures.insert(card, error.to_owned());
                        self.surface
                            .editor_export_status(card, EditorStatus::Failed(error.to_owned()));
                        self.note(format!("{card} could not export: {error}"));
                    }
                }
                EditorEvent::AnalyzeSmartFrame {
                    id,
                    revision,
                    data,
                    cancellation,
                } => {
                    let card = crate::gui::card::CardId(id);
                    let Some(capture) = self.editors.get(&card).cloned() else {
                        self.surface.editor_smart_frame_analyzed(
                            card,
                            revision,
                            Err("the capture history identity is no longer available".to_owned()),
                        );
                        continue;
                    };
                    if !self.pipeline.post(Job::AnalyzeSmartFrame {
                        card,
                        capture,
                        revision,
                        data,
                        cancellation,
                    }) {
                        self.surface.editor_smart_frame_analyzed(
                            card,
                            revision,
                            Err("the Smart Frame worker is no longer available".to_owned()),
                        );
                    }
                }
                EditorEvent::UpsertPreset { id, preset } => {
                    let card = crate::gui::card::CardId(id);
                    let result = crate::settings::SettingsStore::open_default()
                        .and_then(|store| store.upsert_preset(preset));
                    match result {
                        Ok(settings) => {
                            self.config.smart_frame_presets = settings.smart_frame_presets.clone();
                            let open: Vec<_> = self.editors.keys().copied().collect();
                            for editor in open {
                                self.surface.editor_presets_updated(
                                    editor,
                                    settings.smart_frame_presets.clone(),
                                    EditorStatus::Complete(if editor == card {
                                        "Saved custom Smart Frame preset".to_owned()
                                    } else {
                                        "Smart Frame presets updated".to_owned()
                                    }),
                                );
                            }
                        }
                        Err(error) => self.surface.editor_status(
                            card,
                            EditorStatus::Failed(format!("Could not save preset: {error}")),
                        ),
                    }
                }
                EditorEvent::DeletePreset { id, preset_id } => {
                    let card = crate::gui::card::CardId(id);
                    let result = crate::settings::SettingsStore::open_default()
                        .and_then(|store| store.delete_preset(&preset_id));
                    match result {
                        Ok(settings) => {
                            self.config.smart_frame_presets = settings.smart_frame_presets.clone();
                            let open: Vec<_> = self.editors.keys().copied().collect();
                            for editor in open {
                                self.surface.editor_presets_updated(
                                    editor,
                                    settings.smart_frame_presets.clone(),
                                    EditorStatus::Complete(if editor == card {
                                        "Deleted custom Smart Frame preset".to_owned()
                                    } else {
                                        "Smart Frame presets updated".to_owned()
                                    }),
                                );
                            }
                        }
                        Err(error) => self.surface.editor_status(
                            card,
                            EditorStatus::Failed(format!("Could not delete preset: {error}")),
                        ),
                    }
                }
                EditorEvent::Closed { id } => {
                    let card = crate::gui::card::CardId(id);
                    self.editors.remove(&card);
                    self.note(format!("{card} editor closed"));
                }
            }
        }
    }

    fn persist_editor_data(
        &mut self,
        card: crate::gui::card::CardId,
        data: scrozz_annotate::DocumentData,
    ) {
        let revision = self.next_persist_revision;
        self.next_persist_revision = self.next_persist_revision.saturating_add(1);
        self.pending_persists.insert(
            card,
            PendingPersist {
                revision,
                data: data.clone(),
                error: None,
            },
        );
        let Some(capture) = self.editors.get(&card).cloned() else {
            let error = "the capture history identity is no longer available".to_owned();
            if let Some(pending) = self.pending_persists.get_mut(&card) {
                pending.error = Some(error.clone());
            }
            self.surface
                .editor_persist_status(card, EditorStatus::Failed(error.clone()));
            self.note(format!("{card} could not save changes: {error}"));
            return;
        };
        if !self.pipeline.post(Job::Persist {
            card,
            revision,
            capture,
            data,
        }) {
            let error = "the persistence worker is no longer available".to_owned();
            if let Some(pending) = self.pending_persists.get_mut(&card) {
                pending.error = Some(error.clone());
            }
            self.surface
                .editor_persist_status(card, EditorStatus::Failed(error.clone()));
            self.note(format!("{card} could not save changes: {error}"));
        }
    }

    /// Routes editor events synchronously before the worker is stopped.
    ///
    /// Window hosts call this after flushing debounced panels so persistence
    /// jobs enter the worker queue ahead of [`Job::Stop`].
    pub(crate) fn flush_editor_events(&mut self) {
        self.drain_editors();
    }

    /// Carries out one action.
    fn perform(&mut self, action: Action) -> Tick {
        tracing::debug!(action = action.id(), "performing");
        match action {
            Action::Capture(kind) => {
                self.begin_capture(kind);
                Tick::Continue
            }
            Action::ToggleRecording => {
                // Recording is `scrozz-record`, which is behind the same guard
                // and has no session to toggle yet. Saying so beats a menu item
                // that does nothing.
                self.note("recording is not wired up yet");
                Tick::Continue
            }
            Action::OpenHistory => {
                self.note("the history window is not built yet");
                Tick::Continue
            }
            Action::OpenSettings => {
                self.note("the settings window is not built yet");
                Tick::Continue
            }
            Action::Quit => {
                self.note("quit");
                Tick::Stop
            }
        }
    }

    fn begin_capture(&mut self, kind: CaptureKind) {
        // D15: ask at first use, not at launch. This must happen on the main
        // thread before the capture job is posted: the missing piece that made
        // Scrozz report PermissionDenied in an invisible log without ever
        // invoking CGRequestScreenCaptureAccess, so it never appeared in System
        // Settings at all.
        let permissions = SystemPermissions::new();
        if !permissions.is_granted(Capability::ScreenRecording)
            && let Err(error) = permissions.request(Capability::ScreenRecording)
        {
            self.note(format!("capture permission is required: {error}"));
            return;
        }

        let card = self.pipeline.allocate();
        if !self.pipeline.post(Job::Capture { kind, card }) {
            self.note("the capture worker has gone");
        }
    }

    fn note(&mut self, what: impl Into<String>) {
        let what = what.into();
        tracing::info!("{what}");
        self.notes.push(what);
    }

    /// How many cards are on screen.
    #[must_use]
    pub fn showing(&self) -> usize {
        self.surface.len()
    }

    /// What happened, for the report the CLI prints when the app exits.
    #[must_use]
    pub fn report(&self) -> Report {
        let data = Json::obj([
            (
                "captures",
                Json::Int(i64::try_from(self.captures).unwrap_or(i64::MAX)),
            ),
            ("surface", Json::str(self.surface.describe())),
            (
                "bindings",
                Json::arr(self.config.bindings.iter().map(|(accel, action)| {
                    Json::obj([
                        ("accelerator", Json::str(accel.as_str())),
                        ("action", Json::str(action.id())),
                    ])
                })),
            ),
            ("tray", Json::Bool(self.tray.is_some())),
            ("ipc", Json::Bool(self.server.is_some())),
            (
                "notes",
                Json::arr(self.notes.iter().map(|n| Json::str(n.as_str()))),
            ),
        ]);

        let human = if self.captures == 0 {
            "Scrozz ran and took no captures.".to_owned()
        } else if self.captures == 1 {
            "Scrozz took 1 capture.".to_owned()
        } else {
            format!("Scrozz took {} captures.", self.captures)
        };

        Report::new(data, human)
    }

    /// Releases the menu-bar item, the hotkeys and the socket, now.
    ///
    /// `Drop` would do it, but "now" is the point: a menu-bar item that outlives
    /// its usefulness by even a second is the thing most likely to be left on
    /// someone's screen.
    pub fn shut_down(&mut self) {
        self.finish_pending_work();
        self.hotkeys.unregister_all();
        if let Some(tray) = self.tray.take() {
            tray.close();
        }
        self.server = None;
    }

    /// Finishes queued worker jobs and incorporates their outcomes in the report.
    pub(crate) fn finish_pending_work(&mut self) {
        self.pipeline.stop();
        self.drain_pipeline();
    }

    /// Every note recorded so far.
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.shut_down();
    }
}

/// Reports what a hotkey would do to the system, without registering it.
///
/// Used by diagnostics, and worth having separately: the answer for
/// `Cmd+Shift+4` is *not* "it failed", it is "macOS owns this one", and the two
/// send a user to very different places.
///
/// # Errors
///
/// Returns an error only if the accelerator cannot be parsed at all.
pub fn describe_conflict(accelerator: &str) -> CliResult<Option<String>> {
    let parsed = Accelerator::parse(accelerator).map_err(CliError::Core)?;
    Ok(parsed
        .system_owner()
        .map(|reserved| format!("{parsed} is reserved: {reserved:?}")))
}

/// The tray entries this build offers, for diagnostics.
#[must_use]
pub fn menu_actions() -> Vec<Action> {
    TrayAction::ALL
        .iter()
        .copied()
        .map(Action::from_tray)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::card::{Card, CardId, Recording};

    fn app() -> (App, Recording) {
        let surface = Recording::new();
        let handle = surface.handle();
        let app = App::new(Config::sealed(), Box::new(surface)).expect("a sealed app must start");
        (app, handle)
    }

    #[test]
    fn a_sealed_app_touches_nothing() {
        let (app, _) = app();
        assert!(app.tray.is_none(), "no menu-bar item");
        assert!(app.server.is_none(), "no socket");
        assert_eq!(app.config.bindings.len(), 0, "no keyboard registration");
    }

    #[test]
    fn a_sealed_app_stops_at_its_deadline() {
        // The property the hard constraint rests on: a test run cannot outlive
        // its welcome even if every other mechanism fails.
        let (mut app, _) = app();
        assert_eq!(app.tick(), Tick::Continue);

        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(app.tick(), Tick::Stop);
    }

    #[test]
    fn quitting_stops_the_app() {
        let (mut app, _) = app();
        assert_eq!(app.perform(Action::Quit), Tick::Stop);
        assert!(app.notes().iter().any(|n| n == "quit"));
    }

    #[test]
    fn failed_persistence_enqueue_retains_the_latest_snapshot() {
        let (mut app, _) = app();
        let card = CardId(42);
        app.editors
            .insert(card, CaptureId("capture-fixture".to_owned()));
        app.pipeline.stop();
        let data = scrozz_annotate::DocumentData {
            next_id: 17,
            ..Default::default()
        };

        app.persist_editor_data(card, data.clone());

        let pending = app.pending_persists.get(&card).expect("retained edits");
        assert_eq!(pending.data, data);
        assert!(
            pending
                .error
                .as_deref()
                .is_some_and(|error| error.contains("worker"))
        );
    }

    #[test]
    fn an_unwired_action_says_so_rather_than_doing_nothing() {
        let (mut app, _) = app();
        for action in [
            Action::ToggleRecording,
            Action::OpenHistory,
            Action::OpenSettings,
        ] {
            assert_eq!(app.perform(action), Tick::Continue);
        }
        let notes = app.notes().join("\n");
        assert!(notes.contains("recording is not wired up yet"), "{notes}");
        assert!(notes.contains("history window"), "{notes}");
        assert!(notes.contains("settings window"), "{notes}");
    }

    #[test]
    fn dismissing_a_card_removes_it_from_the_surface() {
        let (mut app, surface) = app();
        // Placed directly, because a real capture needs a screen.
        app.surface
            .present(Card::placeholder(CardId(1), CaptureKind::Fullscreen))
            .expect("recording never refuses");
        assert_eq!(app.showing(), 1);

        surface.inject(CardEvent::Dismiss(CardId(1)));
        app.tick();
        assert_eq!(app.showing(), 0);
        assert!(app.notes().iter().any(|n| n.contains("dismissed")));
    }

    #[test]
    fn copying_a_card_reaches_the_worker_and_comes_back() {
        let (mut app, surface) = app();
        surface.inject(CardEvent::Copy(CardId(42)));
        app.tick();

        // The worker has no bytes for a card it never captured, so the answer
        // is a refusal — which is the proof the message got there at all.
        for _ in 0..200 {
            app.drain_pipeline();
            if app.notes().iter().any(|n| n.contains("card:42 refused")) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the copy never reached the worker: {:?}", app.notes());
    }

    #[test]
    fn opening_a_card_reaches_the_document_worker() {
        let (mut app, surface) = app();
        surface.inject(CardEvent::Open(CardId(43)));
        app.tick();

        // As with copy, the absent card is useful here: only the worker can
        // produce this refusal, proving Open is no longer swallowed by the
        // coordinator.
        for _ in 0..200 {
            app.drain_pipeline();
            if app.notes().iter().any(|note| {
                note.contains("card:43 could not open the editor")
                    && note.contains("no capture to edit")
            }) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("open never reached the worker: {:?}", app.notes());
    }

    #[test]
    fn reopening_an_active_editor_never_queues_a_stale_store_read() {
        let (mut app, surface) = app();
        let card = CardId(44);
        app.editors
            .insert(card, CaptureId("active-editor".to_owned()));
        surface.inject(CardEvent::Open(card));

        app.tick();
        std::thread::sleep(Duration::from_millis(25));
        app.drain_pipeline();

        assert!(!app.pending_editors.contains(&card));
        assert!(
            app.notes()
                .iter()
                .all(|note| !note.contains("no capture to edit")),
            "focus-only reopen must not reach the worker: {:?}",
            app.notes()
        );
    }

    #[test]
    fn an_unrouted_card_gesture_is_recorded_not_swallowed() {
        let (mut app, surface) = app();
        surface.inject(CardEvent::Drag(CardId(5)));
        app.tick();
        assert!(
            app.notes().iter().any(|n| n.contains("not routed yet")),
            "{:?}",
            app.notes()
        );
    }

    #[test]
    fn the_report_counts_captures_and_names_the_surface() {
        let (app, _) = app();
        let report = app.report();
        assert_eq!(report.human, "Scrozz ran and took no captures.");

        let json = report.data.to_compact_string();
        assert!(json.contains("\"captures\":0"), "{json}");
        assert!(json.contains("recording surface"), "{json}");
        assert!(json.contains("\"tray\":false"), "{json}");
    }

    #[test]
    fn shutting_down_twice_is_harmless() {
        // `Drop` calls it too, so it must be idempotent.
        let (mut app, _) = app();
        app.shut_down();
        app.shut_down();
    }

    #[test]
    fn the_default_bindings_avoid_the_shortcuts_macos_owns() {
        // The trap: `RegisterEventHotKey` returns success for these and the
        // handler never fires, so the failure is invisible.
        for (accelerator, _) in DEFAULT_BINDINGS {
            let conflict = describe_conflict(accelerator)
                .unwrap_or_else(|e| panic!("{accelerator} should parse: {e}"));
            assert!(
                conflict.is_none(),
                "{accelerator} is system-owned: {conflict:?}"
            );
        }
    }

    #[test]
    fn the_screenshot_shortcuts_are_recognised_as_taken() {
        // The negative of the test above: if this stops holding, the one above
        // has stopped proving anything.
        let taken = ["Cmd+Shift+3", "Cmd+Shift+4", "Cmd+Shift+5"]
            .iter()
            .filter_map(|a| describe_conflict(a).ok().flatten())
            .count();
        assert!(
            taken > 0,
            "scrozz-shell should know macOS owns the screenshot shortcuts"
        );
    }

    #[test]
    fn every_default_binding_names_a_real_action() {
        for (_, action) in DEFAULT_BINDINGS {
            assert_eq!(Action::from_id(action.id()), Some(*action));
        }
    }

    #[test]
    fn bindings_are_parsed_from_the_environment_format() {
        let parsed = parse_bindings("Ctrl+Alt+P=capture.fullscreen, Ctrl+Alt+R=capture.region");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "Ctrl+Alt+P");
        assert_eq!(parsed[0].1, Action::Capture(CaptureKind::Fullscreen));
        assert_eq!(parsed[1].1, Action::Capture(CaptureKind::Region));
    }

    #[test]
    fn an_empty_or_broken_binding_list_binds_nothing() {
        // Which is what makes `SCROZZ_GUI_HOTKEYS=` a safe way to run without
        // touching the keyboard at all.
        assert!(parse_bindings("").is_empty());
        assert!(parse_bindings("   ").is_empty());
        assert!(parse_bindings("nonsense").is_empty());
        assert!(parse_bindings("Ctrl+P=not.an.action").is_empty());
    }

    #[test]
    fn the_menu_offers_every_action_this_build_knows() {
        let menu = menu_actions();
        assert_eq!(menu.len(), TrayAction::ALL.len());
        assert!(menu.contains(&Action::Quit));
        assert!(menu.contains(&Action::Capture(CaptureKind::Fullscreen)));
    }
}
