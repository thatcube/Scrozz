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
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Parser as _;
use scrozz_core::{Capture, Error as CoreError, LockEscape, SelectionCapabilities};
use scrozz_shell::{
    Accelerator, Capability, GlobalHotkeys, HotkeyEvent, KeyState, Permissions, ScreenshotSound,
    Session, SystemPermissions, Tray, TrayAction,
    hotkey::{DesiredBinding, Rejection},
    play_screenshot_sound,
};
use scrozz_store::CaptureId;

use crate::{
    after_capture::{
        ActionEffect, AfterCaptureAction, AfterCaptureSettings, AfterCaptureStore, InstallProfile,
        MediaKind, current_availability,
    },
    cli::{Cli, SettingsCommand},
    fault::{CliError, CliResult},
    gui::{
        action::{Action, CaptureKind, CaptureOrigin},
        card::{CardEvent, CardId, CardSurface},
        drag::{DragHost, DragSpot},
        permission::{
            self, Access, Effect as PermissionEffect, PendingCapture, PermissionStore,
            PickerAvailability, PickerMode, Response as PermissionResponse,
        },
        pipeline::{CaptureBytes, Job, Outcome, Pipeline},
        selection::CaptureSelector,
        server::{Forwarder, Server},
    },
    json::Json,
    report::Report,
    shortcuts::{ShortcutAction, ShortcutStore, Shortcuts},
};
use scrozz_shell::DragOutcome;
use scrozz_ui::{
    editor::{EditorUi, RevisionedFrame},
    permission::{
        PermissionPrompt as PermissionUiPrompt, PermissionStage as PermissionUiStage,
        PickerFallback,
    },
    settings::{
        AfterCaptureCell, AfterCaptureEdit, AfterCaptureMedia, AfterCaptureRow, ShortcutEdit,
        ShortcutRow,
    },
};

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
    /// Whether this run opens the persistent history store.
    pub history: bool,
    /// Ambient actions used only for captures initiated by the GUI.
    pub after_capture: AfterCaptureSettings,
    /// Where successful settings edits are persisted.
    pub after_capture_store: Option<AfterCaptureStore>,
    /// A startup problem that forced the action policy back to safe defaults.
    pub after_capture_warning: Option<String>,
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
            history: true,
            after_capture: AfterCaptureSettings::fresh(),
            after_capture_store: AfterCaptureStore::default_location().ok(),
            after_capture_warning: None,
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

        // What the user stored wins over what this build ships with.
        config.shortcuts = crate::settings::stored_shortcuts();
        config.bindings = bindings_from(&config.shortcuts);
        if let Some(store) = config.after_capture_store.as_ref() {
            let profile = store.inferred_profile();
            match store.load(profile) {
                Ok(settings) => config.after_capture = settings,
                Err(error) => {
                    config.after_capture_warning = Some(format!(
                        "After Capture settings could not be loaded; preserving the inferred profile defaults for this session: {error}"
                    ));
                    config.after_capture = match profile {
                        InstallProfile::Fresh => AfterCaptureSettings::fresh(),
                        InstallProfile::Existing => AfterCaptureSettings::legacy(),
                    };
                }
            }
        }
        match crate::settings::screenshot_sound(&config.after_capture) {
            Ok(sound) => config.screenshot_sound = sound,
            Err(error) => {
                let warning = format!(
                    "Screenshot sound settings are invalid; using 8-bit this session: {error}"
                );
                config.after_capture_warning = Some(
                    config
                        .after_capture_warning
                        .take()
                        .map_or(warning.clone(), |existing| format!("{existing}; {warning}")),
                );
            }
        }

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
            history: false,
            after_capture: AfterCaptureSettings::legacy(),
            after_capture_store: None,
            after_capture_warning: None,
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

fn capture_action_label(kind: CaptureKind) -> &'static str {
    match kind {
        CaptureKind::AllInOne => scrozz_core::product_copy::ALL_IN_ONE,
        CaptureKind::Region => scrozz_core::product_copy::CAPTURE_AREA,
        CaptureKind::Window => scrozz_core::product_copy::CAPTURE_WINDOW,
        CaptureKind::Fullscreen => scrozz_core::product_copy::CAPTURE_FULLSCREEN,
        CaptureKind::AllDisplays => scrozz_core::product_copy::CAPTURE_ALL_DISPLAYS,
    }
}

fn picker_fallback(prompt: permission::Prompt) -> PickerFallback {
    if let Some(mode) = prompt.picker_mode {
        let limitations = match mode {
            PickerMode::Window => {
                "Apple's picker replaces Scrozz's custom Capture Window UI and captures only the \
                 exact window you select. Capture Area, unattended global capture, and \
                 system-audio recording remain unavailable."
            }
            PickerMode::Display => {
                "Apple's picker replaces Scrozz's automatic screen targeting and captures only \
                 the exact screen you select. Capture Area, All Displays, unattended global \
                 capture, and system-audio recording remain unavailable."
            }
            PickerMode::WindowOrDisplay => {
                "Apple's picker replaces Scrozz's custom selector and captures only the exact \
                 window or screen you select. Capture Area, All Displays, unattended global \
                 capture, and system-audio recording remain unavailable."
            }
        };
        return PickerFallback::Available {
            limitations: limitations.to_owned(),
        };
    }

    let reason = match prompt.picker_availability {
        PickerAvailability::OlderMacOs => {
            "Apple's limited content picker requires macOS 14 or later. Direct access through \
             System Settings is the only supported capture path on this version."
        }
        PickerAvailability::Unavailable => {
            "Apple's limited content picker is unavailable in this session. Scrozz will not \
             broaden access or substitute another target."
        }
        PickerAvailability::Available => match prompt.pending.kind {
            CaptureKind::Region => {
                "Apple's picker cannot reproduce Scrozz's custom Capture Area behavior. Choose a \
                 Window or Screen capture, or grant direct access in System Settings."
            }
            CaptureKind::AllDisplays => {
                "Apple's picker authorizes one screen at a time, so it cannot truthfully produce \
                 an All Displays capture."
            }
            CaptureKind::AllInOne | CaptureKind::Window | CaptureKind::Fullscreen => {
                "Apple's picker cannot complete this capture without broadening the requested \
                 access."
            }
        },
    };
    PickerFallback::Unavailable {
        reason: reason.to_owned(),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Whether the host should keep going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Still running.
    Continue,
    /// Quit was asked for, or the deadline passed.
    Stop,
}

const ASSIGNMENT_EVENT_GUARD: Duration = Duration::from_millis(150);

struct AssignmentEventGuard {
    accelerator: Accelerator,
    expires: Instant,
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
    drag: DragHost,
    /// A native modal drag can consume mouse-up and modifier-release events.
    /// The window host clears that stale egui input before another interaction.
    modal_drag_input_release_pending: bool,
    pin_lock_escapes: Vec<LockEscape>,
    terminally_unpinned: HashSet<CaptureId>,
    suppress_locked_restores: bool,
    started: Instant,
    captures: u64,
    sound_warning_shown: bool,
    settings_requested: bool,
    editor_requests: VecDeque<EditorRequest>,
    /// Captures retained only because their editor is open, not by the overlay.
    editor_only_cards: HashSet<CardId>,
    /// The one editor revision currently being prepared on the worker.
    editor_render_pending: Option<(CardId, u64, u64)>,
    /// A failed version is not retried until the document or editor changes.
    editor_render_failed: Option<(CardId, u64, u64)>,
    next_editor_generation: u64,
    notes: Vec<String>,
    shortcuts: Shortcuts,
    shortcut_store: Option<ShortcutStore>,
    rejected: Vec<(ShortcutAction, String)>,
    assignment_guard: Option<AssignmentEventGuard>,
    session: Session,
    selection_capabilities: SelectionCapabilities,
    capture_backend_ready: bool,
    permission_ui_available: bool,
    permission: permission::Flow,
    permission_resume: Option<PendingCapture>,
    permission_store: Option<PermissionStore>,
    #[cfg(target_os = "macos")]
    apple_picker: Option<scrozz_capture::AppleContentPicker>,
    #[cfg(target_os = "macos")]
    picker_surface: Option<PickerSurfaceReservation>,
}

/// A capture the annotation editor has been asked to open.
#[derive(Debug)]
pub struct EditorRequest {
    /// The card it came from, so copy and save can be attributed and the
    /// finished image can be sent back through the same worker.
    pub card: CardId,
    /// Uniquely identifies this opening of the editor.
    pub generation: u64,
    /// The decoded capture.
    pub capture: Capture,
}

/// The live editor state available during one host pass.
#[derive(Clone, Copy)]
pub struct EditorSnapshot<'a> {
    card: CardId,
    generation: u64,
    editor: &'a EditorUi,
}

impl<'a> EditorSnapshot<'a> {
    /// Couples an editor with the card whose document it owns.
    #[must_use]
    pub const fn new(card: CardId, generation: u64, editor: &'a EditorUi) -> Self {
        Self {
            card,
            generation,
            editor,
        }
    }

    fn for_card(self, card: CardId) -> Option<Self> {
        (self.card == card).then_some(self)
    }

    fn render(self) -> CliResult<RevisionedFrame> {
        self.editor.render().map_err(CliError::Core)
    }
}

#[derive(Clone, Copy)]
enum CardOutput {
    Copy,
    Save,
}

impl CardOutput {
    const fn label(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Save => "save",
        }
    }
}

#[cfg(target_os = "macos")]
struct PickerSurfaceReservation {
    mode: PickerMode,
    ready: std::sync::mpsc::Receiver<scrozz_core::Result<()>>,
    release: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
    presented: bool,
}

#[cfg(target_os = "macos")]
impl PickerSurfaceReservation {
    fn start(mode: PickerMode, selector: Arc<dyn CaptureSelector>) -> CliResult<Self> {
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("scrozz-picker-surface".to_owned())
            .spawn(move || {
                let result = selector.begin_capture(false);
                let reserved = result.is_ok();
                let _ = ready_tx.send(result);
                if reserved {
                    let _ = release_rx.recv();
                    selector.capture_finished();
                }
            })
            .map_err(|error| {
                CliError::Core(CoreError::Platform(format!(
                    "could not start the picker surface reservation: {error}"
                )))
            })?;
        Ok(Self {
            mode,
            ready,
            release: Some(release),
            worker: Some(worker),
            presented: false,
        })
    }

    fn poll_ready(&mut self) -> Option<scrozz_core::Result<PickerMode>> {
        if self.presented {
            return None;
        }
        match self.ready.try_recv() {
            Ok(result) => {
                self.presented = result.is_ok();
                Some(result.map(|()| self.mode))
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(CoreError::Platform(
                "the picker surface reservation stopped without answering".to_owned(),
            ))),
        }
    }

