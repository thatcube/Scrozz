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
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use scrozz_core::Error as CoreError;

use scrozz_annotate::Document;
use scrozz_shell::{
    Accelerator, Capability, DragOutcome, DragPayload, GlobalHotkeys, Hotkey, HotkeyManager,
    KeyState, Permissions, SystemPermissions, Tray, TrayAction,
};
use scrozz_store::{CaptureId, RetentionPolicy, Timestamp};
use scrozz_ui::history::{HistoryAction, HistoryViewModel};

use crate::{
    cli::{Cli, Command, SettingsCommand},
    fault::{CliError, CliResult},
    gui::{
        action::{Action, CaptureKind},
        card::{CardEvent, CardSurface},
        pipeline::{DragGeometry, DragSubject, HistoryOperation, Job, Outcome, Pipeline},
        server::Server,
    },
    json::Json,
    report::Report,
    settings_store::SettingsStore,
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

/// Settings keys that currently have a live GUI action.
///
/// Recording and all-display shortcuts remain in the schema, but are not
/// registered until those actions can complete end to end.
pub const SETTING_BINDINGS: &[(&str, Action)] = &[
    (
        "hotkey.capture-region",
        Action::Capture(CaptureKind::Region),
    ),
    (
        "hotkey.capture-window",
        Action::Capture(CaptureKind::Window),
    ),
    (
        "hotkey.capture-display",
        Action::Capture(CaptureKind::Fullscreen),
    ),
];

const OVERLAY_ESCAPE_ACCELERATOR: &str = "Escape";
const OVERLAY_ESCAPE_ACTION: &str = "overlay-dismiss-all";

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

/// Card-size multiplier (`0.5..=2.0`).
pub const CARD_SCALE_ENV: &str = "SCROZZ_GUI_CARD_SCALE";

/// Stack inset from the selected display's work-area edges, in points.
pub const STACK_MARGIN_ENV: &str = "SCROZZ_GUI_STACK_MARGIN";

/// Inactivity interval in milliseconds. `0` disables auto-close.
pub const AUTO_CLOSE_ENV: &str = "SCROZZ_GUI_AUTO_CLOSE_MS";

/// Which display supplies the overlay work area: `active` or `primary`.
pub const DISPLAY_ENV: &str = "SCROZZ_GUI_DISPLAY";

/// How the capture stack is sized and placed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayConfig {
    /// Multiplier for card size and spacing.
    pub card_scale: f32,
    /// Inset from the work-area edges.
    pub stack_margin: f32,
    /// Inactivity interval; `None` keeps unpinned cards open.
    pub auto_close: Option<Duration>,
    /// Display selection policy.
    pub display: OverlayDisplay,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            card_scale: 1.0,
            stack_margin: 16.0,
            auto_close: Some(Duration::from_secs(8)),
            display: OverlayDisplay::Active,
        }
    }
}

