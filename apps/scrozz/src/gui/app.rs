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
    sync::Arc,
    time::{Duration, Instant},
};

use scrozz_core::{Capture, Frame, SelectionCapabilities};
use scrozz_shell::{
    Accelerator, Capability, GlobalHotkeys, KeyState, Permissions, ScreenshotSound, Session,
    SystemPermissions, Tray, TrayAction,
    hotkey::{DesiredBinding, Rejection},
    play_screenshot_sound,
};

use crate::{
    cli::Cli,
    fault::{CliError, CliResult},
    gui::{
        action::{Action, CaptureKind},
        card::{CardEvent, CardId, CardSurface},
        pipeline::{Job, Outcome, Pipeline},
        selection::CaptureSelector,
        server::{Forwarder, Server},
    },
    json::Json,
    report::Report,
    shortcuts::{ShortcutAction, ShortcutStore, Shortcuts},
};
use scrozz_ui::settings::{ShortcutEdit, ShortcutRow};

/// The binding table a set of configured shortcuts asks for.
///
/// The defaults themselves live in [`crate::shortcuts`], which is also what the
/// settings pane edits and what the CLI reports, so there is exactly one table
/// and the three surfaces cannot disagree about what `Cmd+Shift+8` does.
#[must_use]
pub fn bindings_from(shortcuts: &Shortcuts) -> Vec<(String, Action)> {
    ShortcutAction::ALL
        .into_iter()
        .filter_map(|shortcut| {
            shortcuts
                .get(shortcut)
                .map(|accelerator| (accelerator.to_owned(), shortcut.action()))
        })
        .collect()
}

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
    /// The configured shortcuts, as the settings pane shows them.
    ///
    /// Kept beside `bindings` rather than replacing it because the two answer
    /// different questions: this is what the user chose, `bindings` is what will
    /// actually be registered on this run, and `SCROZZ_GUI_HOTKEYS` can make them
    /// differ.
    pub shortcuts: Shortcuts,
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
    /// Audible feedback after a successful screenshot.
    pub screenshot_sound: ScreenshotSound,
}

impl Default for Config {
    fn default() -> Self {
        let shortcuts = Shortcuts::default();
        Self {
            bindings: bindings_from(&shortcuts),
            shortcuts,
            tray: true,
            ipc: true,
            deadline: None,
            capture_on_start: None,
            screenshot_sound: ScreenshotSound::EightBit,
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

        // What the user stored wins over what this build ships with. A missing
        // or unreadable file is the first run, which is the defaults.
        config.shortcuts = crate::settings::stored_shortcuts();
        config.bindings = bindings_from(&config.shortcuts);

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
            config.capture_on_start = Self::capture_on_start(&raw);
        }

        config
    }

    fn capture_on_start(raw: &str) -> Option<CaptureKind> {
        match raw {
            "0" | "false" | "no" | "" => None,
            "all-in-one" => Some(CaptureKind::AllInOne),
            "region" => Some(CaptureKind::Region),
            "window" => Some(CaptureKind::Window),
            "1" | "fullscreen" => Some(CaptureKind::Fullscreen),
            "all-displays" => Some(CaptureKind::AllDisplays),
            _ => None,
        }
    }

