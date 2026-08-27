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
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        mpsc::{self, Receiver, TryRecvError},
    },
    time::{Duration, Instant},
};

use scrozz_core::{CaptureTarget, Error as CoreError};
use scrozz_record::{
    MachineEvent, MachineFailure, Recording, RecordingMachine, RecordingPhase, RecordingRequest,
    RecordingSettings,
    edit::{EditOutput, EditPlan, VideoDocument},
    transcode::{
        NativeTranscoder, TranscodeEvent, TranscodeFailure, TranscodeJob, TranscodeOutput,
        TranscodeStatus, Transcoder as _,
    },
};
use scrozz_shell::{
    Accelerator, Capability, GlobalHotkeys, Hotkey, HotkeyManager, KeyState, Permissions,
    SystemPermissions, Tray, TrayAction,
};
use scrozz_ui::{
    RecordingHudAction, RecordingHudSnapshot, RecordingPresentation, RecordingSurfaceAction,
    VideoEditorAction, VideoEditorSnapshot,
};

use crate::{
    cli::{Cli, Command, RecordControl},
    commands,
    fault::{CliError, CliResult},
    gui::{
        action::{Action, CaptureKind},
        card::{CardEvent, CardSurface},
        pipeline::{Job, Outcome, Pipeline},
        server::{Request, Server},
    },
    json::Json,
    report::Report,
};

struct FinalisedRecording {
    result: scrozz_core::Result<Recording>,
}

#[derive(Clone)]
enum GuiRecordingCompletion {
    Finished(Report),
    Failed(CliError),
}

struct ActiveVideoEditor {
    document: VideoDocument,
    plan: EditPlan,
    transcode_job: Option<Box<dyn TranscodeJob>>,
    transcode_status: Option<TranscodeStatus>,
    transcode_progress: f32,
    transcode_output: Option<TranscodeOutput>,
    transcode_failure: Option<TranscodeFailure>,
}

enum PendingRecordingStart {
    Settings {
        target: CaptureTarget,
        destination: std::path::PathBuf,
    },
    Request(RecordingRequest),
}