/// Which display hosts the transient capture stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayDisplay {
    /// Follow the display containing the pointer.
    Active,
    /// Stay on the menu-bar/primary display.
    Primary,
}

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
    /// Source-image retention applied by the history worker.
    pub retention_policy: RetentionPolicy,
    /// Capture-stack size, position, timeout, and display behavior.
    pub overlay: OverlayConfig,
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
            retention_policy: RetentionPolicy::default(),
            overlay: OverlayConfig::default(),
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
    pub fn from_cli(_cli: &Cli) -> CliResult<Self> {
        let store = SettingsStore::load()?;
        let mut config = Self::from_settings(&store)?;

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
                "scrolling" => Some(CaptureKind::Scrolling),
                _ => Some(CaptureKind::Fullscreen),
            };
        }
        if let Ok(raw) = std::env::var(CARD_SCALE_ENV)
            && let Ok(scale) = raw.parse::<f32>()
            && scale.is_finite()
        {
            config.overlay.card_scale = scale.clamp(0.5, 2.0);
        }
        if let Ok(raw) = std::env::var(STACK_MARGIN_ENV)
            && let Ok(margin) = raw.parse::<f32>()
            && margin.is_finite()
        {
            config.overlay.stack_margin = margin.clamp(0.0, 256.0);
        }
        if let Ok(raw) = std::env::var(AUTO_CLOSE_ENV)
            && let Ok(ms) = raw.parse::<u64>()
        {
            config.overlay.auto_close = (ms > 0).then(|| Duration::from_millis(ms));
        }
        if let Ok(raw) = std::env::var(DISPLAY_ENV) {
            config.overlay.display = if raw.eq_ignore_ascii_case("primary") {
                OverlayDisplay::Primary
            } else {
                OverlayDisplay::Active
            };
        }

        Ok(config)
    }

    fn from_settings(store: &SettingsStore) -> CliResult<Self> {
        let max_image_bytes = store
            .get("history.max-image-bytes")?
            .1
            .parse::<u64>()
            .map_err(|error| {
                CliError::usage(format!(
                    "history.max-image-bytes is not an unsigned integer: {error}"
                ))
            })?;
        let max_age_days = store
            .get("history.max-age-days")?
            .1
            .parse::<u32>()
            .map_err(|error| {
                CliError::usage(format!(
                    "history.max-age-days is not an unsigned integer: {error}"
                ))
            })?;

        Ok(Self {
            bindings: SETTING_BINDINGS
                .iter()
                .map(|(key, action)| {
                    store
                        .get(key)
                        .map(|(_, value, _)| (value.to_owned(), *action))
                })
                .collect::<CliResult<_>>()?,
            tray: store.get("system.tray-icon")?.1 == "true",
            retention_policy: RetentionPolicy::from_limits(max_image_bytes, max_age_days)?,
            ..Self::default()
        })
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
            retention_policy: RetentionPolicy {
                max_image_bytes: u64::MAX,
                max_image_age: scrozz_store::RetentionWindow::Forever,
            },
            overlay: OverlayConfig::default(),
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
    notes: Vec<String>,
    settings_requested: bool,
    settings_revision: u64,
    active_window_capture: Option<crate::gui::card::CardId>,
    window_picker: Option<WindowPickerSession>,
    history: Arc<Mutex<HistoryViewModel>>,
    pending_drags: VecDeque<PendingDrag>,
    pending_editors: VecDeque<(CaptureId, Document)>,
    escape_registered: bool,
    escape_retry_at: Option<Instant>,
    overlay_hidden: bool,
    native_drag_active: bool,
}

struct WindowPickerSession {
    card: crate::gui::card::CardId,
    picker: scrozz_ui::picker::WindowPicker,
    theme: scrozz_ui::Theme,
    notice: Option<String>,
    committing: bool,
}