    /// A configuration that touches nothing outside this process.
    ///
    /// No menu-bar item, no keyboard registration, no socket. What tests use,
    /// and what makes them safe to run on a machine someone is working on.
    #[must_use]
    pub fn sealed() -> Self {
        Self {
            shortcuts: Shortcuts::default(),
            bindings: Vec::new(),
            tray: false,
            ipc: false,
            deadline: Some(Duration::from_millis(250)),
            capture_on_start: None,
            screenshot_sound: ScreenshotSound::Off,
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

/// The rows worth asking the OS for, recording why the others were dropped.
///
/// A shortcut for something this session cannot do is not a failure to report to
/// the user as a hotkey problem — the hotkey is fine, the capability is missing —
/// so it is filtered out here and explained in its own words.
fn wanted(
    bindings: &[(String, Action)],
    available: &dyn Fn(Action) -> bool,
    notes: &mut Vec<String>,
) -> Vec<DesiredBinding> {
    let mut desired = Vec::new();
    for (accelerator, action) in bindings {
        if available(*action) {
            desired.push(DesiredBinding::new(accelerator.clone(), action.id()));
        } else {
            notes.push(format!(
                "{accelerator} not bound: {} is unavailable in this session",
                action.id()
            ));
        }
    }
    desired
}

/// Registers as much of a set as the system will accept.
///
/// [`GlobalHotkeys::apply`] is all-or-nothing on purpose, which is what an edit
/// wants and what startup does not: a file written months ago may name a
/// combination some newly installed application has since taken, and refusing to
/// bind anything because of it would be a puzzling total failure. So a rejected
/// row is reported and dropped, and the rest is retried.
fn install_forgivingly(
    hotkeys: &mut GlobalHotkeys,
    mut desired: Vec<DesiredBinding>,
    notes: &mut Vec<String>,
) {
    for _ in 0..desired.len().max(1) {
        if desired.is_empty() {
            return;
        }
        match hotkeys.apply(&desired) {
            Ok(()) => {
                for want in &desired {
                    notes.push(format!("{} → {}", want.accelerator, want.action));
                }
                return;
            }
            Err(rejections) => {
                for rejection in &rejections {
                    // Wayland answers with the compositor config line to paste,
                    // which is the actual remedy (D11); keeping the whole message
                    // is the point of reporting it at all.
                    notes.push(format!(
                        "{} not bound: {}",
                        rejection.accelerator, rejection.reason
                    ));
                }
                desired.retain(|want| {
                    !rejections
                        .iter()
                        .any(|rejection| rejection.action == want.action)
                });
            }
        }
    }
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
    /// Which surfaces currently hold the keyboard, as a bit per
    /// [`KeyboardOwner`]. See [`App::set_keyboard_owner`].
    keyboard_owners: u8,
    server: Option<Server>,
    forwarder: Option<Forwarder>,
    selector: Arc<dyn CaptureSelector>,
    started: Instant,
    captures: u64,
    sound_warning_shown: bool,
    settings_requested: bool,
    editor_request: Option<EditorRequest>,
    notes: Vec<String>,
    shortcuts: Shortcuts,
    shortcut_store: Option<ShortcutStore>,
    rejected: Vec<(ShortcutAction, String)>,
    session: Session,
    selection_capabilities: SelectionCapabilities,
    capture_backend_ready: bool,
}

/// A capture the annotation editor has been asked to open.
#[derive(Debug)]
pub struct EditorRequest {
    /// The card it came from, so copy and save can be attributed and the
    /// finished image can be sent back through the same worker.
    pub card: CardId,
    /// The decoded capture.
    pub capture: Capture,
}

/// A surface that can take the keyboard away from the global hotkeys.
///
/// See [`App::set_keyboard_owner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyboardOwner {
    /// The annotation editor window.
    Editor,
    /// A shortcut row in Settings that is armed and waiting for a chord.
    ShortcutRecorder,
}

impl KeyboardOwner {
    const fn bit(self) -> u8 {
        match self {
            Self::Editor => 1 << 0,
            Self::ShortcutRecorder => 1 << 1,
        }
    }
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
    pub fn new(
        config: Config,
        surface: Box<dyn CardSurface>,
        selector: Arc<dyn CaptureSelector>,
    ) -> CliResult<Self> {
        let pipeline = Pipeline::start(Arc::clone(&selector))?;
        let mut notes = Vec::new();
        let session = Session::detect();
        let selection_capabilities = selector.capabilities();
        let capture_backend_ready = crate::platform::capture_backend_is_ready();
        let action_available = |action: Action| {
            action.is_available(selection_capabilities, &session, capture_backend_ready)
        };

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
        let forwarder = server
            .as_ref()
            .map(|_| Forwarder::start(Arc::clone(&selector)))
            .transpose()?;

        let tray = if config.tray {
            match Tray::with_tooltip_and_availability("Scrozz", |entry| {
                action_available(Action::from_tray(entry))
            }) {
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

        let desired = wanted(&config.bindings, &action_available, &mut notes);

        let mut hotkeys = if desired.is_empty() {
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

        // Startup is forgiving where an edit is not: one unusable row in a file
        // that has been sitting on disk for months must not cost the user every
        // other shortcut they rely on. Each casualty is recorded so the reason
        // is visible in the panel rather than merely felt.
        install_forgivingly(&mut hotkeys, desired, &mut notes);

        let shortcut_store = match ShortcutStore::default_location() {
            Ok(store) => Some(store),
            Err(err) => {
                notes.push(format!("shortcut changes cannot be saved: {err}"));
                None
            }
        };

        let shortcuts = config.shortcuts.clone();
        let mut app = Self {
            config,
            surface,
            pipeline,
            tray,
            hotkeys,
            keyboard_owners: 0,
            server,
            forwarder,
            selector,
            started: Instant::now(),
            captures: 0,
            sound_warning_shown: false,
            settings_requested: false,
            editor_request: None,
            notes,
            shortcuts,
            shortcut_store,
            rejected: Vec::new(),
            session,
            selection_capabilities,
            capture_backend_ready,
        };
        app.refresh_tray_shortcuts();

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

    /// Steps the global hotkeys aside while something else owns the keyboard.
    ///
    /// Two surfaces need this, and they can be up at the same time. The
    /// annotation editor binds ⌘C, ⌘S, ⌘Z and more, any of which the user may
    /// also have bound to a capture. The shortcut recorder in Settings has the
    /// sharper problem: its whole job is to *observe* a combination, so a global
    /// hotkey firing on the very keystroke being recorded would take a
    /// screenshot instead of recording it.
    ///
    /// Reserving a fixed list would be wrong twice over — the editor's
    /// accelerators are not the only ones that can collide, and the capture
    /// shortcuts are configurable, so the collision set is not known until
    /// runtime. Suspending the whole set is the only rule that stays true
    /// whatever either side is bound to.
    ///
    /// Ownership is a set rather than a flag because the two overlap: opening
    /// the editor from the settings window while a row is armed must not hand
    /// the keyboard back when the editor closes. The keys return when *nobody*
    /// holds them.
    ///
    /// The bindings themselves are untouched throughout, so the menu-bar item
    /// keeps showing the right shortcut beside each command, and a shortcut
    /// edited while suspended is the one that comes back.
    pub fn set_keyboard_owner(&mut self, owner: KeyboardOwner, owns: bool) {
        let before = self.keyboard_owners;
        self.keyboard_owners = if owns {
            before | owner.bit()
        } else {
            before & !owner.bit()
        };
        if self.keyboard_owners == before {
            return;
        }

        if self.keyboard_owners == 0 {
            match self.hotkeys.resume() {
                Ok(()) => tracing::debug!("global hotkeys resumed"),
                Err(rejections) => {
                    // Kept, not dropped: a combination another application took
                    // while we had it released is still what the user
                    // configured, and still what Settings should show.
                    for rejection in &rejections {
                        tracing::warn!(
                            action = %rejection.action,
                            accelerator = %rejection.accelerator,
                            reason = %rejection.reason,
                            "a global hotkey could not be re-grabbed"
                        );
                    }
                    self.note(format!(
                        "{} shortcut(s) could not be restored",
                        rejections.len()
                    ));
                }
            }
        } else {
            self.hotkeys.suspend();
        }
    }

    /// Whether the global hotkeys are currently stood down.
    #[must_use]
    pub const fn hotkeys_suspended(&self) -> bool {
        self.hotkeys.is_suspended()
    }

    /// Whether `owner` currently holds the keyboard.
    #[must_use]
    pub const fn owns_keyboard(&self, owner: KeyboardOwner) -> bool {
        self.keyboard_owners & owner.bit() != 0
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
            let submitted = self
                .forwarder
                .as_ref()
                .is_some_and(|forwarder| forwarder.submit(request));
            if !submitted {
                self.note("the forwarded-command worker has gone");
            }
        }

        let mut completed = Vec::new();
        if let Some(forwarder) = &self.forwarder {
            while let Some(command) = forwarder.poll() {
                completed.push(command);
            }
        }
        for command in completed.into_iter().flatten() {
            self.captures += u64::from(matches!(command, crate::cli::Command::Capture(_)));
            if matches!(command, crate::cli::Command::Gui) {
                // A second `scrozz gui` means "show yourself", not "start
                // again". There is nothing to show yet, so it is a no-op
                // that has at least been answered rather than ignored.
                self.note("a second launch was answered by this instance");
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
                Outcome::Ready(card) => {
                    self.captures += 1;
                    if let Err(error) = play_screenshot_sound(&self.config.screenshot_sound) {
                        let fell_back = !matches!(
                            self.config.screenshot_sound,
                            ScreenshotSound::Shutter | ScreenshotSound::Off
                        );
                        if fell_back {
                            let _ = play_screenshot_sound(&ScreenshotSound::Shutter);
                        }
                        if !self.sound_warning_shown {
                            self.sound_warning_shown = true;
                            self.note(if fell_back {
                                format!(
                                    "the screenshot succeeded, but its selected sound failed; using the default this session: {error}"
                                )
                            } else {
                                format!(
                                    "the screenshot succeeded, but its sound could not play: {error}"
                                )
                            });
                        }
                    }
                    let summary = card.summary();
                    if let Err(err) = self.surface.present(*card) {
                        self.note(format!("a card could not be shown: {err}"));
                    } else {
                        self.note(summary);
                    }
                }
                Outcome::Failed { card, error } => {
                    self.note(format!("{card} failed: {error}"));
                }
                Outcome::Done { card, detail } => {
                    self.note(format!("{card} {detail}"));
                }
                Outcome::Opened { card, capture } => {
                    self.editor_request = Some(EditorRequest {
                        card,
                        capture: *capture,
                    });
                }
                Outcome::Refused { card, error } => {
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
                CardEvent::Open(id) => {
                    // Decoding happens on the worker, so the click that opens
                    // the editor never inflates a 6K PNG on the UI thread.
                    self.pipeline.post(Job::Open(id));
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

    /// The shortcuts as configured, for the settings pane to edit.
    #[must_use]
    pub fn shortcuts(&self) -> &Shortcuts {
        &self.shortcuts
    }

    /// The editable shortcut table, as the settings pane needs to see it.
    #[must_use]
    pub fn shortcut_rows(&self) -> Vec<ShortcutRow> {
        let problems = self.shortcuts.problems();
        ShortcutAction::ALL
            .into_iter()
            .map(|action| {
                let accelerator = self.shortcuts.get(action).unwrap_or_default();
                // A stored value that will not parse is shown back verbatim,
                // because "that is not a combination" is only useful next to what
                // the user actually typed.
                let symbols = Accelerator::parse(accelerator)
                    .map_or_else(|_| accelerator.to_owned(), |parsed| parsed.symbols());
                let problem = problems
                    .iter()
                    .find(|(offender, _)| *offender == action)
                    .map(|(_, why)| why.to_string())
                    .or_else(|| {
                        self.rejected
                            .iter()
                            .find(|(offender, _)| *offender == action)
                            .map(|(_, why)| why.clone())
                    });
                ShortcutRow {
                    id: action.id().to_owned(),
                    label: action.label().to_owned(),
                    accelerator: accelerator.to_owned(),
                    symbols,
                    is_default: self.shortcuts.is_default(action),
                    usable: self.shortcut_is_usable(action),
                    problem,
                }
            })
            .collect()
    }

    /// Applies what the settings pane asked for, reporting anything refused.
    ///
    /// Validation happens on a *copy*, so a rejected edit leaves both the live
    /// registrations and the file on disk exactly as they were — the alternative,
    /// mutating first and rolling back on failure, is where "my other shortcuts
    /// stopped working" comes from.
    pub fn edit_shortcuts(&mut self, edits: &[ShortcutEdit]) {
        if edits.is_empty() {
            return;
        }
        let mut candidate = self.shortcuts.clone();
        let mut touched = Vec::new();
        for edit in edits {
            match edit {
                ShortcutEdit::ResetAll => {
                    candidate.reset_all();
                    touched.extend(ShortcutAction::ALL);
                }
                ShortcutEdit::Set { id, accelerator } => {
                    let Some(action) = ShortcutAction::from_id(id) else {
                        continue;
                    };
                    candidate.set(action, Some(accelerator));
                    touched.push(action);
                }
                ShortcutEdit::Clear { id } => {
                    let Some(action) = ShortcutAction::from_id(id) else {
                        continue;
                    };
                    candidate.set(action, None);
                    touched.push(action);
                }
                ShortcutEdit::Reset { id } => {
                    let Some(action) = ShortcutAction::from_id(id) else {
                        continue;
                    };
                    candidate.reset(action);
                    touched.push(action);
                }
            }
        }

        // Each touched row is checked against the whole candidate table rather
        // than reading `problems()` and filtering: a duplicate is a property of
        // the *pair*, and `problems()` blames whichever row it reaches second.
        // Filtering that by "rows this edit touched" silently dropped the
        // collision whenever the row being edited happened to sort first, and
        // let the duplicate through. `check` skips the row it is checking, so it
        // blames the row the user actually just changed and names the other one.
        //
        // A duplicate already sitting in the file is still not this edit's
        // fault; untouched rows are left to `shortcut_rows` to mark.
        let mut problems = Vec::new();
        for action in &touched {
            let Some(raw) = candidate.get(*action) else {
                continue;
            };
            if let Err(problem) = candidate.check(*action, raw) {
                problems.push((*action, problem));
            }
        }
        if !problems.is_empty() {
            self.rejected = problems
                .iter()
                .map(|(action, why)| (*action, why.to_string()))
                .collect();
            for (action, why) in &problems {
                self.note(format!("{action} not changed: {why}"));
            }
            return;
        }

        match self.apply_shortcuts(&candidate) {
            Ok(()) => self.rejected.clear(),
            Err(rejections) => {
                self.rejected = rejections
                    .iter()
                    .filter_map(|rejection| {
                        Some((
                            ShortcutAction::from_id(&rejection.action)?,
                            rejection.reason.clone(),
                        ))
                    })
                    .collect();
                for rejection in &rejections {
                    self.note(format!(
                        "{} not bound: {}",
                        rejection.accelerator, rejection.reason
                    ));
                }
            }
        }
    }

    /// Whether an action can be triggered at all in this session.
    ///
    /// The pane greys out a row rather than hiding it: an unbindable action that
    /// simply vanished would look like a bug in the settings window rather than a
    /// capability this platform does not have.
    #[must_use]
    pub fn shortcut_is_usable(&self, shortcut: ShortcutAction) -> bool {
        shortcut.action().is_available(
            self.selection_capabilities,
            &self.session,
            self.capture_backend_ready,
        )
    }

    /// Puts an edited set of shortcuts in force, saving it if it takes.
    ///
    /// Atomic by construction: [`GlobalHotkeys::apply`] validates the whole set
    /// before touching the OS and restores the previous registrations if the OS
    /// refuses one, so a rejected edit leaves the shortcuts that were working
    /// still working — and, because nothing is written until registration
    /// succeeds, the file on disk keeps matching what the keyboard does.
    ///
    /// # Errors
    ///
    /// Returns one [`Rejection`] per offending row so the pane can mark each of
    /// them, rather than reporting only the first and sending the user round the
    /// loop again.
    pub fn apply_shortcuts(&mut self, next: &Shortcuts) -> std::result::Result<(), Vec<Rejection>> {
        let session = self.session.clone();
        let capabilities = self.selection_capabilities;
        let ready = self.capture_backend_ready;
        let available = move |action: Action| action.is_available(capabilities, &session, ready);

        let mut skipped = Vec::new();
        let desired = wanted(&bindings_from(next), &available, &mut skipped);

        self.hotkeys.apply(&desired)?;

        self.shortcuts = next.clone();
        self.config.shortcuts = next.clone();
        self.config.bindings = bindings_from(next);
        for note in skipped {
            self.note(note);
        }
        for want in &desired {
            self.note(format!("{} → {}", want.accelerator, want.action));
        }

        // Saving last, and only on success, so the stored set is always one the
        // app was able to put in force.
        if let Some(store) = self.shortcut_store.clone()
            && let Err(err) = store.save(next)
        {
            self.note(format!("shortcuts not saved: {err}"));
        }
        self.refresh_tray_shortcuts();
        Ok(())
    }

    /// Relabels the menu with the shortcuts that are actually registered.
    ///
    /// Reads back from the registrar rather than from the configured set: a menu
    /// naming a combination the app is not listening for is worse than a menu
    /// naming none, because the user blames the key rather than the binding.
    fn refresh_tray_shortcuts(&mut self) {
        let Some(tray) = self.tray.as_ref() else {
            return;
        };
        let live: Vec<(TrayAction, String)> = self
            .hotkeys
            .bindings()
            .filter_map(|(accelerator, action)| {
                let shortcut = ShortcutAction::from_id(action)?;
                Some((shortcut.tray(), accelerator.symbols()))
            })
            .collect();
        tray.set_shortcuts(live);
    }

    /// Takes a pending request to open or focus Settings.
    pub fn take_settings_request(&mut self) -> bool {
        std::mem::take(&mut self.settings_requested)
    }

    /// Takes a pending request to open the annotation editor.
    pub fn take_editor_request(&mut self) -> Option<EditorRequest> {
        self.editor_request.take()
    }

    /// Copies an image the editor has flattened.
    ///
    /// Routed through the worker so the PNG encode and the clipboard write stay
    /// off the UI thread, exactly like a card's own copy.
    pub fn copy_rendered(&mut self, card: CardId, frame: Frame) {
        self.pipeline.post(Job::CopyImage {
            card,
            frame: Box::new(frame),
        });
    }

    /// Saves an image the editor has flattened.
    pub fn save_rendered(&mut self, card: CardId, frame: Frame) {
        self.pipeline.post(Job::SaveImage {
            card,
            frame: Box::new(frame),
        });
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
        self.hotkeys.unregister_all();
        if let Some(tray) = self.tray.take() {
            tray.close();
        }
        self.server = None;
        self.selector.cancel();
        if let Some(mut forwarder) = self.forwarder.take() {
            forwarder.stop();
        }
        self.pipeline.stop();
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
    use crate::gui::selection::UnsupportedSelector;

    fn app() -> (App, Recording) {
        let surface = Recording::new();
        let handle = surface.handle();
        let app = App::new(
            Config::sealed(),
            Box::new(surface),
            Arc::new(UnsupportedSelector::headless()),
        )
        .expect("a sealed app must start");
        (app, handle)
    }

    /// A sealed app with a shortcut set it can actually edit.
    ///
    /// `Config::sealed` deliberately binds nothing, so the edit path has to be
    /// given a starting set explicitly rather than inheriting one.
    fn app_with_shortcuts(shortcuts: Shortcuts) -> App {
        let (mut app, _) = app();
        app.shortcuts = shortcuts.clone();
        app.config.shortcuts = shortcuts;
        app
    }

    #[test]
    fn a_recorded_combination_replaces_the_old_one() {
        let mut app = app_with_shortcuts(Shortcuts::default());
        app.edit_shortcuts(&[ShortcutEdit::Set {
            id: ShortcutAction::CaptureRegion.id().to_owned(),
            accelerator: "Ctrl+Shift+F9".to_owned(),
        }]);
        assert_eq!(
            app.shortcuts.get(ShortcutAction::CaptureRegion),
            Some("Ctrl+Shift+F9")
        );
    }

    #[test]
    fn a_duplicate_is_refused_and_changes_nothing() {
        // The failure this prevents: an edit that half-lands, leaving the file
        // and the keyboard disagreeing about what is bound.
        let mut app = app_with_shortcuts(Shortcuts::default());
        let taken = app
            .shortcuts
            .get(ShortcutAction::CaptureWindow)
            .expect("window ships bound")
            .to_owned();
        let before = app
            .shortcuts
            .get(ShortcutAction::CaptureRegion)
            .map(str::to_owned);

        app.edit_shortcuts(&[ShortcutEdit::Set {
            id: ShortcutAction::CaptureRegion.id().to_owned(),
            accelerator: taken,
        }]);

        assert_eq!(
            app.shortcuts
                .get(ShortcutAction::CaptureRegion)
                .map(str::to_owned),
            before,
            "a refused edit must leave the row exactly as it was"
        );
        let row = app
            .shortcut_rows()
            .into_iter()
            .find(|row| row.id == ShortcutAction::CaptureRegion.id())
            .expect("region is always listed");
        assert!(
            row.problem.is_some(),
            "the user must be told why nothing happened"
        );
    }

    #[test]
    fn clearing_then_resetting_returns_the_shipped_combination() {
        let mut app = app_with_shortcuts(Shortcuts::default());
        let shipped = ShortcutAction::CaptureRegion.default_accelerator();

        app.edit_shortcuts(&[ShortcutEdit::Clear {
            id: ShortcutAction::CaptureRegion.id().to_owned(),
        }]);
        assert_eq!(app.shortcuts.get(ShortcutAction::CaptureRegion), None);

        app.edit_shortcuts(&[ShortcutEdit::Reset {
            id: ShortcutAction::CaptureRegion.id().to_owned(),
        }]);
        assert_eq!(app.shortcuts.get(ShortcutAction::CaptureRegion), shipped);
    }

    #[test]
    fn reset_all_undoes_every_change_at_once() {
        let mut shortcuts = Shortcuts::default();
        shortcuts.set(ShortcutAction::CaptureRegion, Some("Ctrl+Shift+F9"));
        shortcuts.set(ShortcutAction::CaptureWindow, None);
        let mut app = app_with_shortcuts(shortcuts);

        app.edit_shortcuts(&[ShortcutEdit::ResetAll]);
        assert!(app.shortcuts.is_all_default());
    }

    #[test]
    fn an_unparseable_recording_is_refused_with_a_reason() {
        let mut app = app_with_shortcuts(Shortcuts::default());
        app.edit_shortcuts(&[ShortcutEdit::Set {
            id: ShortcutAction::CaptureRegion.id().to_owned(),
            accelerator: "Ctrl+Shift+NotAKey".to_owned(),
        }]);
        let row = app
            .shortcut_rows()
            .into_iter()
            .find(|row| row.id == ShortcutAction::CaptureRegion.id())
            .expect("region is always listed");
        assert!(row.problem.is_some());
        assert_eq!(
            app.shortcuts.get(ShortcutAction::CaptureRegion),
            ShortcutAction::CaptureRegion.default_accelerator(),
            "a rejected recording must not overwrite the working combination"
        );
    }

    #[test]
    fn every_action_gets_a_row_whether_or_not_it_is_usable() {
        // Hiding an unavailable action would read as a missing feature rather
        // than an unavailable one.
        let app = app_with_shortcuts(Shortcuts::default());
        let rows = app.shortcut_rows();
        assert_eq!(rows.len(), ShortcutAction::ALL.len());
        for action in ShortcutAction::ALL {
            assert!(rows.iter().any(|row| row.id == action.id()));
        }
    }

    #[test]
    fn a_row_shows_the_platform_spelling_of_its_combination() {
        let app = app_with_shortcuts(Shortcuts::default());
        let row = app
            .shortcut_rows()
            .into_iter()
            .find(|row| row.id == ShortcutAction::CaptureRegion.id())
            .expect("region is always listed");
        let expected = Accelerator::parse(&row.accelerator)
            .expect("the shipped default parses")
            .symbols();
        assert_eq!(row.symbols, expected);
        if cfg!(target_os = "macos") {
            assert!(
                !row.symbols.contains('+'),
                "a macOS label spells modifiers as glyphs: {}",
                row.symbols
            );
        }
    }

    #[test]
    fn a_new_app_has_its_hotkeys_live() {
        let (app, _) = app();
        assert!(!app.hotkeys_suspended());
    }

    #[test]
    fn opening_the_editor_stands_the_hotkeys_down() {
        let (mut app, _) = app();

        app.set_keyboard_owner(KeyboardOwner::Editor, true);
        assert!(
            app.hotkeys_suspended(),
            "the editor's own accelerators must be the only ones that fire"
        );

        app.set_keyboard_owner(KeyboardOwner::Editor, false);
        assert!(
            !app.hotkeys_suspended(),
            "closing the editor gives the capture shortcuts back"
        );
    }

    #[test]
    fn the_editor_lifecycle_is_idempotent() {
        let (mut app, _) = app();

        // A per-frame sync calls this with the same answer over and over.
        for _ in 0..3 {
            app.set_keyboard_owner(KeyboardOwner::Editor, true);
        }
        assert!(app.hotkeys_suspended());
        for _ in 0..3 {
            app.set_keyboard_owner(KeyboardOwner::Editor, false);
        }
        assert!(!app.hotkeys_suspended());
    }

    #[test]
    fn the_recorder_stands_the_hotkeys_down_on_its_own() {
        // Pressing an existing binding while arming a new one must record it,
        // not fire a capture.
        let (mut app, _) = app();

        app.set_keyboard_owner(KeyboardOwner::ShortcutRecorder, true);
        assert!(app.hotkeys_suspended());

        app.set_keyboard_owner(KeyboardOwner::ShortcutRecorder, false);
        assert!(!app.hotkeys_suspended());
    }

    #[test]
    fn two_owners_overlapping_keep_the_keyboard_until_both_let_go() {
        // Settings can open the editor, so the two claims genuinely overlap.
        // Releasing one while the other still holds it would hand the shortcuts
        // back underneath whoever is left.
        let (mut app, _) = app();

        app.set_keyboard_owner(KeyboardOwner::ShortcutRecorder, true);
        app.set_keyboard_owner(KeyboardOwner::Editor, true);
        assert!(app.hotkeys_suspended());

        app.set_keyboard_owner(KeyboardOwner::ShortcutRecorder, false);
        assert!(app.hotkeys_suspended(), "the editor still has the keyboard");

        app.set_keyboard_owner(KeyboardOwner::Editor, false);
        assert!(
            !app.hotkeys_suspended(),
            "nobody is left holding it, so the shortcuts come back"
        );
    }

    #[test]
    fn releasing_in_the_other_order_works_too() {
        let (mut app, _) = app();

        app.set_keyboard_owner(KeyboardOwner::Editor, true);
        app.set_keyboard_owner(KeyboardOwner::ShortcutRecorder, true);

        app.set_keyboard_owner(KeyboardOwner::Editor, false);
        assert!(app.hotkeys_suspended(), "the recorder still has it");

        app.set_keyboard_owner(KeyboardOwner::ShortcutRecorder, false);
        assert!(!app.hotkeys_suspended());
    }

    #[test]
    fn each_owner_is_tracked_separately() {
        let (mut app, _) = app();

        app.set_keyboard_owner(KeyboardOwner::Editor, true);
        assert!(app.owns_keyboard(KeyboardOwner::Editor));
        assert!(!app.owns_keyboard(KeyboardOwner::ShortcutRecorder));

        app.set_keyboard_owner(KeyboardOwner::ShortcutRecorder, true);
        assert!(app.owns_keyboard(KeyboardOwner::Editor));
        assert!(app.owns_keyboard(KeyboardOwner::ShortcutRecorder));
    }

    #[test]
    fn releasing_an_owner_that_never_held_it_changes_nothing() {
        // The per-frame sync passes `false` on every frame the editor is shut.
        let (mut app, _) = app();

        app.set_keyboard_owner(KeyboardOwner::Editor, true);
        for _ in 0..3 {
            app.set_keyboard_owner(KeyboardOwner::ShortcutRecorder, false);
        }
        assert!(
            app.hotkeys_suspended(),
            "a release from a non-owner released somebody else's claim"
        );
    }

    #[test]
    fn suspension_does_not_disturb_the_configuration() {
        let (mut app, _) = app();
        let before = app.config.bindings.len();

        app.set_keyboard_owner(KeyboardOwner::Editor, true);

        assert_eq!(
            app.config.bindings.len(),
            before,
            "the configured shortcuts are what the tray and settings read"
        );
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
    fn an_unwired_action_says_so_rather_than_doing_nothing() {
        let (mut app, _) = app();
        for action in [Action::ToggleRecording, Action::OpenHistory] {
            assert_eq!(app.perform(action), Tick::Continue);
        }
        let notes = app.notes().join("\n");
        assert!(notes.contains("recording is not wired up yet"), "{notes}");
        assert!(notes.contains("history window"), "{notes}");
    }

    #[test]
    fn settings_requests_are_delivered_once() {
        let (mut app, _) = app();
        assert!(!app.take_settings_request());
        assert_eq!(app.perform(Action::OpenSettings), Tick::Continue);
        assert!(app.take_settings_request());
        assert!(!app.take_settings_request());
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
        for (accelerator, _) in bindings_from(&Shortcuts::default()) {
            let conflict = describe_conflict(&accelerator)
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
        //
        // The probes come from the platform's own table rather than a literal
        // list, because that table *is* platform-specific: `Cmd+Shift+3` is
        // reserved on macOS and completely free on Windows and Linux. Hard-coding
        // the macOS spellings made this pass on the authoring machine and fail on
        // the Linux runner, which was a defect in the test, not in the code.
        let reserved = scrozz_shell::hotkey::reserved_shortcuts();
        assert!(
            !reserved.is_empty(),
            "every platform must declare the combinations it has already taken"
        );

        for shortcut in reserved {
            let conflict = describe_conflict(shortcut.accelerator).unwrap_or_else(|e| {
                panic!(
                    "the reserved table entry {} should parse: {e}",
                    shortcut.accelerator
                )
            });
            assert!(
                conflict.is_some(),
                "scrozz-shell lists {} as owned by {}, so describe_conflict must say so",
                shortcut.accelerator,
                shortcut.owner
            );
        }
    }

    #[test]
    fn every_default_binding_names_a_real_action() {
        for (_, action) in bindings_from(&Shortcuts::default()) {
            assert_eq!(Action::from_id(action.id()), Some(action));
        }
    }

    #[test]
    fn the_default_bindings_are_the_configured_shortcuts() {
        // The two used to be separate literals, and drifted: the settings said
        // `Super+Shift+4` while macOS was actually listening on `Cmd+Shift+8`.
        let bindings = bindings_from(&Shortcuts::default());
        for shortcut in ShortcutAction::ALL {
            match shortcut.default_accelerator() {
                Some(expected) => assert!(
                    bindings
                        .iter()
                        .any(|(accelerator, action)| accelerator == expected
                            && *action == shortcut.action()),
                    "{shortcut:?} defaults to {expected} but is not in the binding table"
                ),
                None => assert!(
                    !bindings
                        .iter()
                        .any(|(_, action)| *action == shortcut.action()),
                    "{shortcut:?} ships unassigned and must not be registered"
                ),
            }
        }
    }

    #[test]
    fn an_unassigned_shortcut_is_simply_not_registered() {
        let mut shortcuts = Shortcuts::default();
        shortcuts.set(ShortcutAction::CaptureRegion, None);
        let bindings = bindings_from(&shortcuts);
        assert!(
            !bindings
                .iter()
                .any(|(_, action)| *action == Action::Capture(CaptureKind::Region)),
            "clearing a row must remove it from the table, not blank its accelerator"
        );
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
    fn documented_startup_capture_value_means_fullscreen() {
        assert_eq!(Config::capture_on_start("1"), Some(CaptureKind::Fullscreen));
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