    fn release(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        // `capture_finished` waits for the main loop to paint the restored
        // surface. Joining here would block that very loop. Dropping a Rust
        // JoinHandle detaches while the worker finishes the handshake.
        let _ = self.worker.take();
    }

    fn release_and_join(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
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
        permission_ui_available: bool,
    ) -> CliResult<Self> {
        let pipeline = Pipeline::start_with_history(Arc::clone(&selector), config.history)?;
        let mut notes = Vec::new();
        if let Some(warning) = config.after_capture_warning.clone() {
            notes.push(warning);
        }
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

        let (permission_store, dismissed_at_unix) = match PermissionStore::default_location() {
            Ok(store) => match store.load() {
                Ok(dismissed) => (Some(store), dismissed),
                Err(error) => {
                    notes.push(format!(
                        "permission dismissal history could not be read: {error}"
                    ));
                    (Some(store), None)
                }
            },
            Err(error) => {
                notes.push(format!(
                    "permission dismissals cannot be remembered: {error}"
                ));
                (None, None)
            }
        };

        let shortcuts = config.shortcuts.clone();
        let unlock_hotkey_registered = hotkeys
            .bindings()
            .any(|(_, action)| action == Action::UnlockPins.id())
            && hotkeys.is_bound_to_os();
        let pin_lock_escapes =
            established_lock_escapes(server.is_some(), tray.is_some(), unlock_hotkey_registered);
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
            drag: DragHost::new(),
            modal_drag_input_release_pending: false,
            pin_lock_escapes,
            terminally_unpinned: HashSet::new(),
            suppress_locked_restores: false,
            started: Instant::now(),
            captures: 0,
            sound_warning_shown: false,
            settings_requested: false,
            editor_requests: VecDeque::new(),
            editor_only_cards: HashSet::new(),
            editor_render_pending: None,
            editor_render_failed: None,
            next_editor_generation: 1,
            notes,
            shortcuts,
            shortcut_store,
            rejected: Vec::new(),
            assignment_guard: None,
            session,
            selection_capabilities,
            capture_backend_ready,
            permission_ui_available,
            permission: permission::Flow::new(dismissed_at_unix),
            permission_resume: None,
            permission_store,
            #[cfg(target_os = "macos")]
            apple_picker: None,
            #[cfg(target_os = "macos")]
            picker_surface: None,
        };
        app.refresh_tray_shortcuts();

        // A drag that was in flight when a previous run ended left its file
        // behind on purpose; nothing is ever coming back for it.
        let swept = app.drag.sweep_orphans();
        if swept > 0 {
            tracing::debug!(files = swept, "removed drag files from a previous run");
        }

        if let Some(kind) = app.config.capture_on_start {
            app.begin_capture(CaptureOrigin::Startup, kind);
        }