/// A worker-built drag waiting for the window host to enter the native loop.
pub struct PendingDrag {
    /// Card or history capture.
    pub subject: DragSubject,
    /// Promised file and image data.
    pub payload: DragPayload,
    /// Source geometry.
    pub geometry: DragGeometry,
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
        let pipeline = Pipeline::start_with_retention(config.retention_policy.clone())?;
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
            notes,
            settings_requested: false,
            settings_revision: 0,
            active_window_capture: None,
            window_picker: None,
            history: Arc::new(Mutex::new(HistoryViewModel::new(Timestamp::now()))),
            pending_drags: VecDeque::new(),
            pending_editors: VecDeque::new(),
            escape_registered: false,
            escape_retry_at: None,
            overlay_hidden: false,
            native_drag_active: false,
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
        self.with_history(|history| history.advance_clock(Timestamp::now()));
        self.drain_pipeline();
        self.drain_cards();
        self.drain_history();
        self.reconcile_escape_binding();

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
            if id == OVERLAY_ESCAPE_ACTION {
                self.surface.dismiss_all();
                continue;
            }
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
                let settings_changed = matches!(
                    &command,
                    Command::Settings(args)
                        if matches!(
                            args.command,
                            SettingsCommand::Set { .. } | SettingsCommand::Reset { .. }
                        )
                );
                self.captures += u64::from(matches!(command, crate::cli::Command::Capture(_)));
                if matches!(command, crate::cli::Command::Gui) {
                    // A second `scrozz gui` means "show yourself", not "start
                    // again". There is nothing to show yet, so it is a no-op
                    // that has at least been answered rather than ignored.
                    self.note("a second launch was answered by this instance");
                } else {
                    self.with_history(|history| history.refresh_from_start(Timestamp::now()));
                }
                if settings_changed && let Err(error) = self.reload_settings() {
                    self.note(format!(
                        "settings changed but could not be reloaded: {error}"
                    ));
                }
            }
        }

        // A forwarded command never ends this process. `scrozz quit` is not a
        // command; quitting is a menu entry, because the thing being quit is
        // the application the user can see, not a request that arrived on a
        // socket.
        Tick::Continue
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn handle_url(&mut self, url: &str) -> CliResult<()> {
        let store = crate::settings_store::SettingsStore::open_default()?;
        let action = crate::commands::enabled_url_action(&store, url)?;
        let command = crate::commands::command_for_url_action(action)?;
        crate::commands::dispatch(&command)?;
        self.captures += u64::from(matches!(command, crate::cli::Command::Capture(_)));
        self.note(format!("handled {}", action.slug()));
        Ok(())
    }

    fn drain_pipeline(&mut self) {
        while let Some(outcome) = self.pipeline.poll() {
            match outcome {
                Outcome::Ready(card) if self.pipeline.is_window_cancelled(card.id) => {
                    self.pipeline.post(Job::Release(card.id));
                }
                Outcome::Progress { card, progress } => {
                    self.note(format!("{card} {}", describe_scroll_progress(&progress)));
                }
                Outcome::Ready(card) => {
                    if self.active_window_capture == Some(card.id) {
                        self.active_window_capture = None;
                        self.window_picker = None;
                    }
                    self.captures += 1;
                    let history_changed = card.capture_id.is_some();
                    let summary = card.summary();
                    let card_id = card.id;
                    if let Err(err) = self.surface.present(*card) {
                        self.pipeline.post(Job::Release(card_id));
                        self.note(format!("a card could not be shown: {err}"));
                    } else {
                        self.ensure_escape_binding();
                        self.note(summary);
                    }
                    if history_changed {
                        self.with_history(|history| history.refresh_from_start(Timestamp::now()));
                    }
                }
                Outcome::Restored(card) => {
                    let summary = card.summary();
                    let card_id = card.id;
                    if let Err(err) = self.surface.present(*card) {
                        self.pipeline.post(Job::Release(card_id));
                        self.note(format!("a restored card could not be shown: {err}"));
                    } else {
                        self.ensure_escape_binding();
                        self.note(format!("restored {summary}"));
                    }
                }
                Outcome::Failed { card, .. } if self.pipeline.is_window_cancelled(card) => {}
                Outcome::Failed {
                    card,
                    error: CliError::Core(CoreError::Cancelled),
                } => {
                    if self.active_window_capture == Some(card) {
                        self.active_window_capture = None;
                        self.window_picker = None;
                    }
                }
                Outcome::Failed { card, error } => {
                    if self.active_window_capture == Some(card) {
                        self.active_window_capture = None;
                        self.window_picker = None;
                    }
                    self.note(format!("{card} failed: {error}"));
                }
                Outcome::PickWindow {
                    card,
                    windows,
                    displays,
                    notice,
                } => {
                    if self.pipeline.is_window_cancelled(card) {
                        continue;
                    }
                    if self.active_window_capture == Some(card) {
                        self.window_picker = Some(WindowPickerSession {
                            card,
                            picker: scrozz_ui::picker::WindowPicker::new(windows, displays),
                            theme: scrozz_ui::Theme::dark(),
                            notice,
                            committing: false,
                        });
                    } else {
                        self.pipeline.cancel_window(card);
                    }
                }
                Outcome::Done {
                    card,
                    detail,
                    dismiss,
                } => {
                    if dismiss {
                        self.surface.dismiss_after_action(card);
                        self.pipeline.post(Job::Release(card));
                    }
                    self.note(format!("{card} {detail}"));
                }
                Outcome::PinUpdated {
                    card,
                    pinned,
                    detail,
                } => {
                    self.surface.set_pinned(card, pinned);
                    self.note(format!("{card} {detail}"));
                }
                Outcome::Refused { card, error } => {
                    self.note(format!("{card} refused: {error}"));
                }
                Outcome::HistoryLoaded { request, page } => {
                    self.with_history(|history| history.apply_page(request, page));
                }
                Outcome::HistoryFailed {
                    request,
                    operation,
                    capture,
                    error,
                } => {
                    let message = format!("could not {}: {error}", operation.label());
                    self.with_history(|history| {
                        if let Some(request) = request {
                            history.apply_query_error(request, &message);
                        } else {
                            history.operation_failed(&message);
                            if capture.is_some() {
                                history.refresh_current(Timestamp::now());
                            }
                        }
                    });
                    if let Some(capture) = capture {
                        self.note(format!("{}: {message}", capture.0));
                    } else {
                        self.note(message);
                    }
                }
                Outcome::HistoryDone {
                    operation,
                    capture,
                    pinned,
                    detail,
                } => {
                    self.with_history(|history| match (operation, capture.as_ref(), pinned) {
                        (HistoryOperation::Pin, Some(id), Some(pinned)) => {
                            history.pinned(id, pinned);
                        }
                        (HistoryOperation::Delete, Some(id), _) => history.deleted(id),
                        (HistoryOperation::Retention, _, _) => {
                            history.completed(&detail);
                            history.refresh_current(Timestamp::now());
                        }
                        _ => history.completed(&detail),
                    });
                    self.note(detail);
                }
                Outcome::EditorReady { capture, document } => {
                    // Until the annotation-editor host lands, retain only the
                    // newest handoff instead of accumulating full-resolution
                    // documents without a bound.
                    self.pending_editors.clear();
                    self.pending_editors.push_back((capture.clone(), *document));
                    self.with_history(|history| {
                        history.completed("Editable document loaded");
                    });
                    self.note(format!("{} loaded for editing", capture.0));
                }
                Outcome::DragReady {
                    subject,
                    payload,
                    geometry,
                } => {
                    self.pending_drags.push_back(PendingDrag {
                        subject,
                        payload,
                        geometry,
                    });
                }
                Outcome::DragFailed { subject, error } => {
                    self.drag_finished(
                        &subject,
                        &DragOutcome::Failed(format!("could not prepare drag: {error}")),
                    );
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
                    self.pipeline.post(Job::Copy(id));
                }
                CardEvent::Save(id) => {
                    self.pipeline.post(Job::Save(id));
                }
                CardEvent::Dismiss(id) => {
                    self.surface.dismiss(id);
                    // The bytes are only worth holding while a card can still
                    // ask for them.
                    self.pipeline.post(Job::Release(id));
                    self.note(format!("{id} dismissed"));
                }
                CardEvent::DragStarted(_) => {
                    // Native drag-out starts only after directional intent commits.
                }
                CardEvent::DragOut { id, rect, pointer } => {
                    if !self.release_escape_binding() {
                        self.surface.finish_drag(id, false);
                        self.note(format!(
                            "{id} could not start drag-out while the Escape binding remained active"
                        ));
                        continue;
                    }
                    if !self.pipeline.post(Job::Drag {
                        card: id,
                        geometry: DragGeometry { rect, pointer },
                    }) {
                        self.surface.finish_drag(id, false);
                        self.ensure_escape_binding();
                        self.note("the capture worker has gone");
                    } else {
                        self.native_drag_active = true;
                    }
                }
                CardEvent::Collapse(_) | CardEvent::DockCollapsed => {
                    self.note("capture stack collapsed");
                }
                CardEvent::DockExpanded => {
                    self.note("capture stack expanded");
                }
                CardEvent::Emptied => {
                    self.release_escape_binding();
                    self.note("capture stack emptied");
                }
                CardEvent::Restored(id) => {
                    self.ensure_escape_binding();
                    self.note(format!("{id} restored"));
                }
                CardEvent::VisibilityChanged { hidden } => {
                    self.overlay_hidden = hidden;
                    self.reconcile_escape_binding();
                }
                CardEvent::Open(id) => {
                    self.pipeline.post(Job::OpenCard(id));
                }
                CardEvent::Upload(id) => {
                    self.pipeline.post(Job::Upload(id));
                }
                CardEvent::Pin { id, pinned } => {
                    self.pipeline.post(Job::Pin { card: id, pinned });
                }
            }
        }
    }

    fn drain_history(&mut self) {
        let actions = self.with_history(HistoryViewModel::drain_actions);
        for action in actions {
            let posted = match action {
                HistoryAction::Query { request, query } => {
                    self.pipeline.query_history(request, query)
                }
                HistoryAction::Restore(capture) => {
                    let card = self.pipeline.allocate();
                    self.pipeline.post(Job::Restore { capture, card })
                }
                HistoryAction::OpenEditor(capture) => self.pipeline.post(Job::OpenEditor(capture)),
                HistoryAction::Copy(capture) => self.pipeline.post(Job::CopyHistory(capture)),
                HistoryAction::Save(capture) => self.pipeline.post(Job::SaveHistory(capture)),
                HistoryAction::Drag { id, rect, pointer } => self.pipeline.post(Job::DragHistory {
                    capture: id,
                    geometry: DragGeometry { rect, pointer },
                }),
                HistoryAction::SetPinned { id, pinned } => self.pipeline.post(Job::SetPinned {
                    capture: id,
                    pinned,
                }),
                HistoryAction::Delete(capture) => self.pipeline.post(Job::Delete(capture)),
            };
            if !posted {
                self.note("the capture worker has gone");
            }
        }
    }

    /// Drains interactions emitted while the overlay was drawing this frame.
    pub(super) fn drain_overlay_events(&mut self) {
        self.drain_cards();
        self.reconcile_escape_binding();
    }

    fn reconcile_escape_binding(&mut self) {
        if self.overlay_hidden || self.surface.is_empty() || self.native_drag_active {
            let _ = self.release_escape_binding();
        } else {
            self.ensure_escape_binding();
        }
    }

    fn ensure_escape_binding(&mut self) {
        if self.escape_registered
            || self.surface.is_empty()
            || self.native_drag_active
            || self.overlay_hidden
            || self
                .escape_retry_at
                .is_some_and(|deadline| Instant::now() < deadline)
        {
            return;
        }
        let hotkey = Hotkey {
            accelerator: OVERLAY_ESCAPE_ACCELERATOR.to_owned(),
        };
        match self.hotkeys.register(&hotkey, OVERLAY_ESCAPE_ACTION) {
            Ok(()) => {
                self.escape_registered = true;
                self.escape_retry_at = None;
            }
            Err(error) => {
                self.escape_retry_at = Some(Instant::now() + Duration::from_secs(5));
                self.note(format!(
                    "Escape could not dismiss the capture stack: {error}"
                ));
            }
        }
    }

    fn release_escape_binding(&mut self) -> bool {
        self.escape_retry_at = None;
        if !self.escape_registered {
            return true;
        }
        let hotkey = Hotkey {
            accelerator: OVERLAY_ESCAPE_ACCELERATOR.to_owned(),
        };
        match self.hotkeys.unregister(&hotkey) {
            Ok(()) => {
                self.escape_registered = false;
                true
            }
            Err(error) => {
                self.note(format!("Escape binding could not be released: {error}"));
                false
            }
        }
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
            Action::RestoreRecent => {
                self.surface.restore_recent();
                self.note("restore-last requested");
                Tick::Continue
            }
            Action::ToggleOverlay => {
                self.surface.toggle_hidden();
                self.note("capture stack visibility toggled");
                Tick::Continue
            }
            Action::OpenHistory => {
                self.with_history(|history| history.open(Timestamp::now()));
                self.note("capture history opened");
                Tick::Continue
            }
            Action::OpenSettings => {
                self.settings_requested = true;
                self.note("settings requested");
                Tick::Continue
            }
            Action::Quit => {
                self.note("quit");
                Tick::Stop
            }
        }
    }

    fn begin_capture(&mut self, kind: CaptureKind) {
        if kind == CaptureKind::Window && self.active_window_capture.is_some() {
            self.note("a window picker is already open");
            return;
        }

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
        } else if kind == CaptureKind::Window {
            self.active_window_capture = Some(card);
        }
    }

    /// Draws the in-process picker as a child of the GUI's existing event loop.
    ///
    /// Wayland never reaches this method with a picker: its trusted portal owns
    /// selection and the worker returns the completed capture directly.
    pub fn paint_window_picker(&mut self, root: &egui::Context) {
        let Some(session) = self.window_picker.as_mut() else {
            return;
        };

        #[cfg(target_os = "windows")]
        let (intent, close_requested) = scrozz_ui::picker::interact_display_viewports(
            root,
            &mut session.picker,
            &session.theme,
            session.notice.as_deref(),
        );

        #[cfg(not(target_os = "windows"))]
        let (intent, close_requested) = {
            let bounds = session.picker.desktop_bounds();
            let mut intent = scrozz_ui::picker::paint::Intent::None;
            let mut close_requested = false;
            root.show_viewport_immediate(
                scrozz_ui::picker::viewport_id(),
                scrozz_ui::picker::viewport(bounds),
                |ctx, _class| {
                    close_requested = ctx.input(|input| input.viewport().close_requested());
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE.fill(egui::Color32::TRANSPARENT))
                        .show(ctx, |ui| {
                            intent = scrozz_ui::picker::paint::interact(
                                ui,
                                &mut session.picker,
                                bounds.origin,
                                &session.theme,
                                session.notice.as_deref(),
                            );
                        });
                },
            );
            (intent, close_requested)
        };

        if close_requested || intent == scrozz_ui::picker::paint::Intent::Cancel {
            self.cancel_window_picker(root);
            return;
        }
        if intent != scrozz_ui::picker::paint::Intent::Commit || session.committing {
            return;
        }

        let Some(window) = session.picker.focused_id().cloned() else {
            self.cancel_window_picker(root);
            return;
        };
        session.committing = true;
        session.notice = Some("Checking that window…".to_owned());
        if !self.pipeline.post(Job::CommitWindow {
            card: session.card,
            window,
        }) {
            self.note("the capture worker has gone");
            self.cancel_window_picker(root);
        }
    }

    fn cancel_window_picker(&mut self, root: &egui::Context) {
        if let Some(session) = self.window_picker.take() {
            self.pipeline.cancel_window(session.card);
            scrozz_ui::picker::close_viewports(root, session.picker.displays());
        }
        self.active_window_capture = None;
        self.note("window capture cancelled");
    }

    fn note(&mut self, what: impl Into<String>) {
        let what = what.into();
        tracing::info!("{what}");
        self.notes.push(what);
    }

    fn with_history<R>(&self, f: impl FnOnce(&mut HistoryViewModel) -> R) -> R {
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut history)
    }

    /// The source-image policy currently applied to new captures.
    #[must_use]
    pub fn retention_policy(&self) -> &RetentionPolicy {
        &self.config.retention_policy
    }

    /// Replaces the history worker's live policy and enforces it immediately.
    ///
    /// Returns `false` only if the worker has already stopped.
    #[must_use]
    pub fn set_retention_policy(&mut self, policy: RetentionPolicy) -> bool {
        if self.config.retention_policy == policy {
            return true;
        }
        if !self.pipeline.set_retention_policy(policy.clone()) {
            return false;
        }
        self.config.retention_policy = policy;
        true
    }

    /// Shared state rendered by the secondary history viewport.
    #[must_use]
    pub fn history(&self) -> Arc<Mutex<HistoryViewModel>> {
        Arc::clone(&self.history)
    }

    /// Takes a drag prepared by the worker.
    pub fn take_drag(&mut self) -> Option<PendingDrag> {
        self.pending_drags.pop_front()
    }

    /// Takes a document prepared for the editor host.
    pub fn take_editor(&mut self) -> Option<(CaptureId, Document)> {
        self.pending_editors.pop_front()
    }

    /// Records a native drag, retiring its source only after an accepted drop.
    pub fn drag_finished(&mut self, subject: &DragSubject, outcome: &DragOutcome) {
        self.native_drag_active = false;
        let detail = match outcome {
            DragOutcome::Accepted(_) => "Capture dragged to another app".to_owned(),
            DragOutcome::Rejected => "The destination did not accept the capture".to_owned(),
            DragOutcome::Cancelled => "Drag cancelled".to_owned(),
            DragOutcome::Failed(reason) => format!("Drag failed: {reason}"),
            _ => "Drag finished".to_owned(),
        };
        if let DragSubject::Card(card) = subject {
            self.surface.finish_drag(*card, outcome.is_accepted());
            if outcome.is_accepted() {
                self.pipeline.post(Job::Release(*card));
            }
        }
        if let DragSubject::History(_) = subject {
            self.with_history(|history| history.completed(&detail));
        }
        self.reconcile_escape_binding();
        self.note(detail);
    }

    /// How many cards are on screen.
    #[must_use]
    pub fn showing(&self) -> usize {
        self.surface.len()
    }

    /// Takes a coalesced request to open or focus the settings viewport.
    pub fn take_settings_request(&mut self) -> bool {
        std::mem::take(&mut self.settings_requested)
    }

    /// Monotonically changes after a settings write handled by this process.
    #[must_use]
    pub const fn settings_revision(&self) -> u64 {
        self.settings_revision
    }

    /// Reloads settings that affect live tray and hotkey integration.
    ///
    /// Environment overrides remain authoritative for the lifetime of this
    /// process. Registration failures are reported in the app notes, matching
    /// startup behavior, rather than turning a usable capture app into a crash.
    pub fn reload_settings(&mut self) -> CliResult<()> {
        let store = SettingsStore::load()?;
        let desired = Config::from_settings(&store)?;

        if std::env::var_os(HOTKEYS_ENV).is_none() && desired.bindings != self.config.bindings {
            self.hotkeys.unregister_all();
            for (accelerator, action) in &desired.bindings {
                let hotkey = Hotkey {
                    accelerator: accelerator.clone(),
                };
                match self.hotkeys.register(&hotkey, action.id()) {
                    Ok(()) => self.notes.push(format!("{accelerator} → {}", action.id())),
                    Err(error) => self
                        .notes
                        .push(format!("{accelerator} not rebound: {error}")),
                }
            }
            self.config.bindings = desired.bindings;
        }

        if std::env::var_os(TRAY_ENV).is_none() && desired.tray != self.config.tray {
            if desired.tray {
                match Tray::with_tooltip("Scrozz") {
                    Ok(tray) => {
                        self.notes.push("menu-bar item shown".to_owned());
                        self.tray = Some(tray);
                    }
                    Err(error) => self
                        .notes
                        .push(format!("menu-bar item could not be shown: {error}")),
                }
            } else if let Some(tray) = self.tray.take() {
                tray.close();
                self.notes.push("menu-bar item hidden".to_owned());
            }
            self.config.tray = desired.tray;
        }

        if !self.set_retention_policy(desired.retention_policy) {
            return Err(CoreError::Platform(
                "history worker stopped before accepting the retention policy".to_owned(),
            )
            .into());
        }

        self.settings_revision = self.settings_revision.saturating_add(1);
        Ok(())
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
        self.hotkeys.unregister_all();
        if let Some(tray) = self.tray.take() {
            tray.close();
        }
        self.server = None;
        if let Some(card) = self.active_window_capture.take() {
            self.pipeline.cancel_window(card);
        }
        self.window_picker = None;
        self.pipeline.stop();
    }

    /// Every note recorded so far.
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

