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

use std::time::{Duration, Instant};

use scrozz_core::{CaptureRequest, CaptureTarget, CursorMode, ScrollAxis};
use scrozz_shell::{
    Accelerator, Capability, GlobalHotkeys, Hotkey, HotkeyManager, KeyState, Permissions,
    SystemPermissions, Tray, TrayAction,
};
use scrozz_stitch::{CancelAction, Progress};
use scrozz_ui::{ScrollHudAction, ScrollHudState, ScrollHudStatus};

use crate::{
    cli::Cli,
    commands::{ScrollingTarget, wayland_portal_picker_target},
    fault::{CliError, CliResult},
    gui::{
        action::{Action, CaptureKind},
        card::{CardEvent, CardId, CardSurface},
        pipeline::{Job, Outcome, Pipeline},
        server::Server,
    },
    json::Json,
    platform,
    report::Report,
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
                "scrolling" => Some(CaptureKind::Scrolling),
                _ => Some(CaptureKind::Fullscreen),
            };
        }

        config
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
    scroll_hud: Option<ScrollHudState>,
    scrolling_card: Option<CardId>,
    scrolling_target: Option<ScrollingTarget>,
    scrolling_ready: Option<Box<Card>>,
    scrolling_abort_pending: Option<CardId>,
    scrolling_target_resolver: Box<dyn Fn() -> CliResult<ScrollingTarget>>,
    scrolling_target_refresher: Box<dyn Fn(ScrollingTarget) -> CliResult<ScrollingTarget>>,
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
        Self::new_with_scrolling_target_handlers(
            config,
            surface,
            Box::new(snapshot_scrolling_target),
            Box::new(refresh_scrolling_target),
        )
    }

    fn new_with_scrolling_target_resolver(
        config: Config,
        surface: Box<dyn CardSurface>,
        scrolling_target_resolver: Box<dyn Fn() -> CliResult<ScrollingTarget>>,
    ) -> CliResult<Self> {
        Self::new_with_scrolling_target_handlers(
            config,
            surface,
            scrolling_target_resolver,
            Box::new(Ok),
        )
    }

    fn new_with_scrolling_target_handlers(
        config: Config,
        surface: Box<dyn CardSurface>,
        scrolling_target_resolver: Box<dyn Fn() -> CliResult<ScrollingTarget>>,
        scrolling_target_refresher: Box<dyn Fn(ScrollingTarget) -> CliResult<ScrollingTarget>>,
    ) -> CliResult<Self> {
        let pipeline = Pipeline::start()?;
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
            scroll_hud: None,
            scrolling_card: None,
            scrolling_target: None,
            scrolling_ready: None,
            scrolling_abort_pending: None,
            scrolling_target_resolver,
            scrolling_target_refresher,
        };

        if let Some(kind) = app.config.capture_on_start {
            app.begin_capture(kind);
        }

        Ok(app)
    }

    /// Services every source once. Never blocks.
    ///
    /// Order matters: HUD cancellation is applied before a completion already
    /// waiting in the worker queue, so an explicit Discard remains authoritative
    /// at the final frame boundary.
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
        self.drain_cards();
        self.drain_scrolling_ready();
        self.drain_pipeline();

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
            self.handle_outcome(outcome);
        }
    }

    fn handle_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Progress { card, progress } => {
                self.update_scroll_hud(card, &progress);
                self.note(format!("{card} {}", describe_scroll_progress(&progress)));
            }
            Outcome::Ready(card) => {
                if self.scrolling_card == Some(card.id)
                    && self.scrolling_abort_pending != Some(card.id)
                {
                    debug_assert!(
                        self.scrolling_ready.is_none(),
                        "only one scrolling capture can be ready at a time"
                    );
                    self.scrolling_ready = Some(card);
                    return;
                }
                self.handle_ready(card);
            }
            Outcome::Failed { card, error } => {
                if self.scrolling_card == Some(card) {
                    self.finish_scrolling_hud();
                }
                self.note(format!("{card} failed: {error}"));
            }
            Outcome::Done { card, detail } => {
                self.note(format!("{card} {detail}"));
            }
            Outcome::Refused { card, error } => {
                self.note(format!("{card} refused: {error}"));
            }
        }
    }

    fn drain_scrolling_ready(&mut self) {
        if let Some(card) = self.scrolling_ready.take() {
            self.handle_ready(card);
        }
    }

    fn handle_ready(&mut self, card: Box<Card>) {
        if self.scrolling_abort_pending == Some(card.id) {
            self.pipeline.post(Job::Discard {
                card: card.id,
                capture: card.capture_id.clone(),
            });
            self.note(format!("{} discarded", card.id));
            self.finish_scrolling_hud();
            return;
        }
        if self.scrolling_card == Some(card.id) {
            self.finish_scrolling_hud();
        }
        self.pipeline.post(Job::Accept(card.id));
        self.captures += 1;
        let summary = card.summary();
        if let Err(err) = self.surface.present(*card) {
            self.note(format!("a card could not be shown: {err}"));
        } else {
            self.note(summary);
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
                // Not yet routed. The drag payload is `scrozz-shell`'s
                // `DragSource`, and collapsing into the dock is the capture
                // stack's own animation — both belong to the surface that
                // raised the event, once there is one that can.
                CardEvent::Drag(id) | CardEvent::Collapse(id) | CardEvent::Open(id) => {
                    self.note(format!("{id}: {event:?} is not routed yet"));
                }
            }
        }

        let mut scrolling = Vec::new();
        while let Some(action) = self.surface.poll_scroll_hud() {
            scrolling.push(action);
        }
        for action in scrolling {
            match action {
                ScrollHudAction::Start(axis) if self.scrolling_card.is_none() => {
                    self.start_scrolling_capture(axis);
                }
                ScrollHudAction::Start(_) => {
                    self.note("a scrolling capture is already running");
                }
                ScrollHudAction::Keep if self.scrolling_card.is_some() => {
                    self.pipeline.cancel_scrolling(CancelAction::Keep);
                    self.note("finishing the scrolling capture with the stitched frames so far");
                }
                ScrollHudAction::Abort if self.scrolling_card.is_some() => {
                    let card = self.scrolling_card.expect("checked above");
                    self.scrolling_abort_pending = Some(card);
                    self.pipeline.cancel_scrolling(CancelAction::Abort);
                    self.hide_scrolling_hud();
                    self.note("discarding the scrolling capture");
                }
                ScrollHudAction::Keep | ScrollHudAction::Abort => {
                    self.finish_scrolling_hud();
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
        if kind == CaptureKind::Scrolling {
            if self.scroll_hud.is_some() || self.scrolling_card.is_some() {
                self.note("a scrolling capture is already open");
                return;
            }
            let target = match (self.scrolling_target_resolver)() {
                Ok(target) => target,
                Err(error) => {
                    let permissions = SystemPermissions::new();
                    if !permissions.is_granted(Capability::ScreenRecording)
                        && permissions.request(Capability::ScreenRecording).is_ok()
                    {
                        self.note(
                            "capture permission was granted; focus the target window and start \
                             scrolling capture again",
                        );
                    } else {
                        self.note(format!("could not select a scrolling target: {error}"));
                    }
                    return;
                }
            };
            self.scrolling_target = Some(target);
            self.set_scroll_hud(ScrollHudState::choosing(ScrollAxis::Vertical));
            self.note(
                "choose whether the scrolling capture should grow vertically or horizontally",
            );
            return;
        }

        self.start_capture(kind);
    }

    fn start_capture(&mut self, kind: CaptureKind) {
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

    fn start_scrolling_capture(&mut self, axis: ScrollAxis) {
        let Some(selected) = self.scrolling_target.take() else {
            self.note("the snapshotted scrolling target is no longer available");
            self.finish_scrolling_hud();
            return;
        };
        let permissions = SystemPermissions::new();
        if !permissions.is_granted(Capability::ScreenRecording)
            && let Err(error) = permissions.request(Capability::ScreenRecording)
        {
            self.note(format!("capture permission is required: {error}"));
            self.finish_scrolling_hud();
            return;
        }
        let target = match (self.scrolling_target_refresher)(selected) {
            Ok(target) => target,
            Err(error) => {
                self.note(format!(
                    "the selected scrolling target changed before capture started: {error}"
                ));
                self.finish_scrolling_hud();
                return;
            }
        };

        let card = self.pipeline.allocate();
        self.scrolling_card = Some(card);
        self.set_scroll_hud(ScrollHudState::prepared(axis, true));
        if !self.pipeline.post(Job::Scrolling {
            axis,
            card,
            target: Box::new(target),
        }) {
            self.note("the capture worker has gone");
            self.finish_scrolling_hud();
        }
    }

    fn set_scroll_hud(&mut self, state: ScrollHudState) {
        self.surface.show_scroll_hud(state.clone());
        self.scroll_hud = Some(state);
    }

    fn finish_scrolling_hud(&mut self) {
        self.hide_scrolling_hud();
        self.scrolling_target = None;
        self.scrolling_abort_pending = None;
        self.scrolling_card = None;
    }

    fn hide_scrolling_hud(&mut self) {
        self.surface.hide_scroll_hud();
        self.scroll_hud = None;
    }

    fn update_scroll_hud(&mut self, card: CardId, progress: &Progress) {
        if self.scrolling_card != Some(card) {
            return;
        }
        let Some(mut state) = self.scroll_hud.clone() else {
            return;
        };
        match progress {
            Progress::Prepared { automatic, .. } => {
                state.status = ScrollHudStatus::Prepared;
                state.automatic = *automatic;
            }
            Progress::FrameCaptured { frame } => {
                state.status = ScrollHudStatus::Capturing;
                state.frame = *frame;
            }
            Progress::WaitingForManualScroll => {
                state.status = ScrollHudStatus::WaitingForManualScroll;
                state.automatic = false;
            }
            Progress::Advanced {
                frame,
                delta,
                output_extent,
                ..
            } => {
                state.status = ScrollHudStatus::Capturing;
                state.frame = *frame;
                state.delta = Some(*delta);
                state.output_extent = *output_extent;
            }
            Progress::Stalled { count } => {
                state.status = ScrollHudStatus::Stalled(*count);
            }
            Progress::Interrupted { .. } => {}
            Progress::Finished { .. } => {
                state.status = ScrollHudStatus::Finalizing;
            }
        }
        self.set_scroll_hud(state);
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
        self.surface.hide_scroll_hud();
        self.hotkeys.unregister_all();
        if let Some(tray) = self.tray.take() {
            tray.close();
        }
        self.server = None;
        self.pipeline.stop();
    }

    /// Every note recorded so far.
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

fn snapshot_scrolling_target() -> CliResult<ScrollingTarget> {
    let backend = platform::capture_backend()?;
    if crate::commands::is_wayland() {
        return crate::commands::resolve_scrolling_target(
            backend.as_ref(),
            CaptureRequest {
                target: wayland_portal_picker_target(),
                cursor: CursorMode::Hidden,
                include_window_shadow: false,
            },
        );
    }
    let display = backend.active_display()?;
    let request = CaptureRequest {
        target: CaptureTarget::Display(display.id),
        cursor: CursorMode::Hidden,
        include_window_shadow: false,
    };
    crate::commands::resolve_scrolling_target(backend.as_ref(), request)
}

fn refresh_scrolling_target(target: ScrollingTarget) -> CliResult<ScrollingTarget> {
    let backend = platform::capture_backend()?;
    target.refresh(backend.as_ref())
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
            output_extent,
            ..
        } => {
            format!(
                "stitched frame {frame} ({delta} px advanced, {output_extent} px along the capture axis)"
            )
        }
        Progress::Stalled { count } => format!("saw no movement ({count})"),
        Progress::Interrupted { reason } => {
            format!("kept the valid stitched prefix after {reason}")
        }
        Progress::Finished {
            reason,
            frames,
            output_extent,
            ..
        } => format!(
            "finished scrolling capture ({reason:?}, {frames} frames, {output_extent} px along the capture axis)"
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
    use super::*;
    use crate::gui::card::{Card, CardId, Recording};
    use scrozz_core::{
        Display, DisplayId, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor, WindowId,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn app() -> (App, Recording) {
        let surface = Recording::new();
        let handle = surface.handle();
        let app = App::new_with_scrolling_target_resolver(
            Config::sealed(),
            Box::new(surface),
            Box::new(|| Ok(fixture_scrolling_target())),
        )
        .expect("a sealed app must start");
        (app, handle)
    }

    fn fixture_scrolling_target() -> ScrollingTarget {
        let display = Display {
            id: DisplayId("fixture-display".to_owned()),
            name: "Fixture display".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(1_200.0, 800.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(0.0, 20.0),
                LogicalSize::new(1_200.0, 780.0),
            ),
            scale: ScaleFactor::IDENTITY,
            is_primary: true,
        };
        let window = WindowId("fixture-window".to_owned());
        ScrollingTarget::new(
            CaptureRequest {
                target: CaptureTarget::Window(window.clone()),
                cursor: CursorMode::Hidden,
                include_window_shadow: false,
            },
            display,
            LogicalRect::new(
                LogicalPoint::new(100.0, 100.0),
                LogicalSize::new(900.0, 600.0),
            ),
            window,
        )
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
    fn scrolling_capture_opens_an_axis_picker() {
        let (mut app, surface) = app();

        assert_eq!(
            app.perform(Action::Capture(CaptureKind::Scrolling)),
            Tick::Continue
        );

        let hud = surface.scrolling_hud().expect("axis picker");
        assert_eq!(hud.status, ScrollHudStatus::ChoosingAxis);
    }

    #[test]
    fn scrolling_target_is_snapshotted_before_the_axis_hud_opens() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver_calls = Arc::clone(&calls);
        let surface = Recording::new();
        let mut app = App::new_with_scrolling_target_resolver(
            Config::sealed(),
            Box::new(surface),
            Box::new(move || {
                resolver_calls.fetch_add(1, Ordering::Relaxed);
                Ok(fixture_scrolling_target())
            }),
        )
        .expect("a sealed app must start");

        app.perform(Action::Capture(CaptureKind::Scrolling));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            app.scrolling_target
                .as_ref()
                .map(ScrollingTarget::capture_target),
            Some(CaptureTarget::Window(WindowId("fixture-window".to_owned())))
        );

        app.perform(Action::Capture(CaptureKind::Scrolling));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "an open HUD must retain the selected target identity"
        );
    }

    #[test]
    fn scrolling_target_is_refreshed_after_axis_selection() {
        let refreshes = Arc::new(AtomicUsize::new(0));
        let refresher_calls = Arc::clone(&refreshes);
        let surface = Recording::new();
        let handle = surface.handle();
        let mut app = App::new_with_scrolling_target_handlers(
            Config::sealed(),
            Box::new(surface),
            Box::new(|| Ok(fixture_scrolling_target())),
            Box::new(move |_| {
                refresher_calls.fetch_add(1, Ordering::Relaxed);
                Err(CliError::Core(scrozz_core::Error::TargetGone(
                    "fixture window moved".to_owned(),
                )))
            }),
        )
        .expect("a sealed app must start");

        app.perform(Action::Capture(CaptureKind::Scrolling));
        handle.inject_scroll_action(ScrollHudAction::Start(ScrollAxis::Horizontal));
        app.tick();

        assert_eq!(refreshes.load(Ordering::Relaxed), 1);
        assert!(app.scrolling_card.is_none());
        assert!(handle.scrolling_hud().is_none());
        assert!(
            app.notes()
                .iter()
                .any(|note| note.contains("changed before capture started")),
            "{:?}",
            app.notes()
        );
    }

    #[test]
    fn scrolling_hud_actions_are_drained_without_card_events() {
        let (mut app, surface) = app();
        app.perform(Action::Capture(CaptureKind::Scrolling));
        surface.inject_scroll_action(ScrollHudAction::Abort);

        app.tick();

        assert!(surface.scrolling_hud().is_none());
    }

    #[test]
    fn abort_suppresses_a_ready_result_that_lost_the_final_send_race() {
        let (mut app, surface) = app();
        let card = CardId(9);
        app.scrolling_card = Some(card);
        app.scrolling_abort_pending = Some(card);
        app.set_scroll_hud(ScrollHudState::prepared(ScrollAxis::Vertical, true));

        app.handle_outcome(Outcome::Ready(Box::new(Card::placeholder(
            card,
            CaptureKind::Scrolling,
        ))));

        assert!(surface.presented().is_empty());
        assert!(surface.scrolling_hud().is_none());
        assert_eq!(app.captures, 0);
        assert!(app.scrolling_card.is_none());
        assert!(app.notes().iter().any(|note| note == "card:9 discarded"));
    }

    #[test]
    fn ready_waits_one_ui_pass_for_a_simultaneous_discard_click() {
        let (mut app, surface) = app();
        let card = CardId(11);
        app.scrolling_card = Some(card);
        app.set_scroll_hud(ScrollHudState::prepared(ScrollAxis::Vertical, true));

        app.handle_outcome(Outcome::Ready(Box::new(Card::placeholder(
            card,
            CaptureKind::Scrolling,
        ))));
        assert!(
            surface.presented().is_empty(),
            "the ready result must wait until the HUD has drawn once more"
        );

        surface.inject_scroll_action(ScrollHudAction::Abort);
        app.tick();

        assert!(surface.presented().is_empty());
        assert_eq!(app.captures, 0);
        assert!(app.scrolling_ready.is_none());
        assert!(app.notes().iter().any(|note| note == "card:11 discarded"));
    }

    #[test]
    fn horizontal_progress_reports_stitched_width() {
        let (mut app, surface) = app();
        let card = CardId(7);
        app.scrolling_card = Some(card);
        app.set_scroll_hud(ScrollHudState::prepared(ScrollAxis::Horizontal, true));

        app.update_scroll_hud(
            card,
            &scrozz_stitch::Progress::Advanced {
                frame: 3,
                delta: 24,
                seam: scrozz_stitch::SeamQuality {
                    mean_absolute_error: 0,
                    confidence: 42,
                },
                output_extent: 640,
                output_height: 180,
            },
        );

        let hud = surface.scrolling_hud().expect("capture HUD");
        assert_eq!(hud.axis, ScrollAxis::Horizontal);
        assert_eq!(hud.output_extent, 640);
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