struct ArmedRecordingStart {
    start: PendingRecordingStart,
    armed_tick: u64,
}

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
    recording: Option<RecordingMachine>,
    recording_permission: fn() -> CliResult<()>,
    recording_target: fn() -> CliResult<CaptureTarget>,
    recording_destination: fn() -> CliResult<std::path::PathBuf>,
    recording_tick: Instant,
    recording_finalisation: Option<Receiver<FinalisedRecording>>,
    recording_completion: Option<GuiRecordingCompletion>,
    recording_replies: Vec<Request>,
    recording_preflight: Option<RecordingHudSnapshot>,
    pending_recording_start: Option<ArmedRecordingStart>,
    recording_editor: Option<ActiveVideoEditor>,
    tick_sequence: u64,
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
        let pipeline = Pipeline::start()?;
        let mut notes = Vec::new();
        let recording = match RecordingMachine::native(RecordingSettings::shipped()) {
            Ok(machine) => {
                notes.push(format!(
                    "recording engine ready with capabilities {:?}",
                    machine.capabilities()
                ));
                Some(machine)
            }
            Err(error) => {
                notes.push(format!("screen recording unavailable: {error}"));
                None
            }
        };
        let recording_available = recording
            .as_ref()
            .is_some_and(|machine| machine.capabilities().video);

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
            match Tray::with_tooltip_and_recording("Scrozz", recording_available) {
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
            recording,
            recording_permission: Self::ensure_recording_permission,
            recording_target: Self::active_recording_target,
            recording_destination: crate::output::default_recording_path,
            recording_tick: Instant::now(),
            recording_finalisation: None,
            recording_completion: None,
            recording_replies: Vec::new(),
            recording_preflight: None,
            pending_recording_start: None,
            recording_editor: None,
            tick_sequence: 0,
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
        self.tick_sequence = self.tick_sequence.saturating_add(1);
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
        self.advance_recording();

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
            let parsed = request.command();
            if matches!(
                parsed,
                Some(Command::Record(ref args))
                    if args.control() == Some(RecordControl::Stop)
            ) {
                self.begin_recording_finalisation(Some(request));
                continue;
            }

            if matches!(
                parsed,
                Some(Command::Record(ref args))
                    if args.control().is_none() && !args.dry_run
            ) {
                match request.dispatch_without_reply(|command| self.dispatch_gui_recording(command))
                {
                    Ok(_) => {
                        self.recording_replies.push(request);
                        self.reply_recording_waiters();
                    }
                    Err(error) => {
                        request.serve_with(|_| Err(error));
                    }
                }
                continue;
            }

            let served = if matches!(parsed, Some(Command::Record(_))) {
                request.serve_with(|command| self.dispatch_gui_recording(command))
            } else {
                request.serve()
            };
            if let Some(command) = served {
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
            match outcome {
                Outcome::Ready(card) => {
                    self.captures += 1;
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
                // Not yet routed. The drag payload is `scrozz-shell`'s
                // `DragSource`, and collapsing into the dock is the capture
                // stack's own animation — both belong to the surface that
                // raised the event, once there is one that can.
                CardEvent::Drag(id) | CardEvent::Collapse(id) | CardEvent::Open(id) => {
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
                self.toggle_recording();
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

    fn toggle_recording(&mut self) {
        if self.pending_recording_start.take().is_some() {
            self.note("pending recording start cancelled");
            return;
        }
        let Some(phase) = self.recording.as_ref().map(RecordingMachine::phase) else {
            self.present_recording_error(CliError::Core(CoreError::Unsupported {
                what: "screen recording".into(),
                why: "no native engine advertised video capture".into(),
            }));
            return;
        };

        match phase {
            RecordingPhase::Idle | RecordingPhase::Finished | RecordingPhase::Failed => {
                self.begin_recording()
            }
            RecordingPhase::Selecting => {
                let result = self
                    .recording
                    .as_mut()
                    .expect("recording phase came from this machine")
                    .cancel_selection();
                self.finish_recording_action(result, "recording selection cancelled");
            }
            RecordingPhase::Countdown => {
                let result = self
                    .recording
                    .as_mut()
                    .expect("recording phase came from this machine")
                    .cancel_countdown();
                self.finish_recording_action(result, "recording countdown cancelled");
            }
            RecordingPhase::Recording | RecordingPhase::Paused => {
                self.begin_recording_finalisation(None);
            }
            RecordingPhase::Finalising => self.note("recording is already finalising"),
        }
    }

    fn begin_recording(&mut self) {
        if let Err(error) = (self.recording_permission)() {
            self.present_recording_error(error);
            return;
        }

        let target = match (self.recording_target)() {
            Ok(target) => target,
            Err(error) => {
                self.present_recording_error(error);
                return;
            }
        };
        let destination = match (self.recording_destination)() {
            Ok(destination) => destination,
            Err(error) => {
                self.present_recording_error(error);
                return;
            }
        };

        let needs_reset = self.recording.as_ref().is_some_and(|machine| {
            matches!(
                machine.phase(),
                RecordingPhase::Finished | RecordingPhase::Failed
            )
        });
        if needs_reset {
            let result = self
                .recording
                .as_mut()
                .expect("recording availability was checked before resetting")
                .reset();
            if let Err(error) = result {
                self.present_recording_error(CliError::Core(error));
                return;
            }
        }

        self.recording_preflight = None;
        self.recording_tick = Instant::now();
        self.recording_completion = None;
        self.pending_recording_start = Some(ArmedRecordingStart {
            start: PendingRecordingStart::Settings {
                target,
                destination,
            },
            armed_tick: self.tick_sequence,
        });
        self.note("recording start armed after overlay suppression");
    }

    fn finish_recording_action(&mut self, result: scrozz_core::Result<()>, success: &str) {
        match result {
            Ok(()) => {
                self.recording_preflight = None;
                self.note(success);
            }
            Err(error) => self.present_recording_error(CliError::Core(error)),
        }
        self.drain_recording_events();
        self.refresh_recording_tray();
    }

    fn advance_recording(&mut self) {
        self.finish_pending_recording();
        self.advance_video_export();
        self.start_pending_recording();
        let now = Instant::now();
        let delta = now.saturating_duration_since(self.recording_tick);
        self.recording_tick = now;

        if let Some(editor) = &mut self.recording_editor {
            editor.document.tick(delta);
        }

        fn start_pending_recording(&mut self) {
            let ready = self
                .pending_recording_start
                .as_ref()
                .is_some_and(|pending| pending.armed_tick < self.tick_sequence);
            if !ready {
                return;
            }
            let pending = self
                .pending_recording_start
                .take()
                .expect("readiness came from the pending start");
            let result = match pending.start {
                PendingRecordingStart::Settings {
                    target,
                    destination,
                } => self
                    .recording
                    .as_mut()
                    .expect("a pending settings start requires a recording machine")
                    .begin_with_destination(target, destination),
                PendingRecordingStart::Request(request) => self
                    .recording
                    .as_mut()
                    .expect("a pending configured start requires a recording machine")
                    .begin_request(request),
            };
            if let Err(error) = result {
                let error = CliError::Core(error);
                self.recording_completion = Some(GuiRecordingCompletion::Failed(error.clone()));
                self.present_recording_error(error);
            } else {
                self.recording_preflight = None;
                self.note("recording countdown started");
            }
            self.drain_recording_events();
            self.refresh_recording_tray();
        }
        let result = self.recording.as_mut().map(|machine| machine.tick(delta));
        if let Some(Err(error)) = result {
            self.note(format!("recording tick failed: {error}"));
        }
        let stopped_without_terminal_event = self
            .recording
            .as_ref()
            .is_some_and(RecordingMachine::requires_finalisation);
        if stopped_without_terminal_event && self.recording_finalisation.is_none() {
            self.begin_recording_finalisation(None);
            return;
        }
        self.drain_recording_events();
        self.refresh_recording_tray();
    }

    fn advance_video_export(&mut self) {
        let Some(editor) = self.recording_editor.as_mut() else {
            return;
        };
        let Some(job) = editor.transcode_job.as_mut() else {
            return;
        };
        let mut terminal = false;
        for _ in 0..64 {
            let Some(event) = job.poll() else {
                break;
            };
            match event {
                TranscodeEvent::Progress(progress) => {
                    editor.transcode_progress = progress;
                    editor.transcode_status = Some(TranscodeStatus::Running { progress });
                }
                TranscodeEvent::Finished(output) => {
                    editor.transcode_progress = 1.0;
                    editor.transcode_output = Some(output);
                    editor.transcode_failure = None;
                    editor.transcode_status = Some(TranscodeStatus::Finished);
                    terminal = true;
                    break;
                }
                TranscodeEvent::Failed(failure) => {
                    editor.transcode_output = None;
                    editor.transcode_failure = Some(failure);
                    editor.transcode_status = Some(TranscodeStatus::Failed);
                    terminal = true;
                    break;
                }
                TranscodeEvent::Cancelled(partial) => {
                    editor.transcode_output = partial;
                    editor.transcode_failure = None;
                    editor.transcode_status = Some(TranscodeStatus::Cancelled);
                    terminal = true;
                    break;
                }
            }
        }
        if terminal {
            editor.transcode_job = None;
        } else {
            editor.transcode_status = Some(job.status());
        }
    }

    fn dispatch_gui_recording(&mut self, command: &Command) -> CliResult<Report> {
        let Command::Record(args) = command else {
            return commands::dispatch(command);
        };
        if args.dry_run {
            return commands::dispatch(command);
        }

        let machine = self.recording.as_mut().ok_or_else(|| {
            CliError::Core(CoreError::Unsupported {
                what: "screen recording".into(),
                why: "no native recording engine is linked for this platform".into(),
            })
        })?;
        match args.control() {
            Some(RecordControl::Pause) => {
                machine.pause()?;
                Ok(recording_machine_report(
                    machine,
                    "paused",
                    "Recording paused.",
                ))
            }
            Some(RecordControl::Resume) => {
                machine.resume()?;
                Ok(recording_machine_report(
                    machine,
                    "recording",
                    "Recording resumed.",
                ))
            }
            Some(RecordControl::Stop) => Err(CliError::Core(CoreError::InvalidRequest(
                "recording stop must be finalised asynchronously".into(),
            ))),
            None => {
                if let Err(error) = (self.recording_permission)() {
                    return Err(CliError::Core(CoreError::PermissionDenied {
                        capability: "screen recording".into(),
                        remedy: error.to_string(),
                    }));
                }
                if matches!(
                    machine.phase(),
                    RecordingPhase::Finished | RecordingPhase::Failed
                ) {
                    machine.reset()?;
                }
                let prepared = commands::prepare_recording_args(args)?;
                let report = prepared.started_report();
                if self
                    .recording_editor
                    .as_ref()
                    .is_some_and(|editor| editor.transcode_job.is_some())
                {
                    return Err(CliError::Core(CoreError::InvalidRequest(
                        "cancel the active export before starting another recording".into(),
                    )));
                }
                self.recording_editor = None;
                if self.pending_recording_start.is_some() || machine.is_active() {
                    return Err(CliError::Core(CoreError::InvalidRequest(
                        "a recording start is already pending or active".into(),
                    )));
                }
                self.pending_recording_start = Some(ArmedRecordingStart {
                    start: PendingRecordingStart::Request(prepared.request),
                    armed_tick: self.tick_sequence,
                });
                self.recording_tick = Instant::now();
                self.recording_completion = None;
                self.drain_recording_events();
                self.refresh_recording_tray();
                Ok(report)
            }
        }
    }

    fn begin_recording_finalisation(&mut self, reply: Option<Request>) {
        if self.pending_recording_start.take().is_some() {
            if let Some(reply) = reply {
                self.recording_replies.push(reply);
            }
            self.recording_completion = Some(GuiRecordingCompletion::Failed(CliError::Core(
                CoreError::Cancelled,
            )));
            self.note("pending recording start cancelled before native capture");
            self.reply_recording_waiters();
            return;
        }
        let result = self
            .recording
            .as_mut()
            .ok_or_else(|| {
                CoreError::InvalidRequest(
                    "no recording is in progress, so there is nothing to stop".into(),
                )
            })
            .and_then(RecordingMachine::begin_finalising);
        let session = match result {
            Ok(session) => session,
            Err(error) => {
                self.note(format!("recording action failed: {error}"));
                if let Some(request) = reply {
                    request.serve_with(|_| Err(CliError::Core(error)));
                }
                return;
            }
        };

        let (send, receive) = mpsc::channel();
        self.recording_finalisation = Some(receive);
        if let Some(reply) = reply {
            self.recording_replies.push(reply);
        }
        self.note("recording finalisation started");
        std::thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| session.stop())).unwrap_or_else(|_| {
                Err(CoreError::Platform(
                    "the native recording finaliser panicked".into(),
                ))
            });
            let _ = send.send(FinalisedRecording { result });
        });
        self.drain_recording_events();
        self.refresh_recording_tray();
    }

    fn finish_pending_recording(&mut self) {
        let received = self.recording_finalisation.as_ref().map(Receiver::try_recv);
        let message = match received {
            Some(Ok(message)) => message,
            Some(Err(TryRecvError::Empty)) | None => return,
            Some(Err(TryRecvError::Disconnected)) => {
                self.recording_finalisation = None;
                let result = self.recording.as_mut().map(|machine| {
                    machine.complete_finalising(Err(CoreError::Platform(
                        "the recording finaliser ended without returning a result".into(),
                    )))
                });
                if let Some(Err(error)) = result {
                    self.note(format!("could not finish recording state: {error}"));
                }
                self.drain_recording_events();
                return;
            }
        };
        self.recording_finalisation = None;
        self.apply_finalised_recording(message);
    }

    fn apply_finalised_recording(&mut self, message: FinalisedRecording) {
        let completed = self
            .recording
            .as_mut()
            .expect("a finalisation receiver belongs to a recording machine")
            .complete_finalising(message.result);
        if let Err(error) = completed {
            self.note(format!("could not finish recording state: {error}"));
        }
        self.drain_recording_events();
    }

    fn drain_recording_events(&mut self) {
        let fallback_target = self
            .recording
            .as_ref()
            .and_then(RecordingMachine::request)
            .map(|request| request.target.clone());
        let events = self
            .recording
            .as_mut()
            .map(|machine| machine.drain_events().collect::<Vec<_>>())
            .unwrap_or_default();

        for event in events {
            match event {
                MachineEvent::PhaseChanged(_) => {}
                MachineEvent::FirstFrame => self.note("recording captured its first frame"),
                MachineEvent::Warning(message) => {
                    self.note(format!("recording warning: {message}"));
                }
                MachineEvent::ClockDrift(drift) => self.note(format!(
                    "recording clock drift: engine is {:+.3}s from the authoritative clock",
                    drift.delta_secs
                )),
                MachineEvent::Finished(output) => {
                    self.retain_recording_completion(output, fallback_target.as_ref());
                }
                MachineEvent::Failed(failure) => {
                    let partial = failure.partial.clone();
                    let partial_note = partial.as_ref().map(|output| {
                        format!("; partial output is at {}", output.path().display())
                    });
                    self.note(format!(
                        "recording failed: {}{}",
                        failure.error,
                        partial_note.unwrap_or_default()
                    ));
                    if let Some(output) = partial {
                        self.retain_recording_completion(output, fallback_target.as_ref());
                    } else {
                        self.recording_completion = Some(GuiRecordingCompletion::Failed(
                            CliError::from(Arc::clone(&failure.error)),
                        ));
                    }
                }
            }
        }
        self.reply_recording_waiters();
    }

    fn reply_recording_waiters(&mut self) {
        if self.recording_replies.is_empty() {
            return;
        }
        let completion = match self.recording_completion.clone() {
            Some(completion) => completion,
            None => return,
        };
        for request in self.recording_replies.drain(..) {
            let completion = completion.clone();
            request.serve_with(|_| match completion {
                GuiRecordingCompletion::Finished(report) => Ok(report),
                GuiRecordingCompletion::Failed(error) => Err(error),
            });
        }
    }

    fn retain_recording_completion(
        &mut self,
        output: Recording,
        fallback_target: Option<&CaptureTarget>,
    ) {
        let path = output.path().to_path_buf();
        let requested_fps = self
            .recording
            .as_ref()
            .and_then(RecordingMachine::request)
            .map(|request| request.fps);
        self.recording_editor = None;
        match active_video_editor(&output, requested_fps) {
            Ok(editor) => self.recording_editor = editor,
            Err(error) => self.note(format!(
                "recording was saved but could not be opened in the video editor: {error}"
            )),
        }
        match commands::finish_recording_report(output, fallback_target) {
            Ok(report) => {
                self.note(format!("recording saved to {}", path.display()));
                self.recording_completion = Some(GuiRecordingCompletion::Finished(report));
            }
            Err(error) => {
                self.note(format!(
                    "recording output was rejected at the real-output boundary: {error}"
                ));
                self.recording_completion = Some(GuiRecordingCompletion::Failed(error));
            }
        }
    }

    fn refresh_recording_tray(&self) {
        let Some(tray) = &self.tray else {
            return;
        };
        let Some(machine) = &self.recording else {
            tray.set_recording_available(false);
            return;
        };
        tray.set_recording_available(machine.capabilities().video);
        tray.set_recording(machine.is_active(), machine.elapsed());
    }

    fn note(&mut self, what: impl Into<String>) {
        let what = what.into();
        tracing::info!("{what}");
        self.notes.push(what);
    }

    fn present_recording_error(&mut self, error: CliError) {
        let core = error
            .core_error()
            .cloned()
            .unwrap_or_else(|| CoreError::Platform(error.to_string()));
        let capabilities = self
            .recording
            .as_ref()
            .map(RecordingMachine::capabilities)
            .unwrap_or_default();
        self.recording_preflight = Some(RecordingHudSnapshot {
            phase: RecordingPhase::Failed,
            elapsed: Duration::ZERO,
            capabilities,
            warning: None,
            drift: None,
            output: None,
            failure: Some(MachineFailure {
                error: Arc::new(core),
                partial: None,
                recovery_error: None,
            }),
        });
        self.note(format!("recording action failed: {error}"));
    }

    fn ensure_recording_permission() -> CliResult<()> {
        let permissions = SystemPermissions::new();
        if !permissions.is_granted(Capability::ScreenRecording) {
            permissions.request(Capability::ScreenRecording)?;
        }
        Ok(())
    }

    fn active_recording_target() -> CliResult<CaptureTarget> {
        let backend = scrozz_capture::backend()?;
        let display = backend.active_display()?;
        Ok(CaptureTarget::Display(display.id))
    }

    /// Recording state rendered by the shared overlay.
    #[must_use]
    pub fn recording_presentation(&self) -> Option<RecordingPresentation> {
        if let Some(hud) = &self.recording_preflight {
            let suppress_all_overlays = cfg!(any(target_os = "linux", target_os = "windows"))
                && self.recording.as_ref().is_some_and(|machine| {
                    matches!(
                        machine.phase(),
                        RecordingPhase::Recording
                            | RecordingPhase::Paused
                            | RecordingPhase::Finalising
                    )
                });
            return Some(RecordingPresentation {
                hud: hud.clone(),
                countdown: RecordingSettings::shipped().countdown,
                countdown_remaining: Duration::ZERO,
                suppress_all_overlays,
                editor: None,
            });
        }
        let machine = self.recording.as_ref()?;
        if machine.phase() == RecordingPhase::Idle
            && self.recording_editor.is_none()
            && self.pending_recording_start.is_none()
        {
            return None;
        }
        let suppress_all_overlays = cfg!(any(target_os = "linux", target_os = "windows"))
            && (self.pending_recording_start.is_some()
                || matches!(
                    machine.phase(),
                    RecordingPhase::Recording | RecordingPhase::Paused | RecordingPhase::Finalising
                ));
        Some(RecordingPresentation {
            hud: RecordingHudSnapshot::from_machine(machine),
            countdown: machine.settings().countdown,
            countdown_remaining: machine.countdown_remaining(),
            suppress_all_overlays,
            editor: self
                .recording_editor
                .as_ref()
                .map(|editor| VideoEditorSnapshot {
                    document: editor.document.clone(),
                    plan: editor.plan,
                    transcode_status: editor.transcode_status,
                    transcode_progress: editor.transcode_progress,
                    transcode_output: editor.transcode_output.clone(),
                    transcode_failure: editor.transcode_failure.clone(),
                }),
        })
    }

    /// Applies one semantic action raised by the shared recording surface.
    pub fn handle_recording_surface_action(&mut self, action: RecordingSurfaceAction) {
        match action {
            RecordingSurfaceAction::Hud(action) => self.handle_recording_hud_action(action),
            RecordingSurfaceAction::Editor(action) => self.handle_video_editor_action(action),
        }
        self.drain_recording_events();
        self.refresh_recording_tray();
    }

    fn handle_recording_hud_action(&mut self, action: RecordingHudAction) {
        match action {
            RecordingHudAction::Dismiss => {
                if self.recording_preflight.take().is_some() {
                    let reset = self.recording.as_mut().and_then(|machine| {
                        matches!(
                            machine.phase(),
                            RecordingPhase::Finished | RecordingPhase::Failed
                        )
                        .then(|| machine.reset())
                    });
                    if let Some(Err(error)) = reset {
                        self.note(format!(
                            "could not reset recording state after dismissing its error: {error}"
                        ));
                    }
                    self.note("recording error dismissed");
                    return;
                }
                self.recording_editor = None;
                let result = self.recording.as_mut().ok_or_else(|| {
                    CoreError::InvalidRequest("no recording state is available".into())
                });
                let result = result.and_then(RecordingMachine::reset);
                self.finish_recording_action(result, "recording result dismissed");
            }
            RecordingHudAction::Pause | RecordingHudAction::Resume => {
                let result = self.recording.as_mut().ok_or_else(|| {
                    CoreError::InvalidRequest("no recording session is available".into())
                });
                let result = result.and_then(|machine| match action {
                    RecordingHudAction::Pause => machine.pause(),
                    RecordingHudAction::Resume => machine.resume(),
                    _ => unreachable!("the outer match accepts only pause or resume"),
                });
                self.finish_recording_action(result, "recording state changed");
            }
            RecordingHudAction::Stop => self.begin_recording_finalisation(None),
            RecordingHudAction::RevealOutput => {
                let path = self
                    .recording_editor
                    .as_ref()
                    .map(|editor| editor.document.recording().path().to_path_buf())
                    .or_else(|| {
                        self.recording
                            .as_ref()
                            .and_then(RecordingMachine::output)
                            .map(|output| output.path().to_path_buf())
                    });
                self.reveal_recording_path(path);
            }
            RecordingHudAction::RevealPartialOutput => {
                let path = self
                    .recording
                    .as_ref()
                    .and_then(RecordingMachine::failure)
                    .and_then(|failure| failure.partial.as_ref())
                    .map(|output| output.path().to_path_buf());
                self.reveal_recording_path(path);
            }
        }
    }

    fn handle_video_editor_action(&mut self, action: VideoEditorAction) {
        if action == VideoEditorAction::Close {
            if self
                .recording_editor
                .as_ref()
                .is_some_and(|editor| editor.transcode_job.is_some())
            {
                self.note("cancel the active export before closing the video editor");
                return;
            }
            self.recording_editor = None;
            if let Some(machine) = self.recording.as_mut()
                && matches!(
                    machine.phase(),
                    RecordingPhase::Finished | RecordingPhase::Failed
                )
                && let Err(error) = machine.reset()
            {
                self.note(format!("could not close the recording editor: {error}"));
            }
            return;
        }
        let Some(editor) = self.recording_editor.as_mut() else {
            self.note("the video editor no longer has a recording");
            return;
        };
        match action {
            VideoEditorAction::Close => {
                unreachable!("close is handled before borrowing the editor")
            }
            VideoEditorAction::Play => editor.document.play(),
            VideoEditorAction::Pause => editor.document.pause(),
            VideoEditorAction::Seek(position) => {
                if let Err(error) = editor.document.seek(position) {
                    self.note(format!("could not seek the recording: {error}"));
                }
            }
            VideoEditorAction::PlanChanged(plan) => {
                editor.plan = plan;
                editor.transcode_status = None;
                editor.transcode_progress = 0.0;
                editor.transcode_output = None;
                editor.transcode_failure = None;
            }
            VideoEditorAction::Export(plan) => {
                editor.plan = plan;
                editor.transcode_progress = 0.0;
                editor.transcode_output = None;
                editor.transcode_failure = None;
                let output = edited_output_path(&editor.document, &plan);
                match output
                    .and_then(|path| NativeTranscoder::new().start(&editor.document, &plan, path))
                {
                    Ok(job) => {
                        editor.transcode_status = Some(job.status());
                        editor.transcode_job = Some(job);
                    }
                    Err(error) => {
                        editor.transcode_status = Some(TranscodeStatus::Failed);
                        editor.transcode_failure = Some(TranscodeFailure {
                            error: Arc::new(error),
                            partial: None,
                        });
                    }
                }
            }
            VideoEditorAction::CancelExport => {
                if let Some(job) = editor.transcode_job.as_mut()
                    && let Err(error) = job.cancel()
                {
                    self.note(format!("could not cancel video export: {error}"));
                }
            }
            VideoEditorAction::RevealOutput | VideoEditorAction::RevealPartialOutput => {
                let path = editor
                    .transcode_output
                    .as_ref()
                    .map(|output| output.path.clone())
                    .or_else(|| {
                        editor
                            .transcode_failure
                            .as_ref()
                            .and_then(|failure| failure.partial.as_ref())
                            .map(|output| output.path.clone())
                    })
                    .or_else(|| Some(editor.document.recording().path().to_path_buf()));
                self.reveal_recording_path(path);
            }
        }
    }

    fn reveal_recording_path(&mut self, path: Option<std::path::PathBuf>) {
        let Some(path) = path else {
            self.note("there is no recording output to reveal");
            return;
        };
        match reveal_file(&path) {
            Ok(()) => self.note(format!("revealed recording at {}", path.display())),
            Err(error) => self.note(format!(
                "could not reveal recording at {}: {error}",
                path.display()
            )),
        }
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
        self.finalise_recording_before_shutdown();
        self.hotkeys.unregister_all();
        if let Some(tray) = self.tray.take() {
            tray.close();
        }
        self.server = None;
        self.pipeline.stop();
    }

    fn finalise_recording_before_shutdown(&mut self) {
        if let Some(receiver) = self.recording_finalisation.take() {
            match receiver.recv() {
                Ok(message) => self.apply_finalised_recording(message),
                Err(_) => {
                    if let Some(machine) = self.recording.as_mut() {
                        let _ = machine.complete_finalising(Err(CoreError::Platform(
                            "the recording finaliser ended during shutdown".into(),
                        )));
                    }
                    self.drain_recording_events();
                }
            }
            return;
        }

        let phase = self.recording.as_ref().map(RecordingMachine::phase);
        if matches!(
            phase,
            Some(RecordingPhase::Recording | RecordingPhase::Paused)
        ) {
            let session = self
                .recording
                .as_mut()
                .expect("the phase came from this machine")
                .begin_finalising();
            match session {
                Ok(session) => {
                    let result =
                        catch_unwind(AssertUnwindSafe(|| session.stop())).unwrap_or_else(|_| {
                            Err(CoreError::Platform(
                                "the native recording finaliser panicked during shutdown".into(),
                            ))
                        });
                    let _ = self
                        .recording
                        .as_mut()
                        .expect("the session came from this machine")
                        .complete_finalising(result);
                    self.drain_recording_events();
                }
                Err(error) => self.note(format!(
                    "could not begin recording finalisation during shutdown: {error}"
                )),
            }
        }
    }

    /// Every note recorded so far.
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

#[cfg(target_os = "macos")]
fn reveal_file(path: &std::path::Path) -> scrozz_core::Result<()> {
    reveal_with(
        std::process::Command::new("open").arg("-R").arg(path),
        "Finder",
    )
}

#[cfg(target_os = "windows")]
fn reveal_file(path: &std::path::Path) -> scrozz_core::Result<()> {
    reveal_with(
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path),
        "File Explorer",
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn reveal_file(path: &std::path::Path) -> scrozz_core::Result<()> {
    let directory = path.parent().unwrap_or(path);
    reveal_with(
        std::process::Command::new("xdg-open").arg(directory),
        "the platform file browser",
    )
}

fn reveal_with(command: &mut std::process::Command, application: &str) -> scrozz_core::Result<()> {
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(CoreError::Platform(format!(
            "{application} exited with {status}"
        )))
    }
}

fn active_video_editor(
    output: &Recording,
    _requested_fps: Option<u32>,
) -> scrozz_core::Result<Option<ActiveVideoEditor>> {
    output.require_native()?;
    if !output.is_playable() {
        return Ok(None);
    }
    let document = VideoDocument::open_native(output.clone())?;
    let plan = EditPlan::video(&document)?;
    Ok(Some(ActiveVideoEditor {
        document,
        plan,
        transcode_job: None,
        transcode_status: None,
        transcode_progress: 0.0,
        transcode_output: None,
        transcode_failure: None,
    }))
}

fn edited_output_path(
    document: &VideoDocument,
    plan: &EditPlan,
) -> scrozz_core::Result<std::path::PathBuf> {
    let source = document.recording().path();
    let parent = source.parent().unwrap_or_else(|| std::path::Path::new("."));
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("recording");
    let extension = match plan.output {
        EditOutput::Video => "mp4",
        EditOutput::Animation(format) => format.extension(),
    };
    for suffix in 0..1_000 {
        let name = if suffix == 0 {
            format!("{stem}-edited.{extension}")
        } else {
            format!("{stem}-edited-{suffix}.{extension}")
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(CoreError::Storage(
        "could not allocate a collision-free edited recording path".into(),
    ))
}

fn recording_machine_report(
    machine: &RecordingMachine,
    state: &'static str,
    human: &'static str,
) -> Report {
    Report::new(
        Json::obj([
            ("state", Json::str(state)),
            (
                "path",
                Json::opt(
                    machine
                        .request()
                        .and_then(|request| request.destination.as_deref()),
                    |path| Json::str(path.to_string_lossy().into_owned()),
                ),
            ),
            ("elapsed_secs", Json::Float(machine.elapsed().as_secs_f64())),
        ]),
        human,
    )
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
    fn unavailable_and_unwired_actions_say_so_rather_than_doing_nothing() {
        let (mut app, _) = app();
        app.recording = None;
        for action in [Action::OpenHistory, Action::OpenSettings] {
            assert_eq!(app.perform(action), Tick::Continue);
        }
        app.recording = None;
        assert_eq!(app.perform(Action::ToggleRecording), Tick::Continue);
        let notes = app.notes().join("\n");
        assert!(notes.contains("history window"), "{notes}");
        assert!(notes.contains("settings window"), "{notes}");
        assert!(
            notes.contains("screen recording is unavailable")
                || notes.contains("recording countdown started"),
            "{notes}"
        );
    }

    #[test]
    fn injected_mock_drives_the_toggle_path_but_is_rejected_as_real_output() {
        fn target() -> CliResult<CaptureTarget> {
            Ok(CaptureTarget::Display(scrozz_core::DisplayId(
                "fixture-display".to_owned(),
            )))
        }
        fn destination() -> CliResult<std::path::PathBuf> {
            Ok(std::env::temp_dir().join("scrozz-gui-recording-test.mp4"))
        }

        let (mut app, _) = app();
        let mut settings = RecordingSettings::shipped();
        settings.countdown.enabled = false;
        app.recording = Some(
            RecordingMachine::with_engine(
                Box::new(scrozz_record::engine::MockEngine::fully_capable(
                    scrozz_record::engine::MockSessionPlan::complete("mock.mp4", 2.0).unwrap(),
                )),
                settings,
            )
            .unwrap(),
        );
        app.recording_permission = || Ok(());
        app.recording_target = target;
        app.recording_destination = destination;

        assert_eq!(app.perform(Action::ToggleRecording), Tick::Continue);
        assert_eq!(
            app.recording.as_ref().map(RecordingMachine::phase),
            Some(RecordingPhase::Recording)
        );
        assert_eq!(
            app.recording
                .as_ref()
                .and_then(RecordingMachine::request)
                .and_then(|request| request.destination.as_deref()),
            Some(destination().unwrap().as_path())
        );

        assert_eq!(app.perform(Action::ToggleRecording), Tick::Continue);
        for _ in 0..100 {
            app.advance_recording();
            if app.recording.as_ref().map(RecordingMachine::phase) == Some(RecordingPhase::Finished)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            app.recording.as_ref().map(RecordingMachine::phase),
            Some(RecordingPhase::Finished)
        );
        assert!(
            app.notes()
                .iter()
                .any(|note| note.contains("rejected at the real-output boundary"))
        );
    }

    #[test]
    fn shared_hud_actions_drive_the_gui_owned_machine() {
        let (mut app, _) = app();
        let mut settings = RecordingSettings::shipped();
        settings.countdown.enabled = false;
        app.recording = Some(
            RecordingMachine::with_engine(
                Box::new(scrozz_record::engine::MockEngine::fully_capable(
                    scrozz_record::engine::MockSessionPlan::complete("mock.mp4", 2.0).unwrap(),
                )),
                settings,
            )
            .unwrap(),
        );
        app.recording
            .as_mut()
            .unwrap()
            .begin(CaptureTarget::AllDisplays)
            .unwrap();

        app.handle_recording_surface_action(RecordingSurfaceAction::Hud(RecordingHudAction::Pause));
        assert_eq!(
            app.recording_presentation().unwrap().hud.phase,
            RecordingPhase::Paused
        );
        app.handle_recording_surface_action(RecordingSurfaceAction::Hud(
            RecordingHudAction::Resume,
        ));
        assert_eq!(
            app.recording_presentation().unwrap().hud.phase,
            RecordingPhase::Recording
        );
    }

    #[test]
    fn editor_requires_real_decodable_media_and_rejects_initialisation_only_output() {
        let metadata = scrozz_record::RecordingMetadata {
            size: Some(scrozz_core::PhysicalSize::new(1920.0, 1080.0)),
            frames: Some(300),
            audio_channels: Some(2),
            ..Default::default()
        };
        let native = scrozz_record::Recording::native("real.mp4", 10.0, "native-test")
            .unwrap()
            .with_native_details(CaptureTarget::AllDisplays, metadata.clone())
            .unwrap();
        assert!(
            active_video_editor(&native, None).is_err(),
            "summary metadata must not make a nonexistent file look playable"
        );

        let partial = scrozz_record::Recording::native_partial_with_salvageability(
            "header-only.mp4",
            0.0,
            "native-test",
            scrozz_record::Salvageability::InitialisationOnly,
            "capture ended before media",
        )
        .unwrap()
        .with_native_details(CaptureTarget::AllDisplays, metadata)
        .unwrap();
        assert!(active_video_editor(&partial, Some(30)).unwrap().is_none());
    }

    #[test]
    fn editor_never_falls_back_to_mock_media() {
        let native = scrozz_record::Recording::native("real.mp4", 4.0, "native-test")
            .unwrap()
            .with_native_details(
                CaptureTarget::AllDisplays,
                scrozz_record::RecordingMetadata {
                    size: Some(scrozz_core::PhysicalSize::new(1280.0, 720.0)),
                    frames: Some(120),
                    audio_channels: Some(0),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(active_video_editor(&native, None).is_err());
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