fn describe_scroll_progress(progress: &scrozz_stitch::Progress) -> String {
    use scrozz_stitch::Progress;

    match progress {
        Progress::Prepared {
            driver,
            automatic,
            manual_reason,
        } => {
            if *automatic {
                format!("started scrolling with {driver}")
            } else {
                format!(
                    "is ready in manual mode ({driver}); scroll the page to continue: {}",
                    manual_reason
                        .as_deref()
                        .unwrap_or("automatic scrolling is unavailable")
                )
            }
        }
        Progress::FrameCaptured { frame } => format!("captured scrolling frame {frame}"),
        Progress::WaitingForManualScroll => "is waiting for the page to scroll".to_owned(),
        Progress::Advanced {
            frame,
            delta,
            output_height,
            ..
        } => {
            format!("stitched frame {frame} ({delta} px advanced, {output_height} px tall)")
        }
        Progress::Stalled { count } => format!("saw no movement ({count})"),
        Progress::Finished {
            reason,
            frames,
            output_height,
        } => format!(
            "finished scrolling capture ({reason:?}, {frames} frames, {output_height} px tall)"
        ),
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
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::gui::card::{Card, CardId, Recording};

    fn app() -> (App, Recording) {
        let surface = Recording::new();
        let handle = surface.handle();
        let app = App::new(Config::sealed(), Box::new(surface)).expect("a sealed app must start");
        (app, handle)
    }

    fn settings_store(name: &str) -> (std::path::PathBuf, SettingsStore) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "scrozz-gui-config-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let store = SettingsStore::open(directory.join("settings.json")).unwrap();
        (directory, store)
    }

    #[test]
    fn a_sealed_app_touches_nothing() {
        let (app, _) = app();
        assert!(app.tray.is_none(), "no menu-bar item");
        assert!(app.server.is_none(), "no socket");
        assert_eq!(app.config.bindings.len(), 0, "no keyboard registration");
    }

    #[test]
    fn persisted_shortcuts_and_tray_visibility_drive_gui_configuration() {
        let (directory, mut store) = settings_store("persisted");
        store.set("hotkey.capture-region", "Ctrl+Alt+P").unwrap();
        store.set("system.tray-icon", "false").unwrap();

        let config = Config::from_settings(&store).unwrap();
        assert!(!config.tray);
        assert_eq!(config.bindings[0].0, "Ctrl+Alt+P");
        assert_eq!(config.bindings[0].1, Action::Capture(CaptureKind::Region));
        let _ = fs::remove_dir_all(directory);
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
    fn unwired_recording_says_so_rather_than_doing_nothing() {
        let (mut app, _) = app();
        assert_eq!(app.perform(Action::ToggleRecording), Tick::Continue);
        let notes = app.notes().join("\n");
        assert!(notes.contains("recording is not wired up yet"), "{notes}");
    }

    #[test]
    fn opening_settings_requests_the_real_viewport() {
        let (mut app, _) = app();
        assert!(!app.take_settings_request());
        assert_eq!(app.perform(Action::OpenSettings), Tick::Continue);
        assert!(app.take_settings_request());
        assert!(!app.take_settings_request(), "requests coalesce");
    }

    #[test]
    fn opening_history_makes_the_window_visible_and_queues_a_query() {
        let (mut app, _) = app();
        assert_eq!(app.perform(Action::OpenHistory), Tick::Continue);
        let history = app.history();
        let mut history = history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(history.is_visible());
        assert!(matches!(
            history.drain_actions().as_slice(),
            [HistoryAction::Query { request: 1, .. }]
        ));
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
        let card = CardId(42);
        app.surface
            .present(Card::placeholder(card, CaptureKind::Fullscreen))
            .expect("recording never refuses");
        surface.inject(CardEvent::Copy(card));
        app.tick();

        // The worker has no bytes for a card it never captured, so the answer
        // is a refusal — which is the proof the message got there at all.
        for _ in 0..200 {
            app.drain_pipeline();
            if app.notes().iter().any(|n| n.contains("card:42 refused")) {
                assert_eq!(app.showing(), 1, "a failed copy must remain retryable");
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the copy never reached the worker: {:?}", app.notes());
    }

    #[test]
    fn rejected_native_drag_keeps_the_original_card_resident() {
        let (mut app, _) = app();
        app.surface
            .present(Card::placeholder(CardId(5), CaptureKind::Fullscreen))
            .expect("recording never refuses");

        app.drag_finished(&DragSubject::Card(CardId(5)), &DragOutcome::Rejected);

        assert_eq!(app.showing(), 1);
    }

    #[test]
    fn accepted_native_drag_retires_the_original_card() {
        let (mut app, _) = app();
        app.surface
            .present(Card::placeholder(CardId(5), CaptureKind::Fullscreen))
            .expect("recording never refuses");

        app.drag_finished(
            &DragSubject::Card(CardId(5)),
            &DragOutcome::Accepted(scrozz_shell::DragOperation::Copy),
        );

        assert_eq!(app.showing(), 0);
    }

    #[test]
    fn stack_events_are_recorded_not_swallowed() {
        let (mut app, surface) = app();
        surface.inject(CardEvent::DockCollapsed);
        surface.inject(CardEvent::DockExpanded);
        surface.inject(CardEvent::Emptied);
        app.tick();
        let notes = app.notes().join("\n");
        assert!(notes.contains("stack collapsed"), "{notes}");
        assert!(notes.contains("stack expanded"), "{notes}");
        assert!(notes.contains("stack emptied"), "{notes}");
    }

    #[test]
    fn escape_is_bound_only_while_the_stack_has_cards() {
        let (mut app, surface) = app();
        let card = CardId(9);
        app.surface
            .present(Card::placeholder(card, CaptureKind::Fullscreen))
            .expect("recording never refuses");

        app.ensure_escape_binding();
        let escape = Accelerator::parse(OVERLAY_ESCAPE_ACCELERATOR).expect("Escape parses");
        assert!(app.escape_registered);
        assert_eq!(app.hotkeys.action_for(&escape), Some(OVERLAY_ESCAPE_ACTION));

        surface.inject(CardEvent::Dismiss(card));
        surface.inject(CardEvent::Emptied);
        app.drain_cards();

        assert!(!app.escape_registered);
        assert_eq!(app.hotkeys.action_for(&escape), None);
    }

    #[test]
    fn an_escape_registration_retry_resets_after_the_stack_empties() {
        let (mut app, surface) = app();
        app.escape_retry_at = Some(Instant::now() + Duration::from_secs(5));

        surface.inject(CardEvent::Emptied);
        app.drain_cards();

        assert!(app.escape_retry_at.is_none());
    }

    #[test]
    fn restoring_after_emptying_the_stack_reinstates_escape() {
        let (mut app, surface) = app();
        let card = CardId(11);
        app.surface
            .present(Card::placeholder(card, CaptureKind::Fullscreen))
            .expect("recording never refuses");
        app.ensure_escape_binding();

        surface.inject(CardEvent::Dismiss(card));
        surface.inject(CardEvent::Emptied);
        app.drain_cards();
        assert!(!app.escape_registered);

        app.surface
            .present(Card::placeholder(card, CaptureKind::Fullscreen))
            .expect("recording never refuses");
        surface.inject(CardEvent::Restored(card));
        app.drain_cards();

        assert!(app.escape_registered);
    }

    #[test]
    fn hiding_the_stack_releases_escape_until_it_is_visible_again() {
        let (mut app, surface) = app();
        let card = CardId(12);
        app.surface
            .present(Card::placeholder(card, CaptureKind::Fullscreen))
            .expect("recording never refuses");
        app.ensure_escape_binding();
        assert!(app.escape_registered);

        surface.inject(CardEvent::VisibilityChanged { hidden: true });
        app.drain_cards();
        assert!(!app.escape_registered);

        surface.inject(CardEvent::VisibilityChanged { hidden: false });
        app.drain_cards();
        assert!(app.escape_registered);
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