        Ok(app)
    }

    /// Routes that were successfully established outside pinned windows.
    pub(crate) fn pin_lock_escapes(&self) -> &[LockEscape] {
        &self.pin_lock_escapes
    }

    /// Services every source once. Never blocks.
    ///
    /// Order matters slightly: input first, so a capture asked for on this tick
    /// is already in flight before outcomes are drained, and card events last,
    /// so a card presented on this tick can be acted on immediately.
    pub fn tick(&mut self) -> Tick {
        self.tick_with_editor(None)
    }

    /// Services every source once with the current editor document available to
    /// card output actions.
    ///
    /// A card being edited must never fall back to its original cached pixels:
    /// that would let Copy or Save bypass destructive redactions. The snapshot
    /// is borrowed only for this pass and rendered into an immutable,
    /// revision-tagged frame before it is sent to the worker.
    pub fn tick_with_editor(&mut self, editor: Option<EditorSnapshot<'_>>) -> Tick {
        if self.expired() {
            self.note("the run deadline passed");
            return Tick::Stop;
        }

        self.drain_permission();

        if self.drain_tray() == Tick::Stop {
            return Tick::Stop;
        }
        self.drain_hotkeys();
        if self.drain_server() == Tick::Stop {
            return Tick::Stop;
        }
        self.drain_pipeline();
        self.drain_cards(editor);
        self.drain_drags();

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
            if self.perform_from(CaptureOrigin::MenuBar, Action::from_tray(entry)) == Tick::Stop {
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
            if before != 0 && !self.hotkeys.is_suspended() {
                // A previous release was partial. The host calls this owner sync
                // every frame, so keep retrying without repeating the warning.
                let _ = self.hotkeys.suspend();
            }
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
            let report = self.hotkeys.suspend();
            if !report.is_complete() {
                for rejection in report.rejections() {
                    tracing::warn!(
                        action = %rejection.action,
                        accelerator = %rejection.accelerator,
                        reason = %rejection.reason,
                        "a global hotkey remained active under an in-app keyboard owner"
                    );
                }
                self.note(format!(
                    "{} shortcut(s) could not be released for the active keyboard owner",
                    report.rejections().len()
                ));
            }
        }
    }

    /// Whether the global hotkeys are currently stood down.
    #[must_use]
    pub fn hotkeys_suspended(&self) -> bool {
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
            pending.push(event);
        }

        for event in pending {
            if let Some(action) = self.action_for_hotkey_event(event) {
                self.perform_from(CaptureOrigin::GlobalHotkey, action);
            }
        }
    }

    fn action_for_hotkey_event(&mut self, event: HotkeyEvent) -> Option<Action> {
        if self.assignment_guard.as_ref().is_some_and(|guard| {
            guard.accelerator == event.accelerator && Instant::now() <= guard.expires
        }) {
            if event.state == KeyState::Released {
                self.assignment_guard = None;
            }
            return None;
        }
        self.assignment_guard
            .take_if(|guard| Instant::now() > guard.expires);

        // Both edges arrive. Acting on the release as well would take two
        // captures per press.
        if event.state != KeyState::Pressed {
            return None;
        }

        let action = Action::from_id(&event.action);
        if action.is_none() {
            tracing::warn!(action = %event.action, "a hotkey fired for an action this build does not know");
        }
        action
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
            let mut with_argv0 = Vec::with_capacity(request.argv.len() + 1);
            with_argv0.push("scrozz".to_owned());
            with_argv0.extend(request.argv.iter().cloned());
            let needs_live_pin_state = Cli::try_parse_from(with_argv0)
                .ok()
                .and_then(|cli| cli.command)
                .is_some_and(|command| {
                    forwarded_unpin(&command).is_some() || forwarded_unlock_pins(&command)
                });

            if needs_live_pin_state {
                let command = request.serve_with(|command| {
                    if let Some(id) = forwarded_unpin(command) {
                        let capture = CaptureId(id.to_owned());
                        self.terminally_unpinned.insert(capture.clone());
                        self.surface.discard_pin(&capture);
                        self.pipeline.terminal_unpin(capture)?;
                        self.note(format!("pinned capture {id} closed after forwarded unpin"));
                    }
                    if forwarded_unlock_pins(command) {
                        self.unlock_all_pins()?;
                        self.note("pinned captures unlocked from the command line");
                    }
                    Ok(())
                });
                if let Some(command) = command {
                    self.observe_forwarded_command(&command);
                }
                continue;
            }

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
            self.observe_forwarded_command(&command);
        }

        // A forwarded command never ends this process. `scrozz quit` is not a
        // command; quitting is a menu entry, because the thing being quit is
        // the application the user can see, not a request that arrived on a
        // socket.
        Tick::Continue
    }

    fn observe_forwarded_command(&mut self, command: &crate::cli::Command) {
        self.captures += u64::from(matches!(command, crate::cli::Command::Capture(_)));
        if matches!(
            command,
            crate::cli::Command::Settings(crate::cli::SettingsArgs {
                command: SettingsCommand::Set { .. }
            })
        ) {
            self.reload_persisted_settings();
        }
        if matches!(command, crate::cli::Command::Gui) {
            self.note("a second launch was answered by this instance");
        }
    }

    fn reload_persisted_settings(&mut self) {
        let Some(store) = self.config.after_capture_store.clone() else {
            self.note("persisted settings changed but no config path is available to reload");
            return;
        };
        match store.load(store.inferred_profile()) {
            Ok(settings) => match crate::settings::screenshot_sound(&settings) {
                Ok(sound) => {
                    self.config.after_capture = settings;
                    self.config.screenshot_sound = sound;
                    self.note("persisted settings reloaded");
                }
                Err(error) => {
                    self.note(format!(
                        "persisted settings changed but could not be applied: {error}"
                    ));
                }
            },
            Err(error) => {
                self.note(format!(
                    "persisted settings changed but could not be reloaded: {error}"
                ));
            }
        }
    }

    fn drain_pipeline(&mut self) {
        while let Some(outcome) = self.pipeline.poll() {
            match outcome {
                Outcome::Ready(ready) => {
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
                    let ready = *ready;
                    let card = ready.card;
                    let card_id = card.id;
                    let summary = card.summary();
                    for (action, error) in ready.actions.failures() {
                        self.note(format!("{card_id} {} failed: {error}", action.label()));
                    }
                    for step in &ready.actions.steps {
                        match &step.outcome {
                            crate::after_capture::ActionOutcome::Succeeded(
                                ActionEffect::Completed,
                            ) if step.action == AfterCaptureAction::CopyToClipboard => {
                                self.note(format!("{card_id} copied to the clipboard"));
                            }
                            crate::after_capture::ActionOutcome::Succeeded(
                                ActionEffect::Saved(path),
                            ) => {
                                self.note(format!("{card_id} saved to {}", path.display()));
                            }
                            crate::after_capture::ActionOutcome::Succeeded(
                                ActionEffect::Uploaded(url),
                            ) => {
                                self.note(format!("{card_id} uploaded to {url}"));
                            }
                            _ => {}
                        }
                    }

                    let show_overlay = ready
                        .actions
                        .has_effect(&ActionEffect::ShowRecentCapturesOverlay);
                    let open_editor = ready.actions.has_effect(&ActionEffect::OpenEditor);
                    let mut retained = false;
                    if show_overlay {
                        if let Err(err) = self.surface.present(card) {
                            self.note(format!(
                                "{card_id} could not be shown in Recent Captures Overlay: {err}"
                            ));
                        } else {
                            retained = true;
                            self.note(summary);
                        }
                    } else {
                        self.note(summary);
                    }
                    if open_editor {
                        if self.pipeline.post(Job::Open(card_id)) {
                            retained = true;
                            if !show_overlay {
                                self.editor_only_cards.insert(card_id);
                            }
                        } else {
                            self.note(format!(
                                "{card_id} could not be queued for Open Editor: the capture worker has gone"
                            ));
                        }
                    }
                    if !retained {
                        self.pipeline.post(Job::Release(card_id));
                    }
                }
                Outcome::Failed {
                    card,
                    kind,
                    origin,
                    error,
                } => {
                    let permission_denied =
                        matches!(&error, CliError::Core(CoreError::PermissionDenied { .. }));
                    self.note(format!("{card} failed: {error}"));
                    if permission_denied {
                        self.handle_capture_permission_failure(kind, origin, Self::screen_access());
                    }
                }
                Outcome::Done { card, detail } => {
                    self.note(format!("{card} {detail}"));
                }
                Outcome::Opened { card, capture } => {
                    let generation = self.next_editor_generation;
                    self.next_editor_generation = self.next_editor_generation.wrapping_add(1);
                    self.editor_requests.push_back(EditorRequest {
                        card,
                        generation,
                        capture: *capture,
                    });
                }
                Outcome::Prepared {
                    card,
                    generation,
                    revision,
                } => {
                    let version = (card, generation, revision);
                    if self.editor_render_pending == Some(version) {
                        self.editor_render_pending = None;
                    }
                    if self.editor_render_failed == Some(version) {
                        self.editor_render_failed = None;
                    }
                }
                Outcome::PreparationFailed {
                    card,
                    generation,
                    revision,
                    error,
                } => {
                    let version = (card, generation, revision);
                    if self.editor_render_pending == Some(version) {
                        self.editor_render_pending = None;
                    }
                    self.editor_render_failed = Some(version);
                    self.note(format!(
                        "{card} editor {generation} revision {revision} could not be prepared for \
                         drag: {error}"
                    ));
                }
                Outcome::Refused { card, error } => {
                    self.note(format!("{card} refused: {error}"));
                }
                Outcome::PinReady(mut pin) => {
                    if self.terminally_unpinned.contains(&pin.id) {
                        continue;
                    }
                    if self.suppress_locked_restores {
                        pin.state.locked = false;
                    }
                    let capture = pin.id.0.clone();
                    if let Err(err) = self.surface.restore_pin(*pin) {
                        self.note(format!(
                            "pinned capture {capture} could not be shown: {err}"
                        ));
                    } else {
                        self.note(format!("pinned capture {capture} restored"));
                    }
                }
                Outcome::PinTextureReady { capture, texture } => {
                    if self.terminally_unpinned.contains(&capture) {
                        continue;
                    }
                    if let Err(err) = self.surface.refresh_pin_texture(&capture, texture) {
                        self.note(format!(
                            "pinned capture {} texture could not be refreshed: {err}",
                            capture.0
                        ));
                    }
                }
                Outcome::PinCreationFailed { capture, error } => {
                    self.surface.discard_pin(&capture);
                    self.note(format!(
                        "pinned capture {} was closed because it could not be persisted: {error}",
                        capture.0
                    ));
                }
                Outcome::PinPersistenceFailed { capture, error } => {
                    self.note(format!(
                        "pinned capture {} could not be persisted: {error}",
                        capture.0
                    ));
                }
            }
        }
    }

    fn drain_cards(&mut self, editor: Option<EditorSnapshot<'_>>) {
        let mut pending = Vec::new();
        while let Some(event) = self.surface.poll() {
            pending.push(event);
        }

        for event in pending {
            match event {
                CardEvent::Copy(id) => {
                    self.post_card_output(id, CardOutput::Copy, editor);
                }
                CardEvent::Save(id) => {
                    self.post_card_output(id, CardOutput::Save, editor);
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
                CardEvent::Drag { card, at } => self.begin_drag(card, at, editor),
                // Collapsing into the dock is the capture stack's own animation
                // and belongs to the surface that raised the event once there
                // is one that can perform it.
                CardEvent::Collapse(id) => {
                    self.note(format!("{id}: {event:?} is not routed yet"));
                }
                CardEvent::Pin(id, capture, state) => {
                    if self.terminally_unpinned.contains(&capture) {
                        self.pipeline.post(Job::Release(id));
                        continue;
                    }
                    self.pipeline.post(Job::PinCard {
                        card: id,
                        capture,
                        state,
                    });
                    self.note(format!("{id} pinned"));
                }
                CardEvent::PinChanged(capture, state) => {
                    if self.terminally_unpinned.contains(&capture) {
                        continue;
                    }
                    self.pipeline.post(Job::SetPin {
                        capture,
                        state: Some(state),
                    });
                }
                CardEvent::Unpin(capture) => {
                    self.terminally_unpinned.insert(capture.clone());
                    self.pipeline.post(Job::SetPin {
                        capture: capture.clone(),
                        state: None,
                    });
                    self.note(format!("pinned capture {} closed", capture.0));
                }
                CardEvent::PinUnavailable { card, reason } => {
                    self.note(format!("{card} could not be pinned: {reason}"));
                }
                CardEvent::PinPositioningUnavailable { capture, reason } => {
                    self.note(format!(
                        "pinned capture {} cannot be positioned: {reason}",
                        capture.0
                    ));
                }
            }
        }
    }

    fn post_card_output(
        &mut self,
        card: CardId,
        output: CardOutput,
        editor: Option<EditorSnapshot<'_>>,
    ) {
        let job = match Self::card_output_job(card, output, editor) {
            Ok(job) => job,
            Err(error) => {
                self.note(format!(
                    "{card} could not be rendered for {}: {error}",
                    output.label()
                ));
                return;
            }
        };

        if !self.pipeline.post(job) {
            self.note(format!(
                "{card} could not be queued for {}: the capture worker has gone",
                output.label()
            ));
        }
    }

    fn card_output_job(
        card: CardId,
        output: CardOutput,
        editor: Option<EditorSnapshot<'_>>,
    ) -> CliResult<Job> {
        let Some(editor) = editor.and_then(|editor| editor.for_card(card)) else {
            return Ok(match output {
                CardOutput::Copy => Job::Copy(card),
                CardOutput::Save => Job::Save(card),
            });
        };

        let rendered = Box::new(editor.render()?);
        Ok(match output {
            CardOutput::Copy => Job::CopyImage { card, rendered },
            CardOutput::Save => Job::SaveImage { card, rendered },
        })
    }

    /// Starts any native drag the surface armed during the frame just drawn.
    ///
    /// **Must be called from the UI pass, immediately after the surface has
    /// drawn.** `beginDraggingSessionWithItems:` on macOS and `DoDragDrop` on
    /// Windows both seize the mouse from where it is *now*, so they only work
    /// while the button the user is holding is still down. [`Self::tick`] runs
    /// in the host's logic pass, which precedes the UI pass that produces the
    /// gesture — draining there means acting a whole frame late, on a button
    /// that may already be up. That is the bug this method exists to close, and
    /// it is why the drag path is the one thing that does not wait its turn.
    ///
    /// Returns how many drags were started, which is what the ordering test
    /// asserts on.
    pub fn pump_drag_starts(&mut self) -> usize {
        self.pump_drag_starts_with_editor(None)
    }

    /// Starts native drags with the current editor document available.
    ///
    /// If the dragged card is being edited, the drag payload is rendered from
    /// that exact revision. A failed render refuses the drag; it never falls
    /// back to the vault's original pixels and risk exposing data under a
    /// destructive redaction.
    pub fn pump_drag_starts_with_editor(&mut self, editor: Option<EditorSnapshot<'_>>) -> usize {
        let armed = self.surface.poll_drag_starts();
        let mut started = 0;
        for event in armed {
            if let CardEvent::Drag { card, at } = event {
                self.begin_drag(card, at, editor);
                started += 1;
            }
        }
        started
    }

    /// Hands a card to the platform's drag machinery.
    ///
    /// Called while the mouse button is still down — see [`crate::gui::drag`]
    /// for why that is the only moment this can work. The card stays where it
    /// is; [`Self::drain_drags`] removes it if and only if something accepts
    /// the drop.
    fn begin_drag(&mut self, card: CardId, at: DragSpot, editor: Option<EditorSnapshot<'_>>) {
        if !self.drag.is_attached()
            && let Some(surface) = self.surface.native_surface()
        {
            self.drag.attach(surface);
        }

        let bytes = match self.drag_bytes(card, editor) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.surface.settle_drag(card, false);
                self.modal_drag_input_release_pending = true;
                self.note(format!("{card} could not be rendered for drag: {error}"));
                return;
            }
        };

        match self.drag.begin(card, at, &bytes) {
            Ok(()) => tracing::debug!(
                %card,
                revision = bytes.revision(),
                "drag started from a rendered document revision"
            ),
            // Visible, not silent: a drag that quietly does nothing is exactly
            // the failure this whole path exists to remove.
            Err(why) => {
                self.surface.settle_drag(card, false);
                self.modal_drag_input_release_pending = true;
                self.note(format!("{card} could not be dragged: {why}"));
            }
        }
    }

    fn drag_bytes(
        &self,
        card: CardId,
        editor: Option<EditorSnapshot<'_>>,
    ) -> CliResult<CaptureBytes> {
        if let Some(editor) = editor.and_then(|editor| editor.for_card(card)) {
            let revision = editor.editor.state().revision();
            return self
                .pipeline
                .captures()
                .get_revision(card, editor.generation, revision)
                .ok_or_else(|| {
                    CliError::usage(format!(
                        "{card} editor {} revision {revision} is still being prepared for drag",
                        editor.generation
                    ))
                });
        }

        self.pipeline
            .captures()
            .get(card)
            .ok_or_else(|| CliError::usage(format!("{card} has no capture to drag")))
    }

    /// Services drags already under way.
    ///
    /// Three jobs: release the surface's gesture whatever happened, take a card
    /// off the stack once its drop was accepted, and keep sweeping the
    /// temporary file afterwards until the retention window closes.
    ///
    /// The gesture release is unconditional and comes first. The platform's
    /// drag loop is modal and may have eaten the mouse-up the surface was
    /// waiting for, so this is the only reliable moment the card is known to be
    /// free — and a card left armed can never be dragged again.
    fn drain_drags(&mut self) {
        for (card, outcome) in self.drag.poll() {
            let accepted = matches!(outcome, DragOutcome::Accepted { .. });
            self.surface.settle_drag(card, accepted);
            self.modal_drag_input_release_pending = true;

            match outcome {
                DragOutcome::Accepted { .. } => {
                    self.surface.dismiss(card);
                    self.pipeline.post(Job::Release(card));
                    self.note(format!("{card} dropped"));
                }
                // The card stays. Said out loud rather than logged quietly:
                // "I dragged it and nothing happened" is the complaint this
                // whole path exists to answer, and the three reasons it can
                // happen are worth telling apart.
                DragOutcome::Cancelled => {
                    tracing::debug!(%card, "drag cancelled");
                }
                DragOutcome::Rejected => {
                    self.note(format!("{card} was not accepted there"));
                }
                DragOutcome::Failed(why) => {
                    self.note(format!("{card} could not be dropped: {why}"));
                }
                // The enum is `#[non_exhaustive]`. A future outcome this build
                // has never heard of still has to release the gesture, which it
                // already did above — so the only thing left is to say so.
                other => {
                    tracing::warn!(%card, ?other, "drag ended in an unrecognised way");
                }
            }
        }
    }

    /// Takes the request to retire input swallowed by a completed native drag.
    ///
    /// AppKit and `DoDragDrop` own a modal loop and may consume the release edge
    /// for both the mouse button and drag modifiers. The eframe host must clear
    /// those persistent states before a selector waits for quiescent input.
    #[must_use]
    pub fn take_modal_drag_input_release(&mut self) -> bool {
        std::mem::take(&mut self.modal_drag_input_release_pending)
    }

    /// Carries out one action.
    fn perform(&mut self, action: Action) -> Tick {
        self.perform_from(CaptureOrigin::Direct, action)
    }

    fn perform_from(&mut self, origin: CaptureOrigin, action: Action) -> Tick {
        tracing::debug!(action = action.id(), origin = origin.label(), "performing");
        match action {
            Action::Capture(kind) => {
                self.begin_capture(origin, kind);
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
            Action::UnlockPins => {
                match self.unlock_all_pins() {
                    Ok(_) => self.note("pinned captures unlocked"),
                    Err(error) => {
                        self.note(format!("pinned captures could not be unlocked: {error}"))
                    }
                }
                Tick::Continue
            }
            Action::Quit => {
                self.note("quit");
                Tick::Stop
            }
        }
    }

    fn begin_capture(&mut self, origin: CaptureOrigin, kind: CaptureKind) {
        let pending = PendingCapture::new(kind, origin);
        let access = Self::screen_access();
        if access != Access::Granted && !self.permission_ui_available {
            self.note_permission_unavailable(kind, access);
            return;
        }
        if self.permission_resume.is_some() {
            self.note("the granted capture is waiting for the permission window to close");
            return;
        }
        if self.permission.has_pending_action() {
            self.note("a capture permission choice is already in progress");
            return;
        }
        let prompt_before = self.permission.prompt();
        let effect =
            self.permission
                .begin(pending, access, Self::picker_availability(), unix_now());
        self.apply_permission_effect(effect);
        if prompt_before.is_none() && self.permission.prompt().is_some() {
            if let Err(error) = scrozz_shell::permissions::activate_application() {
                self.note(format!(
                    "capture permission UI could not be foregrounded: {error}"
                ));
            }
        } else if origin == CaptureOrigin::Startup && !matches!(access, Access::Granted) {
            self.note("startup capture skipped: Screen Recording access is not granted");
        } else if origin == CaptureOrigin::GlobalHotkey
            && !matches!(access, Access::Granted)
            && matches!(effect, PermissionEffect::None)
        {
            self.note(
                "capture permission reminder is snoozed; choose a Capture command from the \
                 Scrozz menu to retry now",
            );
        }
    }

    fn note_permission_unavailable(&mut self, kind: CaptureKind, access: Access) {
        let detail = match access {
            Access::NotGranted => format!(
                "requires Screen Recording access; grant it in {}",
                scrozz_shell::permissions::remedy(Capability::ScreenRecording)
            ),
            Access::Restricted => {
                "is restricted by device management or parental controls".to_owned()
            }
            Access::Unavailable => {
                "requires the macOS 14 ScreenCaptureKit still-capture API".to_owned()
            }
            Access::Granted => {
                "lost access while macOS still reports the grant; retry the action".to_owned()
            }
        };
        self.note(format!("{} {detail}", capture_action_label(kind)));
    }

    fn handle_capture_permission_failure(
        &mut self,
        kind: CaptureKind,
        origin: CaptureOrigin,
        access: Access,
    ) {
        if !self.permission_ui_available {
            self.note_permission_unavailable(kind, access);
            return;
        }
        let prompt_before = self.permission.prompt();
        let effect = self.permission.capture_access_revoked(
            PendingCapture::new(kind, origin),
            access,
            Self::picker_availability(),
            unix_now(),
        );
        self.apply_permission_effect(effect);
        if prompt_before.is_none()
            && self.permission.prompt().is_some()
            && let Err(error) = scrozz_shell::permissions::activate_application()
        {
            self.note(format!(
                "capture permission UI could not be foregrounded: {error}"
            ));
        }
    }

    fn queue_direct_capture(&mut self, pending: PendingCapture) {
        let card = self.pipeline.allocate();
        tracing::debug!(
            %card,
            capture = pending.kind.label(),
            origin = pending.origin.label(),
            "capture job queued"
        );
        if !self.pipeline.post(Job::Capture {
            kind: pending.kind,
            origin: pending.origin,
            card,
            policy: self.config.after_capture.clone(),
        }) {
            self.note("the capture worker has gone");
        }
    }

    #[cfg(target_os = "macos")]
    fn queue_apple_picker_capture(
        &mut self,
        pending: PendingCapture,
        capture: scrozz_capture::PickerCapture,
    ) {
        let card = self.pipeline.allocate();
        tracing::debug!(
            %card,
            capture = pending.kind.label(),
            origin = pending.origin.label(),
            "Apple picker capture job queued"
        );
        if !self.pipeline.post(Job::ApplePickerCapture {
            kind: pending.kind,
            origin: pending.origin,
            card,
            capture,
            policy: self.config.after_capture.clone(),
        }) {
            self.note("the capture worker has gone");
        }
    }

    fn drain_permission(&mut self) {
        let effect = self.permission.application_active_changed(
            scrozz_shell::permissions::application_is_active(),
            Self::screen_access(),
        );
        self.apply_permission_effect(effect);

        #[cfg(target_os = "macos")]
        {
            let reservation = self
                .picker_surface
                .as_mut()
                .and_then(PickerSurfaceReservation::poll_ready);
            match reservation {
                Some(Ok(mode)) => self.present_apple_picker_now(mode),
                Some(Err(error)) => {
                    self.permission.apple_picker_failed();
                    self.finish_picker_surface();
                    self.note(format!(
                        "Scrozz could not hide its surfaces for Apple's picker: {error}"
                    ));
                }
                None => {}
            }

            let event = self
                .apple_picker
                .as_ref()
                .and_then(scrozz_capture::AppleContentPicker::poll);
            match event {
                Some(scrozz_capture::ApplePickerEvent::Captured(capture)) => {
                    self.finish_picker_surface();
                    if let Some(pending) = self.permission.apple_picker_captured() {
                        self.queue_apple_picker_capture(pending, capture);
                    } else {
                        tracing::warn!(
                            "discarding an Apple picker selection with no pending action"
                        );
                    }
                }
                Some(scrozz_capture::ApplePickerEvent::Cancelled) => {
                    self.finish_picker_surface();
                    let effect = self.permission.apple_picker_cancelled(unix_now());
                    self.apply_permission_effect(effect);
                    self.note("Apple's content picker was cancelled");
                }
                Some(scrozz_capture::ApplePickerEvent::Failed(error)) => {
                    self.finish_picker_surface();
                    self.apple_picker = None;
                    self.permission.apple_picker_failed();
                    self.note(format!("Apple's content picker failed: {error}"));
                }
                None => {}
            }
        }
    }

    fn apply_permission_effect(&mut self, initial: PermissionEffect) {
        let mut effect = initial;
        loop {
            effect = match effect {
                PermissionEffect::None => break,
                PermissionEffect::RunDirect(pending) => {
                    self.queue_direct_capture(pending);
                    PermissionEffect::None
                }
                PermissionEffect::RunDirectAfterPermission(pending) => {
                    if self.permission_resume.replace(pending).is_some() {
                        self.note("a pending permission resume was replaced before it ran");
                    }
                    PermissionEffect::None
                }
                PermissionEffect::RequestSystemAccess => {
                    let permissions = SystemPermissions::new();
                    if let Err(error) = permissions.request(Capability::ScreenRecording)
                        && !matches!(error, CoreError::PermissionDenied { .. })
                    {
                        self.note(format!(
                            "macOS could not present Screen Recording access: {error}"
                        ));
                    }
                    self.permission
                        .system_request_finished(Self::screen_access())
                }
                PermissionEffect::OpenSystemSettings => {
                    if let Err(error) =
                        scrozz_shell::permissions::open_settings(Capability::ScreenRecording)
                    {
                        self.permission.settings_open_failed();
                        self.note(format!("System Settings could not be opened: {error}"));
                    }
                    PermissionEffect::None
                }
                PermissionEffect::PresentApplePicker(mode) => {
                    self.prepare_apple_picker(mode);
                    PermissionEffect::None
                }
                PermissionEffect::RememberDismissal(at) => {
                    if let Some(store) = &self.permission_store
                        && let Err(error) = store.save(at)
                    {
                        self.note(format!(
                            "permission dismissal could not be remembered: {error}"
                        ));
                    }
                    PermissionEffect::None
                }
            };
        }
    }

    fn prepare_apple_picker(&mut self, mode: PickerMode) {
        #[cfg(target_os = "macos")]
        {
            if self.picker_surface.is_some() {
                self.permission.apple_picker_failed();
                self.note("Apple's content picker is already being prepared");
                return;
            }
            match PickerSurfaceReservation::start(mode, Arc::clone(&self.selector)) {
                Ok(reservation) => {
                    self.picker_surface = Some(reservation);
                }
                Err(error) => {
                    self.permission.apple_picker_failed();
                    self.note(format!(
                        "Scrozz could not prepare its surfaces for Apple's picker: {error}"
                    ));
                }
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = mode;
            self.permission.apple_picker_failed();
            self.note("Apple's content picker exists only on macOS");
        }
    }

    #[cfg(target_os = "macos")]
    fn present_apple_picker_now(&mut self, mode: PickerMode) {
        if let Err(error) = scrozz_shell::permissions::activate_application() {
            self.permission.apple_picker_failed();
            self.finish_picker_surface();
            self.note(format!("Apple's picker could not be foregrounded: {error}"));
            return;
        }

        let picker = match self.apple_picker.take() {
            Some(picker) => picker,
            None => match scrozz_capture::AppleContentPicker::new() {
                Ok(picker) => picker,
                Err(error) => {
                    self.permission.apple_picker_failed();
                    self.finish_picker_surface();
                    self.note(format!("Apple's content picker is unavailable: {error}"));
                    return;
                }
            },
        };
        let native_mode = match mode {
            PickerMode::Window => scrozz_capture::ApplePickerMode::Window,
            PickerMode::Display => scrozz_capture::ApplePickerMode::Display,
            PickerMode::WindowOrDisplay => scrozz_capture::ApplePickerMode::WindowOrDisplay,
        };
        match picker.present(native_mode) {
            Ok(()) => {
                self.apple_picker = Some(picker);
            }
            Err(error) => {
                self.permission.apple_picker_failed();
                self.finish_picker_surface();
                self.note(format!("Apple's content picker could not open: {error}"));
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn finish_picker_surface(&mut self) {
        if let Some(reservation) = self.picker_surface.take() {
            reservation.release();
        }
    }

    #[cfg(target_os = "macos")]
    fn finish_picker_surface_and_join(&mut self) {
        if let Some(reservation) = self.picker_surface.take() {
            reservation.release_and_join();
        }
    }

    fn screen_access() -> Access {
        #[cfg(target_os = "macos")]
        if !scrozz_capture::still_capture_available() {
            return Access::Unavailable;
        }
        if SystemPermissions::new().is_granted(Capability::ScreenRecording) {
            Access::Granted
        } else {
            Access::NotGranted
        }
    }

    fn picker_availability() -> PickerAvailability {
        #[cfg(target_os = "macos")]
        {
            match scrozz_capture::AppleContentPicker::availability() {
                scrozz_capture::ApplePickerAvailability::Available => PickerAvailability::Available,
                scrozz_capture::ApplePickerAvailability::OlderMacOs => {
                    PickerAvailability::OlderMacOs
                }
                scrozz_capture::ApplePickerAvailability::Unavailable => {
                    PickerAvailability::Unavailable
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            PickerAvailability::Unavailable
        }
    }

    /// The permission surface model for the window host.
    #[must_use]
    pub fn permission_prompt(&self) -> Option<PermissionUiPrompt> {
        self.permission.prompt().map(|prompt| PermissionUiPrompt {
            stage: match prompt.stage {
                permission::PromptStage::Preflight => PermissionUiStage::Preflight,
                permission::PromptStage::Denied => PermissionUiStage::Denied,
                permission::PromptStage::WaitingForSettings => {
                    PermissionUiStage::WaitingForSettings
                }
                permission::PromptStage::Restricted => PermissionUiStage::Restricted,
                permission::PromptStage::Unavailable => PermissionUiStage::Unavailable,
            },
            action: capture_action_label(prompt.pending.kind).to_owned(),
            picker: picker_fallback(prompt),
        })
    }

    /// Applies one response from the permission window.
    pub fn respond_to_permission(&mut self, response: scrozz_ui::permission::PermissionResponse) {
        let response = match response {
            scrozz_ui::permission::PermissionResponse::Continue => PermissionResponse::Continue,
            scrozz_ui::permission::PermissionResponse::UseApplePicker => {
                PermissionResponse::UseApplePicker
            }
            scrozz_ui::permission::PermissionResponse::OpenSystemSettings => {
                PermissionResponse::OpenSystemSettings
            }
            scrozz_ui::permission::PermissionResponse::NotNow => PermissionResponse::NotNow,
        };
        let effect = self.permission.respond(response, unix_now());
        self.apply_permission_effect(effect);
    }

    /// Whether a granted action is waiting for the permission viewport to close.
    #[must_use]
    pub const fn has_permission_resume(&self) -> bool {
        self.permission_resume.is_some()
    }

    /// Queues the exact action after the host has completed one close frame.
    pub fn dispatch_permission_resume(&mut self) {
        if let Some(pending) = self.permission_resume.take() {
            self.queue_direct_capture(pending);
        }
    }

    fn unlock_all_pins(&mut self) -> CliResult<u64> {
        self.suppress_locked_restores = true;
        self.surface.unlock_pins();
        self.pipeline.unlock_pins()
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
                    advisory: (cfg!(target_os = "macos")
                        && Accelerator::parse(accelerator)
                            .is_ok_and(|parsed| parsed.system_owner().is_some()))
                    .then_some(
                        "May conflict with a macOS shortcut. If it does not fire, review System \
                         Settings › Keyboard › Keyboard Shortcuts."
                            .to_owned(),
                    ),
                }
            })
            .collect()
    }

    /// The platform-resolved After Capture matrix shown in Settings.
    #[must_use]
    pub fn after_capture_rows(&self) -> Vec<AfterCaptureRow> {
        let cell = |media, action| {
            let availability = current_availability(media, action);
            AfterCaptureCell {
                enabled: self.config.after_capture.is_enabled(media, action),
                available: availability.available,
                unavailable_reason: availability.reason.map(str::to_owned),
            }
        };
        AfterCaptureAction::UI_ORDER
            .into_iter()
            .map(|action| AfterCaptureRow {
                screenshot_id: action.setting_key(MediaKind::Screenshot),
                recording_id: action
                    .is_contract_available(MediaKind::Recording)
                    .then(|| action.setting_key(MediaKind::Recording)),
                label: action.label().to_owned(),
                description: action.description().to_owned(),
                screenshot: cell(MediaKind::Screenshot, action),
                recording: cell(MediaKind::Recording, action),
            })
            .collect()
    }

    /// Persists accepted After Capture edits before putting them in force.
    pub fn edit_after_capture(&mut self, edits: &[AfterCaptureEdit]) {
        if edits.is_empty() {
            return;
        }
        let mut changes = Vec::new();
        for edit in edits {
            let Some((media, action)) = AfterCaptureSettings::resolve_key(&edit.id) else {
                self.note(format!(
                    "unknown After Capture setting {:?} was ignored",
                    edit.id
                ));
                continue;
            };
            let expected = match edit.media {
                AfterCaptureMedia::Screenshot => MediaKind::Screenshot,
                AfterCaptureMedia::Recording => MediaKind::Recording,
            };
            if media != expected {
                self.note(format!(
                    "{} was not changed because it arrived from the wrong settings column",
                    edit.id
                ));
                continue;
            }
            let availability = current_availability(media, action);
            if edit.enabled && !availability.available {
                self.note(format!(
                    "{} was not enabled: {}",
                    edit.id,
                    availability
                        .reason
                        .unwrap_or("the action is unavailable in this build")
                ));
                continue;
            }
            changes.push((media, action, edit.enabled));
        }
        if changes.is_empty() {
            return;
        }
        let Some(store) = self.config.after_capture_store.clone() else {
            self.note(
                "After Capture settings were not changed because no config directory is available",
            );
            return;
        };
        match store.update(store.inferred_profile(), |latest| {
            for &(media, action, enabled) in &changes {
                latest.set(media, action, enabled);
            }
            Ok(())
        }) {
            Ok(updated) => {
                self.config.after_capture = updated;
                self.note("After Capture settings saved");
            }
            Err(error) => {
                self.note(format!("After Capture settings were not saved: {error}"));
            }
        }
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
        let mut recorded = None;
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
                    recorded = candidate
                        .get(action)
                        .and_then(|raw| Accelerator::parse(raw).ok());
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

        match self.register_shortcuts(&candidate) {
            Ok(()) => {
                self.assignment_guard = recorded.map(|accelerator| AssignmentEventGuard {
                    accelerator,
                    expires: Instant::now() + ASSIGNMENT_EVENT_GUARD,
                });
                self.commit_shortcuts(candidate);
                self.rejected.clear();
            }
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

    fn register_shortcuts(&mut self, next: &Shortcuts) -> std::result::Result<(), Vec<Rejection>> {
        let session = self.session.clone();
        let capabilities = self.selection_capabilities;
        let ready = self.capture_backend_ready;
        let available = move |action: Action| action.is_available(capabilities, &session, ready);

        let mut skipped = Vec::new();
        let desired = wanted(&bindings_from(next), &available, &mut skipped);

        self.hotkeys.apply(&desired)?;
        for note in skipped {
            self.note(note);
        }
        for want in &desired {
            self.note(format!("{} → {}", want.accelerator, want.action));
        }

        Ok(())
    }

    fn commit_shortcuts(&mut self, next: Shortcuts) {
        self.shortcuts = next.clone();
        self.config.shortcuts = next.clone();
        self.config.bindings = bindings_from(&next);

        // Saving last, and only on success, so the stored set is always one the
        // app was able to put in force.
        if let Some(store) = self.shortcut_store.clone()
            && let Err(err) = store.save(&next)
        {
            self.note(format!("shortcuts not saved: {err}"));
        }
        self.refresh_tray_shortcuts();
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
        self.editor_requests.pop_front()
    }

    /// Releases an artifact that was retained only for its editor window.
    pub fn editor_closed(&mut self, card: CardId) {
        if self.editor_only_cards.remove(&card) {
            self.pipeline.post(Job::Release(card));
        }
    }

    /// Asks the worker to prepare the latest settled editor revision for drag.
    ///
    /// At most one full-resolution render is in flight, so a continuous drawing
    /// gesture cannot fill the worker queue with obsolete documents. Once that
    /// render returns, the next frame submits the newest revision if the editor
    /// moved on. Drag refuses while the exact revision is unavailable.
    pub fn prepare_editor(&mut self, editor: EditorSnapshot<'_>) {
        let revision = editor.editor.state().revision();
        let version = (editor.card, editor.generation, revision);
        let captures = self.pipeline.captures();
        if editor.editor.state().is_dragging()
            || captures.get(editor.card).is_none()
            || captures
                .get_revision(editor.card, editor.generation, revision)
                .is_some()
            || self.editor_render_pending.is_some()
            || self.editor_render_failed == Some(version)
        {
            return;
        }

        let job = Job::PrepareImage {
            card: editor.card,
            generation: editor.generation,
            revision,
            data: Box::new(editor.editor.document().data()),
        };
        if self.pipeline.post(job) {
            self.editor_render_pending = Some(version);
        } else {
            self.note(format!(
                "{} editor {} revision {revision} could not be queued for drag preparation: \
                 the capture worker has gone",
                editor.card, editor.generation
            ));
        }
    }

    /// Copies an image the editor has flattened.
    ///
    /// Routed through the worker so the PNG encode and the clipboard write stay
    /// off the UI thread, exactly like a card's own copy.
    pub fn copy_rendered(&mut self, card: CardId, rendered: RevisionedFrame) {
        self.pipeline.post(Job::CopyImage {
            card,
            rendered: Box::new(rendered),
        });
    }

    /// Saves an image the editor has flattened.
    pub fn save_rendered(&mut self, card: CardId, rendered: RevisionedFrame) {
        self.pipeline.post(Job::SaveImage {
            card,
            rendered: Box::new(rendered),
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
        self.drain_cards(None);
        self.hotkeys.unregister_all();
        if let Some(tray) = self.tray.take() {
            tray.close();
        }
        self.server = None;
        self.selector.cancel();
        #[cfg(target_os = "macos")]
        self.finish_picker_surface_and_join();
        #[cfg(target_os = "macos")]
        {
            self.apple_picker = None;
        }
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

fn established_lock_escapes(
    ipc_bound: bool,
    tray_created: bool,
    unlock_hotkey_registered: bool,
) -> Vec<LockEscape> {
    let mut escapes = Vec::new();
    if tray_created {
        escapes.push(LockEscape::TrayMenu);
    }
    if ipc_bound {
        escapes.push(LockEscape::CommandLine);
    }
    if unlock_hotkey_registered {
        escapes.push(LockEscape::GlobalHotkey);
    }
    escapes
}

fn forwarded_unpin(command: &crate::cli::Command) -> Option<&str> {
    let crate::cli::Command::History(history) = command else {
        return None;
    };
    let crate::cli::HistoryCommand::Pin { id, unpin: true } = &history.command else {
        return None;
    };
    Some(id)
}

fn forwarded_unlock_pins(command: &crate::cli::Command) -> bool {
    let crate::cli::Command::History(history) = command else {
        return false;
    };
    matches!(&history.command, crate::cli::HistoryCommand::UnlockPins)
}

impl Drop for App {
    fn drop(&mut self) {
        self.shut_down();
    }
}

/// Reports whether a hotkey is a known system default, without claiming its
/// current availability.
///
/// Used by diagnostics, and worth having separately: the answer for
/// The result is diagnostic metadata only. Users can disable or reassign these
/// defaults, so native registration and live delivery still decide whether the
/// combination is usable.
///
/// # Errors
///
/// Returns an error only if the accelerator cannot be parsed at all.
pub fn describe_conflict(accelerator: &str) -> CliResult<Option<String>> {
    let parsed = Accelerator::parse(accelerator).map_err(CliError::Core)?;
    Ok(parsed
        .system_owner()
        .map(|reserved| format!("{parsed} is a known system default: {reserved:?}")))
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
    use scrozz_core::{
        CaptureTarget, ColorSpace, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize,
        PixelFormat, Provenance, ScaleFactor,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    fn app() -> (App, Recording) {
        let surface = Recording::new();
        let handle = surface.handle();
        let app = App::new(
            Config::sealed(),
            Box::new(surface),
            Arc::new(UnsupportedSelector::headless()),
            false,
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
        app.shortcut_store = None;
        app.selection_capabilities = SelectionCapabilities::CLIENT_OVERLAY;
        app.capture_backend_ready = true;
        app
    }

    #[test]
    fn after_capture_rows_expose_real_capabilities_not_inert_controls() {
        let (app, _) = app();
        let rows = app.after_capture_rows();
        assert_eq!(rows.len(), AfterCaptureAction::UI_ORDER.len());
        assert!(
            rows.iter().all(|row| !row.label.contains("Quick Access")),
            "{rows:?}"
        );

        let copy = rows
            .iter()
            .find(|row| row.label == "Copy to clipboard")
            .expect("copy row");
        assert!(copy.screenshot.available);
        assert!(!copy.recording.available);
        assert!(copy.recording.unavailable_reason.is_some());

        let pin = rows
            .iter()
            .find(|row| row.label == "Pin to Screen")
            .expect("pin row");
        assert!(!pin.screenshot.available);
        assert!(pin.recording_id.is_none());
        assert!(pin.recording.unavailable_reason.is_some());
    }

    #[test]
    fn after_capture_edits_persist_before_taking_effect_and_survive_restart() {
        let root =
            std::env::temp_dir().join(format!("scrozz-app-after-capture-{}", std::process::id()));
        let store = AfterCaptureStore::new(root.join("settings.json"));
        let surface = Recording::new();
        let mut config = Config::sealed();
        config.after_capture = AfterCaptureSettings::fresh();
        config.after_capture_store = Some(store.clone());
        let mut app = App::new(
            config,
            Box::new(surface),
            Arc::new(UnsupportedSelector::headless()),
            false,
        )
        .expect("sealed app");

        app.edit_after_capture(&[AfterCaptureEdit {
            id: AfterCaptureAction::SaveAutomatically.setting_key(MediaKind::Screenshot),
            media: AfterCaptureMedia::Screenshot,
            enabled: true,
        }]);
        assert!(
            app.config
                .after_capture
                .is_enabled(MediaKind::Screenshot, AfterCaptureAction::SaveAutomatically)
        );
        drop(app);

        let restarted = store
            .load(crate::after_capture::InstallProfile::Fresh)
            .unwrap();
        assert!(restarted.is_enabled(MediaKind::Screenshot, AfterCaptureAction::SaveAutomatically));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_settings_write_failure_does_not_create_success_shaped_live_state() {
        let root = std::env::temp_dir().join(format!(
            "scrozz-app-after-capture-failure-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let blocked_parent = root.join("not-a-directory");
        std::fs::write(&blocked_parent, b"file").unwrap();

        let surface = Recording::new();
        let mut config = Config::sealed();
        config.after_capture = AfterCaptureSettings::fresh();
        config.after_capture_store =
            Some(AfterCaptureStore::new(blocked_parent.join("settings.json")));
        let mut app = App::new(
            config,
            Box::new(surface),
            Arc::new(UnsupportedSelector::headless()),
            false,
        )
        .unwrap();

        app.edit_after_capture(&[AfterCaptureEdit {
            id: AfterCaptureAction::SaveAutomatically.setting_key(MediaKind::Screenshot),
            media: AfterCaptureMedia::Screenshot,
            enabled: true,
        }]);
        assert!(
            !app.config
                .after_capture
                .is_enabled(MediaKind::Screenshot, AfterCaptureAction::SaveAutomatically)
        );
        assert!(
            app.notes()
                .iter()
                .any(|note| note.contains("were not saved")),
            "{:?}",
            app.notes()
        );
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn host_reload_applies_settings_written_by_a_forwarded_command() {
        let root = std::env::temp_dir().join(format!(
            "scrozz-app-after-capture-reload-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = AfterCaptureStore::new(root.join("settings.json"));
        let surface = Recording::new();
        let mut config = Config::sealed();
        config.after_capture_store = Some(store.clone());
        let mut app = App::new(
            config,
            Box::new(surface),
            Arc::new(UnsupportedSelector::headless()),
            false,
        )
        .unwrap();

        let mut changed = AfterCaptureSettings::fresh();
        changed.set(
            MediaKind::Screenshot,
            AfterCaptureAction::ShowRecentCapturesOverlay,
            false,
        );
        store.save(&changed).unwrap();
        app.reload_persisted_settings();

        assert!(!app.config.after_capture.is_enabled(
            MediaKind::Screenshot,
            AfterCaptureAction::ShowRecentCapturesOverlay
        ));
        assert!(
            app.notes()
                .iter()
                .any(|note| note == "persisted settings reloaded")
        );
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn automatic_editor_requests_queue_without_discarding_active_work() {
        let (mut app, _) = app();
        let capture = redacted_editor().document().source.clone();
        app.editor_requests.push_back(EditorRequest {
            card: CardId(1),
            generation: 1,
            capture: capture.clone(),
        });
        app.editor_requests.push_back(EditorRequest {
            card: CardId(2),
            generation: 2,
            capture,
        });

        assert_eq!(app.take_editor_request().unwrap().card, CardId(1));
        assert_eq!(app.take_editor_request().unwrap().card, CardId(2));
        assert!(app.take_editor_request().is_none());
    }

    #[cfg(target_os = "macos")]
    struct BlockingFinishSelector {
        started: std::sync::mpsc::Sender<()>,
        resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        done: std::sync::mpsc::Sender<()>,
    }

    #[cfg(target_os = "macos")]
    impl scrozz_core::RegionSelector for BlockingFinishSelector {
        fn name(&self) -> &'static str {
            "blocking-finish-test"
        }

        fn capabilities(&self) -> SelectionCapabilities {
            SelectionCapabilities::NONE
        }

        fn select(
            &self,
            _options: &scrozz_core::SelectionOptions,
        ) -> scrozz_core::Result<scrozz_core::SelectionOutcome> {
            Err(CoreError::Cancelled)
        }
    }

    #[cfg(target_os = "macos")]
    impl CaptureSelector for BlockingFinishSelector {
        fn capture_finished(&self) {
            let _ = self.started.send(());
            let _ = self
                .resume
                .lock()
                .expect("test resume mutex")
                .recv_timeout(Duration::from_secs(2));
            let _ = self.done.send(());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn normal_picker_surface_release_never_joins_the_main_thread() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let selector: Arc<dyn CaptureSelector> = Arc::new(BlockingFinishSelector {
            started: started_tx,
            resume: std::sync::Mutex::new(resume_rx),
            done: done_tx,
        });
        let mut reservation =
            PickerSurfaceReservation::start(PickerMode::Window, selector).expect("start");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = reservation.poll_ready() {
                assert_eq!(result.unwrap(), PickerMode::Window);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "surface reservation did not answer"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let released_at = Instant::now();
        reservation.release();
        assert!(
            released_at.elapsed() < Duration::from_millis(100),
            "normal release joined a worker waiting on the next main-loop frame"
        );
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker entered capture_finished");
        resume_tx.send(()).expect("allow worker to finish");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("detached worker finished cleanly");
    }

    #[test]
    fn headless_revocation_reports_and_never_parks_an_invisible_prompt() {
        let (mut app, _) = app();
        app.handle_capture_permission_failure(
            CaptureKind::Fullscreen,
            CaptureOrigin::Direct,
            Access::NotGranted,
        );
        assert!(!app.permission.has_pending_action());
        assert!(
            app.notes()
                .iter()
                .any(|note| note.contains("System Settings")),
            "the headless refusal must remain actionable"
        );
    }

    fn redacted_editor() -> EditorUi {
        let (width, height) = (64u32, 64u32);
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&[(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8, 255]);
            }
        }
        let bounds = LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(f64::from(width), f64::from(height)),
        );
        let capture = Capture {
            frame: scrozz_core::Frame {
                data,
                size: PhysicalSize::new(f64::from(width), f64::from(height)),
                stride: width as usize * 4,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            provenance: Provenance::Region,
            target: CaptureTarget::Region(bounds),
        };
        let mut editor = EditorUi::new(scrozz_annotate::Document::new(capture));
        editor
            .state_mut()
            .set_tool(scrozz_ui::editor::Tool::Pixelate);
        editor
            .state_mut()
            .pointer_pressed(LogicalPoint::new(8.0, 8.0));
        editor
            .state_mut()
            .pointer_dragged(LogicalPoint::new(56.0, 56.0), false);
        editor.state_mut().pointer_released();
        assert!(editor.state().revision() > 0);
        editor
    }

    fn shortcut_store(name: &str) -> (std::path::PathBuf, ShortcutStore) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "scrozz-gui-shortcuts-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).expect("create shortcut test directory");
        (
            directory.clone(),
            ShortcutStore::new(directory.join("shortcuts.json")),
        )
    }

    #[test]
    fn a_recorded_combination_replaces_the_old_one() {
        let mut app = app_with_shortcuts(Shortcuts::default());
        let recorded = "Ctrl+Alt+Shift+Cmd+0";
        app.edit_shortcuts(&[ShortcutEdit::Set {
            id: ShortcutAction::CaptureRegion.id().to_owned(),
            accelerator: recorded.to_owned(),
        }]);
        let registered = Accelerator::parse(recorded)
            .expect("recorded chord parses")
            .to_string();
        assert_eq!(
            app.shortcuts.get(ShortcutAction::CaptureRegion),
            Some(registered.as_str())
        );
        let row = app
            .shortcut_rows()
            .into_iter()
            .find(|row| row.id == ShortcutAction::CaptureRegion.id())
            .expect("region is always listed");
        assert_eq!(row.accelerator, registered);
        assert_eq!(
            row.symbols,
            Accelerator::parse(&row.accelerator)
                .expect("recorded chord parses")
                .symbols()
        );
    }

    #[cfg(target_os = "macos")]
    fn assign_known_default(app: &mut App) -> Accelerator {
        let accelerator = Accelerator::parse("Cmd+Shift+4").expect("known default parses");
        app.edit_shortcuts(&[ShortcutEdit::Set {
            id: ShortcutAction::CaptureRegion.id().to_owned(),
            accelerator: accelerator.to_string(),
        }]);
        accelerator
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn disabled_system_default_saves_immediately_with_advisory() {
        let mut app = app_with_shortcuts(Shortcuts::default());
        let (directory, store) = shortcut_store("known-default");
        app.shortcut_store = Some(store.clone());
        let accelerator = assign_known_default(&mut app);

        assert_eq!(
            app.shortcuts.get(ShortcutAction::CaptureRegion),
            Some(accelerator.to_string().as_str())
        );
        assert_eq!(
            store.load().get(ShortcutAction::CaptureRegion),
            Some(accelerator.to_string().as_str()),
            "successful native registration saves immediately"
        );
        assert_eq!(
            app.hotkeys.action_for(&accelerator),
            Some(ShortcutAction::CaptureRegion.id()),
            "the live registration and tray source update immediately"
        );
        assert_eq!(
            app.config.bindings,
            bindings_from(&app.shortcuts),
            "the configured and displayed shortcut table updates immediately"
        );
        let row = app
            .shortcut_rows()
            .into_iter()
            .find(|row| row.id == ShortcutAction::CaptureRegion.id())
            .expect("region is listed");
        assert_eq!(row.accelerator, accelerator.to_string());
        assert_eq!(row.symbols, accelerator.symbols());
        assert!(row.problem.is_none());
        assert!(
            row.advisory
                .is_some_and(|message| message.contains("May conflict"))
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn free_chord_has_no_system_advisory() {
        let mut app = app_with_shortcuts(Shortcuts::default());
        app.edit_shortcuts(&[ShortcutEdit::Set {
            id: ShortcutAction::CaptureRegion.id().to_owned(),
            accelerator: "Cmd+Shift+F13".to_owned(),
        }]);
        let row = app
            .shortcut_rows()
            .into_iter()
            .find(|row| row.id == ShortcutAction::CaptureRegion.id())
            .expect("region is listed");
        assert!(row.advisory.is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn known_default_dispatch_never_reopens_or_refocuses_settings() {
        let mut app = app_with_shortcuts(Shortcuts::default());
        assert_eq!(app.perform(Action::OpenSettings), Tick::Continue);
        assert!(
            app.take_settings_request(),
            "the host has opened the visible Settings window"
        );
        let accelerator = assign_known_default(&mut app);
        let event = |state| HotkeyEvent {
            action: ShortcutAction::CaptureRegion.id().to_owned(),
            accelerator,
            state,
        };

        assert_eq!(
            app.action_for_hotkey_event(event(KeyState::Released)),
            None,
            "the recorder's assignment release clears its guard"
        );
        for _ in 0..3 {
            assert_eq!(
                app.action_for_hotkey_event(event(KeyState::Pressed)),
                Some(Action::Capture(CaptureKind::Region)),
                "each press routes exactly one capture action"
            );
            assert_eq!(app.action_for_hotkey_event(event(KeyState::Released)), None);
            assert!(
                !app.take_settings_request(),
                "capture hotkeys must not reopen or foreground Settings"
            );
        }
    }

    #[test]
    fn assignment_event_is_suppressed_but_later_presses_route_only_the_assigned_action() {
        let mut app = app_with_shortcuts(Shortcuts::default());
        let accelerator = Accelerator::parse("Cmd+Shift+F13").expect("free chord parses");
        app.edit_shortcuts(&[ShortcutEdit::Set {
            id: ShortcutAction::CaptureRegion.id().to_owned(),
            accelerator: accelerator.to_string(),
        }]);

        let event = |state| HotkeyEvent {
            action: ShortcutAction::CaptureRegion.id().to_owned(),
            accelerator,
            state,
        };
        assert_eq!(
            app.action_for_hotkey_event(event(KeyState::Pressed)),
            None,
            "the assignment key-down cannot invoke its new action"
        );
        assert_eq!(
            app.action_for_hotkey_event(event(KeyState::Released)),
            None,
            "the assignment release clears the narrow guard"
        );
        for _ in 0..2 {
            assert_eq!(
                app.action_for_hotkey_event(event(KeyState::Pressed)),
                Some(Action::Capture(CaptureKind::Region))
            );
            assert_eq!(app.action_for_hotkey_event(event(KeyState::Released)), None);
            assert!(!app.take_settings_request());
        }
        assert!(!app.settings_requested);
    }

    #[test]
    fn assignment_guard_expires_without_swallowing_a_later_press() {
        let mut app = app_with_shortcuts(Shortcuts::default());
        let accelerator = Accelerator::parse("Cmd+Shift+F13").expect("free chord parses");
        app.edit_shortcuts(&[ShortcutEdit::Set {
            id: ShortcutAction::CaptureRegion.id().to_owned(),
            accelerator: accelerator.to_string(),
        }]);
        app.assignment_guard
            .as_mut()
            .expect("a recorded assignment gets a narrow guard")
            .expires = Instant::now();

        assert_eq!(
            app.action_for_hotkey_event(HotkeyEvent {
                action: ShortcutAction::CaptureRegion.id().to_owned(),
                accelerator,
                state: KeyState::Pressed,
            }),
            Some(Action::Capture(CaptureKind::Region))
        );
        assert!(!app.take_settings_request());
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
        assert!(
            app.pin_lock_escapes().is_empty(),
            "configured resources are not lock escapes until they exist"
        );
    }

    #[test]
    fn only_a_forwarded_unpin_requests_live_viewport_removal() {
        let unpin = Cli::try_parse_from(["scrozz", "history", "pin", "capture-1", "--unpin"])
            .expect("valid unpin")
            .command
            .expect("command");
        assert_eq!(forwarded_unpin(&unpin), Some("capture-1"));

        let pin = Cli::try_parse_from(["scrozz", "history", "pin", "capture-1"])
            .expect("valid pin")
            .command
            .expect("command");
        assert_eq!(forwarded_unpin(&pin), None);
    }

    #[test]
    fn id_free_unlock_command_is_a_live_lock_escape() {
        let unlock = Cli::try_parse_from(["scrozz", "history", "unlock-pins"])
            .expect("valid unlock")
            .command
            .expect("command");
        assert!(forwarded_unlock_pins(&unlock));
        assert_eq!(forwarded_unpin(&unlock), None);
    }

    #[test]
    fn only_established_external_routes_are_lock_escapes() {
        assert!(established_lock_escapes(false, false, false).is_empty());
        assert_eq!(
            established_lock_escapes(true, true, true),
            vec![
                LockEscape::TrayMenu,
                LockEscape::CommandLine,
                LockEscape::GlobalHotkey
            ]
        );
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
    fn card_copy_and_save_use_the_live_revision_when_the_editor_owns_the_card() {
        let editor = redacted_editor();
        let card = CardId(42);
        let expected = editor.state().revision();
        let snapshot = Some(EditorSnapshot::new(card, 1, &editor));

        for output in [CardOutput::Copy, CardOutput::Save] {
            let job = App::card_output_job(card, output, snapshot).expect("the document renders");
            let rendered = match job {
                Job::CopyImage {
                    card: got,
                    rendered,
                }
                | Job::SaveImage {
                    card: got,
                    rendered,
                } => {
                    assert_eq!(got, card);
                    rendered
                }
                Job::Copy(_) | Job::Save(_) => {
                    panic!("an edited card fell back to its original capture")
                }
                _ => panic!("the card output was routed to an unrelated job"),
            };
            assert_eq!(
                rendered.revision(),
                expected,
                "the worker must receive the exact revision the user approved"
            );
            assert_ne!(
                rendered.frame().data,
                editor.document().source.frame.data,
                "the destructive redaction was bypassed with the original pixels"
            );
        }
    }

    #[test]
    fn drag_waits_for_the_live_redacted_revision_instead_of_using_the_card_cache() {
        let (mut app, _) = app();
        let editor = redacted_editor();
        let card = CardId(7);
        app.pipeline
            .captures()
            .store_test_capture(card, &editor.document().source)
            .expect("the original capture encodes");
        let original = app
            .pipeline
            .captures()
            .get(card)
            .expect("the card capture is cached");
        let generation = 7;
        let snapshot = EditorSnapshot::new(card, generation, &editor);

        let stale = match app.drag_bytes(card, Some(snapshot)) {
            Ok(_) => panic!("the original bytes stood in for an edited revision"),
            Err(error) => error,
        };
        assert!(
            stale.to_string().contains("still being prepared"),
            "the safe refusal should explain itself: {stale}"
        );

        app.prepare_editor(snapshot);
        for _ in 0..200 {
            app.drain_pipeline();
            if app
                .pipeline
                .captures()
                .get_revision(card, generation, editor.state().revision())
                .is_some()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let bytes = app
            .drag_bytes(card, Some(snapshot))
            .expect("the prepared edited revision is ready");
        let decoded = scrozz_export::decode(&bytes.full).expect("the drag payload is a PNG");

        assert_eq!(bytes.generation(), Some(generation));
        assert_eq!(bytes.revision(), editor.state().revision());
        assert_ne!(
            decoded.data,
            editor.document().source.frame.data,
            "drag exposed the original pixels under a destructive redaction"
        );
        let after = app
            .pipeline
            .captures()
            .get(card)
            .expect("preparing an edit must not replace the card capture");
        assert_eq!(after.revision(), 0);
        assert_eq!(
            after.full, original.full,
            "closing the editor must still expose the untouched card capture"
        );

        let reopened = EditorSnapshot::new(card, generation + 1, &editor);
        assert!(
            app.drag_bytes(card, Some(reopened)).is_err(),
            "a new editor lifetime reused an old lifetime's matching revision"
        );
    }

    #[test]
    fn a_failed_editor_version_is_not_prepared_again_every_frame() {
        let (mut app, _) = app();
        let editor = redacted_editor();
        let card = CardId(9);
        let generation = 3;
        app.pipeline
            .captures()
            .store_test_capture(card, &editor.document().source)
            .expect("the original capture encodes");
        let version = (card, generation, editor.state().revision());
        app.editor_render_failed = Some(version);

        app.prepare_editor(EditorSnapshot::new(card, generation, &editor));

        assert_eq!(
            app.editor_render_pending, None,
            "an unchanged terminal failure was queued again"
        );
    }

    #[test]
    fn a_released_card_is_not_prepared_again_for_later_editor_frames() {
        let (mut app, _) = app();
        let editor = redacted_editor();

        app.prepare_editor(EditorSnapshot::new(CardId(404), 1, &editor));

        assert_eq!(
            app.editor_render_pending, None,
            "a missing card queued a render that can only fail"
        );
    }

    #[test]
    fn an_unrouted_card_gesture_is_recorded_not_swallowed() {
        let (mut app, surface) = app();
        surface.inject(CardEvent::Collapse(CardId(5)));
        app.tick();
        assert!(
            app.notes().iter().any(|n| n.contains("not routed yet")),
            "{:?}",
            app.notes()
        );
    }

    #[test]
    fn a_drag_without_a_capture_is_refused_in_writing() {
        let (mut app, surface) = app();
        surface.inject(CardEvent::Drag {
            card: CardId(5),
            at: DragSpot {
                card: [0.0, 0.0, 200.0, 140.0],
                pointer: [100.0, 70.0],
            },
        });
        app.tick();

        // The recording surface has no window, and card 5 was never captured.
        // Either way the user is told, because a drag that silently does
        // nothing is the failure this path exists to remove.
        assert!(
            app.notes()
                .iter()
                .any(|n| n.contains("card:5 has no capture to drag")
                    || n.contains("card:5 could not be dragged")),
            "{:?}",
            app.notes()
        );
        assert!(
            surface.trace().contains(&SurfaceCall::Settle {
                id: CardId(5),
                accepted: false,
            }),
            "a refused native start left the overlay gesture armed"
        );
    }

    #[test]
    fn nothing_is_in_flight_before_a_drag_starts() {
        let (app, _) = app();
        assert_eq!(app.drag.in_flight(), 0);
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
    fn system_defaults_are_recognised_as_advisory_metadata() {
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
            "every platform must declare its common system defaults"
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

    // -----------------------------------------------------------------------
    // When the drag starts, not just that it starts
    // -----------------------------------------------------------------------
    //
    // The original bug produced entirely correct events, one frame too late.
    // `beginDraggingSessionWithItems:` and `DoDragDrop` both take the mouse
    // from where it is *now*, so a drag armed during the UI pass and acted on
    // during the next logic pass is acted on after the button came up — and
    // the platform simply refuses. Tests that only assert the event was
    // produced cannot see that, so these assert on the order of the calls.

    use crate::gui::card::SurfaceCall;

    fn drag_spot() -> DragSpot {
        DragSpot {
            card: [10.0, 20.0, 210.0, 150.0],
            pointer: [60.0, 70.0],
        }
    }

    #[test]
    fn an_armed_drag_is_acted_on_without_waiting_for_a_tick() {
        // The property, stated exactly: between the surface arming the drag and
        // the app acting on it, nothing else runs. In particular no `tick`,
        // which is the pass that used to own this and is a frame away.
        let (mut app, surface) = app();
        surface.arm(CardEvent::Drag {
            card: CardId(1),
            at: drag_spot(),
        });
        surface.clear_trace();

        assert_eq!(app.pump_drag_starts(), 1, "the armed drag was not started");
        assert_eq!(
            surface.trace(),
            vec![
                SurfaceCall::PollDragStarts,
                SurfaceCall::Settle {
                    id: CardId(1),
                    accepted: false,
                },
            ],
            "the gesture frame may only drain the drag and settle an immediate refusal"
        );
    }

    #[test]
    fn the_ordinary_drain_is_not_what_starts_a_drag() {
        // If `tick` still started drags, moving the call would be cosmetic and
        // the bug would survive. It must not: `tick` drains `poll`, and `poll`
        // is a frame behind.
        let (mut app, surface) = app();
        surface.arm(CardEvent::Drag {
            card: CardId(1),
            at: drag_spot(),
        });
        surface.clear_trace();

        app.tick();
        assert!(
            !surface.trace().contains(&SurfaceCall::PollDragStarts),
            "the logic pass drained the armed drags: {:?}",
            surface.trace()
        );
        assert_eq!(
            app.pump_drag_starts(),
            1,
            "the drag was consumed by the wrong pass and is gone"
        );
    }

    #[test]
    fn pumping_drags_leaves_the_ordinary_events_alone() {
        // The drag jumps the queue; nothing else may.
        let (mut app, surface) = app();
        surface.inject(CardEvent::Copy(CardId(2)));
        surface.arm(CardEvent::Drag {
            card: CardId(1),
            at: drag_spot(),
        });

        assert_eq!(app.pump_drag_starts(), 1);
        app.tick();
        assert!(
            surface.trace().contains(&SurfaceCall::Poll),
            "the queued copy was never drained"
        );
    }

    #[test]
    fn a_drag_with_no_capture_behind_it_says_so_rather_than_failing_silently() {
        let (mut app, surface) = app();
        surface.arm(CardEvent::Drag {
            card: CardId(77),
            at: drag_spot(),
        });

        assert_eq!(app.pump_drag_starts(), 1, "the attempt still counts");
        assert!(
            app.notes().iter().any(|n| n.contains("no capture")),
            "a drag that could not start said nothing: {:?}",
            app.notes()
        );
        assert!(
            surface.trace().contains(&SurfaceCall::Settle {
                id: CardId(77),
                accepted: false,
            }),
            "a pre-native refusal left the card held"
        );
        assert!(
            app.take_modal_drag_input_release(),
            "a failed handoff can still lose the release edge"
        );
    }

    // -----------------------------------------------------------------------
    // Every outcome reaches the card
    // -----------------------------------------------------------------------
    //
    // The platform's drag loop is modal and can consume the mouse-up, so the
    // surface may never learn the gesture ended. Whatever the outcome, the
    // gesture is released — and only an accepted drop retires the card.

    /// Runs `drain_drags` against a session that has already finished.
    fn settle(outcome: DragOutcome) -> (Vec<SurfaceCall>, Vec<String>) {
        let (mut app, surface) = app();
        app.drag.adopt_finished(CardId(3), outcome);
        surface.clear_trace();
        app.drain_drags();
        (surface.trace(), app.notes().to_vec())
    }

    #[test]
    fn an_accepted_drop_releases_the_gesture_then_retires_the_card() {
        let (trace, notes) = settle(DragOutcome::Accepted(scrozz_shell::DragOperation::Copy));
        assert_eq!(
            trace,
            vec![
                SurfaceCall::Settle {
                    id: CardId(3),
                    accepted: true
                },
                SurfaceCall::Dismiss(CardId(3)),
            ],
            "the card was retired before its gesture was released"
        );
        assert!(notes.iter().any(|n| n.contains("dropped")), "{notes:?}");
    }

    #[test]
    fn a_cancelled_drag_releases_the_gesture_and_keeps_the_card() {
        let (trace, _) = settle(DragOutcome::Cancelled);
        assert_eq!(
            trace,
            vec![SurfaceCall::Settle {
                id: CardId(3),
                accepted: false
            }],
            "a cancelled drag must release the gesture and nothing else"
        );
    }

    #[test]
    fn a_rejected_drop_releases_the_gesture_and_says_so() {
        let (trace, notes) = settle(DragOutcome::Rejected);
        assert_eq!(
            trace,
            vec![SurfaceCall::Settle {
                id: CardId(3),
                accepted: false
            }]
        );
        assert!(
            notes.iter().any(|n| n.contains("not accepted")),
            "a refused drop was silent: {notes:?}"
        );
    }

    #[test]
    fn a_failed_drag_releases_the_gesture_and_reports_the_reason() {
        let (trace, notes) = settle(DragOutcome::Failed("the pasteboard refused".to_owned()));
        assert_eq!(
            trace,
            vec![SurfaceCall::Settle {
                id: CardId(3),
                accepted: false
            }]
        );
        assert!(
            notes.iter().any(|n| n.contains("the pasteboard refused")),
            "the reason a drag failed was swallowed: {notes:?}"
        );
    }

    #[test]
    fn an_outcome_is_only_acted_on_once() {
        // `drain_drags` runs every tick, and the session stays around while its
        // file is being swept. Reporting twice would dismiss a card the user
        // had since brought back.
        let (mut app, surface) = app();
        app.drag.adopt_finished(
            CardId(3),
            DragOutcome::Accepted(scrozz_shell::DragOperation::Copy),
        );
        app.drain_drags();
        surface.clear_trace();
        app.drain_drags();
        assert!(
            surface.trace().is_empty(),
            "the same outcome was acted on twice: {:?}",
            surface.trace()
        );
    }

    #[test]
    fn every_native_drag_outcome_requests_one_modal_input_release() {
        for outcome in [
            DragOutcome::Accepted(scrozz_shell::DragOperation::Copy),
            DragOutcome::Cancelled,
            DragOutcome::Rejected,
            DragOutcome::Failed("test failure".to_owned()),
        ] {
            let (mut app, _) = app();
            app.drag.adopt_finished(CardId(3), outcome);
            app.drain_drags();

            assert!(app.take_modal_drag_input_release());
            assert!(
                !app.take_modal_drag_input_release(),
                "one drag ending requested more than one input reset"
            );
        }
    }
}
