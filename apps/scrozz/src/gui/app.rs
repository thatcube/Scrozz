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
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, channel},
    },
    task::{Context, Poll, Waker},
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileLaunchAction {
    Open,
    Reveal,
}

type FileLauncher =
    Arc<dyn Fn(FileLaunchAction, &Path) -> scrozz_core::Result<()> + Send + Sync + 'static>;

fn default_file_launcher() -> FileLauncher {
    #[cfg(test)]
    {
        Arc::new(|_, _| Ok(()))
    }
    #[cfg(not(test))]
    {
        Arc::new(|action, path| match action {
            FileLaunchAction::Open => crate::gui::recording::open_file(path),
            FileLaunchAction::Reveal => crate::gui::recording::reveal_file(path),
        })
    }
}

use clap::Parser as _;
use scrozz_annotate::{
    AnalysisCancellation, Document, DocumentData, SmartFrameAnalysis, SmartFramePreset,
};
use scrozz_core::{
    CaptureRequest, CaptureTarget, CursorMode, Error as CoreError, LockEscape, ScrollAxis,
    SelectionCapabilities, SelectionMode,
};
use scrozz_record::{
    CameraDevice, CameraDeviceId, CameraPermission, CameraPreviewSession, CameraRequest,
    MachineEvent, Recording, RecordingMachine, RecordingPhase, RecordingSettings,
    handoff::FinalizedMediaHandoff, playback::sync_document, settings::CameraSettings,
};
use scrozz_shell::{
    Accelerator, Capability, DragPayload, GlobalHotkeys, HotkeyEvent, KeyState, Permissions,
    ScreenshotSound, Session, SystemPermissions, Tray, TrayAction,
    hotkey::{DesiredBinding, Rejection},
    play_screenshot_sound,
};
use scrozz_stitch::{CancelAction, Progress};
use scrozz_store::{CaptureId, RetentionPolicy, Timestamp};
use scrozz_ui::history::{HistoryAction, HistoryViewModel};
use scrozz_ui::settings::RecordingSettingsAction;
use scrozz_ui::{ScrollHudAction, ScrollHudState, ScrollHudStatus};

use crate::gui::pipeline::ClipboardTurn;
use crate::{
    after_capture::{
        ActionEffect, AfterCaptureAction, AfterCaptureSettings, AfterCaptureStore, MediaKind,
        current_availability,
    },
    cli::{Cli, InteractiveMode, SettingsCommand},
    commands::{ScrollingTarget, wayland_portal_picker_target},
    fault::{CliError, CliResult},
    gui::{
        action::{Action, CaptureKind, CaptureOrigin},
        card::{
            Card, CardEvent, CardId, CardSurface, PIN_TEXTURE_MAX_EDGE, PinGeneration,
            SurfaceWaker, Thumbnail,
        },
        drag::{DragHost, DragSpot},
        permission::{
            self, Access, Effect as PermissionEffect, PendingCapture, PermissionStore,
            PickerAvailability, PickerMode, Response as PermissionResponse,
        },
        pipeline::{
            CaptureBytes, DragGeometry, DragSubject, FinalizedCapture, HistoryOperation, Job,
            Outcome, PinEditorSnapshot, Pipeline, PreparedHistoryDrag, ReadyCapture,
        },
        recording::{
            ActiveVideoEditor, ArmedStart, Completion, FinalisedRecording, PendingSelection,
            PendingStart, RecordingState, SelectionStart,
        },
        selection::CaptureSelector,
        server::{Admission, ForwardedCapture, Forwarder, Request, Server},
    },
    json::Json,
    platform,
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
    recent_captures_overlay::{PinnedCaptureAction, RecentCapturesAutoCloseAction},
    settings::{
        AfterCaptureCell, AfterCaptureEdit, AfterCaptureMedia, AfterCaptureRow, ShortcutEdit,
        ShortcutRow,
    },
    video_editor::{VideoEditorAction, VideoEditorSnapshot},
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

/// How long automatic scrolling waits for the overlay to confirm that its
/// native window really did become mouse-transparent.
///
/// The viewport command is asynchronous, so a request is not an
/// acknowledgement. Without a confirmation the session is abandoned rather than
/// scrolling Scrozz's own overlay.
const PASSTHROUGH_ACK_TIMEOUT: Duration = Duration::from_secs(1);

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
    /// Source-image retention applied by the history worker.
    pub retention_policy: RetentionPolicy,
    /// Current Recent Captures Overlay behavior.
    pub recent_captures_overlay: scrozz_ui::RecentCapturesOverlaySettings,
    /// Whether construction must avoid every external service.
    ///
    /// A sealed run reads no provider settings and opens no credential vault,
    /// which is what makes headless validation safe to run on a real machine.
    sealed: bool,
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
            retention_policy: RetentionPolicy::default(),
            recent_captures_overlay: scrozz_ui::RecentCapturesOverlaySettings::default(),
            sealed: false,
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
                    // The same defaults a first run would have written, Scenes
                    // included: an existing install whose document cannot be
                    // read must still not start framing captures. Nothing is
                    // saved here — the unreadable file is left exactly as found
                    // rather than overwritten with a guess.
                    config.after_capture = profile.defaults();
                }
            }
        }
        match crate::settings::retention_policy(&config.after_capture) {
            Ok(policy) => config.retention_policy = policy,
            Err(error) => {
                let warning =
                    format!("History retention settings are invalid; using defaults: {error}");
                config.after_capture_warning = Some(
                    config
                        .after_capture_warning
                        .take()
                        .map_or(warning.clone(), |existing| format!("{existing}; {warning}")),
                );
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
        match crate::settings::recent_captures_overlay_settings(&config.after_capture) {
            Ok(settings) => config.recent_captures_overlay = settings,
            Err(error) => {
                let warning = format!(
                    "Recent Captures Overlay settings are invalid; using defaults: {error}"
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
            retention_policy: RetentionPolicy {
                max_image_bytes: u64::MAX,
                max_image_age: scrozz_store::RetentionWindow::Forever,
            },
            recent_captures_overlay: scrozz_ui::RecentCapturesOverlaySettings::default(),
            sealed: true,
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
        CaptureKind::Scrolling => scrozz_core::product_copy::SCROLLING_CAPTURE,
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
            CaptureKind::Scrolling => {
                "Apple's picker returns one still frame, so it cannot follow a window while it \
                 scrolls. Grant direct access in System Settings to capture a scrolling page."
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

/// Why the application event loop is ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// The bounded run duration elapsed.
    Deadline,
    /// A user-visible action explicitly requested Quit.
    Quit(CaptureOrigin),
    /// The native event loop ended without an app action requesting it.
    NativeEventLoop,
    /// A required native lifecycle invariant could not be established.
    NativeLifecycle,
}

impl ExitReason {
    /// Stable spelling for logs and the final diagnostic report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::Quit(CaptureOrigin::MenuBar) => "menu-bar-quit",
            Self::Quit(CaptureOrigin::GlobalHotkey) => "global-hotkey-quit",
            Self::Quit(CaptureOrigin::Startup) => "startup-quit",
            Self::Quit(CaptureOrigin::Direct) => "direct-quit",
            Self::NativeEventLoop => "native-event-loop",
            Self::NativeLifecycle => "native-lifecycle",
        }
    }
}

/// Whether the host should keep going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Still running.
    Continue,
    /// Stop for an explicit, attributable reason.
    Stop(ExitReason),
}

const ASSIGNMENT_EVENT_GUARD: Duration = Duration::from_millis(150);
const RECENT_CAPTURES_SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_millis(250);
const CAMERA_SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_millis(250);

struct AssignmentEventGuard {
    accelerator: Accelerator,
    expires: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PinIntent {
    generation: PinGeneration,
    visible: bool,
}

struct InputWakeMonitor {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl InputWakeMonitor {
    fn start(waker: Option<SurfaceWaker>) -> std::io::Result<Option<Self>> {
        Self::start_with_probe(
            waker,
            || scrozz_shell::tray::events_pending() || scrozz_shell::hotkey::events_pending(),
            Duration::from_millis(8),
        )
    }

    fn start_with_probe(
        waker: Option<SurfaceWaker>,
        pending: impl Fn() -> bool + Send + 'static,
        interval: Duration,
    ) -> std::io::Result<Option<Self>> {
        let Some(waker) = waker else {
            return Ok(None);
        };
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("scrozz-input-wake".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    if pending() {
                        waker();
                    }
                    std::thread::sleep(interval);
                }
            })?;
        Ok(Some(Self {
            stop,
            worker: Some(worker),
        }))
    }
}

impl Drop for InputWakeMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
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
    pending_admissions: VecDeque<Admission>,
    pending_pipeline_outcomes: VecDeque<Outcome>,
    selector: Arc<dyn CaptureSelector>,
    file_launcher: FileLauncher,
    /// Every card with an editor open as of the most recent tick.
    active_editor_cards: HashSet<CardId>,
    /// Cards with a decode in flight for [`Job::Open`], not yet a live editor.
    ///
    /// Closes the gap between the click that starts opening a card and the
    /// first tick where [`Self::active_editor_cards`] can see it, so a rapid
    /// second click (or a second trigger from another menu) during that
    /// window still raises the one editor being opened instead of decoding
    /// the same card twice.
    opening_cards: HashSet<CardId>,
    /// Cards whose already-open (or opening) editor should be raised, because
    /// something asked to edit a card that is already being edited.
    focus_editor_requests: VecDeque<CardId>,
    drag: DragHost,
    /// A native modal drag can consume mouse-up and modifier-release events.
    /// The window host clears that stale egui input before another interaction.
    modal_drag_input_release_pending: bool,
    /// Option/Alt override captured at the exact native drag hand-off.
    drag_keep_after_accept: HashSet<CardId>,
    /// Every dispatched-and-not-yet-resolved Copy/Save/Save-As/Upload action
    /// for each card, keyed by its own unique id (round 12).
    ///
    /// Replaces the previous `close_after_output: HashSet<CardId>` /
    /// `pending_upload: HashMap<CardId, PendingUploadAction>` /
    /// `card_pending_uploads: HashMap<CardId, u32>` trio: none of the three
    /// could tell two genuinely concurrent dispatches for the same card
    /// apart from one another purely by `card`. A card-level Save-As racing
    /// an editor's own in-editor Copy previously shared the single
    /// `close_after_output` bit -- either one's completion could clear it
    /// on the other's behalf, dismissing the card on the wrong action's
    /// say-so or leaving a real dispatch's close policy stranded, and an
    /// in-editor Copy/Save was never tracked *at all*, so nothing ever
    /// stopped a Cancel/Done from pruning its generation's fate out from
    /// under a still-in-flight completion. `card_pending_uploads` had the
    /// matching problem for Upload alone: a *count*, not a set, so a
    /// duplicate delivery of the same stale action's outcome could
    /// double-decrement it and prune a fate while a genuinely different,
    /// still-outstanding action's own completion had not arrived yet.
    ///
    /// Every dispatch site (card-level Copy/Save/Save-As, in-editor
    /// Copy/Save, and Upload) allocates a fresh id from
    /// [`Self::next_output_action`] and registers its own entry here via
    /// [`Self::register_output_action`]; every completion (`Outcome::Done`,
    /// `Outcome::OutputRefused`, `Outcome::UploadDone`,
    /// `Outcome::UploadRefused`) resolves only the exact id it answers for
    /// via [`Self::resolve_output_action`] -- idempotent, since a
    /// since-superseded or already-resolved id is simply absent and
    /// resolving it again is a safe no-op.
    outstanding_output_actions: HashMap<CardId, HashMap<u64, OutstandingAction>>,
    /// Which outstanding [`OutputActionKind::Upload`] action id, if any, is
    /// the *current* one for a card -- i.e. the one status/retention
    /// reasoning and `Outcome::UploadDone`/`Outcome::UploadRefused`'s
    /// success/failure logic should act on.
    ///
    /// Unlike Copy/Save/Save-As, a second Upload dispatch does not wait for
    /// the first to resolve before proceeding -- [`Self::dispatch_upload_action`]
    /// lets a newer request supersede an older one that is still
    /// outstanding in `outstanding_output_actions`, exactly mirroring the
    /// old `pending_upload` map's role, but now separated from the
    /// per-action entry itself so the superseded action's own id stays
    /// tracked in `outstanding_output_actions` until its own completion
    /// actually drains (round 8, Findings #2-#4; round 9, Finding #3; round
    /// 11).
    current_upload_action: HashMap<CardId, u64>,
    /// Source of every action id stored in `outstanding_output_actions`,
    /// spanning every output kind uniformly. Monotonically increasing and
    /// never reused within a process's lifetime in practice, mirroring
    /// `next_editor_generation`. Replaces the previously Upload-only
    /// `next_upload_action` (round 12).
    next_output_action: u64,
    /// Whether each live card has another retained artifact and a visible export.
    card_retention: HashMap<CardId, (bool, bool)>,
    /// Cards already usable in the UI whose ambient durability is still queued.
    finalizing_cards: HashSet<CardId>,
    /// Per-generation output-staleness fate recorded for each card with any
    /// retired (committed or cancelled) editing session that might still
    /// matter (round 5, Finding #2; round 9, Finding #1; round 10,
    /// Finding #1).
    ///
    /// A live editor's Copy/Save/Upload exports its own current, uncommitted
    /// render -- see [`Self::card_output_job`] -- carrying that render's
    /// exact `(generation, revision)` as its completion's `version`. That
    /// dispatch is never retracted by anything that happens to the editor
    /// afterward: Done may later commit a *different* revision of the same
    /// generation, Cancel (or a clean close) may discard the generation
    /// outright, or an entirely new editing session may open and itself
    /// resolve one way or the other before this stale completion drains.
    /// Whichever it is must be recognisable no matter how many editing
    /// sessions have come and gone for this card since.
    ///
    /// Earlier rounds tracked only the single *most recent* generation's
    /// fate per card, in two scalar maps both cleared the instant a new
    /// editor opened. That erased an older generation's own tombstone
    /// before every action dispatched against it had resolved: a second
    /// Cancel silently overwrote the first's record, and merely opening a
    /// new editor discarded it outright -- either way, a subsequently-
    /// arriving late completion for the *forgotten* older generation found
    /// nothing left to compare itself against and was wrongly trusted as
    /// though it answered whichever generation the scalar happened to hold
    /// instead.
    ///
    /// Recording every retired generation's fate independently, keyed by
    /// its own generation number, closes that gap: opening or retiring a
    /// new generation only ever adds an entry here, never erases one. A
    /// generation with no entry at all is either still open or was never
    /// opened -- either way, its completion is trusted, matching a plain
    /// no-live-editor output's behaviour and a fresh session's own first
    /// completion.
    ///
    /// Entries are removed only in the two cases where they can never be
    /// consulted again: [`Self::dismiss_recent_capture`] drops every entry
    /// for a card that is fully retiring, and `Outcome::CardOutputCommitFailed`
    /// rolls back its own single optimistic write (never any other
    /// generation's) when that commit never actually became durable.
    /// `next_editor_generation` is a `u64` incremented once per newly opened
    /// editor and never reused within a process's lifetime in practice, so a
    /// generation-number collision between two editing sessions of the same
    /// card is not a realistic concern.
    card_generation_fates: HashMap<CardId, HashMap<u64, GenerationFate>>,
    /// Durable identities used to re-check source retention at cleanup time.
    card_capture_ids: HashMap<CardId, CaptureId>,
    /// Cards awaiting a current history-retention answer before cleanup.
    pending_retention_close: HashSet<CardId>,
    /// Cards removed by capacity while an atomic release check is pending.
    pending_retention_overflow: HashSet<CardId>,
    /// Cleanup actions that expired while the card's editor owned the live revision.
    deferred_auto_close: HashMap<CardId, RecentCapturesAutoCloseAction>,
    /// Explicit Save presses waiting for initial automatic actions to settle.
    deferred_save: HashSet<CardId>,
    /// Capacity retirement deferred while the card's editor owns the live revision.
    deferred_overflow: HashSet<CardId>,
    /// Rendered recovery exports held while overflow retention is verified.
    pending_overflow_recovery: HashMap<CardId, Job>,
    /// Hidden overflow cards whose recovery export is in flight.
    overflow_recovery_in_flight: HashSet<CardId>,
    /// Latest live overlay settings waiting for one coalesced durable write.
    pending_recent_captures_settings_save:
        Option<(scrozz_ui::RecentCapturesOverlaySettings, Instant)>,
    pin_lock_escapes: Vec<LockEscape>,
    pin_intents: HashMap<CaptureId, PinIntent>,
    input_wake_monitor: Option<InputWakeMonitor>,
    suppress_locked_restores: bool,
    started: Instant,
    captures: u64,
    sound_warning_shown: bool,
    settings_requested: bool,
    /// The camera device/preview surface, present only while its window is open.
    ///
    /// Owned here rather than in the overlay because it is the app thread that
    /// may talk to a camera: the viewport publishes semantic actions and never
    /// touches native capture.
    camera_settings_window: Option<scrozz_ui::CameraSettingsSnapshot>,
    /// A live preview session, started only by an explicit user action.
    camera_preview: Option<Box<dyn CameraPreviewSession>>,
    /// Stable camera preference applied to the next recording.
    camera_device: Option<CameraDeviceId>,
    /// Passive enumeration; injectable so tests never open a camera.
    camera_devices: fn() -> scrozz_core::Result<Vec<CameraDevice>>,
    /// Permission read without prompting; injectable for the same reason.
    camera_permission_status: fn() -> CameraPermission,
    /// Preview start, which is the one call allowed to request permission.
    camera_preview_start: fn(&CameraRequest) -> scrozz_core::Result<Box<dyn CameraPreviewSession>>,
    /// Latest live camera preference waiting for one coalesced durable write.
    pending_camera_preferences_save: Option<Instant>,
    editor_requests: VecDeque<EditorRequest>,
    smart_frame_results: VecDeque<SmartFrameResult>,
    /// Captures retained only because their editor is open, not by the overlay.
    editor_only_cards: HashSet<CardId>,
    /// Done-triggered closes waiting on their commit (and, for a
    /// history-only editor, persist) job to answer before the host may
    /// finalize them.
    ///
    /// Round 5 Finding #1/#4: closing the window (and marking the card no
    /// longer editing) the instant Done posts these jobs let a plain
    /// Copy/Save/Upload or a native drag -- issued in the gap before the
    /// worker actually answers -- read the card's pre-edit bytes even though
    /// the thumbnail already promised a redaction. Recording the pending
    /// state here, and leaving the card `editing` and its frozen viewport in
    /// [`Host`]'s editor map until [`Self::take_editor_close_result`] says
    /// it may finalize, keeps every export routed through the still-open
    /// (frozen) editor's own live render for the whole gap instead.
    pending_editor_closes: HashMap<CardId, PendingEditorClose>,
    /// Resolved [`PendingEditorClose`] entries the host has not yet drained.
    editor_close_results: VecDeque<(CardId, EditorCloseOutcome)>,
    /// At most one full-resolution render per card is in flight at a time.
    editor_render_pending: HashMap<CardId, (u64, u64)>,
    /// A failed version is not retried until the document or editor changes.
    editor_render_failed: HashMap<CardId, (u64, u64)>,
    next_editor_generation: u64,
    notes: Vec<String>,
    exit_reason: Option<ExitReason>,
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
    history: Arc<Mutex<HistoryViewModel>>,
    prepared_history_drags: HashMap<CaptureId, PreparedHistoryDrag>,
    pending_drags: VecDeque<PendingDrag>,
    recording: RecordingState,
    /// Durable recordings currently shown as cards, by card identity.
    recorded_media: HashMap<CardId, FinalizedMediaHandoff>,
    /// Original capture targets retained for derivative editor exports.
    recorded_media_targets: HashMap<CardId, CaptureTarget>,
    /// The history row the next recording card should be attributed to.
    recorded_history_id: Option<CaptureId>,
    /// A native destination chooser polled without blocking the application event loop.
    pending_save_dialog: Option<PendingSaveDialog>,
    /// A modeless destination chooser for a durable pinned capture.
    pending_pin_save_dialog: Option<PendingPinSaveDialog>,
    /// Synthetic upload identities awaiting a pinned-capture result.
    pending_pin_uploads: HashMap<CardId, CaptureId>,
    /// Secret-free private-sharing configuration, as the UI sees it.
    cloud_settings: scrozz_ui::CloudSettingsModel,
    /// A pending request to open the Sharing settings viewport.
    sharing_settings_requested: bool,
    /// An in-flight provider reachability probe, run off the main thread.
    connection_test: Option<Receiver<CliResult<()>>>,
    /// Cards currently on screen, so Upload availability can be refreshed.
    visible_cards: BTreeSet<CardId>,
    /// The scrolling HUD as this coordinator last published it.
    scroll_hud: Option<ScrollHudState>,
    /// The card a running scrolling capture will become.
    scrolling_card: Option<CardId>,
    /// The window/display chosen when the axis picker opened.
    scrolling_target: Option<ScrollingTarget>,
    /// A finished scrolling capture held back one pass, so an abort that raced
    /// the last frame still wins.
    scrolling_ready: Option<Box<ReadyCapture>>,
    /// The card an explicit discard is waiting to arrive for.
    scrolling_abort_pending: Option<CardId>,
    /// A started session waiting for the overlay to confirm click-through.
    scrolling_start_pending: Option<PendingScrollingStart>,
    /// Whether a keep has already been asked for, so a second invocation means
    /// discard rather than a second keep.
    scrolling_keep_pending: bool,
    scrolling_target_resolver: Box<dyn Fn() -> CliResult<ScrollingTarget>>,
    scrolling_target_refresher: Box<dyn Fn(ScrollingTarget) -> CliResult<ScrollingTarget>>,
}

/// A scrolling capture that has been prepared but not yet posted.
///
/// Automatic scrolling must not synthesize its first gesture until the overlay
/// window has *confirmed* it is mouse-transparent; until then the wheel event
/// would land on Scrozz's own window.
struct PendingScrollingStart {
    axis: ScrollAxis,
    card: CardId,
    target: Box<ScrollingTarget>,
    needs_passthrough: bool,
    passthrough_requested_at: Instant,
}

/// A capture the annotation editor has been asked to open.
#[derive(Debug)]
pub struct EditorRequest {
    /// The card it came from, so copy and save can be attributed and the
    /// finished image can be sent back through the same worker.
    pub card: CardId,
    /// Uniquely identifies this opening of the editor.
    pub generation: u64,
    /// The complete editable document.
    pub document: Document,
    /// User-created Smart Frame presets available to this editor.
    pub smart_frame_presets: Vec<SmartFramePreset>,
}

/// One asynchronous Smart Frame result awaiting delivery to its editor.
#[derive(Debug)]
pub struct SmartFrameResult {
    /// The card whose source was analysed.
    pub card: CardId,
    /// The editor lifetime that requested the analysis.
    pub generation: u64,
    /// The editor revision the result belongs to.
    pub revision: u64,
    /// Resolved framing or an actionable failure.
    pub result: std::result::Result<SmartFrameAnalysis, String>,
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

/// Every editor open this frame, so several captures can each have one open
/// simultaneously without any card's Copy/Save/Upload/drag/overflow handling
/// having to know how many others exist.
///
/// A thin, `Copy` view over a slice rather than an owned collection: it is
/// rebuilt every frame from whatever the host is holding open, and every
/// consumer below only ever needs to look one card up at a time.
#[derive(Clone, Copy, Default)]
pub struct EditorSnapshots<'a>(&'a [EditorSnapshot<'a>]);

impl<'a> EditorSnapshots<'a> {
    /// No editors are open.
    pub const EMPTY: Self = Self(&[]);

    /// Wraps one snapshot per currently open editor.
    #[must_use]
    pub const fn new(snapshots: &'a [EditorSnapshot<'a>]) -> Self {
        Self(snapshots)
    }

    /// The live editor for `card`, if one is open.
    #[must_use]
    pub fn for_card(self, card: CardId) -> Option<EditorSnapshot<'a>> {
        self.0
            .iter()
            .copied()
            .find(|snapshot| snapshot.card == card)
    }

    /// Every card with an editor open this frame.
    fn cards(self) -> impl Iterator<Item = CardId> + 'a {
        self.0.iter().map(|snapshot| snapshot.card)
    }
}

/// One Done-triggered close waiting on its async jobs to answer (round 5,
/// Finding #1/#4).
///
/// `commit` gates every close (a plain Copy/Save/Upload or a native drag
/// reads this card's own bytes once the editor is gone, so those bytes must
/// be the committed revision before anything may finalize). `persist` only
/// gates a history-only editor's close, because only a history-only editor's
/// finalize releases its sole vault entry -- a live card's own bytes are
/// already safe once `commit` lands, and gating on persist for it too would
/// make Done unusable for a capture whose retention policy never durably
/// stores it at all (persist then fails deterministically, every time).
struct PendingEditorClose {
    generation: u64,
    revision: u64,
    /// The committed frame, held so the thumbnail refresh can wait for
    /// `commit` to land rather than showing it before the bytes underneath
    /// are actually the ones just rendered.
    frame: scrozz_core::Frame,
    /// Whether this close also needs `persist` to resolve before finalizing.
    editor_only: bool,
    commit: Option<Result<(), String>>,
    persist: Option<Result<(), String>>,
    /// The exact document Done captured, held until `commit` actually
    /// succeeds.
    ///
    /// Durable history persistence is dispatched from here -- by
    /// [`App::dispatch_deferred_persist`], never eagerly alongside the
    /// commit -- so a commit that fails can never be followed by a persist
    /// that durably writes the same rejected edit to history anyway. Before
    /// this a persist posted unconditionally could still land even though
    /// its sibling commit failed and the editor reopened, leaving history
    /// ahead of what the card (or, worse, a future Cancel) ever actually
    /// agreed to keep (round 6, Finding #3). Taken the first time `commit`
    /// resolves, so a duplicate resolution can never dispatch it twice.
    persist_data: Option<Box<DocumentData>>,
}

impl PendingEditorClose {
    /// `None` while still waiting on a required ack; otherwise whether every
    /// required ack that has arrived succeeded.
    ///
    /// A required ack that has already come back `Err` decides failure
    /// immediately, without waiting for any sibling ack still outstanding.
    /// Previously this waited for every required ack unconditionally, so a
    /// known commit failure paired with a persist that is blocked, missing,
    /// or never dispatched (see the deferred persist in
    /// [`App::dispatch_deferred_persist`]) left the close frozen forever --
    /// reopening the editor never happened because nothing ever answered
    /// `ready()` at all (round 6, Finding #2). Only a *success* determination
    /// needs every required ack to have landed.
    fn ready(&self) -> Option<bool> {
        if matches!(self.commit, Some(Err(_))) {
            return Some(false);
        }
        if self.editor_only && matches!(self.persist, Some(Err(_))) {
            return Some(false);
        }
        let commit = self.commit.as_ref()?;
        if self.editor_only {
            let persist = self.persist.as_ref()?;
            Some(commit.is_ok() && persist.is_ok())
        } else {
            Some(commit.is_ok())
        }
    }
}

/// What a resolved [`PendingEditorClose`] means for the host's frozen editor.
#[derive(Debug, Clone)]
pub enum EditorCloseOutcome {
    /// Every required job landed; the host may finalize through
    /// [`App::editor_closed`] and drop its editor entry.
    Committed,
    /// A required job refused; the host must reopen the editor instead of
    /// finalizing, so nothing discards the dirty document.
    Failed(String),
}

/// What family a dispatched output action belongs to (round 12).
///
/// Distinguishing the kind at the point an action is registered -- rather
/// than inferring it later from which map happened to hold it -- is what
/// lets a single unified `outstanding_output_actions` map replace the three
/// separate, kind-specific collections it consolidates, without losing any
/// of the kind-specific policy each one used to encode implicitly by its
/// own existence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputActionKind {
    /// A card-level Copy, Save, or Save-As -- dispatched from the card's own
    /// menu, from auto-close, or from overflow recovery. Mutually exclusive
    /// with itself per card (see
    /// [`App::card_has_outstanding_card_level_output`]) and always retires
    /// the card once it completes.
    CardOutput,
    /// An in-editor Copy or Save, dispatched from [`App::copy_rendered`] or
    /// [`App::save_rendered`] while the user is still actively editing.
    /// Never gates a card-level dispatch, and never retires the overlay
    /// card on its own -- the user has not indicated they are done editing
    /// just because one export finished (round 12, Finding #1: previously
    /// not tracked as outstanding at all, so nothing stopped a concurrent
    /// Cancel/Done from pruning its generation's fate before this action's
    /// own completion -- possibly late, possibly for an already-discarded
    /// revision -- had a chance to arrive).
    EditorOutput,
    /// A cloud upload. Unlike the other two kinds, a newer dispatch may
    /// supersede an older one that is still outstanding -- see
    /// [`App::current_upload_action`].
    Upload,
}

/// One dispatched-and-not-yet-resolved Copy/Save/Save-As/Upload action for a
/// card (round 12). See [`App::outstanding_output_actions`].
#[derive(Debug, Clone, Copy)]
struct OutstandingAction {
    kind: OutputActionKind,
    /// Whether *this* dispatch should retire the card once it completes,
    /// captured fresh at dispatch time. Always `true` for
    /// [`OutputActionKind::CardOutput`], always `false` for
    /// [`OutputActionKind::EditorOutput`], and caller-chosen for
    /// [`OutputActionKind::Upload`].
    close_after: bool,
}

/// The final fate recorded for one editor generation, once known (round 10,
/// Finding #1). See [`App::card_generation_fates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationFate {
    /// This generation's edit was committed by Done, with its final
    /// document revision.
    Committed(u64),
    /// This generation's edit was cancelled, or closed cleanly without ever
    /// committing anything.
    Cancelled,
}

#[derive(Clone)]
enum CardOutput {
    Copy(u64),
    Save(Option<PathBuf>, u64),
    /// The upload action id this dispatch answers for (round 7, Finding #2).
    ///
    /// Carried through `Job::Upload`/`Job::UploadImage` and back on
    /// `Outcome::UploadDone`/`Outcome::UploadRefused` so a completion for a
    /// since-superseded upload request can be told apart from the card's
    /// current one.
    Upload(u64),
}

struct PendingSaveDialog {
    card: CardId,
    /// Editor lifetime that produced `rendered`, so the eventual save can be
    /// matched against the card's currently-committed revision rather than
    /// trusted blindly (round 5, Finding #2). `None` alongside a `None`
    /// `rendered` -- there was no live editor when the dialog opened.
    generation: Option<u64>,
    rendered: Option<Box<RevisionedFrame>>,
    future: Pin<Box<dyn Future<Output = Option<rfd::FileHandle>>>>,
    /// This dialog's own output action id, registered as outstanding for
    /// `card` the moment the dialog opens (round 12) -- unlike the other
    /// card-level dispatches, a Save-As's outstanding window spans the
    /// whole dialog-plus-job lifetime, not just the job itself, so it must
    /// be allocated up front rather than at the point the job is finally
    /// built.
    action: u64,
}

struct PendingPinSaveDialog {
    capture: CaptureId,
    future: Pin<Box<dyn Future<Output = Option<rfd::FileHandle>>>>,
}

impl CardOutput {
    const fn label(&self) -> &'static str {
        match self {
            Self::Copy(_) => "copy",
            Self::Save(..) => "save",
            Self::Upload(_) => "upload",
        }
    }

    const fn action(&self) -> u64 {
        match self {
            Self::Copy(action) | Self::Save(_, action) | Self::Upload(action) => *action,
        }
    }

    const fn uses_clipboard(&self) -> bool {
        matches!(self, Self::Copy(_) | Self::Upload(_))
    }
}

#[cfg(target_os = "macos")]
struct PickerSurfaceReservation {
    mode: PickerMode,
    policy: AfterCaptureSettings,
    ready: std::sync::mpsc::Receiver<scrozz_core::Result<()>>,
    release: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
    clipboard: Option<ClipboardTurn>,
    presented: bool,
}

#[cfg(target_os = "macos")]
impl PickerSurfaceReservation {
    fn start(
        mode: PickerMode,
        selector: Arc<dyn CaptureSelector>,
        policy: AfterCaptureSettings,
        clipboard: Option<ClipboardTurn>,
    ) -> CliResult<Self> {
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
            policy,
            ready,
            release: Some(release),
            worker: Some(worker),
            clipboard,
            presented: false,
        })
    }

    fn take_capture_context(&mut self) -> (AfterCaptureSettings, Option<ClipboardTurn>) {
        (self.policy.clone(), self.clipboard.take())
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

/// A worker-built drag waiting for the window host to enter the native loop.
pub struct PendingDrag {
    /// Card or history capture.
    pub subject: DragSubject,
    /// Promised file and image data.
    pub payload: DragPayload,
    /// Source geometry.
    pub geometry: DragGeometry,
}

/// Chooses the window a scrolling capture will follow, as it is right now.
fn snapshot_scrolling_target() -> CliResult<ScrollingTarget> {
    let backend = platform::capture_backend()?;
    if crate::commands::is_wayland() {
        // Wayland never names a window, so the portal picker is the selection.
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

/// Re-resolves a snapshotted target immediately before capture starts.
fn refresh_scrolling_target(target: ScrollingTarget) -> CliResult<ScrollingTarget> {
    let backend = platform::capture_backend()?;
    target.refresh(backend.as_ref())
}

fn describe_scroll_progress(progress: &Progress) -> String {
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
        } => format!(
            "stitched frame {frame} ({delta} px advanced, {output_extent} px along the capture axis)"
        ),
        Progress::Stalled { count } => format!("saw no movement ({count})"),
        Progress::Interrupted { reason } => {
            format!("kept the valid stitched prefix after {reason}")
        }
        Progress::Finished {
            reason,
            frames,
            output_extent,
            ..
        } => format!("finished after {frames} frames, {output_extent} px ({reason:?})"),
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
        Self::new_with_scrolling_target_handlers(
            config,
            surface,
            selector,
            permission_ui_available,
            Box::new(snapshot_scrolling_target),
            Box::new(refresh_scrolling_target),
        )
    }

    #[cfg(test)]
    fn new_with_scrolling_target_resolver(
        config: Config,
        surface: Box<dyn CardSurface>,
        selector: Arc<dyn CaptureSelector>,
        permission_ui_available: bool,
        scrolling_target_resolver: Box<dyn Fn() -> CliResult<ScrollingTarget>>,
    ) -> CliResult<Self> {
        Self::new_with_scrolling_target_handlers(
            config,
            surface,
            selector,
            permission_ui_available,
            scrolling_target_resolver,
            Box::new(Ok),
        )
    }

    fn new_with_scrolling_target_handlers(
        config: Config,
        surface: Box<dyn CardSurface>,
        selector: Arc<dyn CaptureSelector>,
        permission_ui_available: bool,
        scrolling_target_resolver: Box<dyn Fn() -> CliResult<ScrollingTarget>>,
        scrolling_target_refresher: Box<dyn Fn(ScrollingTarget) -> CliResult<ScrollingTarget>>,
    ) -> CliResult<Self> {
        let waker = surface.waker();
        let pipeline = Pipeline::start_with_history_waker_and_retention(
            Arc::clone(&selector),
            config.history,
            waker.clone(),
            config.retention_policy.clone(),
        )?;
        let mut notes = Vec::new();
        if let Some(warning) = config.after_capture_warning.clone() {
            notes.push(warning);
        }
        let input_wake_monitor = match InputWakeMonitor::start(waker.clone()) {
            Ok(monitor) => monitor,
            Err(error) => {
                notes.push(format!(
                    "native input wake monitor unavailable; menu and hotkey events may wait for another window event: {error}"
                ));
                None
            }
        };
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
            let server = Server::bind_with_waker(waker.clone())?;
            notes.push(format!("listening at {}", server.path().display()));
            Some(server)
        } else {
            None
        };
        let forwarder = server
            .as_ref()
            .map(|_| Forwarder::start(Arc::clone(&selector), waker))
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

        // Read once at start-up so the machine has a validated configuration
        // even before the first recording; the two After Capture cells and the
        // interaction policy are re-read immediately before each recording
        // starts. A rejected document falls back to the shipped defaults, which
        // are the ones that need no Input Monitoring at all.
        let recording_settings =
            match crate::settings::recording_settings_from(&config.after_capture) {
                Ok(settings) => settings,
                Err(error) => {
                    notes.push(format!(
                    "recording preferences were rejected and shipped defaults are in use: {error}"
                ));
                    RecordingSettings::shipped()
                }
            };
        // A stable platform id, not a native handle: an unreadable one falls
        // back to "the platform default camera" rather than to no camera, so a
        // damaged setting cannot silently disable a feature the user enabled.
        let camera_device = match crate::settings::camera_device_from(&config.after_capture) {
            Ok(device) => device,
            Err(error) => {
                notes.push(format!(
                    "the saved camera preference was rejected; the default camera will be used: {error}"
                ));
                None
            }
        };
        let mut recording = RecordingState::new(recording_settings);
        if recording.machine.is_some()
            && let Err(error) = recording.select_camera_device(camera_device.clone())
        {
            notes.push(format!(
                "the saved camera could not be applied to the recording engine: {error}"
            ));
        }
        let cloud_settings = if config.sealed {
            crate::cloud::sealed_settings_model()
        } else {
            match crate::cloud::settings_model(scrozz_ui::CloudConnectionState::Idle) {
                Ok(model) => model,
                Err(error) => {
                    notes.push(format!("sharing settings are unavailable: {error}"));
                    crate::cloud::settings_error_model(error.to_string())
                }
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
            pending_admissions: VecDeque::new(),
            pending_pipeline_outcomes: VecDeque::new(),
            selector,
            file_launcher: default_file_launcher(),
            active_editor_cards: HashSet::new(),
            opening_cards: HashSet::new(),
            focus_editor_requests: VecDeque::new(),
            drag: DragHost::new(),
            modal_drag_input_release_pending: false,
            drag_keep_after_accept: HashSet::new(),
            outstanding_output_actions: HashMap::new(),
            current_upload_action: HashMap::new(),
            next_output_action: 0,
            card_retention: HashMap::new(),
            finalizing_cards: HashSet::new(),
            card_generation_fates: HashMap::new(),
            card_capture_ids: HashMap::new(),
            pending_retention_close: HashSet::new(),
            pending_retention_overflow: HashSet::new(),
            deferred_auto_close: HashMap::new(),
            deferred_save: HashSet::new(),
            deferred_overflow: HashSet::new(),
            pending_overflow_recovery: HashMap::new(),
            overflow_recovery_in_flight: HashSet::new(),
            pending_recent_captures_settings_save: None,
            pin_lock_escapes,
            pin_intents: HashMap::new(),
            input_wake_monitor,
            suppress_locked_restores: false,
            started: Instant::now(),
            captures: 0,
            sound_warning_shown: false,
            settings_requested: false,
            camera_settings_window: None,
            camera_preview: None,
            camera_device,
            camera_devices: scrozz_record::camera_devices,
            camera_permission_status: scrozz_record::camera_permission,
            camera_preview_start: scrozz_record::start_camera_preview,
            pending_camera_preferences_save: None,
            editor_requests: VecDeque::new(),
            smart_frame_results: VecDeque::new(),
            editor_only_cards: HashSet::new(),
            pending_editor_closes: HashMap::new(),
            editor_close_results: VecDeque::new(),
            editor_render_pending: HashMap::new(),
            editor_render_failed: HashMap::new(),
            next_editor_generation: 1,
            notes,
            exit_reason: None,
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
            history: Arc::new(Mutex::new(HistoryViewModel::new(Timestamp::now()))),
            prepared_history_drags: HashMap::new(),
            pending_drags: VecDeque::new(),
            recording,
            recorded_media: HashMap::new(),
            recorded_media_targets: HashMap::new(),
            recorded_history_id: None,
            pending_save_dialog: None,
            pending_pin_save_dialog: None,
            pending_pin_uploads: HashMap::new(),
            cloud_settings,
            sharing_settings_requested: false,
            connection_test: None,
            visible_cards: BTreeSet::new(),
            scroll_hud: None,
            scrolling_card: None,
            scrolling_target: None,
            scrolling_ready: None,
            scrolling_abort_pending: None,
            scrolling_start_pending: None,
            scrolling_keep_pending: false,
            scrolling_target_resolver,
            scrolling_target_refresher,
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

    /// Live Recent Captures Overlay behavior.
    #[must_use]
    pub const fn recent_captures_overlay_settings(
        &self,
    ) -> scrozz_ui::RecentCapturesOverlaySettings {
        self.config.recent_captures_overlay
    }

    /// Whether window captures currently keep the window's drop shadow.
    #[must_use]
    pub fn capture_settings(&self) -> scrozz_ui::settings::CaptureSettings {
        scrozz_ui::settings::CaptureSettings {
            window_shadow: crate::settings::window_shadow(&self.config.after_capture)
                .unwrap_or(true),
        }
    }

    /// Everything the Scenes pane draws, resolved from the persisted document.
    ///
    /// The preset library is the saved Smart Frame preset list: Scenes is a
    /// presentation surface over the same store the editor writes to, so
    /// "Save Scene as Preset" needs no second persistence path.
    #[must_use]
    pub fn scenes_model(&self) -> scrozz_ui::settings::ScenesModel {
        use scrozz_ui::settings::{
            SceneAssignment, SceneCapture, SceneChoice, ScenePreset, ScenePreviewStyle, ScenesModel,
        };

        let persisted = &self.config.after_capture;
        let default = crate::settings::scene_default(persisted)
            .map_or(SceneChoice::Auto, |token| SceneChoice::from_value(&token));
        let assignments = crate::settings::scene_assignments(persisted)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(slug, token)| {
                let kind = SceneCapture::from_slug(slug)?;
                Some((kind, SceneAssignment::from_value(&token)))
            })
            .collect();
        let mut presets = vec![ScenePreset::auto()];
        presets.extend(
            persisted
                .smart_frame_presets()
                .iter()
                .map(|preset| ScenePreset {
                    id: preset.id.clone(),
                    name: preset.name.clone(),
                    builtin: false,
                    style: ScenePreviewStyle::from_preset(&preset.settings),
                }),
        );
        ScenesModel {
            default,
            assignments,
            presets,
            has_recent_capture: self.newest_screenshot_capture().is_some(),
        }
    }

    /// The most recent still capture, newest card first.
    fn newest_screenshot_capture(&self) -> Option<scrozz_store::CaptureId> {
        let mut candidates: Vec<_> = self.card_capture_ids.iter().collect();
        candidates.sort_by_key(|(card, _)| std::cmp::Reverse(card.0));
        candidates
            .into_iter()
            .map(|(_, capture)| capture)
            .find(|capture| !self.history_entry_is_recording(capture))
            .cloned()
    }

    /// Persists capture fidelity preferences.
    pub fn edit_capture_settings(&mut self, settings: scrozz_ui::settings::CaptureSettings) {
        if settings == self.capture_settings() {
            return;
        }
        self.store_scene_value(
            crate::settings::WINDOW_SHADOW_KEY,
            &settings.window_shadow.to_string(),
        );
    }

    /// Applies one Scenes request, persisting assignments and preset edits.
    pub fn handle_scenes_event(&mut self, event: scrozz_ui::settings::ScenesEvent) {
        use scrozz_ui::settings::ScenesEvent;

        match event {
            ScenesEvent::SetDefault(choice) => {
                self.store_scene_value(crate::settings::SCENES_DEFAULT_KEY, &choice.to_value());
            }
            ScenesEvent::SetAssignment(kind, assignment) => {
                let Some((_, key)) = crate::settings::SCENE_CAPTURE_KEYS
                    .iter()
                    .find(|(slug, _)| *slug == kind.slug())
                else {
                    return;
                };
                self.store_scene_value(key, &assignment.to_value());
            }
            ScenesEvent::CreateFromCapture => {
                if let Some(capture) = self.newest_screenshot_capture() {
                    let card = self.pipeline.allocate();
                    if !self.pipeline.post(Job::OpenHistoryEditor { capture, card }) {
                        self.note("the capture worker has gone");
                    }
                } else {
                    self.begin_capture(CaptureOrigin::MenuBar, CaptureKind::Region);
                }
            }
            ScenesEvent::RenamePreset { id, name } => self.edit_scene_preset(&id, Some(name)),
            ScenesEvent::DuplicatePreset(id) => self.duplicate_scene_preset(&id),
            ScenesEvent::DeletePreset(id) => self.edit_scene_preset(&id, None),
        }
    }

    fn store_scene_value(&mut self, key: &str, value: &str) {
        let Some(store) = self.config.after_capture_store.clone() else {
            self.note("Scenes were not changed because no config directory is available");
            return;
        };
        let key = key.to_owned();
        let value = value.to_owned();
        match store.update(store.inferred_profile(), |latest| {
            latest.set_value(key.clone(), value.clone());
            Ok(())
        }) {
            Ok(updated) => self.config.after_capture = updated,
            Err(error) => self.note(format!("Scenes were not saved: {error}")),
        }
    }

    /// Renames a preset, or deletes it when `name` is `None`.
    ///
    /// Assignments naming a deleted preset fall back to the default rather than
    /// dangling: a row pointing at nothing would silently apply no Scene while
    /// still reading as configured.
    fn edit_scene_preset(&mut self, id: &str, name: Option<String>) {
        let Some(store) = self.config.after_capture_store.clone() else {
            self.note("Scenes were not changed because no config directory is available");
            return;
        };
        let id = id.to_owned();
        let token = format!("preset:{id}");
        let outcome = store.update(store.inferred_profile(), |latest| {
            match name.clone() {
                Some(name) => {
                    let Some(mut preset) = latest
                        .smart_frame_presets()
                        .iter()
                        .find(|preset| preset.id == id)
                        .cloned()
                    else {
                        return Ok(());
                    };
                    preset.name = name;
                    latest.upsert_smart_frame_preset(preset)?;
                }
                None => crate::settings::forget_scene_preset(latest, &id)?,
            }
            Ok(())
        });
        match outcome {
            Ok(updated) => self.config.after_capture = updated,
            Err(error) => self.note(format!("Scenes were not saved: {error}")),
        }
    }

    fn duplicate_scene_preset(&mut self, id: &str) {
        let Some(store) = self.config.after_capture_store.clone() else {
            self.note("Scenes were not changed because no config directory is available");
            return;
        };
        let id = id.to_owned();
        let source = self
            .config
            .after_capture
            .smart_frame_presets()
            .iter()
            .find(|preset| preset.id == id)
            .cloned();
        let outcome = store.update(store.inferred_profile(), |latest| {
            // Duplicating the immutable built-in has to synthesize one: `Auto`
            // is not in the preset list, so there is nothing to clone from.
            let mut preset = source.clone().unwrap_or_default();
            if id == scrozz_ui::settings::AUTO_PRESET_ID {
                preset.name = "Auto".to_owned();
            }
            let taken: Vec<_> = latest
                .smart_frame_presets()
                .iter()
                .map(|preset| preset.name.clone())
                .collect();
            let base = format!("{} copy", preset.name);
            let mut name = base.clone();
            let mut suffix = 2;
            while taken.contains(&name) {
                name = format!("{base} {suffix}");
                suffix += 1;
            }
            preset.name = name;
            // Lowest free ordinal rather than a random id: reproducible, and a
            // settings file stays readable by a human debugging it.
            let mut ordinal = 1;
            loop {
                let candidate = format!("scene-{ordinal}");
                if !latest
                    .smart_frame_presets()
                    .iter()
                    .any(|preset| preset.id == candidate)
                {
                    preset.id = candidate;
                    break;
                }
                ordinal += 1;
            }
            latest.upsert_smart_frame_preset(preset)?;
            Ok(())
        });
        match outcome {
            Ok(updated) => self.config.after_capture = updated,
            Err(error) => self.note(format!("Scenes were not saved: {error}")),
        }
    }

    /// Services every source once. Never blocks.
    ///
    /// Order matters: HUD cancellation is applied before a completion already
    /// waiting in the worker queue, so an explicit Discard remains authoritative
    /// at the final frame boundary.
    pub fn tick(&mut self) -> Tick {
        self.tick_with_editor(EditorSnapshots::EMPTY)
    }

    /// Services every source once with the current editor documents available
    /// to card output actions.
    ///
    /// A card being edited must never fall back to its original cached pixels:
    /// that would let Copy or Save bypass destructive redactions. Each
    /// snapshot is borrowed only for this pass and rendered into an
    /// immutable, revision-tagged frame before it is sent to the worker.
    pub fn tick_with_editor(&mut self, editors: EditorSnapshots<'_>) -> Tick {
        self.active_editor_cards = editors.cards().collect();
        self.opening_cards
            .retain(|card| !self.active_editor_cards.contains(card));
        if self.expired() {
            self.note("the run deadline passed");
            return self.stop(ExitReason::Deadline);
        }

        self.drain_permission();

        if let Tick::Stop(reason) = self.drain_tray() {
            return Tick::Stop(reason);
        }
        self.drain_hotkeys();
        if let Tick::Stop(reason) = self.drain_server() {
            return Tick::Stop(reason);
        }
        self.with_history(|history| history.advance_clock(Timestamp::now()));
        self.advance_recording();
        self.advance_camera_preview();
        self.sync_camera_settings_window();
        self.drain_pipeline();
        self.drain_save_dialog();
        self.drain_pin_save_dialog();
        self.drain_connection_test();
        self.drain_cards(editors);
        // After the HUD's own events: an abort raised this frame must beat the
        // finished capture that was held back for exactly one pass.
        self.drain_scrolling_start();
        self.drain_scrolling_ready();
        self.drain_drags();
        self.drain_history();
        self.flush_recent_captures_overlay_settings(false);
        self.flush_camera_preferences(false);

        Tick::Continue
    }

    fn expired(&self) -> bool {
        self.config
            .deadline
            .is_some_and(|limit| self.started.elapsed() >= limit)
    }

    pub(crate) fn remaining_deadline(&self) -> Option<Duration> {
        self.config
            .deadline
            .map(|limit| limit.saturating_sub(self.started.elapsed()))
    }

    /// Whether the host should keep polling a modeless native Save As future.
    pub(crate) const fn has_pending_save_dialog(&self) -> bool {
        self.pending_save_dialog.is_some() || self.pending_pin_save_dialog.is_some()
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

        self.dispatch_tray_batch(pending)
    }

    fn dispatch_tray_batch(&mut self, pending: impl IntoIterator<Item = TrayAction>) -> Tick {
        for entry in pending {
            if let Tick::Stop(reason) =
                self.perform_from(CaptureOrigin::MenuBar, Action::from_tray(entry))
            {
                return Tick::Stop(reason);
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
    /// Ownership is a set because editor focus and shortcut recording can
    /// overlap. Only the recorder suspends global capture bindings: an editor
    /// must remain capturable while it owns ordinary typing and local commands.
    ///
    /// The bindings themselves are untouched throughout, so the menu-bar item
    /// keeps showing the right shortcut beside each command, and a shortcut
    /// edited while suspended is the one that comes back.
    pub fn set_keyboard_owner(&mut self, owner: KeyboardOwner, owns: bool) {
        let before = self.keyboard_owners;
        let recorder_was_active = before & KeyboardOwner::ShortcutRecorder.bit() != 0;
        self.keyboard_owners = if owns {
            before | owner.bit()
        } else {
            before & !owner.bit()
        };
        let should_suspend = self.keyboard_owners & KeyboardOwner::ShortcutRecorder.bit() != 0;
        if should_suspend {
            if self.hotkeys.is_suspended() {
                return;
            }
            let report = self.hotkeys.suspend();
            if !report.is_complete() {
                for rejection in report.rejections() {
                    tracing::warn!(
                        action = %rejection.action,
                        accelerator = %rejection.accelerator,
                        reason = %rejection.reason,
                        "a global hotkey remained active under the shortcut recorder"
                    );
                }
                self.note(format!(
                    "{} shortcut(s) could not be released for the active shortcut recorder",
                    report.rejections().len()
                ));
            }
        } else if recorder_was_active {
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
            guard.accelerator == event.accelerator && Instant::now() < guard.expires
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
        loop {
            // Keep the server borrow inside this expression. The request owns
            // its response channel, so serving it can then borrow the whole app.
            let request = self.server.as_ref().and_then(Server::poll);
            let Some(request) = request else {
                break;
            };
            tracing::debug!(?request, "a forwarded command arrived");
            let mut with_argv0 = Vec::with_capacity(request.argv.len() + 1);
            with_argv0.push("scrozz".to_owned());
            with_argv0.extend(request.argv.iter().cloned());
            let parsed = Cli::try_parse_from(with_argv0)
                .ok()
                .and_then(|cli| cli.command);
            let needs_live_pin_state = parsed.as_ref().is_some_and(|command| {
                forwarded_unpin(command).is_some() || forwarded_unlock_pins(command)
            });
            let opens_history = parsed.as_ref().is_some_and(forwarded_open_history);

            if opens_history {
                self.perform(Action::OpenHistory);
                request.answer(&Ok(Report::new(
                    Json::obj([("state", Json::str("open"))]),
                    "Capture History opened.".to_owned(),
                )));
                continue;
            }

            // A recording belongs to this process, so a forwarded `record`
            // cannot be run on the worker like an ordinary command: it has to
            // reach the one machine that owns the lifecycle.
            if let Some(crate::cli::Command::Record(args)) = parsed
                && !args.dry_run
            {
                self.serve_forwarded_recording(&args, request);
                continue;
            }

            // Pin state lives on this thread and needs no selector, so these
            // run here rather than on the worker.
            if needs_live_pin_state {
                let mut accepted_capture = false;
                let command = request.serve_with(|command, captured| {
                    accepted_capture = captured.is_some();
                    self.accept_forwarded(command, captured)
                });
                if let Some(command) = command {
                    self.observe_forwarded_command(&command, accepted_capture);
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

        // Everything the worker finished is waiting on this thread to take it.
        // The worker is blocked until each is completed, which is what makes a
        // forwarded capture's success reply mean the pixels were accepted.
        if let Some(forwarder) = &self.forwarder {
            while let Some(admission) = forwarder.poll() {
                self.pending_admissions.push_back(admission);
            }
        }
        while let Some(mut admission) = self.pending_admissions.pop_front() {
            if admission.has_capture() && !self.pipeline.can_accept_forwarded_capture() {
                self.pending_admissions.push_front(admission);
                break;
            }
            let captured = admission.take_capture();
            let accepted_capture = captured.is_some();
            let accepted = self.accept_forwarded(admission.command(), captured);
            if let Err(error) = &accepted {
                self.note(format!(
                    "a forwarded command could not be completed by this instance: {error}"
                ));
            }
            if let Some(command) = admission.complete(accepted) {
                self.observe_forwarded_command(&command, accepted_capture);
            }
        }

        // A forwarded command never ends this process. `scrozz quit` is not a
        // command; quitting is a menu entry, because the thing being quit is
        // the application the user can see, not a request that arrived on a
        // socket.
        Tick::Continue
    }

    /// Answers a forwarded `record`, honouring the same IPC contract as before.
    ///
    /// `--stop` is parked until the recording actually finalises, because the
    /// answer is what the file turned out to contain. A start answers as soon
    /// as it is armed: the caller asked for a recording to begin, and it has.
    fn serve_forwarded_recording(&mut self, args: &crate::cli::RecordArgs, request: Request) {
        if args.stop {
            if !self.recording.is_busy() {
                request.answer(&Err(CliError::Core(CoreError::InvalidRequest(
                    "no recording is in progress, so there is nothing to stop".to_owned(),
                ))));
                return;
            }
            self.begin_recording_finalisation(Some(request));
            return;
        }
        let result = self.start_forwarded_recording(args);
        request.answer(&result);
    }

    fn start_forwarded_recording(&mut self, args: &crate::cli::RecordArgs) -> CliResult<Report> {
        if self.recording.machine.is_none() {
            return Err(CliError::Core(self.recording.unavailable_error()));
        }
        if self.recording.is_busy() {
            return Err(CliError::Core(CoreError::InvalidRequest(
                "a recording is already in progress; stop it before starting another".to_owned(),
            )));
        }
        if self
            .recording
            .editor
            .as_ref()
            .is_some_and(ActiveVideoEditor::is_exporting)
        {
            return Err(CliError::Core(CoreError::InvalidRequest(
                "cancel the active export before starting another recording".to_owned(),
            )));
        }
        self.save_recording_settings_panel()?;
        self.reload_recording_settings()?;
        Self::ensure_recording_permission()?;
        self.ensure_recording_input_permission()?;
        self.reset_finished_recording()?;
        self.release_video_editor();
        self.recording.preflight_failure = None;
        self.recording.tick = Instant::now();
        self.recording.completion = None;

        // An explicit CLI target is honoured exactly as typed; only an
        // interactive one opens the selector.
        if let crate::cli::TargetSpec::Interactive(mode) =
            crate::commands::recording_target_spec(args)?
        {
            self.begin_recording_selection(SelectionStart::Request(Box::new(args.clone())), mode)?;
            return Ok(Report::new(
                Json::obj([
                    ("state", Json::str("selecting")),
                    ("mode", Json::str(crate::commands::interactive_slug(mode))),
                ]),
                format!(
                    "Selecting a {} to record.",
                    crate::commands::interactive_slug(mode)
                ),
            ));
        }

        let prepared = crate::commands::prepare_recording_args(args)?;
        let report = prepared.started_report();
        self.arm_recording_start(PendingStart::Request(Box::new(prepared.request)));
        Ok(report)
    }

    fn observe_forwarded_command(&mut self, command: &crate::cli::Command, accepted_capture: bool) {
        // A capture whose pixels reached the pipeline is counted when its card
        // arrives, like every other capture. Counting it here as well would
        // report one screenshot as two. What remains for this branch is the
        // capture that produced no card at all — a dry run, or a plan that
        // never opened a backend.
        self.captures +=
            u64::from(matches!(command, crate::cli::Command::Capture(_)) && !accepted_capture);
        if matches!(
            command,
            crate::cli::Command::Settings(crate::cli::SettingsArgs {
                command: SettingsCommand::Reload
            })
        ) {
            self.reload_persisted_settings();
        }
        if matches!(command, crate::cli::Command::Gui) {
            self.note("a second launch was answered by this instance");
        } else {
            self.with_history(|history| history.refresh_from_start(Timestamp::now()));
        }
    }

    fn reload_persisted_settings(&mut self) {
        let Some(store) = self.config.after_capture_store.clone() else {
            self.note("persisted settings changed but no config path is available to reload");
            return;
        };
        match store.load(store.inferred_profile()) {
            Ok(settings) => {
                let resolved = crate::settings::screenshot_sound(&settings).and_then(|sound| {
                    crate::settings::retention_policy(&settings).and_then(|retention| {
                        crate::settings::recent_captures_overlay_settings(&settings)
                            .map(|overlay| (sound, retention, overlay))
                    })
                });
                match resolved {
                    Ok((sound, retention, overlay)) => {
                        if self.set_retention_policy(retention) {
                            self.config.after_capture = settings;
                            self.config.screenshot_sound = sound;
                            self.config.recent_captures_overlay = overlay;
                            self.surface.configure_recent_captures_overlay(overlay);
                            self.note("persisted settings reloaded");
                        } else {
                            self.note(
                                "persisted settings changed but the capture worker could not apply history retention",
                            );
                        }
                    }
                    Err(error) => {
                        self.note(format!(
                            "persisted settings changed but could not be applied: {error}"
                        ));
                    }
                }
            }
            Err(error) => {
                self.note(format!(
                    "persisted settings changed but could not be reloaded: {error}"
                ));
            }
        }
    }

    /// Completes, on this thread, everything a forwarded command asked the app
    /// for — before its caller is told the command succeeded.
    ///
    /// The pixels are *moved* into the capture pipeline here, so a burst that
    /// the pipeline refuses becomes a failure the caller sees rather than a
    /// capture that quietly vanished.
    fn accept_forwarded(
        &mut self,
        command: &crate::cli::Command,
        captured: Option<ForwardedCapture>,
    ) -> CliResult<()> {
        if let Some(ForwardedCapture { kind, capture }) = captured {
            let card = self.accept_capture(kind, capture)?;
            self.note(format!("{card} accepted from a forwarded capture"));
        }
        if let Some(id) = forwarded_unpin(command) {
            let capture = CaptureId(id.to_owned());
            let generation = self.set_pin_intent(capture.clone(), false);
            self.surface.discard_pin(&capture);
            self.pipeline.terminal_unpin(capture, generation)?;
            self.note(format!("pinned capture {id} closed after forwarded unpin"));
        }
        if forwarded_unlock_pins(command) {
            self.unlock_all_pins()?;
            self.note("pinned captures unlocked from the command line");
        }
        Ok(())
    }

    pub(crate) fn drain_capture_feedback(&mut self) {
        let forwarded_shutters = self
            .forwarder
            .as_ref()
            .map_or_else(Vec::new, crate::gui::server::Forwarder::drain_shutters);
        for acknowledged in forwarded_shutters {
            self.play_shutter_sound();
            let _ = acknowledged.send(());
        }
        while let Some(outcome) = self.pipeline.poll() {
            match outcome {
                Outcome::Shutter { card, acknowledged } => {
                    tracing::debug!(%card, "playing feedback at shutter commitment");
                    self.play_shutter_sound();
                    let _ = acknowledged.send(());
                }
                other => self.pending_pipeline_outcomes.push_back(other),
            }
        }
    }

    fn drain_pipeline(&mut self) {
        while let Some(outcome) = self
            .pending_pipeline_outcomes
            .pop_front()
            .or_else(|| self.pipeline.poll())
        {
            match outcome {
                Outcome::Shutter { card, acknowledged } => {
                    tracing::debug!(%card, "playing feedback at shutter commitment");
                    self.play_shutter_sound();
                    let _ = acknowledged.send(());
                }
                Outcome::HistoryRecording {
                    capture,
                    path,
                    duration_secs,
                    target,
                } => self.open_history_recording(&capture, path, duration_secs, target),
                Outcome::Ready(ready) => {
                    // Scrolling publication is atomically sealed on the worker
                    // before durable actions run. Hold the completed value until
                    // the end of this coordinator pass, then present it.
                    if self.scrolling_card == Some(ready.card.id) {
                        debug_assert!(
                            self.scrolling_ready.is_none(),
                            "only one scrolling capture can be ready at a time"
                        );
                        self.scrolling_ready = Some(ready);
                        continue;
                    }
                    self.handle_ready(*ready);
                }
                Outcome::Finalized(finalized) => self.handle_finalized(*finalized),
                Outcome::Progress { card, progress } => {
                    self.update_scroll_hud(card, &progress);
                    self.note(format!("{card} {}", describe_scroll_progress(&progress)));
                }
                Outcome::Restored(mut card) => {
                    card.upload_available = self.cloud_settings.upload_enabled;
                    card.upload_unavailable_reason = self.cloud_settings.unavailable_reason.clone();
                    let summary = card.summary();
                    let card_id = card.id;
                    let capture_id = card.capture_id.clone();
                    if let Err(err) = self.surface.present(*card) {
                        self.pipeline.post(Job::Release(card_id));
                        self.note(format!("a restored card could not be shown: {err}"));
                    } else {
                        self.visible_cards.insert(card_id);
                        self.card_retention.insert(card_id, (true, false));
                        if let Some(capture_id) = capture_id {
                            self.card_capture_ids.insert(card_id, capture_id);
                        }
                        self.note(format!("restored {summary}"));
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
                    if kind != CaptureKind::Scrolling
                        || !self.finish_failed_scrolling_capture(card, &error)
                    {
                        self.note(format!("{card} failed: {error}"));
                    }
                    if permission_denied {
                        self.handle_capture_permission_failure(kind, origin, Self::screen_access());
                    }
                }
                Outcome::Started { card, detail } => {
                    self.surface.set_status(card, Some(detail.clone()));
                    self.note(format!("{card} {detail}"));
                }
                Outcome::Done {
                    card,
                    detail,
                    version,
                    action,
                } => {
                    // Round 13: existence is validated *before* anything
                    // else, by peeking (never removing) this exact action in
                    // `outstanding_output_actions` first. A duplicate
                    // delivery of an already-resolved completion -- the same
                    // dispatch answered twice -- must be a total no-op: no
                    // status, no note, no staleness check, no retention
                    // mutation. Checking existence any later would be too
                    // late for its own sake: once this card's last
                    // outstanding action resolves,
                    // `prune_settled_generation_fates` erases every fate
                    // recorded for it, so a stale-but-already-resolved
                    // duplicate arriving after that point would find no
                    // fate to disagree with and be wrongly trusted as
                    // current by `output_version_is_stale`'s own `None`
                    // branch -- exactly the gap this closes.
                    let outstanding = self
                        .outstanding_output_actions
                        .get(&card)
                        .is_some_and(|actions| actions.contains_key(&action));
                    if !outstanding {
                        continue;
                    }
                    // Round 5, Finding #2 / round 9, Finding #1: a Save
                    // dispatched against a live editor's revision can
                    // complete after a *later* Done has already committed a
                    // newer revision and reset retention for it, or after
                    // that exact editing session was Cancelled and never
                    // committed anything at all. Only trust the completion
                    // when it is not known to be stale -- i.e. no
                    // *different* revision has since been committed for this
                    // exact editor generation, and this generation was never
                    // cancelled. A plain (no live editor) output has no
                    // version to race and always applies, and the first
                    // completion of a fresh, still-open editing session
                    // (nothing committed, nothing cancelled, yet) is trusted
                    // too. This is evaluated only now that `action` is
                    // confirmed still outstanding, so the fate this reads (if
                    // any) cannot have already been pruned out from under it.
                    let stale = self.output_version_is_stale(card, version);
                    if stale {
                        // Round 6, Finding #1: this Save/Copy answered
                        // *after* a later Done committed a newer revision --
                        // or, since round 9, after this exact edit was
                        // discarded by Cancel.
                        // `complete_output_action` unconditionally dismisses
                        // the card once `close_after_output` clears, and if
                        // the editor that produced the newer revision has
                        // since closed, dismissal's `Job::Release` would
                        // destroy that newer, still-live committed revision
                        // to make room for an action that only ever knew
                        // about the one before it. Retire only this stale
                        // action's own bookkeeping, resolving any overflow
                        // retirement waiting behind it against whatever the
                        // card holds now instead. Neither retention nor the
                        // status line nor the notes log is touched -- for a
                        // cancelled edit that is the card's pre-edit
                        // retention, untouched by Cancel, which is precisely
                        // what round 9, Finding #1 requires; round 13
                        // extends the same "only a matching, current,
                        // non-stale completion may mutate anything user-
                        // visible" rule to the status/notes surface too.
                        self.resolve_stale_output_completion(card, action);
                        continue;
                    }
                    // Only a matching, current, non-stale completion ever
                    // reaches here to update the status line, the notes
                    // log, retention, or trigger dismissal (round 13).
                    self.surface.set_status(card, Some(detail.clone()));
                    self.note(format!("{card} {detail}"));
                    if let Some(retention) = self.card_retention.get_mut(&card)
                        && detail.starts_with("saved")
                    {
                        retention.0 = true;
                        retention.1 = true;
                    }
                    self.complete_output_action(card, action);
                }
                Outcome::UploadDone {
                    card,
                    detail,
                    version,
                    action,
                } => {
                    if let Some(capture) = self.pending_pin_uploads.remove(&card) {
                        self.note(format!("pinned capture {} {detail}", capture.0));
                        self.pipeline.post(Job::Release(card));
                        continue;
                    }
                    let Some(_) = self.outstanding_upload_action(card, action) else {
                        continue;
                    };
                    let is_current_action = self.current_upload_action.get(&card) == Some(&action);
                    if !is_current_action {
                        self.resolve_output_action(card, action);
                        continue;
                    }
                    let stale = self.output_version_is_stale(card, version);
                    if stale {
                        // A stale upload may settle only the action it answers
                        // for. It cannot resume overflow retirement against a
                        // newer committed revision.
                        self.resolve_output_action(card, action);
                        continue;
                    }
                    self.surface.set_status(card, Some(detail.clone()));
                    self.note(format!("{card} {detail}"));
                    // Round 12: a single call resolves this exact action's
                    // own entry (idempotently) and reports whether it was
                    // still outstanding and asked to close the card, in one
                    // step -- replacing the previous separate
                    // `pending_upload.remove` (for `close_after`) and
                    // `resolve_upload_dispatch` (for the outstanding count)
                    // calls, which raced each other across two independent
                    // collections.
                    let close_after = self
                        .resolve_output_action(card, action)
                        .is_some_and(|resolved| resolved.close_after);
                    if let Some(retention) = self.card_retention.get_mut(&card) {
                        retention.0 = true;
                    }
                    self.complete_upload(card, close_after);
                }
                Outcome::Opened {
                    card,
                    document,
                    editor_only,
                } => {
                    let generation = self.next_editor_generation;
                    self.next_editor_generation = self.next_editor_generation.wrapping_add(1);
                    // Round 10, Finding #1: a brand-new editing session
                    // needs no clean-slate reset here at all -- its
                    // `generation` number is freshly allocated and has never
                    // been used before, so `card_generation_fates` has no
                    // entry for it yet, and an absent entry is exactly what
                    // `Self::output_version_is_stale` already trusts as "not
                    // known to be stale". Previously this scrubbed *every*
                    // fate recorded for the card so far, which erased an
                    // older, still-unresolved generation's own tombstone the
                    // instant any new editor opened -- long before every
                    // action dispatched against that older generation had
                    // actually resolved.
                    if editor_only {
                        self.editor_only_cards.insert(card);
                    }
                    self.editor_requests.push_back(EditorRequest {
                        card,
                        generation,
                        document: *document,
                        smart_frame_presets: self
                            .config
                            .after_capture
                            .smart_frame_presets()
                            .to_vec(),
                    });
                }
                Outcome::Prepared {
                    card,
                    generation,
                    revision,
                } => {
                    let version = (generation, revision);
                    if self.editor_render_pending.get(&card) == Some(&version) {
                        self.editor_render_pending.remove(&card);
                    }
                    if self.editor_render_failed.get(&card) == Some(&version) {
                        self.editor_render_failed.remove(&card);
                    }
                }
                Outcome::PreparationFailed {
                    card,
                    generation,
                    revision,
                    error,
                } => {
                    let version = (generation, revision);
                    if self.editor_render_pending.get(&card) == Some(&version) {
                        self.editor_render_pending.remove(&card);
                    }
                    self.editor_render_failed.insert(card, version);
                    self.note(format!(
                        "{card} editor {generation} revision {revision} could not be prepared for \
                         drag: {error}"
                    ));
                }
                Outcome::SmartFrameAnalyzed {
                    card,
                    generation,
                    revision,
                    result,
                } => {
                    self.smart_frame_results.push_back(SmartFrameResult {
                        card,
                        generation,
                        revision,
                        result: *result,
                    });
                }
                Outcome::Refused { card, error } => {
                    if self.opening_cards.remove(&card) {
                        self.surface.set_editing(card, false);
                        if self.editor_only_cards.remove(&card) {
                            self.pipeline.post(Job::Release(card));
                        }
                        // The open that a deferred auto-close or overflow was
                        // waiting on never produced an editor to close later,
                        // so nothing will ever call `editor_closed` for it.
                        // Resolve deferred cleanup now, against no editor, so
                        // a card cannot stay pinned open forever behind a
                        // decode failure.
                        if let Some(action) = self.deferred_auto_close.remove(&card) {
                            self.handle_auto_close(card, action, EditorSnapshots::EMPTY);
                        }
                        if self.deferred_overflow.remove(&card) {
                            self.handle_overflow(card, EditorSnapshots::EMPTY);
                        }
                    }
                    self.surface
                        .set_status(card, Some(format!("Action failed: {error}")));
                    self.note(format!("{card} refused: {error}"));
                }
                Outcome::UploadRefused {
                    card,
                    error,
                    action,
                } => {
                    if let Some(capture) = self.pending_pin_uploads.remove(&card) {
                        self.note(format!(
                            "pinned capture {} upload refused: {error}",
                            capture.0
                        ));
                        self.pipeline.post(Job::Release(card));
                        continue;
                    }
                    let Some(_) = self.outstanding_upload_action(card, action) else {
                        continue;
                    };
                    let is_current_action = self.current_upload_action.get(&card) == Some(&action);
                    let resolved = self.resolve_output_action(card, action);
                    if !is_current_action {
                        continue;
                    }
                    let close_after = resolved.is_some_and(|resolved| resolved.close_after);
                    // A failed upload leaves the card exactly where it is: the
                    // link the user asked for never arrived, so hiding it now
                    // would throw away the only copy they can still act on.
                    self.fail_upload(card, close_after);
                    self.surface
                        .set_status(card, Some(format!("Action failed: {error}")));
                    self.note(format!("{card} upload refused: {error}"));
                }
                Outcome::CardOutputCommitted {
                    card,
                    generation,
                    revision,
                } => {
                    if let Some(pending) = self.pending_editor_closes.get_mut(&card)
                        && pending.generation == generation
                        && pending.revision == revision
                    {
                        pending.commit = Some(Ok(()));
                    }
                    // Only now that the commit is known to have succeeded may
                    // the durable history write for this exact revision be
                    // dispatched -- never unconditionally alongside the
                    // commit itself (round 6, Finding #3). A no-op once
                    // already dispatched or if no pending close matches.
                    self.dispatch_deferred_persist(card, generation, revision);
                    self.resolve_pending_editor_close(card, generation, revision);
                }
                Outcome::CardOutputCommitFailed {
                    card,
                    generation,
                    revision,
                    error,
                } => {
                    if let Some(pending) = self.pending_editor_closes.get_mut(&card)
                        && pending.generation == generation
                        && pending.revision == revision
                    {
                        pending.commit = Some(Err(error.to_string()));
                    }
                    // The optimistic dispatch-time write in
                    // `commit_card_output` never actually became durable --
                    // clear it rather than leave a phantom "committed"
                    // revision on record. Otherwise a legitimate Save the
                    // user makes after Finding #1 reopens this same still-
                    // live editor (a later revision, same generation) would
                    // wrongly compare unequal to this failed attempt and be
                    // suppressed as "stale" even though it is the only
                    // record of the card's current content (round 5,
                    // Finding #2). Only this exact generation's own entry is
                    // ever touched here -- any other generation's recorded
                    // fate for this card is a separate, already-resolved
                    // editing session this failure has nothing to say about
                    // (round 10, Finding #1).
                    if let Some(fates) = self.card_generation_fates.get_mut(&card) {
                        if fates.get(&generation) == Some(&GenerationFate::Committed(revision)) {
                            fates.remove(&generation);
                        }
                        if fates.is_empty() {
                            self.card_generation_fates.remove(&card);
                        }
                    }
                    self.surface
                        .set_status(card, Some(format!("Action failed: {error}")));
                    self.note(format!(
                        "{card} editor {generation}'s committed edit could not be filed: {error}"
                    ));
                    self.resolve_pending_editor_close(card, generation, revision);
                }
                Outcome::EditorClosePersisted {
                    card,
                    generation,
                    revision,
                    capture,
                    pin_texture,
                } => {
                    let matches_pending =
                        self.pending_editor_closes
                            .get(&card)
                            .is_some_and(|pending| {
                                pending.generation == generation && pending.revision == revision
                            });
                    if !matches_pending {
                        continue;
                    }
                    match pin_texture {
                        Ok(Some((texture, natural_size))) => {
                            if let Err(error) =
                                self.surface
                                    .refresh_pin_texture(&capture, texture, natural_size)
                            {
                                self.surface.discard_pin(&capture);
                                self.note(format!(
                                    "pinned capture {} closed because editor {generation} revision \
                                     {revision} could not replace its pixels: {error}",
                                    capture.0
                                ));
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            // Never leave pre-edit pixels visible after a
                            // destructive revision has become durable.
                            self.surface.discard_pin(&capture);
                            self.note(format!(
                                "pinned capture {} closed because editor {generation} revision \
                                 {revision} could not render safe replacement pixels: {error}",
                                capture.0
                            ));
                        }
                    }
                    self.pending_editor_closes
                        .get_mut(&card)
                        .expect("matching pending editor close")
                        .persist = Some(Ok(()));
                    self.resolve_pending_editor_close(card, generation, revision);
                }
                Outcome::EditorClosePersistFailed {
                    card,
                    generation,
                    revision,
                    error,
                } => {
                    if let Some(pending) = self.pending_editor_closes.get_mut(&card)
                        && pending.generation == generation
                        && pending.revision == revision
                    {
                        pending.persist = Some(Err(error.to_string()));
                    }
                    self.surface
                        .set_status(card, Some(format!("Action failed: {error}")));
                    self.note(format!(
                        "{card} editor {generation}'s edit could not be persisted: {error}"
                    ));
                    self.resolve_pending_editor_close(card, generation, revision);
                }
                Outcome::OutputRefused {
                    card,
                    error,
                    action,
                } => {
                    // Round 12, Finding #1: resolved by exact action id, so
                    // an in-editor Copy/Save's own failure never consumes a
                    // concurrently outstanding card-level dispatch's
                    // bookkeeping (or vice versa) purely because they share
                    // a card. The overflow-recovery-failure dismissal below
                    // is specific to a card-level dispatch -- an editor's
                    // own failed export must never trigger it.
                    let resolved = self.resolve_output_action(card, action);
                    if resolved
                        .is_some_and(|resolved| resolved.kind == OutputActionKind::CardOutput)
                        && self.overflow_recovery_in_flight.remove(&card)
                    {
                        self.dismiss_recent_capture(
                            card,
                            "released after overflow recovery export failed",
                        );
                    }
                    self.note(format!("{card} refused output: {error}"));
                }
                Outcome::RetentionRelease { card, released } => {
                    let overflowed = self.pending_retention_overflow.remove(&card);
                    let auto_closed = self.pending_retention_close.remove(&card);
                    if !overflowed && !auto_closed {
                        continue;
                    }
                    if released {
                        self.pending_overflow_recovery.remove(&card);
                        self.dismiss_recent_capture(card, "auto-closed after retention check");
                    } else {
                        if let Some(state) = self.card_retention.get_mut(&card) {
                            state.0 = state.1;
                        }
                        if overflowed {
                            if let Some(job) = self.pending_overflow_recovery.remove(&card) {
                                // Round 12: the action id was already
                                // allocated and embedded into `job` back
                                // when it was first built (see
                                // `handle_overflow`'s "retained" branch) --
                                // but it only becomes *outstanding* now, at
                                // the point it is actually posted, matching
                                // every other dispatch site's "register on
                                // successful post, never before" rule.
                                let action = Self::job_action(&job);
                                if self.pipeline.post(job) {
                                    if let Some(action) = action {
                                        self.register_output_action(
                                            card,
                                            action,
                                            OutputActionKind::CardOutput,
                                            true,
                                        );
                                    }
                                    self.overflow_recovery_in_flight.insert(card);
                                    self.note(format!(
                                        "{card} is being saved because history no longer retains its overflowed pixels"
                                    ));
                                } else {
                                    self.note(format!(
                                        "{card} live pixels were preserved because its recovery save could not be queued"
                                    ));
                                }
                            }
                        } else {
                            self.note(format!(
                                "{card} stayed visible because it is the only retained artifact"
                            ));
                        }
                    }
                }
                Outcome::PinReady(mut pin) => {
                    if !self.accept_pin_restore(&pin.id) {
                        continue;
                    }
                    if self.suppress_locked_restores {
                        pin.state.locked = false;
                    }
                    let capture = pin.id.0.clone();
                    let content_error = pin.content_error.clone();
                    if let Err(err) = self.surface.restore_pin(*pin) {
                        self.note(format!(
                            "pinned capture {capture} could not be shown: {err}"
                        ));
                    } else {
                        if let Some(error) = content_error {
                            self.note(format!(
                                "pinned capture {capture} restored with unavailable pixels: {error}"
                            ));
                        } else {
                            self.note(format!("pinned capture {capture} restored"));
                        }
                    }
                }
                Outcome::PinCreated {
                    card,
                    capture,
                    generation,
                    texture,
                    warning,
                } => {
                    if !self.pin_is_current(&capture, generation, true) {
                        continue;
                    }
                    if let Err(err) = self.surface.commit_pin(&capture, texture) {
                        let close_generation = self.set_pin_intent(capture.clone(), false);
                        self.surface.fail_pin(&capture, err.to_string());
                        self.pipeline.post(Job::SetPin {
                            capture: capture.clone(),
                            generation: close_generation,
                            state: None,
                        });
                        self.note(format!(
                            "pinned capture {} could not finish its card handoff: {err}",
                            capture.0
                        ));
                    } else {
                        self.pipeline.post(Job::Release(card));
                    }
                    if let Some(warning) = warning {
                        self.note(format!(
                            "pinned capture {} kept its bounded preview because full pixels could not be loaded: {warning}",
                            capture.0
                        ));
                    }
                }
                Outcome::PinCreationFailed {
                    capture,
                    generation,
                    error,
                } => {
                    if !self.pin_is_current(&capture, generation, true) {
                        continue;
                    }
                    let rollback_generation = self.set_pin_intent(capture.clone(), false);
                    self.surface.fail_pin(&capture, error.to_string());
                    self.pipeline.post(Job::SetPin {
                        capture: capture.clone(),
                        generation: rollback_generation,
                        state: None,
                    });
                    self.note(format!(
                        "pinned capture {} returned to its source card because it could not be persisted: {error}",
                        capture.0
                    ));
                }
                Outcome::PinPersistenceFailed {
                    capture,
                    generation,
                    error,
                } => {
                    if !self.pin_generation_is_current(&capture, generation) {
                        continue;
                    }
                    self.note(format!(
                        "pinned capture {} could not be persisted: {error}",
                        capture.0
                    ));
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
                        } else if operation == HistoryOperation::Drag {
                            if let Some(capture) = capture.as_ref() {
                                history.drag_preparation_failed(capture, &message);
                            } else {
                                history.operation_failed(&message);
                            }
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
                    if matches!(operation, HistoryOperation::Delete | HistoryOperation::Edit)
                        && let Some(capture) = capture.as_ref()
                    {
                        self.prepared_history_drags.remove(capture);
                    }
                    self.with_history(|history| match (operation, capture.as_ref(), pinned) {
                        (HistoryOperation::Pin, Some(id), Some(pinned)) => {
                            history.pinned(id, pinned);
                        }
                        (HistoryOperation::Delete, Some(id), _) => history.deleted(id),
                        (HistoryOperation::Edit, Some(id), _) => {
                            history.completed(&detail);
                            history.invalidate_drag(id);
                            history.refresh_current(Timestamp::now());
                        }
                        (HistoryOperation::Retention, _, _) => {
                            history.completed(&detail);
                            history.refresh_current(Timestamp::now());
                        }
                        _ => history.completed(&detail),
                    });
                    self.note(detail);
                }
                Outcome::HistoryDragPrepared { capture, prepared } => {
                    self.prepared_history_drags
                        .insert(capture.clone(), prepared);
                    self.with_history(|history| history.drag_prepared(&capture));
                }
            }
        }
    }

    fn drain_cards(&mut self, editors: EditorSnapshots<'_>) {
        let mut pending = Vec::new();
        while let Some(event) = self.surface.poll() {
            pending.push(event);
        }

        for event in pending {
            if self.route_recorded_card_event(&event) {
                continue;
            }
            match event {
                CardEvent::Copy(id) => {
                    if self.card_has_outstanding_card_level_output(id) {
                        self.note(format!("{id} already has an action in progress"));
                        continue;
                    }
                    let action = self.allocate_output_action();
                    if self.post_card_output(id, CardOutput::Copy(action), editors) {
                        self.register_output_action(id, action, OutputActionKind::CardOutput, true);
                    }
                }
                CardEvent::Save {
                    card,
                    choose_destination,
                } => {
                    if !choose_destination && self.finalizing_cards.contains(&card) {
                        self.deferred_save.insert(card);
                        self.surface
                            .set_status(card, Some("Finishing capture...".to_owned()));
                        self.note(format!(
                            "{card} save will resume after initial capture actions finish"
                        ));
                        continue;
                    }
                    if self.card_has_outstanding_card_level_output(card) {
                        self.note(format!("{card} already has an action in progress"));
                        continue;
                    }
                    if !choose_destination
                        && self
                            .card_retention
                            .get(&card)
                            .is_some_and(|(_, exported)| *exported)
                        && editors.for_card(card).is_none()
                    {
                        self.dismiss_recent_capture(card, "used its existing export");
                        self.note(format!("{card} was already saved to Export Location"));
                        continue;
                    }
                    if choose_destination {
                        self.begin_save_dialog(card, editors);
                    } else {
                        let action = self.allocate_output_action();
                        if self.post_card_output(card, CardOutput::Save(None, action), editors) {
                            self.register_output_action(
                                card,
                                action,
                                OutputActionKind::CardOutput,
                                true,
                            );
                        }
                    }
                }
                CardEvent::AutoClose(id, action) => {
                    if self.finalizing_cards.contains(&id) {
                        self.deferred_auto_close.insert(id, action);
                        self.note(format!(
                            "{id} stayed visible while capture durability finishes"
                        ));
                        continue;
                    }
                    if self.card_has_outstanding_card_level_output(id) {
                        self.note(format!(
                            "{id} stayed visible while its output action finishes"
                        ));
                        continue;
                    }
                    if editors.for_card(id).is_some() || self.opening_cards.contains(&id) {
                        // An editor is open, or is still being decoded for
                        // one opened this very frame: `entry.editing` cannot
                        // yet be true for either case (it only flips on the
                        // async `Command::SetEditing` round trip), so the
                        // overlay's own same-frame exclusion cannot see it.
                        // Defer here instead of auto-closing out from under
                        // an editor that exists, or is about to.
                        self.deferred_auto_close.insert(id, action);
                        self.note(format!("{id} stayed visible while its editor is open"));
                        continue;
                    }
                    self.handle_auto_close(id, action, editors);
                }
                CardEvent::Upload(id) => {
                    self.refresh_cloud_settings(scrozz_ui::CloudConnectionState::Idle);
                    if self.cloud_settings.upload_enabled {
                        self.surface.set_status(id, Some("Uploading...".to_owned()));
                        let close_after = self.config.recent_captures_overlay.close_after_upload;
                        let dispatched =
                            self.dispatch_upload_action(id, close_after, |app, action| {
                                app.post_card_output(id, CardOutput::Upload(action), editors)
                            });
                        if !dispatched {
                            // Round 9, Finding #3: nothing is in flight for
                            // this card now (any prior action was just
                            // invalidated inside `dispatch_upload_action`),
                            // so an explicit failure status replaces the
                            // "Uploading..." status set moments ago rather
                            // than leaving it to linger as though an upload
                            // were still underway.
                            self.surface
                                .set_status(id, Some("upload could not be started".to_owned()));
                        }
                    } else {
                        let reason = self
                            .cloud_settings
                            .unavailable_reason
                            .clone()
                            .unwrap_or_else(|| "sharing is unavailable".to_owned());
                        self.surface.set_status(id, Some(reason.clone()));
                        self.note(format!("{id} upload unavailable: {reason}"));
                    }
                }
                CardEvent::Dismiss(id) => {
                    if self.card_has_outstanding_card_level_output(id) {
                        self.note(format!(
                            "{id} stayed visible while its output action finishes"
                        ));
                        continue;
                    }
                    self.dismiss_recent_capture(id, "dismissed");
                    self.note(format!("{id} dismissed"));
                }
                CardEvent::Overflow(id) => {
                    // Releasing an editor's source would strand the window on
                    // pixels it can no longer re-read, so capacity retirement
                    // waits and resumes against the revision the editor ends
                    // on. The same guard covers a card still being decoded
                    // for one opened this very frame: `Outcome::Opened` has
                    // not landed yet, so no live editor exists for the check
                    // above to find, but retiring the card now would still
                    // strand that in-flight open (and, per `Outcome::Refused`
                    // above, the deferral this inserts resolves correctly
                    // either way -- against the editor once it opens, or
                    // against no editor at all if the open fails instead).
                    if self.finalizing_cards.contains(&id)
                        || editors.for_card(id).is_some()
                        || self.opening_cards.contains(&id)
                    {
                        self.deferred_overflow.insert(id);
                        self.note(format!(
                            "{id} overflow cleanup was deferred while its capture is still owned"
                        ));
                    } else {
                        self.handle_overflow(id, editors);
                    }
                }
                CardEvent::Open(id) => {
                    if editors.for_card(id).is_some() || self.opening_cards.contains(&id) {
                        // Already open, or already being decoded for one:
                        // raise the one editor instead of starting a second.
                        self.focus_editor_requests.push_back(id);
                    } else if self.pipeline.post(Job::Open(id)) {
                        // Decoding happens on the worker, so the click that
                        // opens the editor never inflates a 6K PNG on the UI
                        // thread. The card's timer pauses optimistically here,
                        // before the decode even finishes, so a slow decode
                        // can never let the card auto-close out from under it.
                        // Bookkeeping only follows a successful post: marking
                        // the card as opening/editing first would leave it
                        // stuck that way forever if the worker had already
                        // gone, with its timer paused and Continue routing to
                        // an editor that will never exist.
                        self.opening_cards.insert(id);
                        self.surface.set_editing(id, true);
                    } else {
                        self.note(format!(
                            "{id} could not be queued for Open Editor: the capture worker has gone"
                        ));
                    }
                }
                CardEvent::Drag { card, at } => {
                    if at.keep_after_accept {
                        self.drag_keep_after_accept.insert(card);
                    }
                    self.begin_drag(card, at, editors);
                }
                // Collapsing into the dock is the capture stack's own animation
                // and belongs to the surface that raised the event once there
                // is one that can perform it.
                CardEvent::Collapse(id) => {
                    self.note(format!("{id}: {event:?} is not routed yet"));
                }
                CardEvent::Pin(id, capture, state) => {
                    let editor = if let Some(editor) = editors.for_card(id) {
                        let rendered = match editor.render() {
                            Ok(rendered) => rendered,
                            Err(error) => {
                                self.surface.fail_pin(&capture, error.to_string());
                                self.note(format!(
                                    "{id} could not be pinned from editor revision {}: {error}",
                                    editor.editor.state().revision()
                                ));
                                continue;
                            }
                        };
                        let texture =
                            match Thumbnail::from_frame(rendered.frame(), PIN_TEXTURE_MAX_EDGE) {
                                Ok(texture) => texture,
                                Err(error) => {
                                    self.surface.fail_pin(&capture, error.to_string());
                                    self.note(format!(
                                        "{id} could not prepare safe edited pin pixels: {error}"
                                    ));
                                    continue;
                                }
                            };
                        let natural_size = scrozz_core::LogicalSize::new(
                            rendered.frame().size.width / rendered.frame().scale.get(),
                            rendered.frame().size.height / rendered.frame().scale.get(),
                        );
                        if let Err(error) =
                            self.surface
                                .refresh_pin_texture(&capture, texture, natural_size)
                        {
                            self.surface.fail_pin(&capture, error.to_string());
                            self.note(format!(
                                "{id} could not replace provisional pin pixels: {error}"
                            ));
                            continue;
                        }
                        Some(Box::new(PinEditorSnapshot {
                            generation: editor.generation,
                            revision: rendered.revision(),
                            rendered,
                            document: editor.editor.document().data(),
                        }))
                    } else {
                        None
                    };
                    let generation = self.set_pin_intent(capture.clone(), true);
                    if self.pipeline.post(Job::PinCard {
                        card: id,
                        capture: capture.clone(),
                        generation,
                        state,
                        editor,
                    }) {
                        self.note(format!("{id} pinned"));
                    } else {
                        self.set_pin_intent(capture.clone(), false);
                        self.surface.fail_pin(
                            &capture,
                            "the capture worker stopped before the pin could be persisted".into(),
                        );
                        self.note(format!(
                            "{id} could not be pinned because the capture worker has gone"
                        ));
                    }
                }
                CardEvent::PinChanged(capture, state) => {
                    let Some(generation) = self.active_pin_generation(&capture) else {
                        continue;
                    };
                    self.pipeline.post(Job::SetPin {
                        capture,
                        generation,
                        state: Some(state),
                    });
                }
                CardEvent::Unpin(capture) => {
                    let generation = self.set_pin_intent(capture.clone(), false);
                    self.pipeline.post(Job::SetPin {
                        capture: capture.clone(),
                        generation,
                        state: None,
                    });
                    self.note(format!("pinned capture {} closed", capture.0));
                }
                CardEvent::PinnedAction(capture, action) => {
                    self.perform_pinned_capture_action(capture, action);
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
                    self.keep_scrolling_capture();
                }
                ScrollHudAction::Abort if self.scrolling_card.is_some() => {
                    self.abort_scrolling_capture();
                }
                ScrollHudAction::Keep | ScrollHudAction::Abort => {
                    self.finish_scrolling_hud();
                }
            }
        }
    }

    fn perform_pinned_capture_action(&mut self, capture: CaptureId, action: PinnedCaptureAction) {
        let queued = match action {
            PinnedCaptureAction::Annotate => {
                self.perform_history_action(HistoryAction::OpenEditor(capture.clone()))
            }
            PinnedCaptureAction::Copy => self.pipeline.post(Job::CopyHistory(capture.clone())),
            PinnedCaptureAction::SaveAs => {
                self.begin_pin_save_dialog(capture.clone());
                true
            }
            PinnedCaptureAction::Upload => {
                self.refresh_cloud_settings(scrozz_ui::CloudConnectionState::Idle);
                if !self.cloud_settings.upload_enabled {
                    let reason = self
                        .cloud_settings
                        .unavailable_reason
                        .clone()
                        .unwrap_or_else(|| "sharing is unavailable".to_owned());
                    self.note(format!(
                        "pinned capture {} upload unavailable: {reason}",
                        capture.0
                    ));
                    return;
                }
                let card = self.pipeline.allocate();
                let posted = self.pipeline.post(Job::UploadHistory {
                    capture: capture.clone(),
                    card,
                });
                if posted {
                    self.pending_pin_uploads.insert(card, capture.clone());
                }
                posted
            }
            PinnedCaptureAction::ExtractText => {
                self.pipeline.post(Job::ExtractHistoryText(capture.clone()))
            }
        };
        if !queued {
            self.note(format!(
                "pinned capture {} action could not be queued: the capture worker has gone",
                capture.0
            ));
        }
    }

    /// Whether a history row is a recording, as the loaded page reports it.
    ///
    /// A row that is not on the current page answers `false` and takes the
    /// still path, which fails with a named error rather than guessing —
    /// history actions are only ever raised from rows the user can see.
    fn history_entry_is_recording(&self, capture: &CaptureId) -> bool {
        self.history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .media_kind_of(capture)
            == Some(scrozz_store::MediaKind::Video)
    }

    fn drain_history(&mut self) {
        let actions = self.with_history(HistoryViewModel::drain_actions);
        for action in actions {
            let posted = self.perform_history_action(action);
            if !posted {
                self.note("the capture worker has gone");
            }
        }
    }

    fn perform_history_action(&mut self, action: HistoryAction) -> bool {
        match action {
            HistoryAction::Query { request, query } => self.pipeline.query_history(request, query),
            HistoryAction::Restore(capture) => {
                // A recording has no raster to rebuild a still card from, so
                // restoring one opens the media it actually has.
                if self.history_entry_is_recording(&capture) {
                    return self.pipeline.post(Job::OpenHistoryRecording { capture });
                }
                let card = self.pipeline.allocate();
                self.pipeline.post(Job::Restore { capture, card })
            }
            HistoryAction::OpenEditor(capture) => {
                if self.history_entry_is_recording(&capture) {
                    return self.pipeline.post(Job::OpenHistoryRecording { capture });
                }
                let card = self.pipeline.allocate();
                self.pipeline.post(Job::OpenHistoryEditor { capture, card })
            }
            HistoryAction::Copy(capture) => self.pipeline.post(Job::CopyHistory(capture)),
            HistoryAction::Save(capture) => self.pipeline.post(Job::SaveHistory(capture)),
            HistoryAction::PrepareDrag(capture) => {
                if self.prepared_history_drags.contains_key(&capture) {
                    self.with_history(|history| history.drag_prepared(&capture));
                    true
                } else {
                    self.pipeline.post(Job::PrepareHistoryDrag(capture))
                }
            }
            HistoryAction::Drag { id, rect, pointer } => {
                let geometry = DragGeometry { rect, pointer };
                let prepared = self.prepared_history_drags.get(&id).cloned();
                let payload = prepared
                    .ok_or_else(|| "the capture has not finished preparing".to_owned())
                    .and_then(|prepared| {
                        prepared
                            .payload(geometry)
                            .map_err(|error| error.to_string())
                    });
                match payload {
                    Ok(payload) => {
                        self.pending_drags.push_back(PendingDrag {
                            subject: DragSubject::History(id),
                            payload,
                            geometry,
                        });
                    }
                    Err(error) => {
                        let message = format!(
                            "Drag was not ready; select the capture and try again: {error}"
                        );
                        self.with_history(|history| {
                            history.drag_preparation_failed(&id, &message);
                        });
                        self.note(message);
                    }
                }
                true
            }
            HistoryAction::SetPinned { id, pinned } => {
                let screen_unpinned = if pinned {
                    true
                } else {
                    let generation = self.set_pin_intent(id.clone(), false);
                    self.surface.discard_pin(&id);
                    self.pipeline.post(Job::SetPin {
                        capture: id.clone(),
                        generation,
                        state: None,
                    })
                };
                let retention_updated = self.pipeline.post(Job::SetPinned {
                    capture: id,
                    pinned,
                });
                screen_unpinned && retention_updated
            }
            HistoryAction::Delete(capture) => {
                let generation = self.set_pin_intent(capture.clone(), false);
                self.surface.discard_pin(&capture);
                let screen_unpinned = self.pipeline.post(Job::SetPin {
                    capture: capture.clone(),
                    generation,
                    state: None,
                });
                let deleted = self.pipeline.post(Job::Delete(capture));
                screen_unpinned && deleted
            }
        }
    }

    /// Retires a card that capacity evicted, without ever stranding its pixels.
    ///
    /// Runs either when the overflow arrives with no editor open, or when a
    /// deferred overflow resumes at editor close. In the resumed case `editor`
    /// carries the exact revision the window ended on, so the recovery export
    /// writes what the user last saw rather than the original capture. An
    /// existing export is only reused when nothing is editing the card, because
    /// a stale file must not stand in for unsaved edits.
    fn handle_overflow(&mut self, id: CardId, editors: EditorSnapshots<'_>) {
        // An in-flight Save As or output already owns this card's outcome.
        // Retiring it here would invalidate that work, so overflow only records
        // that the card is no longer reachable and lets the outcome decide.
        if self.card_has_outstanding_card_level_output(id) || self.current_upload_close_after(id) {
            self.overflow_recovery_in_flight.insert(id);
            self.note(format!(
                "{id} left the display while its output or upload action finishes"
            ));
            return;
        }
        if editors.for_card(id).is_some() {
            let action = self.allocate_output_action();
            match Self::card_output_job(id, CardOutput::Save(None, action), editors) {
                Ok(recovery) => {
                    if self.pipeline.post(recovery) {
                        self.register_output_action(id, action, OutputActionKind::CardOutput, true);
                        self.overflow_recovery_in_flight.insert(id);
                        self.note(format!(
                            "{id} is saving the exact editor revision before overflow cleanup"
                        ));
                    } else {
                        self.dismiss_recent_capture(
                            id,
                            "released after edited overflow recovery could not be queued",
                        );
                    }
                }
                Err(error) => {
                    self.note(format!(
                        "{id} edited overflow recovery could not be prepared: {error}"
                    ));
                    self.dismiss_recent_capture(
                        id,
                        "released after edited overflow recovery preparation failed",
                    );
                }
            }
            return;
        }
        let (retained, exported) = self.card_retention.get(&id).copied().unwrap_or_default();
        if exported {
            self.dismiss_recent_capture(id, "overflowed with a durable export");
        } else if retained {
            if let Some(capture) = self.card_capture_ids.get(&id).cloned() {
                // Round 12: this action id is allocated and embedded into
                // the recovery `Job` right now, but only becomes
                // *outstanding* later, at the point the stashed job is
                // actually posted in the `Outcome::RetentionRelease`
                // handler -- registering it here, before the retention
                // check has even run, would track an action that might
                // never be posted at all (e.g. if this same card is
                // dismissed for an unrelated reason before the check
                // resolves, dropping `pending_overflow_recovery` with it).
                let action = self.allocate_output_action();
                match Self::card_output_job(id, CardOutput::Save(None, action), editors) {
                    Ok(recovery) => {
                        self.pending_overflow_recovery.insert(id, recovery);
                        if self.pending_retention_overflow.insert(id)
                            && !self
                                .pipeline
                                .post(Job::ReleaseIfRetained { card: id, capture })
                        {
                            self.pending_retention_overflow.remove(&id);
                            self.pending_overflow_recovery.remove(&id);
                            self.dismiss_recent_capture(
                                id,
                                "released after overflow retention check could not be queued",
                            );
                        }
                    }
                    Err(error) => {
                        self.note(format!(
                            "{id} overflow recovery export could not be prepared: {error}"
                        ));
                        self.dismiss_recent_capture(
                            id,
                            "released after overflow recovery preparation failed",
                        );
                    }
                }
            } else {
                self.dismiss_recent_capture(id, "overflowed with retained history");
            }
        } else {
            let action = self.allocate_output_action();
            if self.post_card_output(id, CardOutput::Save(None, action), editors) {
                self.register_output_action(id, action, OutputActionKind::CardOutput, true);
                self.overflow_recovery_in_flight.insert(id);
                self.note(format!(
                    "{id} is being saved because no durable overflow artifact exists"
                ));
            } else {
                self.dismiss_recent_capture(
                    id,
                    "released after overflow recovery save could not be queued",
                );
            }
        }
    }

    /// Allocates a fresh, never-reused-in-practice id for one dispatched
    /// Copy/Save/Save-As/Upload action, spanning every kind uniformly
    /// (round 12). Replaces the previous Upload-only `next_upload_action`.
    fn allocate_output_action(&mut self) -> u64 {
        let action = self.next_output_action;
        self.next_output_action = self.next_output_action.wrapping_add(1);
        action
    }

    /// Registers one freshly, successfully dispatched action as outstanding
    /// for `card`, keyed by its own unique id (round 12).
    ///
    /// For [`OutputActionKind::Upload`], also makes `action` the card's
    /// *current* upload -- see [`Self::current_upload_action`]. Callers must
    /// only register an action once its job has actually been posted to the
    /// pipeline successfully; a failed post has nothing outstanding to
    /// track (round 12, Finding #1: this is exactly what
    /// `copy_rendered`/`save_rendered` previously skipped entirely).
    fn register_output_action(
        &mut self,
        card: CardId,
        action: u64,
        kind: OutputActionKind,
        close_after: bool,
    ) {
        self.outstanding_output_actions
            .entry(card)
            .or_default()
            .insert(action, OutstandingAction { kind, close_after });
        if kind == OutputActionKind::Upload {
            self.current_upload_action.insert(card, action);
        }
    }

    /// Resolves exactly one dispatched action for `card`, whether it is
    /// still outstanding, was already resolved, or was never registered at
    /// all -- idempotent, so a duplicate delivery of the same terminal
    /// outcome is a safe no-op the second time it arrives (round 12,
    /// Finding #2).
    ///
    /// Returns the removed entry, if this exact id was actually
    /// outstanding. Pruning only ever runs off the back of an *actual*
    /// matching removal -- a duplicate resolution that removes nothing must
    /// never re-trigger `prune_settled_generation_fates`, which an earlier,
    /// real removal may already have run to completion.
    fn resolve_output_action(&mut self, card: CardId, action: u64) -> Option<OutstandingAction> {
        let removed = self
            .outstanding_output_actions
            .get_mut(&card)
            .and_then(|actions| actions.remove(&action));
        removed?;
        if self
            .outstanding_output_actions
            .get(&card)
            .is_some_and(HashMap::is_empty)
        {
            self.outstanding_output_actions.remove(&card);
        }
        if self.current_upload_action.get(&card) == Some(&action) {
            self.current_upload_action.remove(&card);
        }
        self.prune_settled_generation_fates(card);
        removed
    }

    fn outstanding_upload_action(&self, card: CardId, action: u64) -> Option<OutstandingAction> {
        self.outstanding_output_actions
            .get(&card)
            .and_then(|actions| actions.get(&action))
            .copied()
            .filter(|outstanding| outstanding.kind == OutputActionKind::Upload)
    }

    /// Whether a card-level (Copy/Save/Save-As) dispatch is currently
    /// outstanding for `card` -- the mutual-exclusion gate that family
    /// alone enforces per card, replacing the previous flat
    /// `close_after_output: HashSet<CardId>` membership test. Upload has
    /// never participated in this gating -- see
    /// [`Self::current_upload_close_after`] -- and neither does an
    /// in-editor [`OutputActionKind::EditorOutput`] dispatch, which is
    /// never exclusive with anything.
    fn card_has_outstanding_card_level_output(&self, card: CardId) -> bool {
        self.outstanding_output_actions
            .get(&card)
            .is_some_and(|actions| {
                actions
                    .values()
                    .any(|a| a.kind == OutputActionKind::CardOutput)
            })
    }

    /// Whether the card's *current* upload action (if any) was dispatched
    /// with `close_after: true` -- replacing the previous
    /// `pending_upload.get(&card).is_some_and(|p| p.close_after)` check.
    /// Only the current upload's policy matters here, exactly as before: a
    /// superseded upload's own `close_after` never gates anything once a
    /// newer dispatch has replaced it as current (round 8, Findings #2-#4).
    fn current_upload_close_after(&self, card: CardId) -> bool {
        self.current_upload_action
            .get(&card)
            .and_then(|action| self.outstanding_output_actions.get(&card)?.get(action))
            .is_some_and(|a| a.close_after)
    }

    fn complete_output_action(&mut self, card: CardId, action: u64) {
        let Some(resolved) = self.resolve_output_action(card, action) else {
            return;
        };
        // An in-editor Copy/Save (`OutputActionKind::EditorOutput`) always
        // registers with `close_after: false`, so this can never dismiss
        // the card mid-edit purely because one export finished -- the user
        // has given no indication they are done editing.
        if resolved.close_after && !self.current_upload_close_after(card) {
            self.dismiss_recent_capture(card, "completed its action");
        }
    }

    fn complete_upload(&mut self, card: CardId, close_after: bool) {
        if close_after {
            self.dismiss_recent_capture(card, "completed its upload");
        }
    }

    fn fail_upload(&mut self, card: CardId, close_after: bool) {
        if !close_after
            || !self.overflow_recovery_in_flight.contains(&card)
            || self.card_has_outstanding_card_level_output(card)
        {
            return;
        }
        self.overflow_recovery_in_flight.remove(&card);
        if self.active_editor_cards.contains(&card) {
            self.deferred_overflow.insert(card);
        } else {
            self.handle_overflow(card, EditorSnapshots::EMPTY);
        }
    }

    /// Whether a Copy/Save/Upload completion's `version` no longer describes
    /// this card's current, exportable content (round 5, Finding #2; round 9,
    /// Finding #1; round 10, Finding #1).
    ///
    /// A `None` version (a plain, no-live-editor output) never races
    /// anything and is always current. A `Some((generation, revision))` is
    /// looked up by its exact `generation` in [`Self::card_generation_fates`]:
    /// - no entry at all means that generation is either still open or was
    ///   never opened -- the first completion of a fresh, still-open editing
    ///   session (neither committed nor cancelled yet) is trusted, matching
    ///   a plain output's behaviour;
    /// - [`GenerationFate::Committed`] with a *different* revision means a
    ///   later Done already superseded this exact completion's revision
    ///   within the same generation;
    /// - [`GenerationFate::Cancelled`] means the edit this completion
    ///   answers for no longer exists anywhere for the user to see, even
    ///   though nothing was ever committed to disagree with it.
    ///
    /// Looking this up per-generation, rather than against a single most-
    /// recent value for the whole card, is what lets an *older* generation's
    /// own fate stay correctly recorded no matter how many newer editing
    /// sessions have opened, committed, or cancelled since (round 10,
    /// Finding #1).
    fn output_version_is_stale(&self, card: CardId, version: Option<(u64, u64)>) -> bool {
        version.is_some_and(|(generation, revision)| {
            match self
                .card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&generation))
            {
                Some(GenerationFate::Committed(committed)) => *committed != revision,
                Some(GenerationFate::Cancelled) => true,
                None => false,
            }
        })
    }

    /// Retires a stale Save/Copy/Upload completion's own bookkeeping without
    /// dismissing the card (round 6, Finding #1).
    ///
    /// `complete_output_action`/`complete_upload` unconditionally dismiss the
    /// card once the resolved action's own `close_after` policy says so --
    /// correct for an ordinary completion, but exactly wrong for one that
    /// raced a *later* Done: the card's live vault entry now holds that
    /// Done's committed revision, not whatever this stale action exported,
    /// so dismissal's `Job::Release` (fired the instant the editor that
    /// produced the newer revision happens to have already closed) would
    /// destroy a legitimately newer, still-live revision to make room for
    /// an action that only ever knew about the one before it. This clears
    /// only the exact action id this stale completion answered for and,
    /// mirroring [`Self::fail_upload`]'s existing recovery-resume idiom,
    /// resolves any overflow retirement that was waiting behind it against
    /// whatever the card holds now instead of the stale action's own
    /// outcome. Retention itself is left untouched: for a stale answer to a
    /// committed-over edit that already holds its own (freshly reset)
    /// retention, and for a cancelled edit that never touched retention in
    /// the first place, "untouched" is exactly correct in both cases (round
    /// 9, Finding #1).
    fn resolve_stale_output_completion(&mut self, card: CardId, action: u64) {
        let Some(resolved) = self.resolve_output_action(card, action) else {
            return;
        };
        let was_waiting = resolved.close_after;
        let other_action_still_pending = match resolved.kind {
            OutputActionKind::Upload => self.card_has_outstanding_card_level_output(card),
            OutputActionKind::CardOutput | OutputActionKind::EditorOutput => {
                self.current_upload_close_after(card)
            }
        };
        if !was_waiting
            || !self.overflow_recovery_in_flight.contains(&card)
            || other_action_still_pending
        {
            return;
        }
        self.overflow_recovery_in_flight.remove(&card);
        if self.active_editor_cards.contains(&card) {
            self.deferred_overflow.insert(card);
        } else {
            self.handle_overflow(card, EditorSnapshots::EMPTY);
        }
    }

    /// Whether nothing dispatched for `card` -- any Copy, Save, Save As,
    /// in-editor output, or Upload, at any editor generation, current or
    /// already superseded -- remains unresolved right now (round 11,
    /// extended to every output kind uniformly in round 12).
    fn card_has_no_outstanding_output(&self, card: CardId) -> bool {
        !self.outstanding_output_actions.contains_key(&card)
    }

    /// Prunes every generation fate currently recorded for `card` once
    /// nothing dispatched against it, of any generation, remains
    /// unresolved (round 11).
    ///
    /// Safe because [`Self::card_output_job`] only ever attaches a
    /// generation to a job while that generation's editor is still open,
    /// so any action that could ever consult a generation's fate was
    /// dispatched no later than the instant that generation's own editor
    /// closed -- and every such dispatch keeps this card "outstanding" (via
    /// `outstanding_output_actions`) from that exact moment until its own
    /// completion resolves. This becoming true therefore proves nothing
    /// outstanding can still consult *any* fate recorded here, however many
    /// generations old -- closing the leak `editor_closed`'s Cancel
    /// tombstone and `commit_card_output`'s Committed tombstone would
    /// otherwise leave behind forever for a card whose editor keeps
    /// reopening and closing with nothing ever dispatched against it, and
    /// pruning a still-recorded fate once the last thing dispatched against
    /// an old generation finally resolves. See the type docs on
    /// `card_generation_fates` for why pruning any earlier than this is
    /// unsafe.
    fn prune_settled_generation_fates(&mut self, card: CardId) {
        if self.card_has_no_outstanding_output(card) {
            self.card_generation_fates.remove(&card);
        }
    }

    /// Applies an expired automatic-close action to a card nothing is editing.
    ///
    /// Resumed with the closing editor's snapshot when the action was deferred,
    /// so **Save then hide** exports the revision the user finished with instead
    /// of reusing an export that predates their edits.
    fn handle_auto_close(
        &mut self,
        id: CardId,
        action: RecentCapturesAutoCloseAction,
        editors: EditorSnapshots<'_>,
    ) {
        let (retained, exported) = self.card_retention.get(&id).copied().unwrap_or_default();
        match action {
            RecentCapturesAutoCloseAction::Hide if retained => {
                if let Some(capture) = self.card_capture_ids.get(&id).cloned()
                    && !exported
                {
                    if self.pending_retention_close.insert(id)
                        && !self
                            .pipeline
                            .post(Job::ReleaseIfRetained { card: id, capture })
                    {
                        self.pending_retention_close.remove(&id);
                        self.note(format!(
                            "{id} stayed visible because history retention could not be checked"
                        ));
                    }
                } else {
                    self.dismiss_recent_capture(id, "auto-closed");
                }
            }
            RecentCapturesAutoCloseAction::Hide => self.note(format!(
                "{id} stayed visible because it is the only retained artifact"
            )),
            RecentCapturesAutoCloseAction::SaveThenHide
                if exported && editors.for_card(id).is_none() =>
            {
                self.dismiss_recent_capture(id, "auto-closed after its existing save");
            }
            RecentCapturesAutoCloseAction::SaveThenHide => {
                if self.card_has_outstanding_card_level_output(id) {
                    self.note(format!("{id} already has an action in progress"));
                } else {
                    let output_action = self.allocate_output_action();
                    if self.post_card_output(id, CardOutput::Save(None, output_action), editors) {
                        self.register_output_action(
                            id,
                            output_action,
                            OutputActionKind::CardOutput,
                            true,
                        );
                    }
                }
            }
        }
    }

    /// Dispatches a fresh upload action for `card`, invalidating whatever
    /// action preceded it *before* this attempt's own enqueue is known to
    /// succeed (round 9, Finding #3).
    ///
    /// A second Upload dispatch for the same card -- whether the user
    /// pressed Upload again before the first's outcome drained, or a
    /// recorded-media retry -- always represents a fresh intent, and must
    /// always retire the previous action's *current*-ness, regardless of
    /// whether this new attempt itself reaches the pipeline. Previously
    /// only a *successful* new dispatch replaced `pending_upload`: if
    /// `try_post` failed (a render error, or the capture/upload worker
    /// having gone), the prior action's entry -- and its close policy --
    /// was left untouched, so a late outcome for that now-truly-superseded
    /// action still matched `pending_upload` and applied its (possibly
    /// obsolete) close policy, exactly as though the second dispatch had
    /// never happened. Clearing `current_upload_action` first means a
    /// failed re-dispatch leaves nothing tracked as current for this card,
    /// so any old outcome that still arrives -- for the action just
    /// invalidated -- falls through to the ordinary "a newer one has since
    /// replaced" no-op every upload outcome handler already applies to an
    /// action mismatch. The invalidated action's own entry in
    /// `outstanding_output_actions` is left untouched here -- unlike the
    /// `current` pointer, it must keep tracking that action's own eventual
    /// completion until *that* drains, whether or not anything still
    /// considers it current (round 11; round 12 unifies this alongside
    /// every other output kind).
    ///
    /// Returns whether `try_post` reached the pipeline. On success, the
    /// freshly allocated action becomes outstanding for `card` with
    /// `close_after` as given, and the card's tracked *current* upload. On
    /// failure, nothing new is registered -- callers own reporting the
    /// failure (status/notes), since what counts as informative differs
    /// between the still-image and recorded-media paths.
    fn dispatch_upload_action(
        &mut self,
        card: CardId,
        close_after: bool,
        try_post: impl FnOnce(&mut Self, u64) -> bool,
    ) -> bool {
        self.current_upload_action.remove(&card);
        let action = self.allocate_output_action();
        if try_post(self, action) {
            self.register_output_action(card, action, OutputActionKind::Upload, close_after);
            true
        } else {
            false
        }
    }

    fn post_card_output(
        &mut self,
        card: CardId,
        output: CardOutput,
        editors: EditorSnapshots<'_>,
    ) -> bool {
        let label = output.label();
        let clipboard = output
            .uses_clipboard()
            .then(|| self.pipeline.reserve_clipboard());
        let job = match Self::card_output_job(card, output, editors) {
            Ok(job) => job,
            Err(error) => {
                self.note(format!(
                    "{card} could not be rendered for {}: {error}",
                    label
                ));
                return false;
            }
        };
        let job = match clipboard {
            Some(turn) => Job::OrderedClipboard {
                turn,
                job: Box::new(job),
            },
            None => job,
        };

        if !self.pipeline.post(job) {
            self.note(format!(
                "{card} could not be queued for {}: the capture worker has gone",
                label
            ));
            return false;
        }
        true
    }

    fn begin_save_dialog(&mut self, card: CardId, editors: EditorSnapshots<'_>) {
        if self.pending_save_dialog.is_some() || self.card_has_outstanding_card_level_output(card) {
            self.note(format!(
                "{card} stayed visible because another Save As dialog is already open"
            ));
            return;
        }
        let generation = editors.for_card(card).map(|editor| editor.generation);
        let rendered = match editors
            .for_card(card)
            .map(EditorSnapshot::render)
            .transpose()
        {
            Ok(rendered) => rendered.map(Box::new),
            Err(error) => {
                self.note(format!("{card} could not be rendered for save: {error}"));
                return;
            }
        };
        let future = Box::pin(
            rfd::AsyncFileDialog::new()
                .set_title("Save Recent Capture")
                .set_file_name("Scrozz Capture.png")
                .add_filter("PNG image", &["png"])
                .save_file(),
        );
        // Round 12: registered now, spanning the whole dialog, not just the
        // eventual job -- a dialog the user is still looking at is exactly
        // as "an action is outstanding for this card" as a job already in
        // flight, and this is what `card_has_outstanding_card_level_output`
        // above must see on a second Save As attempt while this one is
        // still open.
        let action = self.allocate_output_action();
        self.register_output_action(card, action, OutputActionKind::CardOutput, true);
        self.pending_save_dialog = Some(PendingSaveDialog {
            card,
            generation,
            rendered,
            future,
            action,
        });
    }

    fn drain_save_dialog(&mut self) {
        let Some(pending) = self.pending_save_dialog.as_mut() else {
            return;
        };
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let Poll::Ready(result) = pending.future.as_mut().poll(&mut context) else {
            return;
        };
        let PendingSaveDialog {
            card,
            generation,
            rendered,
            action,
            ..
        } = self
            .pending_save_dialog
            .take()
            .expect("polled dialog exists");
        let Some(file) = result else {
            self.resolve_output_action(card, action);
            if self.overflow_recovery_in_flight.remove(&card) {
                self.dismiss_recent_capture(
                    card,
                    "released after overflowed Save As was cancelled",
                );
            }
            self.note(format!("{card} save was cancelled"));
            return;
        };
        let path = file.path().to_path_buf();
        let job = match rendered {
            Some(rendered) => Job::SaveImageTo {
                card,
                generation: generation.unwrap_or_default(),
                rendered,
                path,
                action,
            },
            None => Job::SaveTo { card, path, action },
        };
        if !self.pipeline.post(job) {
            self.resolve_output_action(card, action);
            self.note(format!(
                "{card} could not be queued for save: the capture worker has gone"
            ));
        }
    }

    fn begin_pin_save_dialog(&mut self, capture: CaptureId) {
        if self.has_pending_save_dialog() {
            self.note(format!(
                "pinned capture {} stayed on screen because another Save As dialog is open",
                capture.0
            ));
            return;
        }
        let future = Box::pin(
            rfd::AsyncFileDialog::new()
                .set_title("Save Pinned Capture")
                .set_file_name("Scrozz Capture.png")
                .add_filter("PNG image", &["png"])
                .save_file(),
        );
        self.pending_pin_save_dialog = Some(PendingPinSaveDialog { capture, future });
    }

    fn drain_pin_save_dialog(&mut self) {
        let Some(pending) = self.pending_pin_save_dialog.as_mut() else {
            return;
        };
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let Poll::Ready(result) = pending.future.as_mut().poll(&mut context) else {
            return;
        };
        let PendingPinSaveDialog { capture, .. } = self
            .pending_pin_save_dialog
            .take()
            .expect("polled pin save dialog exists");
        let Some(file) = result else {
            self.note(format!("pinned capture {} save was cancelled", capture.0));
            return;
        };
        if !self.pipeline.post(Job::SaveHistoryTo {
            capture: capture.clone(),
            path: file.path().to_path_buf(),
        }) {
            self.note(format!(
                "pinned capture {} could not be queued for save: the capture worker has gone",
                capture.0
            ));
        }
    }

    fn dismiss_recent_capture(&mut self, card: CardId, reason: &str) {
        if self.deferred_save.contains(&card) {
            self.note(format!(
                "{card} stayed visible while its requested save finishes"
            ));
            return;
        }
        let editor_owns_card =
            self.active_editor_cards.contains(&card) || self.opening_cards.contains(&card);
        let save_dialog_owns_card = self
            .pending_save_dialog
            .as_ref()
            .is_some_and(|pending| pending.card == card);
        // Round 12: an in-editor (`EditorOutput`) action is never touched by
        // dismissal -- its own generation's editor is still open (that is
        // the only way an `EditorOutput` action can exist at all), so it
        // always outlives this specific card-overlay retirement regardless
        // of `editor_owns_card`/`save_dialog_owns_card`. A card-level
        // action survives only for the currently-open Save As dialog that
        // owns this exact retirement (unchanged from round 11); Upload
        // never survives dismissal at all.
        if let Some(actions) = self.outstanding_output_actions.get_mut(&card) {
            actions.retain(|_, a| {
                a.kind == OutputActionKind::EditorOutput
                    || (save_dialog_owns_card && a.kind == OutputActionKind::CardOutput)
            });
            if actions.is_empty() {
                self.outstanding_output_actions.remove(&card);
            }
        }
        self.current_upload_action.remove(&card);
        self.visible_cards.remove(&card);
        self.drag_keep_after_accept.remove(&card);
        self.card_retention.remove(&card);
        // Round 10, Finding #1: this card's generation history can never be
        // consulted again once it is gone -- no overlay card remains for a
        // completion racing an already-retired generation to update, and no
        // recorded fate here could ever again distinguish "stale" from
        // "current" for it.
        self.card_generation_fates.remove(&card);
        self.card_capture_ids.remove(&card);
        self.pending_retention_close.remove(&card);
        self.pending_retention_overflow.remove(&card);
        self.deferred_auto_close.remove(&card);
        self.deferred_save.remove(&card);
        self.deferred_overflow.remove(&card);
        self.pending_overflow_recovery.remove(&card);
        if !save_dialog_owns_card {
            self.overflow_recovery_in_flight.remove(&card);
        }
        self.surface.dismiss(card);
        if save_dialog_owns_card {
            self.overflow_recovery_in_flight.insert(card);
        } else if editor_owns_card {
            self.editor_only_cards.insert(card);
        } else {
            self.pipeline.post(Job::Release(card));
        }
        tracing::debug!(%card, reason, "retired Recent Captures Overlay card");
    }

    /// Recovers the action id embedded in an already-constructed `Job`, for
    /// the one code path (`Outcome::RetentionRelease`'s stashed-job-post
    /// site) that allocates and embeds an action well before it is known
    /// whether that job will ever actually be posted, and so cannot
    /// register it as outstanding at construction time (round 12).
    const fn job_action(job: &Job) -> Option<u64> {
        match job {
            Job::Copy { action, .. }
            | Job::CopyImage { action, .. }
            | Job::Save { action, .. }
            | Job::SaveImage { action, .. }
            | Job::SaveImageTo { action, .. }
            | Job::SaveTo { action, .. }
            | Job::Upload { action, .. }
            | Job::UploadImage { action, .. }
            | Job::UploadRecording { action, .. } => Some(*action),
            _ => None,
        }
    }

    fn card_output_job(
        card: CardId,
        output: CardOutput,
        editors: EditorSnapshots<'_>,
    ) -> CliResult<Job> {
        let Some(editor) = editors.for_card(card) else {
            return Ok(match output {
                CardOutput::Copy(action) => Job::Copy { card, action },
                CardOutput::Save(None, action) => Job::Save { card, action },
                CardOutput::Save(Some(path), action) => Job::SaveTo { card, path, action },
                CardOutput::Upload(action) => Job::Upload { card, action },
            });
        };

        let generation = editor.generation;
        let rendered = Box::new(editor.render()?);
        Ok(match output {
            CardOutput::Copy(action) => Job::CopyImage {
                card,
                generation,
                rendered,
                action,
            },
            CardOutput::Save(None, action) => Job::SaveImage {
                card,
                generation,
                rendered,
                action,
            },
            CardOutput::Save(Some(path), action) => Job::SaveImageTo {
                card,
                generation,
                rendered,
                path,
                action,
            },
            CardOutput::Upload(action) => Job::UploadImage {
                card,
                generation,
                rendered,
                action,
            },
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
        self.pump_drag_starts_with_editor(EditorSnapshots::EMPTY)
    }

    /// Starts native drags with the current editor document available.
    ///
    /// If the dragged card is being edited, the drag payload is rendered from
    /// that exact revision. A failed render refuses the drag; it never falls
    /// back to the vault's original pixels and risk exposing data under a
    /// destructive redaction.
    pub fn pump_drag_starts_with_editor(&mut self, editors: EditorSnapshots<'_>) -> usize {
        let armed = self.surface.poll_drag_starts();
        let mut started = 0;
        for event in armed {
            if let CardEvent::Drag { card, at } = event {
                if at.keep_after_accept {
                    self.drag_keep_after_accept.insert(card);
                }
                if self.recorded_media.contains_key(&card) {
                    self.begin_recorded_drag(card, at);
                } else {
                    self.begin_drag(card, at, editors);
                }
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
    fn begin_drag(&mut self, card: CardId, at: DragSpot, editors: EditorSnapshots<'_>) {
        if !self.drag.is_attached()
            && let Some(surface) = self.surface.native_surface()
        {
            self.drag.attach(surface);
        }

        let bytes = match self.drag_bytes(card, editors) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.drag_keep_after_accept.remove(&card);
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
                self.drag_keep_after_accept.remove(&card);
                self.surface.settle_drag(card, false);
                self.modal_drag_input_release_pending = true;
                self.note(format!("{card} could not be dragged: {why}"));
            }
        }
    }

    fn drag_bytes(&self, card: CardId, editors: EditorSnapshots<'_>) -> CliResult<CaptureBytes> {
        if let Some(editor) = editors.for_card(card) {
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
            let keep_after_accept = self.drag_keep_after_accept.remove(&card);
            self.surface
                .settle_drag(card, accepted && !keep_after_accept);
            self.modal_drag_input_release_pending = true;

            match outcome {
                DragOutcome::Accepted { .. } if !keep_after_accept => {
                    self.dismiss_recent_capture(card, "accepted external drag");
                    self.recorded_media.remove(&card);
                    self.recorded_media_targets.remove(&card);
                    self.note(format!("{card} dropped"));
                }
                DragOutcome::Accepted { .. } => {
                    self.note(format!("{card} dropped and kept with Option/Alt"));
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
                self.toggle_recording();
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
                self.stop(ExitReason::Quit(origin))
            }
        }
    }

    fn stop(&mut self, reason: ExitReason) -> Tick {
        if self.exit_reason.is_none() {
            tracing::info!(exit_reason = reason.label(), "application exit requested");
            self.exit_reason = Some(reason);
        }
        Tick::Stop(reason)
    }

    #[must_use]
    pub(crate) const fn exit_reason(&self) -> Option<ExitReason> {
        self.exit_reason
    }

    pub(crate) fn record_native_event_loop_exit(&mut self) {
        if self.exit_reason.is_none() {
            tracing::warn!(
                exit_reason = ExitReason::NativeEventLoop.label(),
                "application event loop ended without an app stop request"
            );
            self.exit_reason = Some(ExitReason::NativeEventLoop);
        }
    }

    pub(crate) fn stop_for_native_lifecycle(&mut self, detail: impl Into<String>) -> Tick {
        self.note(format!("native lifecycle failure: {}", detail.into()));
        self.stop(ExitReason::NativeLifecycle)
    }

    fn begin_capture(&mut self, origin: CaptureOrigin, kind: CaptureKind) {
        // Scrolling is a session, not a shot. A second invocation of the same
        // command is the keyboard-only way to end it: keep first, discard on the
        // one after that.
        if kind == CaptureKind::Scrolling {
            self.begin_scrolling_capture(origin);
            return;
        }
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

    fn play_shutter_sound(&mut self) {
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
                    format!("the screenshot succeeded, but its sound could not play: {error}")
                });
            }
        }
    }

    /// Publishes a processed capture: card, editor, retention, and history.
    ///
    /// Split out of the pipeline drain so a scrolling capture can be held for
    /// one pass and then published through exactly the same path as any other.
    fn handle_ready(&mut self, ready: ReadyCapture) {
        let finalization_pending = ready.finalization_pending;
        let finalization_ack = ready.finalization_ack;
        if self.scrolling_card == Some(ready.card.id) {
            self.finish_scrolling_hud();
        }
        self.captures += 1;
        let mut card = ready.card;
        // The card is told what Upload can do the moment it is
        // built, so a control that cannot work is never offered as
        // if it could.
        card.upload_available = self.cloud_settings.upload_enabled;
        card.upload_unavailable_reason = self.cloud_settings.unavailable_reason.clone();
        let history_changed = card.capture_id.is_some();
        let card_id = card.id;
        let capture_id = card.capture_id.clone();
        let retained_elsewhere = ready.retained_elsewhere;
        let exported = ready.exported;
        let summary = card.summary();
        for (action, error) in ready.actions.failures() {
            self.note(format!("{card_id} {} failed: {error}", action.label()));
        }
        for step in &ready.actions.steps {
            match &step.outcome {
                crate::after_capture::ActionOutcome::Succeeded(ActionEffect::Completed)
                    if step.action == AfterCaptureAction::CopyToClipboard =>
                {
                    self.note(format!("{card_id} copied to the clipboard"));
                }
                crate::after_capture::ActionOutcome::Succeeded(ActionEffect::Saved(path)) => {
                    self.note(format!("{card_id} saved to {}", path.display()));
                }
                crate::after_capture::ActionOutcome::Succeeded(ActionEffect::Uploaded(url)) => {
                    self.note(format!("{card_id} uploaded to {url}"));
                }
                _ => {}
            }
        }

        let show_overlay = ready
            .actions
            .has_effect(&ActionEffect::ShowRecentCapturesOverlay);
        let open_editor = ready.actions.has_effect(&ActionEffect::OpenEditor);
        if finalization_pending {
            self.finalizing_cards.insert(card_id);
        }
        if show_overlay {
            if let Err(err) = self.surface.present(card) {
                self.note(format!(
                    "{card_id} could not be shown in Recent Captures Overlay: {err}"
                ));
            } else {
                self.visible_cards.insert(card_id);
                self.card_retention
                    .insert(card_id, (retained_elsewhere, exported));
                if let Some(capture_id) = capture_id {
                    self.card_capture_ids.insert(card_id, capture_id);
                }
                self.note(summary);
            }
        } else {
            self.note(summary);
        }
        if open_editor {
            if self.pipeline.post(Job::Open(card_id)) {
                self.opening_cards.insert(card_id);
                self.surface.set_editing(card_id, true);
                if !show_overlay {
                    self.editor_only_cards.insert(card_id);
                }
            } else {
                self.note(format!(
                    "{card_id} could not be queued for Open Editor: the capture worker has gone"
                ));
            }
        }
        if !finalization_pending && !show_overlay && !open_editor {
            self.pipeline.post(Job::Release(card_id));
        }
        if history_changed {
            self.with_history(|history| history.refresh_from_start(Timestamp::now()));
        }
        if let Some(acknowledged) = finalization_ack {
            let _ = acknowledged.send(());
        }
    }

    fn handle_finalized(&mut self, finalized: FinalizedCapture) {
        let FinalizedCapture {
            card,
            capture_id,
            actions,
            written,
            retained_elsewhere,
            exported,
        } = finalized;
        self.finalizing_cards.remove(&card);

        for (action, error) in actions.failures() {
            self.note(format!("{card} {} failed: {error}", action.label()));
        }
        for step in &actions.steps {
            match &step.outcome {
                crate::after_capture::ActionOutcome::Succeeded(ActionEffect::Completed)
                    if step.action == AfterCaptureAction::CopyToClipboard =>
                {
                    self.note(format!("{card} copied to the clipboard"));
                }
                crate::after_capture::ActionOutcome::Succeeded(ActionEffect::Saved(path)) => {
                    self.note(format!("{card} saved to {}", path.display()));
                }
                crate::after_capture::ActionOutcome::Succeeded(ActionEffect::Uploaded(url)) => {
                    self.note(format!("{card} uploaded to {url}"));
                }
                _ => {}
            }
        }

        let owned = self.visible_cards.contains(&card)
            || self.opening_cards.contains(&card)
            || self.active_editor_cards.contains(&card)
            || self.editor_only_cards.contains(&card)
            || self.deferred_save.contains(&card);
        if owned {
            if self.visible_cards.contains(&card) {
                self.card_retention
                    .insert(card, (retained_elsewhere, exported));
                self.surface
                    .finalize_capture(card, capture_id.clone(), written.first().cloned());
            }
            if let Some(capture_id) = capture_id.clone() {
                self.card_capture_ids.insert(card, capture_id);
            }
        } else {
            self.pipeline.post(Job::Release(card));
        }

        if capture_id.is_some() {
            self.with_history(|history| history.refresh_from_start(Timestamp::now()));
        }

        if self.deferred_save.remove(&card) {
            if exported {
                self.dismiss_recent_capture(card, "used its completed automatic export");
                self.note(format!("{card} was already saved to Export Location"));
            } else {
                let action = self.allocate_output_action();
                if self.post_card_output(
                    card,
                    CardOutput::Save(None, action),
                    EditorSnapshots::EMPTY,
                ) {
                    self.register_output_action(card, action, OutputActionKind::CardOutput, true);
                }
            }
        }

        if !self.active_editor_cards.contains(&card) && !self.opening_cards.contains(&card) {
            if !self.card_has_outstanding_card_level_output(card)
                && let Some(action) = self.deferred_auto_close.remove(&card)
            {
                self.handle_auto_close(card, action, EditorSnapshots::EMPTY);
            }
            if self.deferred_overflow.remove(&card) {
                self.handle_overflow(card, EditorSnapshots::EMPTY);
            }
        }
    }

    fn begin_scrolling_capture(&mut self, origin: CaptureOrigin) {
        if self.scrolling_card.is_some() {
            if self.scrolling_keep_pending {
                self.abort_scrolling_capture();
            } else {
                self.keep_scrolling_capture();
            }
            return;
        }
        if self.scroll_hud.is_some() {
            self.finish_scrolling_hud();
            self.note("cancelled scrolling capture before it started");
            return;
        }

        let access = Self::screen_access();
        if access != Access::Granted {
            self.handle_capture_permission_failure(CaptureKind::Scrolling, origin, access);
            return;
        }

        let target = match (self.scrolling_target_resolver)() {
            Ok(target) => target,
            Err(error) => {
                self.note(format!("could not select a scrolling target: {error}"));
                return;
            }
        };
        self.scrolling_target = Some(target);
        self.set_scroll_hud(ScrollHudState::choosing(ScrollAxis::Vertical));
        self.note("choose whether the scrolling capture should grow vertically or horizontally");
    }

    fn start_scrolling_capture(&mut self, axis: ScrollAxis) {
        let Some(selected) = self.scrolling_target.take() else {
            self.note("the snapshotted scrolling target is no longer available");
            self.finish_scrolling_hud();
            return;
        };
        let access = Self::screen_access();
        if access != Access::Granted {
            self.note_permission_unavailable(CaptureKind::Scrolling, access);
            self.finish_scrolling_hud();
            return;
        }
        // Re-resolved on purpose: the axis picker gave the user time to focus
        // something else, and capturing the window they were looking at when
        // the HUD opened would be a different capture than the one they asked
        // for.
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

        self.play_shutter_sound();
        let card = self.pipeline.allocate();
        self.scrolling_card = Some(card);
        let needs_passthrough = target.requires_overlay_passthrough();
        self.set_scroll_hud(ScrollHudState::prepared(axis, needs_passthrough));
        self.surface.request_scroll_passthrough(needs_passthrough);
        self.scrolling_start_pending = Some(PendingScrollingStart {
            axis,
            card,
            target: Box::new(target),
            needs_passthrough,
            passthrough_requested_at: Instant::now(),
        });
    }

    fn drain_scrolling_start(&mut self) {
        let Some(pending) = self.scrolling_start_pending.as_ref() else {
            return;
        };
        if pending.needs_passthrough && !self.surface.scroll_passthrough_ready() {
            if pending.passthrough_requested_at.elapsed() >= PASSTHROUGH_ACK_TIMEOUT {
                self.note(
                    "automatic scrolling did not start because the overlay could not confirm \
                     native click-through",
                );
                self.finish_scrolling_hud();
            }
            return;
        }
        let pending = self
            .scrolling_start_pending
            .take()
            .expect("checked pending scrolling start");
        if !self
            .pipeline
            .post_scrolling(pending.axis, pending.card, pending.target)
        {
            self.note("the capture worker has gone");
            self.finish_scrolling_hud();
        }
    }

    fn drain_scrolling_ready(&mut self) {
        if let Some(ready) = self.scrolling_ready.take() {
            self.handle_ready(*ready);
        }
    }

    fn keep_scrolling_capture(&mut self) {
        if self.scrolling_start_pending.is_some() {
            self.note("cancelled scrolling capture before the first frame");
            self.finish_scrolling_hud();
            return;
        }
        if !self.pipeline.cancel_scrolling(CancelAction::Keep) {
            self.note("the scrolling capture is already publishing its output");
            return;
        }
        self.scrolling_keep_pending = true;
        self.note(
            "finishing the scrolling capture with the stitched frames so far; \
             invoke Capture Scrolling again to discard",
        );
    }

    fn abort_scrolling_capture(&mut self) {
        let Some(card) = self.scrolling_card else {
            self.finish_scrolling_hud();
            return;
        };
        if self.scrolling_start_pending.take().is_some() {
            self.note("discarded scrolling capture before the first frame");
            self.finish_scrolling_hud();
            return;
        }
        if !self.pipeline.cancel_scrolling(CancelAction::Abort) {
            self.note("the scrolling capture is already publishing and cannot be discarded");
            return;
        }
        self.scrolling_abort_pending = Some(card);
        self.hide_scrolling_hud();
        self.note("discarding the scrolling capture");
    }

    fn set_scroll_hud(&mut self, state: ScrollHudState) {
        self.surface.show_scroll_hud(state.clone());
        self.scroll_hud = Some(state);
    }

    fn finish_scrolling_hud(&mut self) {
        self.hide_scrolling_hud();
        self.scrolling_target = None;
        self.scrolling_abort_pending = None;
        self.scrolling_start_pending = None;
        self.scrolling_keep_pending = false;
        self.scrolling_card = None;
    }

    fn finish_failed_scrolling_capture(&mut self, card: CardId, error: &CliError) -> bool {
        if self.scrolling_card != Some(card) {
            return false;
        }
        if error.is_cancellation() {
            self.note(format!("{card} discarded"));
        } else {
            self.note(format!("{card} scrolling capture failed: {error}"));
        }
        self.finish_scrolling_hud();
        true
    }

    fn hide_scrolling_hud(&mut self) {
        self.surface.request_scroll_passthrough(false);
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
                if !*automatic {
                    // Manual capture needs the pointer back: the user is the
                    // one scrolling now.
                    self.surface.request_scroll_passthrough(false);
                }
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
        if let Err(error) = self.pipeline.post_capture(
            pending.kind,
            pending.origin,
            card,
            self.config.after_capture.clone(),
        ) {
            self.note(format!("{card} could not start: {error}"));
        }
    }

    #[cfg(target_os = "macos")]
    fn queue_apple_picker_capture(
        &mut self,
        pending: PendingCapture,
        capture: scrozz_capture::PickerCapture,
        policy: AfterCaptureSettings,
        clipboard: Option<ClipboardTurn>,
    ) {
        self.play_shutter_sound();
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
            policy,
            clipboard,
        }) {
            self.note("the capture worker has gone");
        }
    }

    fn drain_permission(&mut self) {
        let effect = self.permission.application_active_changed(
            scrozz_shell::permissions::application_is_active(),
            Self::screen_access,
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
                    let (policy, clipboard) = self
                        .picker_surface
                        .as_mut()
                        .map(PickerSurfaceReservation::take_capture_context)
                        .unwrap_or_else(|| (self.config.after_capture.clone(), None));
                    self.finish_picker_surface();
                    if let Some(pending) = self.permission.apple_picker_captured() {
                        self.queue_apple_picker_capture(pending, capture, policy, clipboard);
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
                    let access = match permissions.request(Capability::ScreenRecording) {
                        Ok(()) => Access::Granted,
                        Err(CoreError::PermissionDenied { .. }) => Access::NotGranted,
                        Err(error) => {
                            self.note(format!(
                                "macOS could not present Screen Recording access: {error}"
                            ));
                            Access::Unavailable
                        }
                    };
                    self.permission.system_request_finished(access)
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
            let policy = self.config.after_capture.clone();
            let clipboard = self.pipeline.reserve_capture_clipboard(&policy);
            match PickerSurfaceReservation::start(
                mode,
                Arc::clone(&self.selector),
                policy,
                clipboard,
            ) {
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
        // The picker answers on its own callback, so the shadow preference is
        // latched now rather than read when the selection arrives.
        let include_window_shadow =
            crate::settings::window_shadow(&self.config.after_capture).unwrap_or(true);
        match picker.present(native_mode, include_window_shadow) {
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

    /// Accept a region/window selector's completed pixels into the live card stack.
    ///
    /// The selector remains platform-owned; from this point onward the capture
    /// follows the same persistence, bounded-texture, and Pin to Screen path as
    /// a display hotkey capture.
    pub fn accept_capture(
        &mut self,
        kind: CaptureKind,
        capture: scrozz_core::Capture,
    ) -> CliResult<CardId> {
        self.pipeline.accept_capture(
            kind,
            CaptureOrigin::Direct,
            capture,
            AfterCaptureSettings::direct_command(),
        )
    }

    fn unlock_all_pins(&mut self) -> CliResult<u64> {
        self.suppress_locked_restores = true;
        self.surface.unlock_pins();
        self.pipeline.unlock_pins()
    }

    fn set_pin_intent(&mut self, capture: CaptureId, visible: bool) -> PinGeneration {
        let generation = self
            .pin_intents
            .get(&capture)
            .map_or(PinGeneration(1), |intent| {
                PinGeneration(intent.generation.0.saturating_add(1))
            });
        self.pin_intents.insert(
            capture,
            PinIntent {
                generation,
                visible,
            },
        );
        generation
    }

    fn accept_pin_restore(&mut self, capture: &CaptureId) -> bool {
        match self.pin_intents.get(capture) {
            Some(intent) => intent.visible,
            None => {
                self.pin_intents.insert(
                    capture.clone(),
                    PinIntent {
                        generation: PinGeneration(1),
                        visible: true,
                    },
                );
                true
            }
        }
    }

    fn active_pin_generation(&self, capture: &CaptureId) -> Option<PinGeneration> {
        self.pin_intents
            .get(capture)
            .filter(|intent| intent.visible)
            .map(|intent| intent.generation)
    }

    fn pin_is_current(
        &self,
        capture: &CaptureId,
        generation: PinGeneration,
        visible: bool,
    ) -> bool {
        self.pin_intents
            .get(capture)
            .is_some_and(|intent| intent.generation == generation && intent.visible == visible)
    }

    fn pin_generation_is_current(&self, capture: &CaptureId, generation: PinGeneration) -> bool {
        self.pin_intents
            .get(capture)
            .is_some_and(|intent| intent.generation == generation)
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
        // Presentation lives in Scenes; this pane answers destinations only.
        let mut rows: Vec<AfterCaptureRow> = Vec::new();
        rows.extend(AfterCaptureAction::UI_ORDER.into_iter().map(|action| {
            AfterCaptureRow {
                screenshot_id: action.setting_key(MediaKind::Screenshot),
                recording_id: action
                    .is_contract_available(MediaKind::Recording)
                    .then(|| action.setting_key(MediaKind::Recording)),
                label: action.label().to_owned(),
                description: action.description().to_owned(),
                screenshot: cell(MediaKind::Screenshot, action),
                recording: cell(MediaKind::Recording, action),
            }
        }));
        rows
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

    /// Applies Recent Captures Overlay preferences and coalesces persistence.
    pub fn edit_recent_captures_overlay(
        &mut self,
        settings: scrozz_ui::RecentCapturesOverlaySettings,
    ) {
        let settings = settings.normalized();
        if settings == self.config.recent_captures_overlay {
            return;
        }
        if self.config.after_capture_store.is_none() {
            self.note(
                "Recent Captures Overlay settings were not changed because no config directory is available",
            );
            return;
        }
        self.config.recent_captures_overlay = settings;
        self.surface.configure_recent_captures_overlay(settings);
        self.pending_recent_captures_settings_save = Some((
            settings,
            Instant::now() + RECENT_CAPTURES_SETTINGS_SAVE_DEBOUNCE,
        ));
    }

    fn flush_recent_captures_overlay_settings(&mut self, force: bool) {
        let Some((settings, deadline)) = self.pending_recent_captures_settings_save else {
            return;
        };
        if !force && Instant::now() < deadline {
            return;
        }
        self.pending_recent_captures_settings_save = None;
        let Some(store) = self.config.after_capture_store.clone() else {
            self.note("Recent Captures Overlay settings were not saved: no config path");
            return;
        };
        match store.update(store.inferred_profile(), |latest| {
            latest.set_value(
                crate::settings::RECENT_CAPTURES_OVERLAY_PLACEMENT_KEY,
                settings.placement.slug(),
            );
            latest.set_value(
                crate::settings::RECENT_CAPTURES_OVERLAY_FOLLOW_ACTIVE_DISPLAY_KEY,
                settings.follow_active_display.to_string(),
            );
            latest.set_value(
                crate::settings::RECENT_CAPTURES_OVERLAY_CARD_WIDTH_KEY,
                settings.card_width.round().to_string(),
            );
            latest.set_value(
                crate::settings::RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_ENABLED_KEY,
                settings.auto_close_enabled.to_string(),
            );
            latest.set_value(
                crate::settings::RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_ACTION_KEY,
                settings.auto_close_action.slug(),
            );
            latest.set_value(
                crate::settings::RECENT_CAPTURES_OVERLAY_AUTO_CLOSE_SECONDS_KEY,
                settings.auto_close_seconds.to_string(),
            );
            latest.set_value(
                crate::settings::RECENT_CAPTURES_OVERLAY_CLOSE_AFTER_DRAG_KEY,
                settings.close_after_drag.to_string(),
            );
            latest.set_value(
                crate::settings::RECENT_CAPTURES_OVERLAY_CLOSE_AFTER_UPLOAD_KEY,
                settings.close_after_upload.to_string(),
            );
            latest.set_value(
                crate::settings::RECENT_CAPTURES_OVERLAY_SAVE_BUTTON_KEY,
                settings.save_behavior.slug(),
            );
            Ok(())
        }) {
            Ok(updated) => {
                self.config.after_capture = updated;
                self.note("Recent Captures Overlay settings saved");
            }
            Err(error) => {
                self.note(format!(
                    "Recent Captures Overlay settings were not saved: {error}"
                ));
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

    /// Takes a pending request to raise a card's already-open (or opening)
    /// editor instead of starting a second one for the same card.
    pub fn take_focus_editor_request(&mut self) -> Option<CardId> {
        self.focus_editor_requests.pop_front()
    }

    /// Takes one completed Smart Frame analysis for delivery by the viewport host.
    pub fn take_smart_frame_result(&mut self) -> Option<SmartFrameResult> {
        self.smart_frame_results.pop_front()
    }

    /// Puts back a Smart Frame result the host could not deliver this tick.
    ///
    /// The result's card had a Done-triggered close pending (round 6,
    /// Finding #4's freeze): delivering now would set a new revision on a
    /// document a commit is already mid-flight for, so the host defers
    /// rather than applying or discarding it. The freeze can only resolve
    /// two ways -- a committed close removes the editor entirely, so a
    /// later delivery attempt finds nothing to apply to and drops the
    /// result on its own; a failed close reopens the editor at the exact
    /// revision Done captured (nothing else could have mutated it while
    /// frozen), so the result is still valid to apply once the freeze
    /// lifts. Either way this never needs to inspect the freeze's outcome
    /// itself (round 7, Finding #1).
    pub fn requeue_smart_frame_result(&mut self, result: SmartFrameResult) {
        self.smart_frame_results.push_back(result);
    }

    /// The durable Smart Frame preset library currently in force.
    #[must_use]
    pub fn smart_frame_presets(&self) -> &[SmartFramePreset] {
        self.config.after_capture.smart_frame_presets()
    }

    /// Queues analysis of one immutable editor revision.
    pub fn analyze_smart_frame(
        &mut self,
        card: CardId,
        generation: u64,
        revision: u64,
        data: DocumentData,
        cancellation: AnalysisCancellation,
    ) {
        if !self.pipeline.post(Job::AnalyzeSmartFrame {
            card,
            generation,
            revision,
            data: Box::new(data),
            cancellation,
        }) {
            self.smart_frame_results.push_back(SmartFrameResult {
                card,
                generation,
                revision,
                result: Err("the Smart Frame worker is no longer available".to_owned()),
            });
        }
    }

    /// Persists one custom Smart Frame preset and updates the live policy snapshot.
    pub fn upsert_smart_frame_preset(
        &mut self,
        preset: SmartFramePreset,
    ) -> CliResult<Vec<SmartFramePreset>> {
        let store = self.config.after_capture_store.clone().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "no configuration directory is available for Smart Frame presets".to_owned(),
            ))
        })?;
        let updated = store
            .update(store.inferred_profile(), |settings| {
                settings.upsert_smart_frame_preset(preset)
            })
            .map_err(CliError::Core)?;
        let presets = updated.smart_frame_presets().to_vec();
        self.config.after_capture = updated;
        self.note("Smart Frame preset saved");
        Ok(presets)
    }

    /// Removes one custom Smart Frame preset and updates the live policy snapshot.
    ///
    /// Shares [`crate::settings::forget_scene_preset`] with the Settings-side
    /// delete so a preset can never be removed while Scene assignments still
    /// name it, whichever list the user deleted it from.
    pub fn delete_smart_frame_preset(
        &mut self,
        preset_id: &str,
    ) -> CliResult<Vec<SmartFramePreset>> {
        let store = self.config.after_capture_store.clone().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "no configuration directory is available for Smart Frame presets".to_owned(),
            ))
        })?;
        let updated = store
            .update(store.inferred_profile(), |settings| {
                crate::settings::forget_scene_preset(settings, preset_id)
            })
            .map_err(CliError::Core)?;
        let presets = updated.smart_frame_presets().to_vec();
        self.config.after_capture = updated;
        self.note("Smart Frame preset deleted");
        Ok(presets)
    }

    /// Releases an artifact retained only for its editor, then resumes deferred cleanup.
    ///
    /// Cleanup that expired while the window was open runs here with the
    /// editor's final snapshot -- but only when `committed` says the editor
    /// closed through Done. Any recovery export that fires for a card whose
    /// editor merely closed (Cancel, or a clean window close) must instead
    /// see no live editor at all, so it falls back to the card's own,
    /// unmodified bytes rather than rendering and possibly saving or
    /// uploading annotations the user just discarded. Resuming before the
    /// editor-only release means a card the deferred work retires still
    /// hands its live bytes back in the same pass.
    pub fn editor_closed(&mut self, editor: EditorSnapshot<'_>, committed: bool) {
        let card = editor.card;
        let editors = if committed {
            EditorSnapshots::new(std::slice::from_ref(&editor))
        } else {
            // Round 9, Finding #1; round 10, Finding #1: a live Copy/Save/
            // Upload dispatched before this Cancel (or clean close) exports
            // this exact generation's uncommitted render and can still be
            // in flight. Recording this generation's fate here lets a
            // completion that arrives afterward be recognised as answering
            // a discarded edit, even though nothing was ever committed for
            // this card to compare it against. Keyed by generation, not
            // merely by card, so an older cancelled generation's own record
            // survives however many later editing sessions open, commit, or
            // are themselves cancelled before that late completion drains.
            self.card_generation_fates
                .entry(card)
                .or_default()
                .insert(editor.generation, GenerationFate::Cancelled);
            // Round 11: most Cancels (and clean closes) have nothing
            // dispatched against this generation still outstanding -- prune
            // the tombstone just inserted right back out immediately rather
            // than let it, and its container entry, outlive every later
            // session that could ever consult it. A history-only editor's
            // `CardId` is never reused, so leaving this unpruned would leak
            // one entry per editor-only session forever.
            self.prune_settled_generation_fates(card);
            EditorSnapshots::EMPTY
        };
        if let Some(action) = self.deferred_auto_close.remove(&card) {
            self.handle_auto_close(card, action, editors);
        }
        if self.deferred_overflow.remove(&card) {
            self.handle_overflow(card, editors);
        }
        if self.editor_only_cards.remove(&card) {
            self.pipeline.post(Job::Release(card));
        }
        self.opening_cards.remove(&card);
        self.surface.set_editing(card, false);
    }

    /// Files a Done-committed editor revision into the card's own exportable bytes.
    ///
    /// Companion to [`Self::dispatch_deferred_persist`] and
    /// [`Self::refresh_card_thumbnail`]: those update the durable history
    /// record and the visible thumbnail, this updates what a plain Copy,
    /// Save or Upload posted once the editor is gone actually reads. Called
    /// only for the Done exception D14 sanctions --
    /// never for Cancel or a clean close, both of which must leave a card's own
    /// bytes exactly as they were before the editor opened.
    ///
    /// `data` travels alongside the render so the worker can keep its
    /// in-memory reopen document in lock-step with the bytes it is about to
    /// replace -- durable history persistence is a separate job posted right
    /// after this one and can fail or lag independently, and a reopen must
    /// never reconstruct a document older than what this call just committed.
    pub fn commit_card_output(
        &mut self,
        card: CardId,
        generation: u64,
        rendered: RevisionedFrame,
        data: DocumentData,
    ) {
        let revision = rendered.revision();
        let job = Job::CommitCardOutput {
            card,
            generation,
            rendered: Box::new(rendered),
            data: Box::new(data),
        };
        if self.pipeline.post(job) {
            // Any retention recorded before now describes the pre-edit
            // content, not the revision just filed above: trusting it would
            // let a later overflow or auto-close treat this card as already
            // durable-and-exported when the newly committed revision has
            // not actually been saved or uploaded anywhere yet.
            if self.card_retention.contains_key(&card) {
                self.card_retention.insert(card, (false, false));
            }
            // Recorded *now*, synchronously, rather than once the commit is
            // acknowledged: the capture worker is a single FIFO thread, so
            // any Copy/Save/Upload dispatched earlier is always processed no
            // later than this commit -- its completion can only be observed
            // afterward. Comparing against a value set here is what lets a
            // completion for an older revision of this same editor
            // generation be recognised as stale and ignored, rather than
            // wrongly re-marking this newly committed (and not yet exported)
            // revision as retained/exported (round 5, Finding #2). Keyed by
            // generation, not merely by card, so this generation's own
            // record survives however many later editing sessions open,
            // commit, or are cancelled before a stale completion for *this*
            // one drains (round 10, Finding #1).
            self.card_generation_fates
                .entry(card)
                .or_default()
                .insert(generation, GenerationFate::Committed(revision));
            // Round 11: mirrors the Cancel path in `Self::editor_closed` --
            // if nothing dispatched before this commit remains outstanding,
            // this generation's own fresh tombstone (and any older one still
            // recorded for this card) can be pruned immediately instead of
            // waiting on a completion that may never arrive again for a
            // card whose editor keeps reopening.
            self.prune_settled_generation_fates(card);
        } else {
            let error = "the capture worker has gone".to_string();
            self.note(format!(
                "{card} editor {generation}'s committed edit could not be filed for export: \
                 {error}"
            ));
            // Never leave the pending close waiting on an ack that a gone
            // worker will now never send -- resolve it as failed instead of
            // stalling the editor frozen-but-unfinalized forever.
            self.fail_pending_editor_close(card, generation, revision, "commit", error);
        }
    }

    /// Whether `card`'s editor is frozen: either still waiting on a Done's
    /// commit/persist ack, or that ack has already resolved but the host has
    /// not yet drained [`Self::take_editor_close_result`] to finalize it.
    ///
    /// A modeless native panel shared with an editor (the system colour
    /// picker) keeps delivering events on its own schedule, independent of
    /// whatever egui intent loop the editor itself has stopped servicing --
    /// so the host must check this explicitly before applying one of those
    /// events, rather than relying on the editor having already stopped
    /// asking for input. Used by [`Self::prepare_editor`] internally for the
    /// render-prep half of the same freeze (round 6, Finding #4).
    ///
    /// Checking only `pending_editor_closes` (this method's previous, exact
    /// behavior) left a real gap: [`Self::resolve_pending_editor_close`]
    /// removes the card from `pending_editor_closes` and pushes its outcome
    /// into `editor_close_results` the instant every required ack resolves
    /// -- but the host does not actually finalize (close on success, reopen
    /// on failure) until it separately drains that result via
    /// [`Self::take_editor_close_result`], which for a still-showing editor
    /// only happens once per frame from `show_editor`. Between those two
    /// moments a stale "not frozen" answer let an async delivery (a color
    /// picker change, a Smart Frame/Scene analysis result) mutate an editor
    /// whose Done has already captured its authoritative snapshot -- for a
    /// *successful* close this mutation would simply be discarded once the
    /// editor is torn down, but for a close that turns out to have *failed*
    /// and reopens the editor, an in-between mutation would silently land on
    /// top of the reopened state as if Done had never happened (round 8,
    /// Finding #1). Treating an unresolved result as frozen too closes that
    /// gap: the freeze lifts only once the editor is torn down (success) or
    /// the failure path has finished reopening it (failure), matching
    /// exactly when a mutation becomes safe again.
    pub fn editor_close_frozen(&self, card: CardId) -> bool {
        self.pending_editor_closes.contains_key(&card)
            || self.editor_close_results.iter().any(|(c, _)| *c == card)
    }

    /// Asks the worker to prepare the latest settled editor revision for drag.
    ///
    /// At most one full-resolution render per card is in flight, so a
    /// continuous drawing gesture cannot fill the worker queue with obsolete
    /// documents, and one card's drag preparation never blocks another's.
    /// Once that render returns, the next frame submits the newest revision
    /// if the editor moved on. Drag refuses while the exact revision is
    /// unavailable.
    pub fn prepare_editor(&mut self, editor: EditorSnapshot<'_>) {
        let revision = editor.editor.state().revision();
        let version = (editor.generation, revision);
        // A Done pending its commit (and, for a history-only editor,
        // persist) ack has already captured and frozen this exact document;
        // its editor stays mapped only so that ack has somewhere to land,
        // not to keep taking new render-prep requests against whatever the
        // still-open editor drifts to next. Preparing a later revision here
        // would cache drag bytes the committed revision never agreed to,
        // and a later native drag would then read those instead of the
        // committed (possibly redacted) ones (round 6, Finding #4). The
        // freeze persists through the ack-resolved-but-undrained window too
        // (round 8, Finding #1) -- see `editor_close_frozen`.
        if self.editor_close_frozen(editor.card) {
            return;
        }
        let captures = self.pipeline.captures();
        if editor.editor.state().is_dragging()
            || captures.get(editor.card).is_none()
            || captures
                .get_revision(editor.card, editor.generation, revision)
                .is_some()
            || self.editor_render_pending.contains_key(&editor.card)
            || self.editor_render_failed.get(&editor.card) == Some(&version)
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
            self.editor_render_pending.insert(editor.card, version);
        } else {
            self.note(format!(
                "{} editor {} revision {revision} could not be queued for drag preparation: \
                 the capture worker has gone",
                editor.card, editor.generation
            ));
        }
    }

    /// Persists the exact edited scene before its editor-only cache is released.
    ///
    /// Used directly by tests exercising the persist ack path in isolation.
    /// The production Done path never calls this eagerly any more -- see
    /// [`Self::dispatch_deferred_persist`], which posts the same job only
    /// once the sibling commit is known to have succeeded (round 6,
    /// Finding #3).
    #[cfg(test)]
    pub fn persist_editor(&mut self, editor: EditorSnapshot<'_>) {
        let revision = editor.editor.state().revision();
        self.post_persist_document(
            editor.card,
            editor.generation,
            revision,
            editor.editor.document().data(),
        );
    }

    /// Posts a `PersistDocument` job for an exact editor generation/revision,
    /// resolving the matching pending close as an immediate failure if the
    /// job cannot even be queued (the worker is gone).
    fn post_persist_document(
        &mut self,
        card: CardId,
        generation: u64,
        revision: u64,
        data: DocumentData,
    ) {
        let job = Job::PersistDocument {
            card,
            generation,
            revision,
            data: Box::new(data),
        };
        if !self.pipeline.post(job) {
            let error = "the capture worker has gone".to_string();
            self.note(format!(
                "{card} editor {generation} revision {revision} could not be queued for \
                 history persistence: {error}"
            ));
            // Same reasoning as `commit_card_output`'s post-failure branch: a
            // gone worker will never answer, so a history-only close waiting
            // on this ack must not wait for it forever.
            self.fail_pending_editor_close(card, generation, revision, "persist", error);
        }
    }

    /// Dispatches the durable history write for a Done-triggered close, but
    /// only once its commit has actually succeeded.
    ///
    /// Called from [`Self::resolve_pending_editor_close`]'s `Ok` path so a
    /// commit that failed never has a matching persist posted at all: with
    /// nothing dispatched, nothing can land, and history stays exactly as it
    /// was before this Done (round 6, Finding #3). Idempotent -- the stashed
    /// document is taken, so a resolution that somehow runs twice for the
    /// same pending close dispatches the job only the first time.
    fn dispatch_deferred_persist(&mut self, card: CardId, generation: u64, revision: u64) {
        let Some(pending) = self.pending_editor_closes.get_mut(&card) else {
            return;
        };
        if pending.generation != generation || pending.revision != revision {
            return;
        }
        let Some(data) = pending.persist_data.take() else {
            return;
        };
        self.post_persist_document(card, generation, revision, *data);
    }

    /// Begins a Done-triggered close: records the pending state that
    /// [`Self::take_editor_close_result`] resolves once the commit job
    /// posted right alongside this call actually answers -- and, for a
    /// history-only editor, once the persist job [`Self::dispatch_deferred_persist`]
    /// posts once that commit succeeds answers too.
    ///
    /// Must be called before the commit job is posted, so its answer -- which
    /// can arrive as soon as the very next `drain_pipeline` pass -- always
    /// finds this entry already recorded (round 5, Finding #1/#4). `data` is
    /// the exact document Done captured, held until the commit succeeds
    /// rather than persisted alongside it unconditionally (round 6,
    /// Finding #3).
    pub fn begin_editor_close(
        &mut self,
        card: CardId,
        generation: u64,
        revision: u64,
        frame: scrozz_core::Frame,
        data: DocumentData,
    ) {
        let editor_only = self.editor_only_cards.contains(&card);
        self.pending_editor_closes.insert(
            card,
            PendingEditorClose {
                generation,
                revision,
                frame,
                editor_only,
                commit: None,
                persist: None,
                persist_data: Some(Box::new(data)),
            },
        );
    }

    /// Marks a pending close's commit (or persist) ack as an immediate
    /// failure, for a job that could not even be posted (the worker is
    /// gone). Named by which ack failed only for the log line; resolution
    /// itself does not care which one it was.
    fn fail_pending_editor_close(
        &mut self,
        card: CardId,
        generation: u64,
        revision: u64,
        which: &str,
        error: String,
    ) {
        let Some(pending) = self.pending_editor_closes.get_mut(&card) else {
            return;
        };
        if pending.generation != generation || pending.revision != revision {
            return;
        }
        match which {
            "commit" => pending.commit = Some(Err(error)),
            "persist" => pending.persist = Some(Err(error)),
            _ => unreachable!("fail_pending_editor_close called with an unknown ack name"),
        }
        self.resolve_pending_editor_close(card, generation, revision);
    }

    /// Resolves `card`'s pending close if every ack it needs has now
    /// arrived, pushing the outcome for [`Self::take_editor_close_result`]
    /// to drain.
    fn resolve_pending_editor_close(&mut self, card: CardId, generation: u64, revision: u64) {
        let Some(pending) = self.pending_editor_closes.get(&card) else {
            return;
        };
        if pending.generation != generation || pending.revision != revision {
            return;
        }
        let Some(success) = pending.ready() else {
            return;
        };
        let pending = self
            .pending_editor_closes
            .remove(&card)
            .expect("just matched above");
        if success {
            self.refresh_card_thumbnail(card, &pending.frame);
            self.editor_close_results
                .push_back((card, EditorCloseOutcome::Committed));
        } else {
            let error = pending
                .commit
                .and_then(Result::err)
                .or_else(|| pending.persist.and_then(Result::err))
                .unwrap_or_else(|| "the edit could not be filed durably".to_string());
            self.editor_close_results
                .push_back((card, EditorCloseOutcome::Failed(error)));
        }
    }

    /// Takes one Done-triggered close that has finished waiting on its jobs.
    ///
    /// The host polls this once per frame for every editor it still has
    /// mapped, finalizing through [`Self::editor_closed`] on `Committed` or
    /// reopening the (already visually closed) window on `Failed`.
    pub fn take_editor_close_result(&mut self) -> Option<(CardId, EditorCloseOutcome)> {
        self.editor_close_results.pop_front()
    }

    /// Reserves the clipboard intent before the editor begins a potentially slow render.
    pub fn reserve_clipboard_intent(&self) -> ClipboardTurn {
        self.pipeline.reserve_clipboard()
    }

    /// Copies an image the editor has flattened.
    ///
    /// Routed through the worker so the PNG encode and the clipboard write stay
    /// off the UI thread, exactly like a card's own copy. `generation` is the
    /// editor's own lifetime, carried through so the completion can be matched
    /// against the card's currently-committed revision (round 5, Finding #2)
    /// rather than trusted blindly if a newer Done races it.
    ///
    /// Round 12, Finding #1: registers a fresh
    /// [`OutputActionKind::EditorOutput`] action for `card` on a successful
    /// post, exactly like every other output dispatch site -- previously
    /// this posted the job with no outstanding-registration at all, so
    /// Cancel/Done could prune `card`'s generation fate immediately, and a
    /// late in-flight result from this exact dispatch could then appear
    /// "current" against nothing left to compare it to. `close_after` is
    /// always `false`: an in-editor export must never dismiss the overlay
    /// card out from under the user mid-edit.
    pub fn copy_rendered(
        &mut self,
        card: CardId,
        generation: u64,
        rendered: RevisionedFrame,
        clipboard: ClipboardTurn,
    ) {
        let action = self.allocate_output_action();
        if self.pipeline.post(Job::OrderedClipboard {
            turn: clipboard,
            job: Box::new(Job::CopyImage {
                card,
                generation,
                rendered: Box::new(rendered),
                action,
            }),
        }) {
            self.register_output_action(card, action, OutputActionKind::EditorOutput, false);
        }
    }

    /// Saves an image the editor has flattened. See [`Self::copy_rendered`]
    /// for why `generation` is threaded through and why this registers an
    /// [`OutputActionKind::EditorOutput`] action (round 12, Finding #1).
    pub fn save_rendered(&mut self, card: CardId, generation: u64, rendered: RevisionedFrame) {
        let action = self.allocate_output_action();
        if self.pipeline.post(Job::SaveImage {
            card,
            generation,
            rendered: Box::new(rendered),
            action,
        }) {
            self.register_output_action(card, action, OutputActionKind::EditorOutput, false);
        }
    }

    /// Replaces a card's visible thumbnail with a freshly rendered document.
    ///
    /// The card keeps its pre-edit thumbnail for the entire editing session —
    /// this is only ever called once, when the editor commits (Done) a dirty
    /// document. Per D14 a capture's own pixels are never replaced by an
    /// annotated version unless the user explicitly saves one; Done is that
    /// explicit action, distinct from a plain window close or Cancel, neither
    /// of which reaches this method.
    pub fn refresh_card_thumbnail(&mut self, card: CardId, frame: &scrozz_core::Frame) {
        self.surface.refresh_card_image(card, frame);
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

    /// Takes a history-window drag prepared by the worker.
    pub fn take_history_drag(&mut self) -> Option<PendingDrag> {
        self.pending_drags.pop_front()
    }

    /// Records completion of a native drag from the history window.
    pub fn history_drag_finished(&mut self, subject: &DragSubject, outcome: &DragOutcome) {
        let detail = match outcome {
            DragOutcome::Accepted(_) => "Capture dragged to another app".to_owned(),
            DragOutcome::Rejected => "The destination did not accept the capture".to_owned(),
            DragOutcome::Cancelled => "Drag cancelled".to_owned(),
            DragOutcome::Failed(reason) => format!("Drag failed: {reason}"),
            _ => "Drag finished".to_owned(),
        };
        if let DragSubject::History(_) = subject {
            self.with_history(|history| history.completed(&detail));
        }
        self.note(detail);
    }

    /// Asks for the Sharing settings window on the next host pass.
    pub const fn request_sharing_settings(&mut self) {
        self.sharing_settings_requested = true;
    }

    /// Takes a pending request to open or focus the Sharing settings window.
    ///
    /// Separate from [`Self::take_settings_request`]: the main Settings
    /// window is the aggregate's own, and Sharing is a viewport beside it.
    pub fn take_sharing_settings_request(&mut self) -> bool {
        std::mem::take(&mut self.sharing_settings_requested)
    }

    /// Current secret-free Settings model.
    #[must_use]
    pub fn cloud_settings(&self) -> &scrozz_ui::CloudSettingsModel {
        &self.cloud_settings
    }

    /// Applies one Settings intent.
    pub fn apply_cloud_settings(&mut self, event: scrozz_ui::CloudSettingsEvent) {
        match event {
            scrozz_ui::CloudSettingsEvent::Save(draft) => {
                self.invalidate_connection_test();
                match crate::cloud::save_settings(&draft) {
                    Ok(()) => {
                        self.note("sharing settings saved");
                        self.refresh_cloud_settings(scrozz_ui::CloudConnectionState::Idle);
                    }
                    Err(error) => {
                        self.note(format!("sharing settings were not saved: {error}"));
                        self.cloud_settings.connection =
                            scrozz_ui::CloudConnectionState::Failed(error.to_string());
                    }
                }
            }
            scrozz_ui::CloudSettingsEvent::StoreCredentials(credentials) => {
                self.invalidate_connection_test();
                let token = (!credentials.session_token.is_empty())
                    .then_some(credentials.session_token.as_str());
                let password = (!credentials.share_password.is_empty())
                    .then_some(credentials.share_password.as_str());
                match crate::cloud::store_credentials(
                    &credentials.access_key_id,
                    &credentials.secret_access_key,
                    token,
                    password,
                ) {
                    Ok(()) => {
                        self.note("provider credentials stored in the native vault");
                        self.refresh_cloud_settings(scrozz_ui::CloudConnectionState::Idle);
                    }
                    Err(error) => {
                        self.note(format!("provider credentials were not stored: {error}"));
                        self.cloud_settings.connection =
                            scrozz_ui::CloudConnectionState::Failed(error.to_string());
                    }
                }
            }
            scrozz_ui::CloudSettingsEvent::RemoveCredentials => {
                self.invalidate_connection_test();
                match crate::cloud::remove_credentials() {
                    Ok(_) => {
                        self.note("provider credentials removed from the native vault");
                        self.refresh_cloud_settings(scrozz_ui::CloudConnectionState::Idle);
                    }
                    Err(error) => {
                        self.note(format!("provider credentials were not removed: {error}"));
                        self.cloud_settings.connection =
                            scrozz_ui::CloudConnectionState::Failed(error.to_string());
                    }
                }
            }
            scrozz_ui::CloudSettingsEvent::TestConnection => {
                if self.connection_test.is_some() {
                    return;
                }
                let (sender, receiver) = channel();
                match std::thread::Builder::new()
                    .name("scrozz-cloud-test".to_owned())
                    .spawn(move || {
                        let _ = sender.send(crate::cloud::test_connection());
                    }) {
                    Ok(_) => {
                        self.connection_test = Some(receiver);
                        self.cloud_settings.connection = scrozz_ui::CloudConnectionState::Testing;
                    }
                    Err(error) => {
                        self.cloud_settings.connection = scrozz_ui::CloudConnectionState::Failed(
                            format!("could not start the connection test: {error}"),
                        );
                    }
                }
            }
        }
    }

    fn drain_connection_test(&mut self) {
        let Some(receiver) = &self.connection_test else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(())) => {
                self.connection_test = None;
                self.note("cloud connection test passed");
                self.refresh_cloud_settings(scrozz_ui::CloudConnectionState::Passed);
            }

            Ok(Err(error)) => {
                self.connection_test = None;
                self.note(format!("cloud connection test failed: {error}"));
                self.refresh_cloud_settings(scrozz_ui::CloudConnectionState::Failed(
                    error.to_string(),
                ));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.connection_test = None;
                self.refresh_cloud_settings(scrozz_ui::CloudConnectionState::Failed(
                    "the connection-test worker stopped without an answer".to_owned(),
                ));
            }
        }
    }

    fn invalidate_connection_test(&mut self) {
        self.connection_test = None;
        if matches!(
            self.cloud_settings.connection,
            scrozz_ui::CloudConnectionState::Testing
        ) {
            self.cloud_settings.connection = scrozz_ui::CloudConnectionState::Idle;
        }
    }

    fn refresh_cloud_settings(&mut self, connection: scrozz_ui::CloudConnectionState) {
        match crate::cloud::settings_model(connection) {
            Ok(model) => self.cloud_settings = model,
            Err(error) => {
                self.cloud_settings = crate::cloud::settings_error_model(error.to_string());
            }
        }
        for id in self.visible_cards.iter().copied() {
            self.surface.set_upload_availability(
                id,
                self.cloud_settings.upload_enabled,
                self.cloud_settings.unavailable_reason.clone(),
            );
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
                "exit_reason",
                self.exit_reason
                    .map_or(Json::Null, |reason| Json::str(reason.label())),
            ),
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
        // First: a running scrolling session holds the overlay click-through,
        // and a quit that left it held would leave a transparent window over
        // the desktop for as long as the process took to die.
        if self.scrolling_card.is_some() || self.scrolling_start_pending.is_some() {
            self.pipeline.cancel_scrolling(CancelAction::Keep);
        }
        self.finish_scrolling_hud();
        if let Err(error) = self.save_recording_settings_panel() {
            self.note(format!(
                "recording settings could not be saved during shutdown: {error}"
            ));
        }
        self.flush_recent_captures_overlay_settings(true);
        self.flush_camera_preferences(true);
        self.stop_camera_preview();
        self.settle_recording_before_shutdown();
        self.recording.discard_interactions();
        self.input_wake_monitor = None;
        self.drain_cards(EditorSnapshots::EMPTY);
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
            while let Some(admission) = self.pending_admissions.pop_front() {
                let _ = admission.complete(Err(CliError::ipc(
                    "the running instance shut down before capture admission completed",
                )));
            }
            forwarder.stop();
        }
        self.pipeline.stop();
    }

    // -----------------------------------------------------------------------
    // Recording
    // -----------------------------------------------------------------------

    /// Starts, cancels or stops a recording, depending on where one is.
    ///
    /// One entry point for the tray item, the global hotkey and a forwarded
    /// `record --toggle`, because all three mean the same thing to the user and
    /// three code paths would eventually disagree about what "toggle" does
    /// while a countdown is running.
    fn toggle_recording(&mut self) {
        if self.recording.pending_start.is_some() {
            self.begin_recording_finalisation(None);
            return;
        }
        let Some(phase) = self.recording.phase() else {
            let error = CliError::Core(self.recording.unavailable_error());
            self.present_recording_error(&error);
            return;
        };
        match phase {
            RecordingPhase::Idle | RecordingPhase::Finished | RecordingPhase::Failed => {
                self.begin_recording();
            }
            RecordingPhase::Selecting => self.cancel_recording_selection(),
            RecordingPhase::Countdown => {
                let result = self
                    .recording
                    .machine
                    .as_mut()
                    .expect("a phase came from this machine")
                    .cancel_countdown();
                self.finish_recording_action(result, "recording countdown cancelled");
            }
            RecordingPhase::Recording | RecordingPhase::Paused => {
                self.begin_recording_finalisation(None);
            }
            RecordingPhase::Finalising => self.note("recording is already finalising"),
        }
    }

    fn cancel_recording_selection(&mut self) {
        if let Some(selection) = self.recording.selection.as_mut() {
            // The selector is a synchronous, modal contract owned by another
            // thread; there is no supported way to retract a selection request
            // that is already on screen. Marking it is the honest thing: the
            // answer is discarded when it arrives, and the user is told the one
            // gesture that closes the window they are looking at.
            selection.cancel_requested = true;
            self.note(
                "recording cancelled; press Escape to dismiss the selection overlay still on screen",
            );
            return;
        }
        let result = self
            .recording
            .machine
            .as_mut()
            .expect("a selecting phase came from this machine")
            .cancel_selection();
        self.finish_recording_action(result, "recording selection cancelled");
    }

    /// Begins a GUI recording: permission, destination, then a target.
    ///
    /// The order matters. Permission is asked for before anything is reserved,
    /// so a refusal leaves no orphan file; the destination is reserved before
    /// the selector opens, so a selection is never thrown away because the
    /// capture folder turned out to be unwritable.
    fn begin_recording(&mut self) {
        if self
            .recording
            .editor
            .as_ref()
            .is_some_and(ActiveVideoEditor::is_exporting)
        {
            self.note("cancel the active export before starting another recording");
            return;
        }
        if let Err(error) = self.save_recording_settings_panel() {
            self.present_recording_error(&error);
            return;
        }
        if let Err(error) = self.reload_recording_settings() {
            self.present_recording_error(&error);
            return;
        }
        if let Err(error) = Self::ensure_recording_permission() {
            self.present_recording_error(&error);
            return;
        }
        if let Err(error) = self.ensure_recording_input_permission() {
            self.present_recording_error(&error);
            return;
        }
        let destination = match crate::output::default_recording_path() {
            Ok(destination) => destination,
            Err(error) => {
                self.present_recording_error(&error);
                return;
            }
        };
        if let Err(error) = self.reset_finished_recording() {
            self.present_recording_error(&error);
            return;
        }
        self.release_video_editor();
        self.recording.preflight_failure = None;
        self.recording.tick = Instant::now();
        self.recording.completion = None;

        // The current selector is the same one screenshots use, so a recording
        // is framed with the same magnifier, aspect locks and remembered region
        // the user already knows. Only a build whose selector cannot draw a
        // region falls back to a display.
        if self.selection_capabilities.supports(SelectionMode::Region) {
            if let Err(error) = self.begin_recording_selection(
                SelectionStart::Settings { destination },
                InteractiveMode::AllInOne,
            ) {
                self.present_recording_error(&error);
            }
            return;
        }
        match Self::active_recording_target() {
            Ok(target) => self.arm_recording_start(PendingStart::Settings {
                target,
                destination,
            }),
            Err(error) => self.present_recording_error(&error),
        }
    }

    fn reset_finished_recording(&mut self) -> CliResult<()> {
        let needs_reset = matches!(
            self.recording.phase(),
            Some(RecordingPhase::Finished | RecordingPhase::Failed)
        );
        if !needs_reset {
            return Ok(());
        }
        self.recording
            .machine
            .as_mut()
            .expect("a phase came from this machine")
            .reset()
            .map_err(CliError::Core)
    }

    fn arm_recording_start(&mut self, start: PendingStart) {
        self.stop_camera_preview();
        self.recording.pending_start = Some(ArmedStart {
            start,
            armed_tick: self.recording.sequence,
        });
        self.note("recording start armed until the capture surfaces are down");
    }

    fn begin_recording_selection(
        &mut self,
        start: SelectionStart,
        mode: InteractiveMode,
    ) -> CliResult<()> {
        if self.recording.selection.is_some() || self.recording.pending_start.is_some() {
            return Err(CliError::Core(CoreError::InvalidRequest(
                "a recording selection or start is already pending".to_owned(),
            )));
        }
        let unavailable = self.recording.unavailable_error();
        let machine = self
            .recording
            .machine
            .as_mut()
            .ok_or(CliError::Core(unavailable))?;
        machine.begin_selection().map_err(CliError::Core)?;

        let selector = Arc::clone(&self.selector);
        let (send, result) = std::sync::mpsc::channel();
        if let Err(error) = std::thread::Builder::new()
            .name("scrozz-recording-selector".to_owned())
            .spawn(move || {
                let selected = crate::commands::select_recording_target_with_memory(
                    mode,
                    Some(selector.as_ref()),
                    true,
                );
                let _ = send.send(selected);
            })
        {
            let _ = machine.cancel_selection();
            return Err(CliError::Core(CoreError::Platform(format!(
                "could not start the recording selector: {error}"
            ))));
        }
        self.recording.selection = Some(PendingSelection {
            start,
            result,
            cancel_requested: false,
        });
        self.note("recording target selection started");
        Ok(())
    }

    fn finish_recording_action(&mut self, result: scrozz_core::Result<()>, success: &str) {
        match result {
            Ok(()) => {
                self.recording.preflight_failure = None;
                self.note(success);
            }
            Err(error) => self.present_recording_error(&CliError::Core(error)),
        }
        self.drain_recording_events();
        self.refresh_recording_tray();
    }

    /// Services every recording-owned worker once, then advances the clock.
    ///
    /// Called from [`App::tick`] before card events, so a recording that
    /// finished on this tick has already produced its card by the time the
    /// surface is drained.
    fn advance_recording(&mut self) {
        self.recording.sequence = self.recording.sequence.wrapping_add(1);
        self.finish_pending_recording();
        self.advance_video_playback();
        if let Some(export) = self.recording.advance_export() {
            self.finish_video_export(&export);
            let target = match &export.document.recording().provenance {
                scrozz_record::RecordingProvenance::Native { target, .. } => target.clone(),
                scrozz_record::RecordingProvenance::Synthetic { .. } => None,
            };
            self.present_recorded_media(target);
        }
        self.finish_recording_selection();
        self.start_pending_recording();

        let now = Instant::now();
        let delta = now.saturating_duration_since(self.recording.tick);
        self.recording.tick = now;
        let result = self
            .recording
            .machine
            .as_mut()
            .map(|machine| machine.tick(delta));
        if let Some(Err(error)) = result {
            self.note(format!("recording tick failed: {error}"));
        }

        let stopped_without_terminal_event = self
            .recording
            .machine
            .as_ref()
            .is_some_and(RecordingMachine::requires_finalisation);
        if stopped_without_terminal_event && self.recording.finalisation.is_none() {
            self.begin_recording_finalisation(None);
            return;
        }
        self.drain_recording_events();
        self.refresh_recording_tray();
    }

    /// Takes a settled editor export into history and the aggregate card stack.
    ///
    /// Both halves are best-effort and independently reported: an export that
    /// cannot be indexed is still a file the user has, and one that cannot
    /// produce a card is still in history. Neither failure is allowed to look
    /// like a failed export.
    fn finish_video_export(&mut self, export: &crate::gui::recording::CompletedExport) {
        let crate::gui::recording::CompletedExport {
            document,
            plan,
            output,
            poster,
        } = export;

        self.recorded_history_id =
            match crate::commands::persist_transcode_output(document, plan, output) {
                Ok(id) => {
                    self.note(format!(
                        "{} export entered capture history as {}",
                        plan.output.label(),
                        id.0
                    ));
                    // A visible history viewport must show the new row now rather
                    // than on the user's next interaction with it.
                    self.with_history(|history| history.refresh_from_start(Timestamp::now()));
                    Some(id)
                }
                Err(error) => {
                    self.note(format!(
                        "the export was saved but could not enter capture history: {error}"
                    ));
                    None
                }
            };

        let handoff = poster.as_ref().map_or_else(
            || {
                Err(CoreError::Codec(
                    "the completed export has no decoded preview for a card".to_owned(),
                ))
            },
            |frame| FinalizedMediaHandoff::from_export(document, plan, output, frame),
        );
        match handoff {
            Ok(handoff) => self.recording.handoff = Some(handoff),
            Err(error) => self.note(format!(
                "the export was saved but its aggregate media handoff failed: {error}"
            )),
        }
    }

    fn advance_video_playback(&mut self) {
        let result = self.recording.editor.as_mut().map(|editor| {
            let snapshot = editor.playback.poll()?.clone();
            sync_document(&mut editor.document, &snapshot)
        });
        if let Some(Err(error)) = result {
            self.note(format!("recording preview failed: {error}"));
        }
    }

    fn finish_recording_selection(&mut self) {
        let received = self
            .recording
            .selection
            .as_ref()
            .map(|selection| selection.result.try_recv());
        let mut result = match received {
            None | Some(Err(std::sync::mpsc::TryRecvError::Empty)) => return,
            Some(Ok(result)) => result,
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                Err(CliError::Core(CoreError::Platform(
                    "the recording selector stopped without returning a target".to_owned(),
                )))
            }
        };
        let selection = self
            .recording
            .selection
            .take()
            .expect("a completed selector result has pending state");
        if selection.cancel_requested {
            result = Err(CliError::Core(CoreError::Cancelled));
        }
        let reset = self
            .recording
            .machine
            .as_mut()
            .ok_or_else(|| {
                CoreError::InvalidRequest("no recording machine owns the selection".to_owned())
            })
            .and_then(RecordingMachine::cancel_selection);
        if let Err(error) = reset {
            let error = CliError::Core(error);
            self.present_recording_error(&error);
            self.recording.fail(&error);
            return;
        }
        let target = match result {
            Ok(target) => target,
            Err(error) => {
                self.present_recording_error(&error);
                self.recording.fail(&error);
                return;
            }
        };
        let start = match selection.start {
            SelectionStart::Settings { destination } => PendingStart::Settings {
                target,
                destination,
            },
            SelectionStart::Request(args) => {
                match crate::commands::prepare_recording_args_for_target(&args, target) {
                    Ok(prepared) => PendingStart::Request(Box::new(prepared.request)),
                    Err(error) => {
                        self.present_recording_error(&error);
                        self.recording.fail(&error);
                        return;
                    }
                }
            }
        };
        self.arm_recording_start(start);
    }

    /// Starts an armed recording, one tick after it was armed.
    ///
    /// The tick gap is the whole point: the selector's window and the card
    /// surface are still composited in the frame the target was chosen in, and
    /// an encoder started there records Scrozz's own UI.
    fn start_pending_recording(&mut self) {
        let ready = self
            .recording
            .pending_start
            .as_ref()
            .is_some_and(|pending| pending.armed_tick < self.recording.sequence);
        if !ready {
            return;
        }
        let pending = self
            .recording
            .pending_start
            .take()
            .expect("readiness came from the pending start");

        if self.recording.editor_is_open() {
            let error = CliError::Core(CoreError::InvalidRequest(
                "close the video editor before starting a recording".to_owned(),
            ));
            self.present_recording_error(&error);
            self.recording.fail(&error);
            return;
        }

        // Read at the last honest moment, so a change made while the selector
        // was open applies to this recording rather than the previous one.
        let policy = self.config.after_capture.recording_policy();
        if let Err(error) = self.recording.apply_after_capture(policy) {
            let error = CliError::Core(error);
            self.present_recording_error(&error);
            self.recording.fail(&error);
            return;
        }

        let machine = self
            .recording
            .machine
            .as_mut()
            .expect("a pending start requires a recording machine");
        let result = match pending.start {
            PendingStart::Settings {
                target,
                destination,
            } => machine.begin_with_destination(target, destination),
            PendingStart::Request(request) => machine.begin_request_with_settings(*request),
        };
        match result {
            Ok(()) => {
                self.recording.preflight_failure = None;
                self.note("recording started");
            }
            Err(error) => {
                let error = CliError::Core(error);
                self.present_recording_error(&error);
                self.recording.fail(&error);
            }
        }
        self.drain_recording_events();
        self.refresh_recording_tray();
    }

    /// Stops a recording and hands finalisation to its own thread.
    ///
    /// `reply` is a forwarded `record --stop` waiting for the answer: it cannot
    /// be answered here, because the answer is whatever the file turns out to
    /// contain once the encoder has drained.
    fn begin_recording_finalisation(&mut self, reply: Option<Request>) {
        if let Some(selection) = self.recording.selection.as_mut() {
            selection.cancel_requested = true;
            if let Some(reply) = reply {
                self.recording.replies.push(reply);
            }
            self.note("recording selection cancelled before native capture");
            return;
        }
        if self.recording.pending_start.take().is_some() {
            if let Some(reply) = reply {
                self.recording.replies.push(reply);
            }
            self.recording.completion =
                Some(Completion::Failed("the recording was cancelled".to_owned()));
            self.note("pending recording start cancelled before native capture");
            self.recording.reply_waiters();
            return;
        }

        let actions = self.recording.completion_actions();
        let result = self
            .recording
            .machine
            .as_mut()
            .ok_or_else(|| {
                CoreError::InvalidRequest(
                    "no recording is in progress, so there is nothing to stop".to_owned(),
                )
            })
            .and_then(RecordingMachine::begin_finalising);
        let session = match result {
            Ok(session) => session,
            Err(error) => {
                self.note(format!("recording action failed: {error}"));
                if let Some(request) = reply {
                    request.answer(&Err(CliError::Core(error)));
                }
                return;
            }
        };

        let (send, receive) = std::sync::mpsc::channel();
        self.recording.finalisation = Some(receive);
        if let Some(reply) = reply {
            self.recording.replies.push(reply);
        }
        self.note("recording finalisation started");
        crate::gui::recording::spawn_finalisation(session, actions, send);
        self.drain_recording_events();
        self.refresh_recording_tray();
    }

    fn finish_pending_recording(&mut self) {
        let received = self
            .recording
            .finalisation
            .as_ref()
            .map(std::sync::mpsc::Receiver::try_recv);
        let message = match received {
            Some(Ok(message)) => message,
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => return,
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                self.recording.finalisation = None;
                let result = self.recording.machine.as_mut().map(|machine| {
                    machine.complete_finalising(Err(CoreError::Platform(
                        "the recording finaliser ended without returning a result".to_owned(),
                    )))
                });
                if let Some(Err(error)) = result {
                    self.note(format!("could not finish recording state: {error}"));
                }
                self.drain_recording_events();
                return;
            }
        };
        self.recording.finalisation = None;
        self.apply_finalised_recording(message);
    }

    fn apply_finalised_recording(&mut self, message: FinalisedRecording) {
        match message.handoff {
            Ok(Some(handoff)) => self.recording.handoff = Some(handoff),
            Ok(None) => {}
            Err(error) => self.note(format!(
                "recording was saved but its aggregate media handoff failed: {error}"
            )),
        }
        let completed = self
            .recording
            .machine
            .as_mut()
            .map(|machine| machine.complete_finalising(message.result));
        match completed {
            Some(Ok(())) | None => {}
            Some(Err(error)) => self.note(format!("could not finish recording state: {error}")),
        }
        self.drain_recording_events();
    }

    fn drain_recording_events(&mut self) {
        let fallback_target = self
            .recording
            .machine
            .as_ref()
            .and_then(RecordingMachine::request)
            .map(|request| request.target.clone());
        let events = self
            .recording
            .machine
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
                    let where_partial = partial
                        .as_ref()
                        .map(|output| format!("; partial output is at {}", output.path.display()));
                    self.note(format!(
                        "recording failed: {}{}",
                        failure.error,
                        where_partial.unwrap_or_default()
                    ));
                    if let Some(output) = partial {
                        self.retain_recording_completion(output, fallback_target.as_ref());
                    } else {
                        self.recording.completion =
                            Some(Completion::Failed(failure.error.to_string()));
                    }
                }
            }
        }
        self.recording.reply_waiters();
    }

    /// Turns finished output into a card, a history row, and maybe an editor.
    ///
    /// Every step is independent: an editor that will not open must not stop
    /// the card appearing, and a history row that will not write must not make
    /// a saved recording look like a failed one.
    fn retain_recording_completion(
        &mut self,
        mut output: Recording,
        fallback_target: Option<&CaptureTarget>,
    ) {
        if let scrozz_record::RecordingProvenance::Native { target, .. } = &mut output.provenance
            && target.is_none()
        {
            *target = fallback_target.cloned();
        }
        let media_target = match &output.provenance {
            scrozz_record::RecordingProvenance::Native { target, .. } => target.clone(),
            scrozz_record::RecordingProvenance::Synthetic { .. } => None,
        };
        let path = output.path.clone();
        let actions = self.recording.completion_actions();
        self.release_video_editor();

        if actions.open_editor {
            match ActiveVideoEditor::open(&output) {
                Ok(Some(editor)) => {
                    self.recording.editor = Some(editor);
                    self.note("recording opened in the video editor");
                }
                Ok(None) => self.note(
                    "recording was saved but has no playable media to open in the video editor",
                ),
                Err(error) => self.note(format!(
                    "recording was saved but could not be opened in the video editor: {error}"
                )),
            }
        }

        // History first, so the card can carry its durable identity and
        // "reveal in history" works from the moment it appears.
        let finished = crate::commands::finish_recording(&output, fallback_target);
        self.recorded_history_id = finished.history_id;

        if actions.recent_captures_overlay {
            self.present_recorded_media(media_target);
        } else {
            self.recording.handoff = None;
            self.recorded_history_id = None;
        }

        match finished.report {
            Ok(report) => {
                self.note(format!("recording saved to {}", path.display()));
                self.recording.completion = Some(Completion::Finished(Box::new(report)));
            }
            Err(error) => {
                self.note(format!("recording output was not accepted: {error}"));
                self.recording.completion = Some(Completion::Failed(error.to_string()));
            }
        }
        self.with_history(|history| history.refresh_from_start(Timestamp::now()));
    }

    /// Puts the completed recording onto the modern card stack.
    ///
    /// Takes the handoff rather than borrowing it, so a card is created exactly
    /// once. The durable path travels with the card as its written location:
    /// dismissing the card releases nothing, because the card never owned the
    /// bytes in the first place.
    fn present_recorded_media(&mut self, target: Option<CaptureTarget>) {
        let Some(handoff) = self.take_finalized_media_handoff() else {
            return;
        };
        let id = self.pipeline.allocate();
        let capture_id = self.recorded_history_id.take();
        let mut card = Card::from_finalized_media(id, capture_id, &handoff);
        card.upload_available = self.cloud_settings.upload_enabled;
        card.upload_unavailable_reason = self.cloud_settings.unavailable_reason.clone();
        let path = handoff.path.clone();
        self.recorded_media.insert(id, handoff);
        if let Some(target) = target {
            self.recorded_media_targets.insert(id, target);
        }
        match self.surface.present(card) {
            Ok(()) => {
                self.visible_cards.insert(id);
                self.card_retention.insert(id, (true, true));
                self.captures += 1;
                self.note(format!("recording card shown for {}", path.display()));
            }
            Err(error) => {
                self.recorded_media.remove(&id);
                self.recorded_media_targets.remove(&id);
                self.note(format!(
                    "recording was saved to {} but its card could not be shown: {error}",
                    path.display()
                ));
            }
        }
    }

    /// The durable recording a card is showing, if it is showing one.
    #[must_use]
    pub fn recorded_media(&self, card: CardId) -> Option<&FinalizedMediaHandoff> {
        self.recorded_media.get(&card)
    }

    /// Shares the durable media a recording card is showing.
    ///
    /// A recording never entered the capture vault — the card points at a file
    /// on disk — so it takes its own route to the upload worker, carrying the
    /// content type and file name the recorder actually produced rather than
    /// pretending a video is a PNG.
    fn upload_recorded_media(&mut self, card: CardId) {
        self.refresh_cloud_settings(scrozz_ui::CloudConnectionState::Idle);
        if !self.cloud_settings.upload_enabled {
            let reason = self
                .cloud_settings
                .unavailable_reason
                .clone()
                .unwrap_or_else(|| "sharing is unavailable".to_owned());
            self.surface.set_status(card, Some(reason.clone()));
            self.note(format!("{card} upload unavailable: {reason}"));
            return;
        }
        let Some(handoff) = self.recorded_media.get(&card) else {
            self.note(format!("{card} has no recording to upload"));
            return;
        };
        let path = handoff.path.clone();
        let content_type = handoff.content_type.clone();
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("Recording-{}", card.0));
        let capture = self.card_capture_ids.get(&card).cloned();
        self.surface
            .set_status(card, Some("Uploading...".to_owned()));
        // Round 8, Finding #3: this path previously allocated an action id
        // but never recorded it in the card's upload bookkeeping, on the
        // theory that a recording never auto-dismisses on upload completion
        // so nothing needed to be "current". That reasoning missed that a
        // *missing* `pending_upload` entry is exactly the signal
        // `Outcome::UploadDone`/`Outcome::UploadRefused` treat as "trust
        // this outcome" -- so a stale or duplicate recorded-media outcome
        // could overwrite status/notes belonging to a newer, still-in-flight
        // request. Track this action like any other upload; `close_after:
        // false` preserves the existing, intentional behavior that a
        // recording's own upload never retires the card by itself.
        let dispatched = self.dispatch_upload_action(card, false, move |app, action| {
            app.pipeline.post(Job::UploadRecording {
                card,
                capture,
                path,
                content_type,
                file_name,
                action,
            })
        });
        self.report_recorded_media_dispatch_outcome(card, dispatched);
    }

    /// Reports the result of a recorded-media upload dispatch attempt.
    ///
    /// Split out of [`Self::upload_recorded_media`] so its failure path is
    /// directly testable: that function's own cloud-availability check
    /// cannot be made to pass in a build without the `cloud` feature (see
    /// the comment on `a_stale_recorded_media_upload_outcome_never_overwrites_a_newer_ones_status`),
    /// so a test exercising this branch calls this method directly instead.
    fn report_recorded_media_dispatch_outcome(&mut self, card: CardId, dispatched: bool) {
        if !dispatched {
            // Round 10, Finding #2: this branch previously only logged a
            // note, leaving the "Uploading..." status set moments ago to
            // linger forever as though a recording upload were still
            // underway. Mirror the still-image path's own fix (round 9,
            // Finding #3, `CardEvent::Upload` above): nothing is in flight
            // for this card now, so an explicit failure status replaces it.
            self.surface
                .set_status(card, Some("upload could not be started".to_owned()));
            self.note(format!(
                "{card} could not be queued for upload: the capture worker has gone"
            ));
        }
    }

    /// Handles a card gesture that belongs to a recording rather than a still.
    ///
    /// Returns `true` when the event was a recording's, so the ordinary capture
    /// path is not asked to find pixels that were never in the worker's cache.
    fn route_recorded_card_event(&mut self, event: &CardEvent) -> bool {
        let Some(card) = event.card() else {
            return false;
        };
        if !self.recorded_media.contains_key(&card) {
            return false;
        }
        match event {
            CardEvent::Open(_) => {
                self.open_recorded_media(card);
                true
            }
            CardEvent::Drag { at, .. } => {
                self.begin_recorded_drag(card, *at);
                true
            }
            CardEvent::Dismiss(_) | CardEvent::Overflow(_) => {
                // The card goes; the video stays exactly where it was written.
                self.dismiss_recent_capture(card, "dismissed recording card");
                self.recorded_media.remove(&card);
                self.recorded_media_targets.remove(&card);
                self.note(format!("{card} dismissed; its recording is untouched"));
                true
            }
            CardEvent::AutoClose(_, _) => {
                self.dismiss_recent_capture(card, "auto-closed recording card");
                self.recorded_media.remove(&card);
                self.recorded_media_targets.remove(&card);
                self.note(format!(
                    "{card} auto-closed; its recording remains at its saved location"
                ));
                true
            }
            CardEvent::Upload(_) => {
                self.upload_recorded_media(card);
                true
            }
            CardEvent::Copy(_) | CardEvent::Save { .. } | CardEvent::Pin(..) => {
                let availability = current_availability(
                    MediaKind::Recording,
                    match event {
                        CardEvent::Copy(_) => AfterCaptureAction::CopyToClipboard,
                        CardEvent::Save { .. } => AfterCaptureAction::SaveAutomatically,
                        _ => AfterCaptureAction::PinToScreen,
                    },
                );
                // Never falls through to the still path: a recording card has
                // no entry in the worker's pixel vault, so the ordinary handler
                // would answer with "this card owns no persisted capture",
                // which is true and completely unhelpful.
                if let CardEvent::Pin(card, capture, _) = event {
                    self.surface.fail_pin(
                        capture,
                        availability
                            .reason
                            .unwrap_or("Pin to Screen does not apply to a recording.")
                            .to_owned(),
                    );
                    let _ = card;
                }
                self.note(
                    availability
                        .reason
                        .unwrap_or("this recording action is unavailable in this build"),
                );
                true
            }
            _ => false,
        }
    }

    /// Opens a stored recording in the video editor.
    ///
    /// The duration comes from the history row rather than being re-derived:
    /// the editor's own document reads the real container, and this value only
    /// has to be good enough to report the failure honestly if it cannot.
    fn open_history_recording(
        &mut self,
        capture: &CaptureId,
        path: std::path::PathBuf,
        duration_secs: f64,
        target: CaptureTarget,
    ) {
        if self.recording.editor_is_open() {
            self.note("the video editor is already open");
            return;
        }
        match Recording::native(path.clone(), duration_secs, "history")
            .and_then(|recording| {
                recording.with_native_details(target, scrozz_record::RecordingMetadata::default())
            })
            .and_then(|recording| ActiveVideoEditor::open(&recording))
        {
            Ok(Some(editor)) => {
                self.recording.editor = Some(editor);
                self.note(format!(
                    "history capture {} opened in the video editor",
                    capture.0
                ));
            }
            Ok(None) => self.note(format!(
                "history capture {} has no playable media to edit",
                capture.0
            )),
            Err(error) => self.note(format!(
                "could not open {} in the video editor: {error}",
                path.display()
            )),
        }
    }

    fn open_recorded_media(&mut self, card: CardId) {
        let Some(handoff) = self.recorded_media.get(&card) else {
            return;
        };
        let path = handoff.path.clone();
        // The handoff already decided this: only real video opens the editor.
        // A GIF has no editable media track, so its card opens the artifact
        // itself rather than failing inside a decoder that cannot read it.
        if handoff.open_action != scrozz_record::handoff::FinalizedVideoAction::OpenEditor {
            match (self.file_launcher)(FileLaunchAction::Open, &path) {
                Ok(()) => self.note(format!("opened {}", path.display())),
                Err(error) => self.note(format!("could not open {}: {error}", path.display())),
            }
            return;
        }
        if self.recording.editor_is_open() {
            self.note("the video editor is already open");
            return;
        }
        let target = self.recorded_media_targets.get(&card).cloned();
        match Recording::native(path.clone(), handoff.duration.as_secs_f64(), "handoff")
            .and_then(|recording| match target {
                Some(target) => recording
                    .with_native_details(target, scrozz_record::RecordingMetadata::default()),
                None => Ok(recording),
            })
            .and_then(|recording| ActiveVideoEditor::open(&recording))
        {
            Ok(Some(editor)) => {
                self.recording.editor = Some(editor);
                self.note(format!("video editor opened for {}", path.display()));
            }
            Ok(None) => self.note(format!("{} has no playable media to edit", path.display())),
            Err(error) => self.note(format!(
                "could not open {} in the video editor: {error}",
                path.display()
            )),
        }
    }

    fn begin_recorded_drag(&mut self, card: CardId, at: DragSpot) {
        if !self.drag.is_attached()
            && let Some(surface) = self.surface.native_surface()
        {
            self.drag.attach(surface);
        }
        let Some(handoff) = self.recorded_media.get(&card) else {
            return;
        };
        let path = handoff.path.clone();
        let poster = poster_png(&handoff.poster);
        match self.drag.begin_media(card, at, &path, poster.as_deref()) {
            Ok(()) => tracing::debug!(%card, path = %path.display(), "recording drag started"),
            Err(why) => {
                self.surface.settle_drag(card, false);
                self.modal_drag_input_release_pending = true;
                self.note(format!("{card} could not be dragged: {why}"));
            }
        }
    }

    fn present_recording_error(&mut self, error: &CliError) {
        self.recording.preflight_failure = Some(crate::gui::recording::preflight_failure(error));
        self.note(format!("recording action failed: {error}"));
    }

    fn ensure_recording_permission() -> CliResult<()> {
        let permissions = SystemPermissions::new();
        if !permissions.is_granted(Capability::ScreenRecording) {
            permissions.request(Capability::ScreenRecording)?;
        }
        Ok(())
    }

    /// Asks for Input Monitoring only when the settings actually need it.
    ///
    /// This is the whole permission story for interaction overlays: nothing is
    /// requested at launch, nothing is requested to open the settings pane, and
    /// nothing is requested for a recording whose click and keystroke overlays
    /// are both off — which is what ships.
    fn ensure_recording_input_permission(&self) -> CliResult<()> {
        if !self.recording.settings().needs_input_monitoring() {
            return Ok(());
        }
        let permissions = SystemPermissions::new();
        if !permissions.is_granted(Capability::InputMonitoring) {
            permissions.request(Capability::InputMonitoring)?;
        }
        Ok(())
    }

    /// Re-reads the persisted interaction policy just before a recording runs.
    ///
    /// A settings edit made in another process — `scrozz settings set` — is as
    /// authoritative as one made in the pane, and reading here is the last
    /// honest moment before the engine installs anything.
    fn reload_recording_settings(&mut self) -> CliResult<()> {
        if self.recording.machine.is_none() {
            return Ok(());
        }
        // With no durable store the in-memory policy *is* the policy; re-reading
        // a document that was never written would silently undo this session's
        // choices right before the recording that was about to use them.
        let Some(store) = self.config.after_capture_store.clone() else {
            return Ok(());
        };
        let persisted = store
            .load(store.inferred_profile())
            .map_err(CliError::Core)?;
        let settings = crate::settings::recording_settings_from(&persisted)?;
        self.config.after_capture = persisted;
        self.recording
            .apply_settings(settings)
            .map_err(CliError::Core)?;
        self.recording.settings_panel = None;
        Ok(())
    }

    /// Writes any unsaved settings-pane edit through to the settings document.
    ///
    /// The edit is already live on the machine, so a run with no durable store
    /// keeps it for the session rather than losing it here.
    ///
    /// # Errors
    ///
    /// Returns a usage error for a policy this build rejects, or a storage
    /// error when the document cannot be replaced atomically.
    fn save_recording_settings_panel(&mut self) -> CliResult<()> {
        let Some(settings) = self.recording.take_settings_panel() else {
            return Ok(());
        };
        let Some(store) = self.config.after_capture_store.clone() else {
            return Ok(());
        };
        self.config.after_capture = crate::settings::save_recording_settings(&store, settings)?;
        Ok(())
    }

    /// The recording policy and capability the settings pane draws from.
    #[must_use]
    pub fn recording_settings_pane(&self) -> scrozz_ui::settings::RecordingPane {
        scrozz_ui::settings::RecordingPane {
            settings: self.recording.settings(),
            capabilities: self.recording.capabilities(),
            active: self.recording.is_busy(),
            camera: self.recording.camera_status().map(|status| {
                Box::new(scrozz_ui::CameraLiveSnapshot {
                    settings: self.recording.settings().camera,
                    enabled: status.active,
                    status,
                    preview: self.recording.camera_preview(),
                })
            }),
        }
    }

    /// The camera device and preview surface, when its window is open.
    #[must_use]
    pub fn camera_settings_snapshot(&self) -> Option<scrozz_ui::CameraSettingsSnapshot> {
        self.camera_settings_window.clone()
    }

    /// Opens the camera window with passively enumerated devices.
    ///
    /// Enumeration and permission are both read without prompting, so opening
    /// this window never turns a camera light on. Only Preview does that.
    fn open_camera_settings(&mut self) {
        let (devices, error) = match (self.camera_devices)() {
            Ok(devices) => (devices, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        self.camera_settings_window = Some(scrozz_ui::CameraSettingsSnapshot {
            settings: self.recording.settings().camera,
            devices,
            selected_device: self.camera_device.clone(),
            capture_configuration_locked: self.recording.is_busy(),
            permission: (self.camera_permission_status)(),
            preview: None,
            preview_status: None,
            error,
        });
    }

    /// Applies one semantic action from the camera window.
    pub fn handle_camera_settings_action(&mut self, action: scrozz_ui::CameraSettingsAction) {
        match action {
            scrozz_ui::CameraSettingsAction::Close => {
                self.flush_camera_preferences(true);
                self.stop_camera_preview();
                self.camera_settings_window = None;
            }
            scrozz_ui::CameraSettingsAction::SettingsChanged(camera) => {
                self.apply_camera_settings(camera);
            }
            scrozz_ui::CameraSettingsAction::DeviceChanged(device) => {
                // Switching device releases the running preview: two sessions on
                // one camera is exactly the contention this feature must avoid.
                self.stop_camera_preview();
                match self.recording.select_camera_device(device.clone()) {
                    Ok(()) => {
                        self.camera_device = device.clone();
                        if let Some(window) = &mut self.camera_settings_window {
                            window.selected_device = device;
                            window.error = None;
                        }
                        self.persist_camera_preferences();
                    }
                    Err(error) => self.report_camera_error(&error),
                }
            }
            scrozz_ui::CameraSettingsAction::StartPreview => self.start_camera_preview(),
            scrozz_ui::CameraSettingsAction::StopPreview => {
                self.stop_camera_preview();
                self.note("camera preview stopped and the camera released");
            }
        }
    }

    /// Applies a composition change from either camera surface.
    ///
    /// A running recording takes it live; otherwise it becomes the pending
    /// preference. Either way it is persisted, so the next recording opens with
    /// the composition the user last chose.
    fn apply_camera_settings(&mut self, camera: CameraSettings) {
        let applied = self.recording.apply_camera_preference(camera);
        match applied {
            Ok(()) => {
                let preview_error = if camera.enabled {
                    self.camera_preview
                        .as_mut()
                        .and_then(|preview| preview.update_settings(camera).err())
                } else {
                    None
                };
                if !camera.enabled || preview_error.is_some() {
                    self.stop_camera_preview();
                }
                if let Some(window) = &mut self.camera_settings_window {
                    window.settings = camera;
                    window.error = preview_error.as_ref().map(ToString::to_string);
                }
                self.pending_camera_preferences_save =
                    Some(Instant::now() + CAMERA_SETTINGS_SAVE_DEBOUNCE);
                if let Some(error) = preview_error {
                    self.note(format!(
                        "camera preview stopped because its composition could not update: {error}"
                    ));
                }
            }
            Err(error) => self.report_camera_error(&error),
        }
    }

    fn persist_camera_preferences(&mut self) {
        self.pending_camera_preferences_save = None;
        let Some(store) = self.config.after_capture_store.clone() else {
            return;
        };
        let settings = self.recording.settings();
        let device = self.camera_device.clone();
        match crate::settings::save_camera_preferences(&store, settings, device.as_ref()) {
            Ok(updated) => self.config.after_capture = updated,
            Err(error) => self.note(format!("camera preferences could not be saved: {error}")),
        }
    }

    fn flush_camera_preferences(&mut self, force: bool) {
        let Some(deadline) = self.pending_camera_preferences_save else {
            return;
        };
        if !force && Instant::now() < deadline {
            return;
        }
        self.persist_camera_preferences();
    }

    /// Starts the explicit preview, which is the only path that may prompt.
    fn start_camera_preview(&mut self) {
        self.stop_camera_preview();
        if self.recording.is_busy() {
            self.note("the camera cannot be previewed while a recording owns it");
            return;
        }
        let mut request = CameraRequest::new(self.recording.settings().camera);
        request.settings.enabled = true;
        if let Some(device) = self.camera_device.clone() {
            request = request.with_device(device);
        }
        match (self.camera_preview_start)(&request) {
            Ok(session) => {
                if let Some(window) = &mut self.camera_settings_window {
                    window.preview_status = Some(session.status());
                    window.error = None;
                }
                self.camera_preview = Some(session);
                self.note("camera preview started; the system privacy indicator is on");
            }
            Err(error) => self.report_camera_error(&error),
        }
    }

    /// Releases the preview immediately, turning the camera light off.
    fn stop_camera_preview(&mut self) {
        if let Some(session) = self.camera_preview.take() {
            session.stop();
        }
        if let Some(window) = &mut self.camera_settings_window {
            window.preview = None;
            window.preview_status = None;
        }
    }

    /// Pulls the freshest preview frame and reacts to a revoked camera.
    fn advance_camera_preview(&mut self) {
        let Some(session) = self.camera_preview.as_mut() else {
            return;
        };
        let frame = session.poll();
        let status = session.status();
        let revoked = !status.active
            && matches!(
                status.device_state,
                scrozz_record::CameraDeviceState::Disconnected
                    | scrozz_record::CameraDeviceState::PermissionDenied
            );
        if let Some(window) = &mut self.camera_settings_window {
            window.preview_status = Some(status.clone());
            if let Some(frame) = frame {
                window.preview = Some(frame);
            }
            window.permission = (self.camera_permission_status)();
        }
        if revoked {
            let reason = status
                .warning
                .clone()
                .unwrap_or_else(|| "the camera became unavailable".to_owned());
            self.stop_camera_preview();
            if let Some(window) = &mut self.camera_settings_window {
                window.error = Some(reason.clone());
            }
            self.note(format!("camera preview stopped: {reason}"));
        }
    }

    fn sync_camera_settings_window(&mut self) {
        let settings = self.recording.settings().camera;
        let locked = self.recording.is_busy();
        if let Some(window) = &mut self.camera_settings_window {
            window.settings = settings;
            window.selected_device = self.camera_device.clone();
            window.capture_configuration_locked = locked;
        }
    }

    fn report_camera_error(&mut self, error: &CoreError) {
        let message = error.to_string();
        if let Some(window) = &mut self.camera_settings_window {
            window.error = Some(message.clone());
        }
        self.note(format!("camera action failed: {message}"));
    }

    /// Applies what the settings pane asked for during one frame.
    pub fn edit_recording_settings(&mut self, actions: &[RecordingSettingsAction]) {
        for action in actions {
            match action {
                RecordingSettingsAction::Changed(settings) => {
                    match self.recording.apply_settings(*settings) {
                        Ok(()) => {
                            if !settings.camera.enabled {
                                self.stop_camera_preview();
                            }
                        }
                        Err(error) => self.present_recording_error(&CliError::Core(error)),
                    }
                }
                RecordingSettingsAction::Close => match self.save_recording_settings_panel() {
                    Ok(()) => self.note("recording settings saved"),
                    Err(error) => self.present_recording_error(&error),
                },
                RecordingSettingsAction::OpenCamera => self.open_camera_settings(),
                RecordingSettingsAction::CameraChanged(camera) => {
                    self.apply_camera_settings(*camera);
                }
                RecordingSettingsAction::StartRecording => {
                    self.begin_recording();
                }
            }
        }
    }

    fn active_recording_target() -> CliResult<CaptureTarget> {
        let backend = crate::platform::capture_backend()?;
        Ok(CaptureTarget::Display(backend.active_display()?.id))
    }

    fn refresh_recording_tray(&self) {
        let Some(tray) = &self.tray else {
            return;
        };
        tray.set_recording(self.recording.is_active());
    }

    fn release_video_editor(&mut self) {
        for problem in self.recording.release_editor() {
            self.note(problem);
        }
    }

    /// The completed video waiting to enter the aggregate capture stack.
    ///
    /// **This is the whole seam.** Everything the recording lifecycle wants the
    /// modern card stack to know arrives here as one validated, durable value,
    /// and nothing else crosses. Starting another recording does not clear it:
    /// the owner takes it explicitly, so a card can never be silently lost to a
    /// second recording that happened first.
    #[must_use]
    pub const fn finalized_media_handoff(&self) -> Option<&FinalizedMediaHandoff> {
        self.recording.handoff.as_ref()
    }

    /// Transfers the completed video to the aggregate capture owner.
    pub fn take_finalized_media_handoff(&mut self) -> Option<FinalizedMediaHandoff> {
        self.recording.handoff.take()
    }

    /// The video editor state one viewport pass needs, if one is open.
    #[must_use]
    pub fn video_editor_snapshot(&self) -> Option<VideoEditorSnapshot> {
        self.recording
            .editor
            .as_ref()
            .map(ActiveVideoEditor::snapshot)
    }

    /// Whether the recording editor window should be showing.
    #[must_use]
    pub const fn video_editor_is_open(&self) -> bool {
        self.recording.editor_is_open()
    }

    /// Whether recording is busy enough that other surfaces must not park it.
    #[must_use]
    pub fn recording_is_busy(&self) -> bool {
        self.recording.is_busy()
    }

    /// Applies one action raised by the video editor viewport.
    pub fn handle_video_editor_action(&mut self, action: VideoEditorAction) {
        use crate::gui::recording::action_allowed_during_export;

        if action == VideoEditorAction::Close {
            if self
                .recording
                .editor
                .as_ref()
                .is_some_and(ActiveVideoEditor::is_exporting)
            {
                self.note("cancel the active export before closing the video editor");
                return;
            }
            self.release_video_editor();
            if let Err(error) = self.reset_finished_recording() {
                self.note(format!("could not close the video editor: {error}"));
            }
            self.note("video editor closed");
            return;
        }
        if self
            .recording
            .editor
            .as_ref()
            .is_some_and(ActiveVideoEditor::is_exporting)
            && !action_allowed_during_export(&action)
        {
            self.note("wait for the active export or cancel it before changing the editor");
            return;
        }
        let Some(editor) = self.recording.editor.as_mut() else {
            self.note("the video editor no longer has a recording");
            return;
        };

        let mut problem = None;
        let mut reveal = None;
        match action {
            VideoEditorAction::Close => unreachable!("close returns before the editor is borrowed"),
            VideoEditorAction::Play => match editor.playback.play() {
                Ok(()) => editor.document.play(),
                Err(error) => problem = Some(format!("could not play the recording: {error}")),
            },
            VideoEditorAction::Pause => {
                editor.playback.pause();
                editor.document.pause();
            }
            VideoEditorAction::Seek(position) => {
                if let Err(error) = editor
                    .playback
                    .seek(position)
                    .and_then(|()| editor.document.seek(position))
                {
                    problem = Some(format!("could not seek the recording: {error}"));
                }
            }
            VideoEditorAction::SetRate(rate) => {
                if let Err(error) = editor.playback.set_rate(rate) {
                    problem = Some(format!(
                        "could not change the recording playback rate: {error}"
                    ));
                }
            }
            VideoEditorAction::PlanChanged(plan) => {
                editor.plan = plan;
                problem = editor
                    .playback
                    .set_plan(&editor.document, plan)
                    .err()
                    .map(|error| format!("could not update the recording preview: {error}"));
                editor.transcode_status = None;
                editor.transcode_progress = 0.0;
                editor.transcode_output = None;
                editor.transcode_failure = None;
            }
            VideoEditorAction::Export(plan) => {
                editor.playback.pause();
                editor.document.pause();
                editor.plan = plan;
                editor.transcode_progress = 0.0;
                editor.transcode_output = None;
                editor.transcode_failure = None;
                match crate::gui::recording::start_export(&editor.document, &plan) {
                    Ok(job) => {
                        editor.transcode_status = Some(job.status());
                        editor.transcode_job = Some(job);
                    }
                    Err(error) => {
                        editor.transcode_status =
                            Some(scrozz_record::transcode::TranscodeStatus::Failed);
                        editor.transcode_failure =
                            Some(scrozz_record::transcode::TranscodeFailure {
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
                    problem = Some(format!("could not cancel the video export: {error}"));
                }
            }
            VideoEditorAction::RevealOutput | VideoEditorAction::RevealPartialOutput => {
                reveal = editor
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
            }
        }
        if let Some(problem) = problem {
            self.note(problem);
        }
        if let Some(path) = reveal {
            self.reveal_recording_path(&path);
        }
    }

    fn reveal_recording_path(&mut self, path: &std::path::Path) {
        match (self.file_launcher)(FileLaunchAction::Reveal, path) {
            Ok(()) => self.note(format!("revealed recording at {}", path.display())),
            Err(error) => self.note(format!(
                "could not reveal recording at {}: {error}",
                path.display()
            )),
        }
    }

    fn settle_recording_before_shutdown(&mut self) {
        let cancellation = self
            .recording
            .editor
            .as_mut()
            .and_then(|editor| editor.transcode_job.as_mut())
            .map(|job| job.cancel());
        if let Some(Err(error)) = cancellation {
            self.note(format!(
                "video export cancellation was already settling during shutdown: {error}"
            ));
        }
        let deadline = Instant::now() + crate::gui::recording::SHUTDOWN_FINALISE_TIMEOUT;
        while self
            .recording
            .editor
            .as_ref()
            .is_some_and(ActiveVideoEditor::is_exporting)
            && Instant::now() < deadline
        {
            if let Some(export) = self.recording.advance_export() {
                self.finish_video_export(&export);
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        if let Some(receiver) = self.recording.finalisation.take() {
            match receiver.recv_timeout(crate::gui::recording::SHUTDOWN_FINALISE_TIMEOUT) {
                Ok(message) => self.apply_finalised_recording(message),
                Err(_) => {
                    if let Some(machine) = self.recording.machine.as_mut() {
                        let _ = machine.complete_finalising(Err(CoreError::Platform(
                            "the recording finaliser ended during shutdown".to_owned(),
                        )));
                    }
                    self.drain_recording_events();
                }
            }
        } else if matches!(
            self.recording.phase(),
            Some(RecordingPhase::Recording | RecordingPhase::Paused)
        ) {
            // A recording in flight when the app quits is finished here rather
            // than abandoned: the alternative is a half-written container the
            // user never asked for and cannot play.
            let session = self
                .recording
                .machine
                .as_mut()
                .expect("the phase came from this machine")
                .begin_finalising();
            match session {
                Ok(session) => {
                    let result = session.stop();
                    if let Some(machine) = self.recording.machine.as_mut() {
                        let _ = machine.complete_finalising(result);
                    }
                    self.drain_recording_events();
                }
                Err(error) => self.note(format!(
                    "could not finalise the recording during shutdown: {error}"
                )),
            }
        }
        self.release_video_editor();
    }

    /// Every note recorded so far.
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// Encodes a bounded poster as PNG for a drag thumbnail.
///
/// Returns `None` rather than failing the drag: a drag with no thumbnail still
/// carries the file, and a drag that refuses to start because a preview could
/// not be encoded would be strictly worse.
fn poster_png(poster: &scrozz_record::handoff::VideoPoster) -> Option<Vec<u8>> {
    let image = scrozz_export::RgbaImage {
        width: poster.width,
        height: poster.height,
        data: poster.bytes.clone(),
    };
    scrozz_export::FrameEncoder::new()
        .encode_rgba(
            &image,
            scrozz_core::ColorSpace::Srgb,
            scrozz_export::ImageFormat::Png,
        )
        .map_err(|error| {
            tracing::debug!(%error, "the recording drag preview could not be encoded");
        })
        .ok()
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

fn forwarded_open_history(command: &crate::cli::Command) -> bool {
    let crate::cli::Command::History(history) = command else {
        return false;
    };
    matches!(&history.command, crate::cli::HistoryCommand::Show)
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
    use crate::after_capture::InstallProfile;
    use crate::gui::card::{Card, CardId, Recording};
    use crate::gui::selection::UnsupportedSelector;
    use scrozz_core::{
        Capture, CaptureTarget, ColorSpace, Display, DisplayId, Frame, LogicalPoint, LogicalRect,
        LogicalSize, PhysicalSize, PixelFormat, Provenance, ScaleFactor, WindowId,
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

    fn install_mock_recording_engine(app: &mut App) {
        let settings = app.recording.settings();
        let plan = scrozz_record::engine::MockSessionPlan::complete(
            std::env::temp_dir().join(format!("scrozz-recording-mock-{}", std::process::id())),
            1.0,
        )
        .expect("valid mock recording");
        app.recording.machine = Some(
            RecordingMachine::with_engine(
                Box::new(scrozz_record::engine::MockEngine::fully_capable(plan)),
                settings,
            )
            .expect("mock recording machine"),
        );
        app.recording.unavailable = None;
    }

    fn forwarded_capture(kind: CaptureKind) -> ForwardedCapture {
        let (provenance, target) = match kind {
            // A scrolling capture is never forwarded as a one-shot; the test
            // fixture treats it as the window capture it is assembled from.
            CaptureKind::Scrolling => (
                Provenance::Window,
                CaptureTarget::Window(WindowId("fixture-window".to_owned())),
            ),
            CaptureKind::Fullscreen => (
                Provenance::Display,
                CaptureTarget::Region(LogicalRect::new(
                    LogicalPoint::new(0.0, 0.0),
                    LogicalSize::new(4.0, 4.0),
                )),
            ),
            CaptureKind::Region => (
                Provenance::Region,
                CaptureTarget::Region(LogicalRect::new(
                    LogicalPoint::new(0.0, 0.0),
                    LogicalSize::new(4.0, 4.0),
                )),
            ),
            CaptureKind::Window | CaptureKind::AllInOne => (
                Provenance::Window,
                CaptureTarget::Window(WindowId("forwarded-window".into())),
            ),
            CaptureKind::AllDisplays => (Provenance::AllDisplays, CaptureTarget::AllDisplays),
        };
        ForwardedCapture {
            kind,
            capture: Capture {
                frame: Frame {
                    data: vec![255; 4 * 4 * 4],
                    size: PhysicalSize::new(4.0, 4.0),
                    stride: 16,
                    format: PixelFormat::Rgba8,
                    color_space: ColorSpace::Srgb,
                    scale: ScaleFactor::IDENTITY,
                },
                provenance,
                target,
            },
        }
    }

    #[test]
    fn persisted_camera_device_reaches_preview_and_recording_machine() {
        let selected = CameraDeviceId::new("stable-camera-2").expect("camera id");
        let mut config = Config::sealed();
        config
            .after_capture
            .set_value("record.camera-device", selected.as_str());
        let app = App::new(
            config,
            Box::new(Recording::new()),
            Arc::new(UnsupportedSelector::headless()),
            false,
        )
        .expect("app");

        assert_eq!(app.camera_device.as_ref(), Some(&selected));
        if let Some(machine) = app.recording.machine.as_ref() {
            assert_eq!(
                machine.camera_device(),
                Some(&selected),
                "a restart must not make recording use a different camera than preview"
            );
        }
    }

    #[test]
    fn arming_a_recording_releases_the_settings_camera_preview() {
        struct Preview {
            stopped: Arc<AtomicBool>,
        }

        impl CameraPreviewSession for Preview {
            fn status(&self) -> scrozz_record::CameraRuntimeStatus {
                scrozz_record::CameraRuntimeStatus::default()
            }

            fn poll(&mut self) -> Option<scrozz_record::CameraPreview> {
                None
            }

            fn update_settings(&mut self, _settings: CameraSettings) -> scrozz_core::Result<()> {
                Ok(())
            }

            fn stop(self: Box<Self>) {
                self.stopped.store(true, Ordering::Release);
            }
        }

        let (mut app, _) = app();
        let stopped = Arc::new(AtomicBool::new(false));
        app.camera_preview = Some(Box::new(Preview {
            stopped: Arc::clone(&stopped),
        }));

        app.arm_recording_start(PendingStart::Request(Box::new(
            scrozz_record::RecordingRequest::new(CaptureTarget::AllDisplays),
        )));

        assert!(stopped.load(Ordering::Acquire));
        assert!(app.camera_preview.is_none());
        assert!(app.recording.pending_start.is_some());
    }

    #[test]
    fn disabling_camera_releases_a_live_preview() {
        struct Preview {
            stopped: Arc<AtomicBool>,
        }

        impl CameraPreviewSession for Preview {
            fn status(&self) -> scrozz_record::CameraRuntimeStatus {
                scrozz_record::CameraRuntimeStatus::default()
            }

            fn poll(&mut self) -> Option<scrozz_record::CameraPreview> {
                None
            }

            fn update_settings(&mut self, _settings: CameraSettings) -> scrozz_core::Result<()> {
                Ok(())
            }

            fn stop(self: Box<Self>) {
                self.stopped.store(true, Ordering::Release);
            }
        }

        let (mut app, _) = app();
        install_mock_recording_engine(&mut app);
        let stopped = Arc::new(AtomicBool::new(false));
        app.camera_preview = Some(Box::new(Preview {
            stopped: Arc::clone(&stopped),
        }));
        let mut camera = app.recording.settings().camera;
        camera.enabled = false;

        app.apply_camera_settings(camera);

        assert!(stopped.load(Ordering::Acquire));
        assert!(app.camera_preview.is_none());
    }

    #[test]
    fn continuous_camera_edits_apply_live_but_persist_once_when_flushed() {
        let root = scratch("camera-settings-debounce");
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
        .expect("app");
        install_mock_recording_engine(&mut app);
        let mut first = app.recording.settings().camera;
        first.size = 0.25;
        let mut latest = first;
        latest.size = 0.30;

        app.apply_camera_settings(first);
        app.apply_camera_settings(latest);

        assert_eq!(app.recording.settings().camera, latest);
        assert!(app.pending_camera_preferences_save.is_some());
        let before = store
            .load(store.inferred_profile())
            .expect("fresh settings");
        assert_ne!(
            crate::settings::recording_settings_from(&before)
                .expect("recording settings")
                .camera,
            latest,
            "dragging should not fsync every intermediate camera value"
        );

        app.flush_camera_preferences(true);

        let saved = store
            .load(store.inferred_profile())
            .expect("saved settings");
        assert_eq!(
            crate::settings::recording_settings_from(&saved)
                .expect("recording settings")
                .camera,
            latest
        );
        assert!(app.pending_camera_preferences_save.is_none());
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn open_camera_window_tracks_authoritative_recording_settings() {
        let (mut app, _) = app();
        install_mock_recording_engine(&mut app);
        let mut stale = app.recording.settings().camera;
        stale.enabled = true;
        app.camera_settings_window = Some(scrozz_ui::CameraSettingsSnapshot {
            settings: stale,
            devices: Vec::new(),
            selected_device: None,
            capture_configuration_locked: false,
            permission: CameraPermission::Unsupported,
            preview: None,
            preview_status: None,
            error: None,
        });
        let mut authoritative = app.recording.settings();
        authoritative.camera.enabled = false;
        app.recording
            .apply_settings(authoritative)
            .expect("settings apply");

        app.sync_camera_settings_window();

        let window = app.camera_settings_window.as_ref().expect("window");
        assert!(!window.settings.enabled);
        assert_eq!(window.capture_configuration_locked, app.recording.is_busy());
    }

    #[test]
    fn shutdown_releases_the_camera_preview_immediately() {
        struct Preview {
            stopped: Arc<AtomicBool>,
        }

        impl CameraPreviewSession for Preview {
            fn status(&self) -> scrozz_record::CameraRuntimeStatus {
                scrozz_record::CameraRuntimeStatus::default()
            }

            fn poll(&mut self) -> Option<scrozz_record::CameraPreview> {
                None
            }

            fn update_settings(&mut self, _settings: CameraSettings) -> scrozz_core::Result<()> {
                Ok(())
            }

            fn stop(self: Box<Self>) {
                self.stopped.store(true, Ordering::Release);
            }
        }

        let (mut app, _) = app();
        let stopped = Arc::new(AtomicBool::new(false));
        app.camera_preview = Some(Box::new(Preview {
            stopped: Arc::clone(&stopped),
        }));

        app.shut_down();

        assert!(stopped.load(Ordering::Acquire));
        assert!(app.camera_preview.is_none());
    }

    #[test]
    fn recording_pane_disabling_camera_releases_a_live_preview() {
        struct Preview {
            stopped: Arc<AtomicBool>,
        }

        impl CameraPreviewSession for Preview {
            fn status(&self) -> scrozz_record::CameraRuntimeStatus {
                scrozz_record::CameraRuntimeStatus::default()
            }

            fn poll(&mut self) -> Option<scrozz_record::CameraPreview> {
                None
            }

            fn update_settings(&mut self, _settings: CameraSettings) -> scrozz_core::Result<()> {
                Ok(())
            }

            fn stop(self: Box<Self>) {
                self.stopped.store(true, Ordering::Release);
            }
        }

        let (mut app, _) = app();
        install_mock_recording_engine(&mut app);
        let stopped = Arc::new(AtomicBool::new(false));
        app.camera_preview = Some(Box::new(Preview {
            stopped: Arc::clone(&stopped),
        }));
        let mut settings = app.recording.settings();
        settings.camera.enabled = false;

        app.edit_recording_settings(&[RecordingSettingsAction::Changed(settings)]);

        assert!(stopped.load(Ordering::Acquire));
        assert!(app.camera_preview.is_none());
    }

    #[test]
    fn camera_composition_during_selection_is_staged_for_the_next_recording() {
        let (mut app, _) = app();
        install_mock_recording_engine(&mut app);
        app.recording
            .machine
            .as_mut()
            .expect("native machine")
            .begin_selection()
            .expect("selecting");
        let mut camera = app.recording.settings().camera;
        camera.size = 0.31;

        app.apply_camera_settings(camera);

        assert_eq!(app.recording.settings().camera, camera);
        assert!(
            !app.notes()
                .iter()
                .any(|note| note.contains("camera action failed"))
        );
    }

    #[test]
    fn camera_window_composition_updates_the_running_preview() {
        struct Preview {
            updates: Arc<Mutex<Vec<CameraSettings>>>,
        }

        impl CameraPreviewSession for Preview {
            fn status(&self) -> scrozz_record::CameraRuntimeStatus {
                scrozz_record::CameraRuntimeStatus::default()
            }

            fn poll(&mut self) -> Option<scrozz_record::CameraPreview> {
                None
            }

            fn update_settings(&mut self, settings: CameraSettings) -> scrozz_core::Result<()> {
                self.updates.lock().expect("updates").push(settings);
                Ok(())
            }

            fn stop(self: Box<Self>) {}
        }

        let (mut app, _) = app();
        install_mock_recording_engine(&mut app);
        let updates = Arc::new(Mutex::new(Vec::new()));
        app.camera_preview = Some(Box::new(Preview {
            updates: Arc::clone(&updates),
        }));
        let mut camera = app.recording.settings().camera;
        camera.enabled = true;
        camera.shape = scrozz_record::settings::CameraShape::Square;

        app.apply_camera_settings(camera);

        assert_eq!(*updates.lock().expect("updates"), vec![camera]);
        assert!(app.camera_preview.is_some());
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
    fn recording_settings_pane_ships_needing_no_input_monitoring() {
        let (app, _) = app();
        let pane = app.recording_settings_pane();

        assert!(!pane.settings.clicks.enabled);
        assert!(!pane.settings.keystrokes.enabled);
        assert_eq!(
            pane.settings.keystrokes.scope,
            scrozz_record::settings::KeystrokeScope::ModifiersOnly
        );
        assert!(!pane.settings.needs_input_monitoring());
        assert!(!pane.active, "an idle app must not lock the pane");
    }

    #[test]
    fn a_privacy_choice_reaches_the_engine_without_starting_a_capture() {
        let (mut app, _) = app();
        #[cfg(target_os = "macos")]
        assert!(
            app.recording.machine.is_some(),
            "macOS always links a native recording engine"
        );
        if app.recording.machine.is_none() {
            return;
        }

        let mut changed = app.recording_settings_pane().settings;
        changed.keystrokes.enabled = true;
        changed.keystrokes.scope = scrozz_record::settings::KeystrokeScope::All;
        app.edit_recording_settings(&[RecordingSettingsAction::Changed(changed)]);

        assert!(
            app.recording.settings().needs_input_monitoring(),
            "an enabled keystroke overlay is what makes Input Monitoring necessary"
        );
        assert_eq!(app.recording.phase(), Some(RecordingPhase::Idle));
        assert!(
            app.recording.settings_panel.is_some(),
            "the edit stays unsaved until the pane closes"
        );

        app.edit_recording_settings(&[RecordingSettingsAction::Close]);
        assert!(
            app.recording.settings_panel.is_none(),
            "closing the pane settles the edit"
        );
        assert_eq!(app.recording.phase(), Some(RecordingPhase::Idle));
    }

    #[test]
    fn pending_native_input_wakes_an_idle_window_host() {
        let pending = Arc::new(AtomicBool::new(true));
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&wakes);
        let waker: SurfaceWaker = Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        });
        let pending_probe = Arc::clone(&pending);
        let monitor = InputWakeMonitor::start_with_probe(
            Some(waker),
            move || pending_probe.load(Ordering::Acquire),
            Duration::from_millis(1),
        )
        .expect("wake monitor starts")
        .expect("a waker creates a monitor");

        let deadline = Instant::now() + Duration::from_secs(1);
        while wakes.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        pending.store(false, Ordering::Release);
        drop(monitor);

        assert!(wakes.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn overflow_waits_for_initial_capture_finalization() {
        let (mut app, surface) = app();
        let card = CardId(40);
        let actions = crate::after_capture::ExecutionReport {
            steps: vec![crate::after_capture::ActionStep {
                action: AfterCaptureAction::ShowRecentCapturesOverlay,
                outcome: crate::after_capture::ActionOutcome::Succeeded(
                    ActionEffect::ShowRecentCapturesOverlay,
                ),
            }],
        };
        app.pipeline
            .inject_outcome_for_test(Outcome::Ready(Box::new(ReadyCapture {
                card: Card::placeholder(card, CaptureKind::Region),
                actions,
                retained_elsewhere: false,
                exported: false,
                finalization_pending: true,
                finalization_ack: None,
            })));
        app.drain_pipeline();
        assert!(app.finalizing_cards.contains(&card));

        surface.inject(CardEvent::Overflow(card));
        app.drain_cards(EditorSnapshots::EMPTY);
        assert!(app.deferred_overflow.contains(&card));
        assert!(
            !surface
                .trace()
                .contains(&crate::gui::card::SurfaceCall::Dismiss(card)),
            "capacity cleanup must not guess before history/save results exist"
        );

        app.pipeline
            .inject_outcome_for_test(Outcome::Finalized(Box::new(FinalizedCapture {
                card,
                capture_id: None,
                actions: crate::after_capture::ExecutionReport::default(),
                written: Vec::new(),
                retained_elsewhere: true,
                exported: false,
            })));
        app.drain_pipeline();

        assert!(!app.finalizing_cards.contains(&card));
        assert!(!app.deferred_overflow.contains(&card));
        assert!(
            surface
                .trace()
                .contains(&crate::gui::card::SurfaceCall::Dismiss(card)),
            "deferred cleanup must resume against finalized retention truth"
        );
    }

    #[test]
    fn save_during_initial_finalization_reuses_the_automatic_export() {
        let (mut app, surface) = app();
        let card = CardId(41);
        let actions = crate::after_capture::ExecutionReport {
            steps: vec![crate::after_capture::ActionStep {
                action: AfterCaptureAction::ShowRecentCapturesOverlay,
                outcome: crate::after_capture::ActionOutcome::Succeeded(
                    ActionEffect::ShowRecentCapturesOverlay,
                ),
            }],
        };
        app.pipeline
            .inject_outcome_for_test(Outcome::Ready(Box::new(ReadyCapture {
                card: Card::placeholder(card, CaptureKind::Region),
                actions,
                retained_elsewhere: false,
                exported: false,
                finalization_pending: true,
                finalization_ack: None,
            })));
        app.drain_pipeline();
        surface.inject(CardEvent::Save {
            card,
            choose_destination: false,
        });
        app.drain_cards(EditorSnapshots::EMPTY);
        assert!(app.deferred_save.contains(&card));
        assert!(!app.card_has_outstanding_card_level_output(card));
        surface.inject(CardEvent::Dismiss(card));
        app.drain_cards(EditorSnapshots::EMPTY);
        assert!(
            !surface
                .trace()
                .contains(&crate::gui::card::SurfaceCall::Dismiss(card)),
            "closing must not discard an explicit deferred Save"
        );
        app.opening_cards.insert(card);

        app.pipeline
            .inject_outcome_for_test(Outcome::Finalized(Box::new(FinalizedCapture {
                card,
                capture_id: None,
                actions: crate::after_capture::ExecutionReport::default(),
                written: vec!["/tmp/Scrozz Capture.png".to_owned()],
                retained_elsewhere: true,
                exported: true,
            })));
        app.drain_pipeline();

        assert!(!app.deferred_save.contains(&card));
        assert!(!app.card_has_outstanding_card_level_output(card));
        assert!(
            app.editor_only_cards.contains(&card),
            "automatic export may hide the card but must preserve its opening editor source"
        );
        assert!(
            surface
                .trace()
                .contains(&crate::gui::card::SurfaceCall::Dismiss(card)),
            "the explicit Save intent should consume the completed automatic export"
        );
    }

    #[test]
    fn auto_close_never_removes_the_only_retained_artifact() {
        let (mut app, surface) = app();
        let card = CardId(41);
        app.card_retention.insert(card, (false, false));
        surface.inject(CardEvent::AutoClose(
            card,
            RecentCapturesAutoCloseAction::Hide,
        ));

        app.tick();

        assert!(
            !surface
                .trace()
                .contains(&crate::gui::card::SurfaceCall::Dismiss(card))
        );
        assert!(
            app.notes()
                .iter()
                .any(|note| note.contains("only retained artifact"))
        );
    }

    #[test]
    fn auto_close_hides_a_card_that_history_retains() {
        let (mut app, surface) = app();
        let card = CardId(42);
        app.card_retention.insert(card, (true, false));
        surface.inject(CardEvent::AutoClose(
            card,
            RecentCapturesAutoCloseAction::Hide,
        ));

        app.tick();

        assert!(
            surface
                .trace()
                .contains(&crate::gui::card::SurfaceCall::Dismiss(card))
        );
    }

    #[test]
    fn auto_close_rechecks_history_before_releasing_live_bytes() {
        let (mut app, surface) = app();
        let card = CardId(45);
        app.card_retention.insert(card, (true, false));
        app.card_capture_ids
            .insert(card, CaptureId("already-evicted".into()));
        surface.inject(CardEvent::AutoClose(
            card,
            RecentCapturesAutoCloseAction::Hide,
        ));

        app.tick();
        for _ in 0..100 {
            if !app.pending_retention_close.contains(&card) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
            app.tick();
        }

        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a stale history identity must not release the only live pixels"
        );
        assert_eq!(app.card_retention.get(&card), Some(&(false, false)));
        assert!(!app.pending_retention_close.contains(&card));
    }

    #[test]
    fn capacity_overflow_releases_unreachable_bytes_when_recovery_fails() {
        let (mut app, surface) = app();
        let card = CardId(46);
        app.card_retention.insert(card, (true, false));
        app.card_capture_ids
            .insert(card, CaptureId("overflow-evicted".into()));
        surface.inject(CardEvent::Overflow(card));

        app.tick();
        for _ in 0..200 {
            if !app.pending_retention_overflow.contains(&card)
                && !app.card_has_outstanding_card_level_output(card)
                && !app.overflow_recovery_in_flight.contains(&card)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
            app.tick();
        }

        assert!(
            surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "an overflowed card with a failed recovery is unreachable and must not leak"
        );
        assert!(!app.card_retention.contains_key(&card));
        assert!(!app.pending_retention_overflow.contains(&card));
        assert!(!app.card_has_outstanding_card_level_output(card));
        assert!(!app.overflow_recovery_in_flight.contains(&card));
    }

    #[test]
    fn save_to_export_location_reuses_an_after_capture_export() {
        let (mut app, surface) = app();
        let card = CardId(43);
        app.card_retention.insert(card, (true, true));
        surface.inject(CardEvent::Save {
            card,
            choose_destination: false,
        });

        app.tick();

        assert!(surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert!(
            app.notes()
                .iter()
                .any(|note| note.contains("already saved to Export Location"))
        );
    }

    #[test]
    fn save_does_not_reuse_an_old_export_while_the_card_is_being_edited() {
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(47);
        app.card_retention.insert(card, (true, true));
        surface.inject(CardEvent::Save {
            card,
            choose_destination: false,
        });

        app.tick_with_editor(EditorSnapshots::new(std::slice::from_ref(
            &EditorSnapshot::new(card, 1, &editor),
        )));

        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "an old export must not discard the active editor revision"
        );
        assert!(
            app.card_has_outstanding_card_level_output(card),
            "the edited revision should be queued and close only after success"
        );
    }

    #[test]
    fn automatic_close_waits_while_the_card_editor_is_open() {
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(48);
        app.card_retention.insert(card, (true, true));
        surface.inject(CardEvent::AutoClose(
            card,
            RecentCapturesAutoCloseAction::SaveThenHide,
        ));

        app.drain_cards(EditorSnapshots::new(std::slice::from_ref(
            &EditorSnapshot::new(card, 1, &editor),
        )));

        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert!(
            app.notes()
                .iter()
                .any(|note| note.contains("editor is open"))
        );
        assert_eq!(
            app.deferred_auto_close.get(&card),
            Some(&RecentCapturesAutoCloseAction::SaveThenHide)
        );

        app.editor_closed(EditorSnapshot::new(card, 1, &editor), true);

        assert!(!app.deferred_auto_close.contains_key(&card));
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "an export that predates the editor must not stand in for its edits"
        );
        assert!(
            app.card_has_outstanding_card_level_output(card),
            "the revision the editor ended on must be exported before the card closes"
        );
    }

    #[test]
    fn opening_a_card_and_its_expiry_landing_the_same_frame_defers_to_the_open() {
        let (mut app, surface) = app();
        let card = CardId(69);

        // Deadline race: the dwell timer expires in the exact same batch as
        // the click that opens the card. `entry.editing` cannot yet be true
        // for a decode that only just got queued (it flips on the async
        // `Command::SetEditing` round trip), so the overlay's own same-frame
        // exclusion can't see it either. Without deferring on
        // `opening_cards` this would auto-close (and, for `SaveThenHide`,
        // export) the card the same frame its editor is starting.
        surface.inject(CardEvent::Open(card));
        surface.inject(CardEvent::AutoClose(
            card,
            RecentCapturesAutoCloseAction::SaveThenHide,
        ));

        app.drain_cards(EditorSnapshots::EMPTY);

        assert!(
            app.opening_cards.contains(&card),
            "the open must still proceed"
        );
        assert_eq!(
            app.deferred_auto_close.get(&card),
            Some(&RecentCapturesAutoCloseAction::SaveThenHide),
            "the same-frame expiry must defer rather than race the open"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a card must not be dismissed the same frame its editor starts opening"
        );
        assert!(
            !app.card_has_outstanding_card_level_output(card),
            "a deferred expiry must not export before the editor the user just opened exists"
        );
    }

    #[test]
    fn a_fresh_open_marks_the_card_opening_and_pauses_its_timer() {
        let (mut app, surface) = app();
        let card = CardId(70);
        surface.inject(CardEvent::Open(card));

        app.drain_cards(EditorSnapshots::EMPTY);

        assert!(
            app.opening_cards.contains(&card),
            "a fresh open must be tracked until the decode produces a live editor"
        );
        assert!(
            surface.trace().contains(&SurfaceCall::SetEditing {
                id: card,
                editing: true
            }),
            "the timer must pause optimistically before the decode even finishes"
        );
        assert_eq!(
            app.take_focus_editor_request(),
            None,
            "a fresh open is not a duplicate and must not raise anything"
        );
    }

    #[test]
    fn a_second_open_while_the_first_is_still_decoding_focuses_instead_of_reopening() {
        let (mut app, surface) = app();
        let card = CardId(71);
        surface.inject(CardEvent::Open(card));
        app.drain_cards(EditorSnapshots::EMPTY);
        assert!(app.opening_cards.contains(&card));
        assert_eq!(app.take_focus_editor_request(), None);

        // The decode for `card` has not produced an editor yet (still no
        // snapshot), but something clicks the card again -- or a menu's Edit
        // action fires -- before it lands.
        surface.inject(CardEvent::Open(card));
        app.drain_cards(EditorSnapshots::EMPTY);

        assert_eq!(
            app.take_focus_editor_request(),
            Some(card),
            "a duplicate open mid-decode must raise the one editor being opened, not start a second"
        );
        assert_eq!(
            surface
                .trace()
                .iter()
                .filter(|call| **call
                    == SurfaceCall::SetEditing {
                        id: card,
                        editing: true
                    })
                .count(),
            1,
            "the timer must only be paused once for the one decode in flight"
        );
    }

    #[test]
    fn opening_an_already_open_card_focuses_the_existing_editor_without_a_second_decode() {
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(72);
        let snapshot = EditorSnapshot::new(card, 1, &editor);
        let editors = EditorSnapshots::new(std::slice::from_ref(&snapshot));

        // The card's editor is already open and live this frame (its decode
        // finished on an earlier tick); the click that opens it again -- the
        // whole-card click, Enter/Space, or an Edit menu action -- must raise
        // it, never queue a second decode.
        surface.inject(CardEvent::Open(card));
        app.drain_cards(editors);

        assert_eq!(
            app.take_focus_editor_request(),
            Some(card),
            "opening an already-open card must raise its existing editor"
        );
        assert!(
            !app.opening_cards.contains(&card),
            "an already-open card was never queued for decode and must not become so now"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::SetEditing {
                id: card,
                editing: true
            }),
            "the timer is already paused for an open editor; opening it again must not re-pause it"
        );
    }

    #[test]
    fn a_refused_open_enqueue_leaves_the_card_closed_instead_of_stuck_editing() {
        let (mut app, surface) = app();
        let card = CardId(73);

        // Simulate "the capture worker has gone": stop the pipeline first, so
        // its `post` can no longer reach a receiver and every subsequent
        // enqueue is refused, exactly like a worker that has already exited.
        app.pipeline.stop();

        surface.inject(CardEvent::Open(card));
        app.drain_cards(EditorSnapshots::EMPTY);

        assert!(
            !app.opening_cards.contains(&card),
            "a refused enqueue must not leave the card marked as opening forever"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::SetEditing {
                id: card,
                editing: true
            }),
            "a refused enqueue must not pause the card's auto-close timer with no editor to show for it"
        );
        assert!(
            app.notes()
                .iter()
                .any(|note| note.contains("could not be queued for Open Editor")),
            "the failure must be surfaced, not silently swallowed: {:?}",
            app.notes()
        );
    }

    #[test]
    fn two_cards_opening_at_once_are_tracked_and_focused_independently() {
        let (mut app, surface) = app();
        let a = CardId(73);
        let b = CardId(74);
        surface.inject(CardEvent::Open(a));
        surface.inject(CardEvent::Open(b));

        app.drain_cards(EditorSnapshots::EMPTY);

        assert!(app.opening_cards.contains(&a));
        assert!(app.opening_cards.contains(&b));
        assert_eq!(app.take_focus_editor_request(), None);

        // `a`'s decode has since finished (it now has a live editor); `b`'s
        // has not. A duplicate open arrives for each in the same pass. Using
        // `drain_cards` directly (rather than a full `tick_with_editor`) keeps
        // this test isolated to card-dedup routing: a full tick would also
        // drain the pipeline and try to service the `Job::Open` jobs posted
        // above, which is a different concern with its own coverage.
        let editor = redacted_editor();
        let snapshot_a = EditorSnapshot::new(a, 1, &editor);
        let editors = EditorSnapshots::new(std::slice::from_ref(&snapshot_a));
        surface.inject(CardEvent::Open(a));
        surface.inject(CardEvent::Open(b));

        app.drain_cards(editors);

        assert!(
            app.opening_cards.contains(&b),
            "`b` is still mid-decode and must remain tracked"
        );
        // Both duplicates must be queued to focus, each exactly once, and
        // neither card's routing may bleed into the other's.
        let mut focused = Vec::new();
        while let Some(card) = app.take_focus_editor_request() {
            focused.push(card);
        }
        assert_eq!(focused, vec![a, b]);
    }

    #[test]
    fn a_decode_in_flight_stops_being_tracked_once_its_editor_goes_live() {
        let (mut app, _surface) = app();
        let a = CardId(200);
        let b = CardId(201);
        // Simulate the state a real tick would reach after two opens were
        // posted for decode: both tracked as in-flight, no editor yet.
        app.opening_cards.insert(a);
        app.opening_cards.insert(b);

        // `a`'s decode has landed and produced a live editor this tick;
        // `b`'s has not.
        let editor = redacted_editor();
        let snapshot_a = EditorSnapshot::new(a, 1, &editor);
        let editors = EditorSnapshots::new(std::slice::from_ref(&snapshot_a));

        app.tick_with_editor(editors);

        assert!(
            !app.opening_cards.contains(&a),
            "a card with a live editor this tick must fall out of the decode-in-flight set"
        );
        assert!(
            app.opening_cards.contains(&b),
            "a card with no editor yet must remain tracked as still decoding"
        );
    }

    #[test]
    fn closing_one_cards_editor_leaves_a_second_open_editor_untouched() {
        let (mut app, surface) = app();
        let editor_a = redacted_editor();
        let editor_b = redacted_editor();
        let a = CardId(75);
        let b = CardId(76);
        app.card_retention.insert(a, (true, true));
        app.card_retention.insert(b, (true, true));
        surface.inject(CardEvent::AutoClose(a, RecentCapturesAutoCloseAction::Hide));
        surface.inject(CardEvent::AutoClose(b, RecentCapturesAutoCloseAction::Hide));

        let snapshot_a = EditorSnapshot::new(a, 1, &editor_a);
        let snapshot_b = EditorSnapshot::new(b, 1, &editor_b);
        let both = [snapshot_a, snapshot_b];
        app.drain_cards(EditorSnapshots::new(&both));

        assert_eq!(
            app.deferred_auto_close.get(&a),
            Some(&RecentCapturesAutoCloseAction::Hide)
        );
        assert_eq!(
            app.deferred_auto_close.get(&b),
            Some(&RecentCapturesAutoCloseAction::Hide)
        );

        // Closing `a`'s editor must resolve only `a`'s deferred state, resume
        // only `a`'s timer, and leave `b` -- still open -- completely alone.
        app.editor_closed(EditorSnapshot::new(a, 1, &editor_a), true);

        assert!(!app.deferred_auto_close.contains_key(&a));
        assert!(
            app.deferred_auto_close.contains_key(&b),
            "closing one card's editor must not resolve a still-open card's deferred auto-close"
        );
        assert!(surface.trace().contains(&SurfaceCall::Dismiss(a)));
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(b)),
            "`b` is still being edited and must not be dismissed by `a`'s close"
        );
        assert!(
            surface.trace().contains(&SurfaceCall::SetEditing {
                id: a,
                editing: false
            }),
            "closing `a`'s editor must resume `a`'s timer"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::SetEditing {
                id: b,
                editing: false
            }),
            "`b`'s editor is still open; its timer must stay paused"
        );
    }

    #[test]
    fn a_deferred_hide_retires_the_card_once_its_editor_closes() {
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(52);
        app.card_retention.insert(card, (true, true));
        surface.inject(CardEvent::AutoClose(
            card,
            RecentCapturesAutoCloseAction::Hide,
        ));

        app.drain_cards(EditorSnapshots::new(std::slice::from_ref(
            &EditorSnapshot::new(card, 1, &editor),
        )));

        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert_eq!(
            app.deferred_auto_close.get(&card),
            Some(&RecentCapturesAutoCloseAction::Hide)
        );

        app.editor_closed(EditorSnapshot::new(card, 1, &editor), true);

        assert!(surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert!(!app.deferred_auto_close.contains_key(&card));
    }

    #[test]
    fn a_committed_done_resets_stale_retention_so_the_next_overflow_re_exports() {
        // Finding #1 (round 4): `card_retention` is keyed per card, not per
        // revision. If a card had already been saved and exported once
        // before the user reopened it and made a destructive redaction, the
        // recorded `(retained, exported)` describes the pre-edit content --
        // trusting it after Done would let a later overflow dismiss the card
        // as "already exported" without ever saving the redacted revision.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(73);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("committed-retention".into()));
        app.card_retention.insert(card, (true, true));

        let rendered =
            RevisionedFrame::from_document(editor.document(), 2).expect("render edited revision");
        app.commit_card_output(card, 1, rendered, editor.document().data());

        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "a committed edit must invalidate retention recorded for the pre-edit content"
        );

        // The editor has since closed (Done always closes shortly after
        // committing); overflow now arrives with no live editor for the card.
        surface.inject(CardEvent::Overflow(card));
        app.drain_cards(EditorSnapshots::EMPTY);

        assert!(
            app.card_has_outstanding_card_level_output(card),
            "stale retention flags must not let overflow skip exporting the redacted \
             revision Done just committed -- only a fresh save, not pre-edit export \
             history, may let this card leave the display"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "the card must not be dismissed before the redacted revision is actually saved"
        );
    }

    /// Drains `app`'s pipeline until `ready` reports true or two seconds pass.
    ///
    /// The capture worker runs on a real background thread even in tests, so
    /// asserting on an outcome it produces needs a bounded poll rather than a
    /// single `drain_pipeline` call.
    fn drain_until(app: &mut App, mut ready: impl FnMut(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            app.drain_pipeline();
            if ready(app) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the worker never produced the outcome this test is waiting for"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A fresh, unique path in the OS temp directory for a `SaveImageTo` job.
    ///
    /// Writing to an explicit path -- rather than through `Job::SaveImage`'s
    /// configured-folder default, or `Job::CopyImage`'s system clipboard --
    /// keeps these tests deterministic and free of side effects on whatever
    /// real settings or pasteboard the test machine happens to have.
    fn temp_export_path(name: &str) -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "scrozz-gui-finding2-{name}-{}-{}.png",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn a_stale_save_completion_is_ignored_once_a_newer_revision_is_committed() {
        // Round 5, Finding #2: a Save dispatched against revision 1 of an
        // editor's document can complete only after Done has since committed
        // revision 2 for the very same editor generation (the capture worker
        // is a single FIFO thread, so the Save -- dispatched first -- is
        // always processed and its outcome emitted no later than the commit
        // that superseded it, but that outcome is only *observed* here in a
        // later frame). Marking retention off the stale completion would let
        // an overflow or auto-close trust an export that in fact wrote the
        // pre-edit revision, not the one the thumbnail now shows committed.
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(201);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("stale-save".into()));
        app.card_retention.insert(card, (false, false));

        let generation = 1;
        let stale_path = temp_export_path("stale-save");
        let stale_rendered =
            RevisionedFrame::from_document(editor.document(), 1).expect("render revision 1");
        let action = app.allocate_output_action();
        assert!(app.pipeline.post(Job::SaveImageTo {
            card,
            generation,
            rendered: Box::new(stale_rendered),
            path: stale_path.clone(),
            action,
        }));
        // Every real Save dispatch site registers an outstanding action
        // alongside its own `pipeline.post` call; posting directly here
        // skips that, so it is registered explicitly to keep this
        // still-in-flight Save's own fate alive through the commit below
        // rather than letting round 11's pruning (nothing else is
        // outstanding for this card yet) erase it before this stale
        // completion ever gets a chance to consult it.
        app.register_output_action(card, action, OutputActionKind::CardOutput, true);

        // Done commits revision 2 for the same generation immediately after
        // dispatching the stale Save above -- exactly the ordering the
        // finding describes.
        let committed = RevisionedFrame::from_document(editor.document(), 2)
            .expect("render committed revision 2");
        app.commit_card_output(card, generation, committed, editor.document().data());
        assert_eq!(
            app.card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&generation)),
            Some(&GenerationFate::Committed(2)),
            "the commit must be recorded synchronously, before the stale Save's own \
             completion is ever drained"
        );

        drain_until(&mut app, |app| {
            app.card_retention.get(&card) == Some(&(false, false)) && stale_path.exists()
        });

        assert!(
            stale_path.exists(),
            "the stale save must still actually write its file -- it is stale for \
             retention purposes only, not silently dropped"
        );
        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "a completion for a since-superseded revision must never mark retention, \
             or a later overflow could treat the newly committed (and still \
             unexported) revision as already safely saved"
        );
        let _ = std::fs::remove_file(&stale_path);
    }

    #[test]
    fn the_first_save_completion_of_a_fresh_editing_session_is_trusted() {
        // The counterpart to the stale case above: with no prior commit for
        // this card, `card_generation_fates` holds nothing to compare
        // against, so the very first Save/Copy/Upload completion of a session
        // must still be trusted -- otherwise every card's first export would
        // wrongly be treated as unproven.
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(202);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_retention.insert(card, (false, false));
        assert!(
            !app.card_generation_fates.contains_key(&card),
            "no Done has committed anything yet for this card"
        );

        let path = temp_export_path("first-save");
        let rendered =
            RevisionedFrame::from_document(editor.document(), 1).expect("render revision 1");
        let action = app.allocate_output_action();
        assert!(app.pipeline.post(Job::SaveImageTo {
            card,
            generation: 1,
            rendered: Box::new(rendered),
            path: path.clone(),
            action,
        }));
        // Round 13: `Outcome::Done` now validates this exact action is
        // still outstanding before trusting its completion at all -- every
        // real dispatch site registers alongside its own successful post,
        // so this direct-post test does the same.
        app.register_output_action(card, action, OutputActionKind::CardOutput, false);

        drain_until(&mut app, |app| {
            app.card_retention.get(&card) == Some(&(true, true))
        });

        assert_eq!(
            app.card_retention.get(&card),
            Some(&(true, true)),
            "a completion with nothing tracked to compare against must be trusted"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_second_commit_supersedes_the_first_monotonically() {
        // A second Done, committing a newer revision of the same editor
        // generation, must fully replace the tracked version rather than
        // merging with or being ignored in favor of the first -- otherwise a
        // completion for the *first* commit's revision could still be
        // (wrongly) trusted as current after a second, newer commit has since
        // superseded it.
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(206);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_retention.insert(card, (false, false));

        let generation = 1;
        let first = RevisionedFrame::from_document(editor.document(), 2)
            .expect("render first committed revision");
        // Mirrors a real dispatch site's own bookkeeping (see the matching
        // comment in `a_stale_save_completion_is_ignored_once_a_newer_revision_is_committed`)
        // so round 11's pruning does not erase this generation's fate
        // between the two commits below, before either completion this
        // test dispatches has had a chance to consult it.
        let action = app.allocate_output_action();
        app.register_output_action(card, action, OutputActionKind::CardOutput, true);
        app.commit_card_output(card, generation, first, editor.document().data());
        assert_eq!(
            app.card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&generation)),
            Some(&GenerationFate::Committed(2))
        );

        let second = RevisionedFrame::from_document(editor.document(), 3)
            .expect("render second, newer committed revision");
        app.commit_card_output(card, generation, second, editor.document().data());
        assert_eq!(
            app.card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&generation)),
            Some(&GenerationFate::Committed(3)),
            "the second commit must fully supersede the first, not merge with it"
        );

        // A completion for the now-superseded first revision must not mark
        // retention...
        let stale_path = temp_export_path("superseded-save");
        let stale = RevisionedFrame::from_document(editor.document(), 2)
            .expect("render the now-superseded revision");
        let action = app.allocate_output_action();
        assert!(app.pipeline.post(Job::SaveImageTo {
            card,
            generation,
            rendered: Box::new(stale),
            path: stale_path.clone(),
            action,
        }));
        // Round 13: registered so `Outcome::Done` trusts this action is
        // still outstanding, exactly as every real dispatch site does
        // alongside its own successful post.
        app.register_output_action(card, action, OutputActionKind::CardOutput, false);
        drain_until(&mut app, |app| stale_path.exists());
        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "a completion for the superseded first revision must be treated as stale"
        );
        let _ = std::fs::remove_file(&stale_path);

        // ...but a completion matching the latest, second revision must.
        // The stale Save above's own resolution already retired the marker
        // inserted before the first commit, and with nothing else
        // outstanding that pruned generation 1's fate outright (round 11) --
        // exactly as it should, since generation 1 has now fully settled.
        // This dispatch needs no marker of its own: an absent entry is
        // trusted as current by `output_version_is_stale`'s own `None`
        // branch, and never inserting into `close_after_output` here means
        // this completion's own resolution cannot wrongly dismiss the card.
        let current_path = temp_export_path("current-after-supersede-save");
        let current = RevisionedFrame::from_document(editor.document(), 3)
            .expect("render the latest committed revision");
        let action = app.allocate_output_action();
        assert!(app.pipeline.post(Job::SaveImageTo {
            card,
            generation,
            rendered: Box::new(current),
            path: current_path.clone(),
            action,
        }));
        // Round 13: registered for the same reason as the stale dispatch
        // above -- `Outcome::Done` now requires this exact action to still
        // be outstanding before it will trust the completion at all.
        app.register_output_action(card, action, OutputActionKind::CardOutput, false);
        drain_until(&mut app, |app| {
            app.card_retention.get(&card) == Some(&(true, true))
        });
        assert_eq!(
            app.card_retention.get(&card),
            Some(&(true, true)),
            "a completion matching the latest committed revision must be trusted"
        );
        let _ = std::fs::remove_file(&current_path);
    }

    #[test]
    fn a_save_completion_matching_the_exact_committed_revision_is_trusted() {
        // A Save dispatched *after* Done has committed the same revision (the
        // ordinary "commit, then export the result" order) must still mark
        // retention: the gate only distinguishes stale completions, not every
        // completion that happens to follow a commit.
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(203);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_retention.insert(card, (false, false));

        let generation = 1;
        let committed = RevisionedFrame::from_document(editor.document(), 4)
            .expect("render committed revision 4");
        app.commit_card_output(card, generation, committed, editor.document().data());
        // Round 11: nothing was ever dispatched against generation 1 before
        // it closed, so this freshly-recorded fate is pruned immediately --
        // correctly, since nothing outstanding could ever need to compare
        // itself against it. The Save below is a brand-new dispatch, never
        // one that raced this exact commit, so an absent entry (trusted by
        // `output_version_is_stale`'s own `None` branch) is exactly right.
        assert!(
            !app.card_generation_fates.contains_key(&card),
            "a fate nothing is outstanding against is pruned as soon as it is recorded"
        );

        let path = temp_export_path("current-save");
        let rendered = RevisionedFrame::from_document(editor.document(), 4)
            .expect("render the exact committed revision");
        let action = app.allocate_output_action();
        assert!(app.pipeline.post(Job::SaveImageTo {
            card,
            generation,
            rendered: Box::new(rendered),
            path: path.clone(),
            action,
        }));
        // Round 13: registered so `Outcome::Done` trusts this action is
        // still outstanding, exactly as every real dispatch site does
        // alongside its own successful post.
        app.register_output_action(card, action, OutputActionKind::CardOutput, false);

        drain_until(&mut app, |app| {
            app.card_retention.get(&card) == Some(&(true, true))
        });

        assert_eq!(
            app.card_retention.get(&card),
            Some(&(true, true)),
            "a completion for exactly the committed revision must mark retention"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reopening_a_card_does_not_erase_an_older_generations_recorded_fate() {
        // Round 10, Finding #1: earlier rounds cleared *every* fate recorded
        // for a card the instant any new editor opened for it. That erased
        // an older, not-yet-fully-resolved generation's own tombstone before
        // every action dispatched against it had actually settled -- a late
        // completion for that older generation then found nothing left to
        // compare itself against and was wrongly trusted as though it
        // answered whichever generation the map happened to describe
        // instead. A per-generation record must survive a newer editor
        // opening; only that exact generation's own later resolution, or
        // the card's own final retirement, may ever remove it.
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(204);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_generation_fates
            .entry(card)
            .or_default()
            .insert(99, GenerationFate::Committed(7));

        assert!(app.pipeline.post(Job::Open(card)));
        drain_until(&mut app, |app| app.next_editor_generation != 1);

        assert_eq!(
            app.card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&99)),
            Some(&GenerationFate::Committed(7)),
            "opening a new editing session must not erase an older, unrelated \
             generation's own recorded fate"
        );
        assert!(
            !app.output_version_is_stale(card, Some((1, 1))),
            "the freshly opened generation's own first completion has no \
             entry yet and must still be trusted, unaffected by an older \
             generation's fate"
        );
    }

    #[test]
    fn cancel_gen1_then_open_gen2_a_late_gen1_upload_completion_is_still_recognised_stale() {
        // Round 10, Finding #1's central regression: Cancel records
        // generation 1's own fate, then a brand-new editing session --
        // generation 2 -- opens for the very same card before generation
        // 1's own already-dispatched upload has completed. Before this
        // fix, opening generation 2 would have scrubbed generation 1's
        // record outright, so this late completion would have found
        // nothing to compare itself against and been wrongly trusted.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(230);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("cancel-gen1-open-gen2".into()));
        app.card_retention.insert(card, (false, false));

        // Generation 1 is cancelled -- its own already-dispatched upload
        // (registered below with its own action id) is still in flight.
        // Round 12: recorded here, before the Cancel, exactly as
        // `dispatch_upload_action` would have registered it at the moment
        // that upload was actually dispatched -- this is what keeps the
        // Cancelled fate below from being pruned immediately for having
        // nothing tracked as outstanding yet.
        app.register_output_action(card, 7, OutputActionKind::Upload, true);
        app.editor_closed(EditorSnapshot::new(card, 1, &editor), false);
        assert_eq!(
            app.card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&1)),
            Some(&GenerationFate::Cancelled)
        );

        // A brand-new editing session -- generation 2 -- now opens for the
        // same card.
        assert!(app.pipeline.post(Job::Open(card)));
        drain_until(&mut app, |app| app.next_editor_generation != 1);
        assert_eq!(
            app.card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&1)),
            Some(&GenerationFate::Cancelled),
            "opening generation 2 must never erase generation 1's own \
             recorded fate"
        );

        // Generation 1's own upload -- registered above with its own
        // action id, naming the exact action generation 2 has not itself
        // superseded -- now completes late.
        app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
            card,
            detail: "uploaded and copied the private share link".to_owned(),
            version: Some((1, 3)),
            action: 7,
        });
        app.drain_pipeline();

        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "the stale upload's own bookkeeping must still retire"
        );
        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "a late completion for a cancelled generation must never mark \
             retention, even after a newer generation has since opened"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a late completion for a cancelled generation must never dismiss \
             the card, even after a newer generation has since opened"
        );
    }

    #[test]
    fn multiple_cancelled_generations_each_retain_their_own_independent_fate() {
        // Round 10, Finding #1: a second (or third) Cancel must not
        // silently overwrite an earlier generation's own record -- every
        // editing session a card has been through keeps its own
        // independent tombstone.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(231);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("multi-cancel".into()));
        app.card_retention.insert(card, (false, false));

        // Round 12: one outstanding upload action per generation, keyed by
        // its own generation number, registered before each Cancel below
        // exactly as each would have been at its own real dispatch time --
        // keeps all three tombstones alive simultaneously until each
        // generation's own late completion resolves later in this test,
        // rather than the first (or second) Cancel's own prune check
        // erasing the others for having nothing left tracked yet.
        for generation in [1_u64, 2, 3] {
            app.register_output_action(card, generation, OutputActionKind::Upload, true);
        }
        app.editor_closed(EditorSnapshot::new(card, 1, &editor), false);
        app.editor_closed(EditorSnapshot::new(card, 2, &editor), false);
        app.editor_closed(EditorSnapshot::new(card, 3, &editor), false);

        for generation in [1_u64, 2, 3] {
            assert_eq!(
                app.card_generation_fates
                    .get(&card)
                    .and_then(|fates| fates.get(&generation)),
                Some(&GenerationFate::Cancelled),
                "generation {generation}'s own Cancelled fate must survive \
                 every later generation's own cancellation"
            );
        }

        // A late completion naming any of the three discarded generations
        // must still be recognised as stale, not merely the most recent one.
        for generation in [1_u64, 2, 3] {
            // Re-registering (idempotent -- the action id and kind are
            // unchanged) makes this generation's own action the card's
            // *current* upload again immediately before its completion
            // arrives below, exactly mirroring the pre-round-12
            // `pending_upload.insert` right before each injected
            // completion.
            app.register_output_action(card, generation, OutputActionKind::Upload, true);
            app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
                card,
                detail: "uploaded and copied the private share link".to_owned(),
                version: Some((generation, 1)),
                action: generation,
            });
            app.drain_pipeline();
            assert_eq!(
                app.card_retention.get(&card),
                Some(&(false, false)),
                "a late completion for cancelled generation {generation} must \
                 never mark retention"
            );
        }
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "none of the three late, stale completions may ever dismiss the card"
        );
    }

    #[test]
    fn repeated_editor_only_sessions_never_leak_generation_fates() {
        // Round 11: an editor-only card's `CardId` is never reused, so the
        // pre-fix leak this finding describes was not one card's map entry
        // growing without bound but every editor-only session the app had
        // ever hosted (each its own history-less card) leaving its own
        // never-cleared entry behind, forever, in the same shared
        // `card_generation_fates` map -- because releasing such a card goes
        // through `editor_closed`'s `Job::Release` path, never
        // `dismiss_recent_capture`, which was previously the *only* place
        // that ever cleared it. Every ordinary Cancel with nothing
        // outstanding must prune its own tombstone immediately, so many
        // such sessions in a row must never accumulate map entries.
        let (mut app, _surface) = app();
        let editor = redacted_editor();

        for i in 0..5_u64 {
            let card = CardId(240 + i);
            app.pipeline
                .captures()
                .store_test_capture(card, editor.document().source())
                .expect("editor source");
            app.editor_only_cards.insert(card);
            assert!(
                !app.card_capture_ids.contains_key(&card),
                "an editor-only card has no durable history row behind it"
            );

            // Nothing dispatched against this generation before the Cancel
            // below -- the ordinary case, and the one this finding says
            // must not leak.
            app.editor_closed(EditorSnapshot::new(card, 1, &editor), false);

            assert!(
                app.card_generation_fates.is_empty(),
                "session {i}'s own editor-only Cancel must leave the shared \
                 generation-fate map completely empty, not merely absent for \
                 this one card, proving no session before it left anything \
                 behind either"
            );
            assert!(
                !app.editor_only_cards.contains(&card),
                "editor_closed must retire the editor-only card it just released"
            );
        }
    }

    #[test]
    fn a_generations_fate_survives_until_its_last_outstanding_action_resolves_then_prunes() {
        // Round 11: a generation with more than one action still in flight
        // when its editor closes must keep its fate recorded until *every*
        // one of those actions has resolved -- pruning as soon as the first
        // of several resolves would let a still-outstanding second action's
        // completion find nothing left to compare itself against and be
        // wrongly trusted as current.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(233);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("two-outstanding-actions".into()));
        app.card_retention.insert(card, (false, false));

        // Two independently-tracked outstanding actions against the same
        // still-open generation: a Save and an Upload, both dispatched
        // before the Cancel below with their own unique action ids, exactly
        // as two real dispatch sites would have left them (round 12: each
        // is now its own entry in `outstanding_output_actions`, not a flat
        // bit/counter shared across every kind).
        let save_action = app.allocate_output_action();
        app.register_output_action(card, save_action, OutputActionKind::CardOutput, true);
        let upload_action = app.allocate_output_action();
        app.register_output_action(card, upload_action, OutputActionKind::Upload, false);
        app.editor_closed(EditorSnapshot::new(card, 1, &editor), false);
        assert_eq!(
            app.card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&1)),
            Some(&GenerationFate::Cancelled),
            "the fate must survive the Cancel while two actions are still outstanding"
        );

        // The Save resolves first -- a stale completion for the discarded
        // edit, since this generation was cancelled -- settling one of the
        // two outstanding actions. The Upload is still outstanding, so the
        // fate must still survive.
        app.pipeline.inject_outcome_for_test(Outcome::Done {
            card,
            detail: "saved to disk".to_owned(),
            version: Some((1, 1)),
            action: save_action,
        });
        app.drain_pipeline();
        assert_eq!(
            app.card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&1)),
            Some(&GenerationFate::Cancelled),
            "one of two outstanding actions resolving must not prune the fate \
             while the other is still outstanding"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a completion for a cancelled generation must never dismiss the card"
        );

        // The Upload resolves last -- nothing is outstanding for this card
        // anymore, so the fate (and every other fate this card might still
        // be holding) may now finally be pruned.
        app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
            card,
            detail: "uploaded and copied the private share link".to_owned(),
            version: Some((1, 1)),
            action: upload_action,
        });
        app.drain_pipeline();
        assert!(
            !app.card_generation_fates.contains_key(&card),
            "the fate must finally be pruned once its last outstanding action resolves"
        );
    }

    #[test]
    fn dismissing_a_card_clears_its_recorded_generation_fates() {
        // Round 10, Finding #1: `card_generation_fates` must not silently
        // accumulate forever. Once a card is fully retired there is no
        // overlay card left for a late completion to race, and no fate
        // recorded here could ever again distinguish "stale" from "current"
        // for it.
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(232);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("dismiss-clears-fates".into()));
        app.card_retention.insert(card, (false, false));
        // Round 11: an outstanding upload, still unresolved, keeps this
        // generation's fate from being pruned by the Cancel below on its
        // own -- so the assertion right after it exercises the *ordinary*
        // survival case, and dismiss_recent_capture's own clear further
        // down proves it wipes the map unconditionally, even with an
        // outstanding action still recorded, not merely once nothing is
        // left tracked.
        let upload_action = app.allocate_output_action();
        app.register_output_action(card, upload_action, OutputActionKind::Upload, false);
        app.editor_closed(EditorSnapshot::new(card, 1, &editor), false);
        assert!(app.card_generation_fates.contains_key(&card));

        app.dismiss_recent_capture(card, "test retirement");

        assert!(
            !app.card_generation_fates.contains_key(&card),
            "a fully retired card must not leave any generation fates behind"
        );
        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "a fully retired card must not leave any outstanding upload action behind either"
        );
    }

    #[test]
    fn a_failed_commit_clears_the_provisional_version_so_a_later_save_in_the_same_session_is_trusted()
     {
        // If the dispatch-time write in `commit_card_output` were left in
        // place after the commit itself failed, it would permanently
        // describe a revision that was never actually filed -- Finding #1
        // keeps the editor open (or reopens it) after such a failure so the
        // user can retry or continue editing, and a legitimate subsequent
        // Save for a later revision in that same still-open generation must
        // not be wrongly treated as stale against a commit that never
        // actually happened.
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(205);
        // Deliberately not stored in the vault: `commit_rendered` then
        // reports "already left the vault", giving a deterministic,
        // network-free `CardOutputCommitFailed`.
        let generation = 1;
        let attempted = RevisionedFrame::from_document(editor.document(), 5)
            .expect("render the attempted commit");
        // Round 11: protects the optimistic write below from being pruned
        // before the assertion right after it ever runs -- nothing else is
        // tracked as outstanding for this card yet.
        let action = app.allocate_output_action();
        app.register_output_action(card, action, OutputActionKind::CardOutput, true);
        app.commit_card_output(card, generation, attempted, editor.document().data());
        assert_eq!(
            app.card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&generation)),
            Some(&GenerationFate::Committed(5)),
            "the optimistic dispatch-time write happens before the worker can report \
             the commit failed"
        );
        // The marker above was only ever a stand-in for a real dispatch
        // protecting the assertion just made; nothing in this test actually
        // dispatched a Copy/Save through it, so it must not linger into the
        // legitimate Save below -- left in place, that Save's own
        // (non-stale) completion would find its own action's entry
        // unrelated to this stand-in, but the stand-in must still be
        // resolved so it does not itself count as still outstanding.
        app.resolve_output_action(card, action);

        drain_until(&mut app, |app| {
            !app.card_generation_fates.contains_key(&card)
        });
        assert_eq!(
            app.card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&generation)),
            None,
            "a failed commit must clear the provisional fate it optimistically \
             recorded, not leave it describing a revision that was never filed"
        );

        // Now the user continues editing in the same still-open generation
        // and saves a later revision -- this must be trusted, not compared
        // against the failed attempt's stale bookkeeping.
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_retention.insert(card, (false, false));
        let path = temp_export_path("post-failure-save");
        let rendered = RevisionedFrame::from_document(editor.document(), 6)
            .expect("render the later revision");
        let action = app.allocate_output_action();
        assert!(app.pipeline.post(Job::SaveImageTo {
            card,
            generation,
            rendered: Box::new(rendered),
            path: path.clone(),
            action,
        }));
        // Round 13: registered so `Outcome::Done` trusts this action is
        // still outstanding, exactly as every real dispatch site does
        // alongside its own successful post.
        app.register_output_action(card, action, OutputActionKind::CardOutput, false);

        drain_until(&mut app, |app| {
            app.card_retention.get(&card) == Some(&(true, true))
        });
        assert_eq!(
            app.card_retention.get(&card),
            Some(&(true, true)),
            "a legitimate save after a failed commit must still be trusted"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_stale_save_completion_never_dismisses_a_card_holding_a_newer_committed_revision() {
        // Round 6, Finding #1: the existing stale-Save test above proves
        // retention is left alone, but a stale completion that was also the
        // action an overflow/auto-close dismissal was waiting behind is a
        // stronger and separate hazard -- `complete_output_action`
        // unconditionally dismisses the card once `close_after_output`
        // clears, and the card's live vault entry now holds a *newer*
        // revision than this stale action ever knew about. Dismissing here
        // would release that newer, still-live revision to make room for an
        // action that answered for the one before it.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(220);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("stale-save-dismiss".into()));
        app.card_retention.insert(card, (false, false));
        // Simulates an auto-close/overflow dismissal already waiting for
        // this exact Save to answer -- the editor that produced the newer
        // revision below has, in the scenario the finding describes, since
        // closed, so nothing else stands between this stale completion and
        // `complete_output_action`'s unconditional dismissal.
        let action = app.allocate_output_action();
        app.register_output_action(card, action, OutputActionKind::CardOutput, true);

        let generation = 1;
        let stale_path = temp_export_path("stale-save-dismiss");
        let stale_rendered =
            RevisionedFrame::from_document(editor.document(), 1).expect("render revision 1");
        assert!(app.pipeline.post(Job::SaveImageTo {
            card,
            generation,
            rendered: Box::new(stale_rendered),
            path: stale_path.clone(),
            action,
        }));

        let committed = RevisionedFrame::from_document(editor.document(), 2)
            .expect("render committed revision 2");
        app.commit_card_output(card, generation, committed, editor.document().data());

        drain_until(&mut app, |app| {
            !app.card_has_outstanding_card_level_output(card)
        });

        assert!(
            !app.card_has_outstanding_card_level_output(card),
            "the stale completion's own close-after-output bookkeeping must still \
             retire, or a later, legitimate completion could double-dismiss"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a stale Save completion must never dismiss a card now holding a newer \
             committed revision -- that would release live bytes the stale action \
             never exported"
        );
        let _ = std::fs::remove_file(&stale_path);
    }

    #[test]
    fn a_stale_copy_completion_never_dismisses_a_card_holding_a_newer_committed_revision() {
        // Round 6, Finding #1 (copy side): `Job::CopyImage` completes through
        // the same `Outcome::Done` as Save, so the identical race applies --
        // see the Save-side test above for the full explanation.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(221);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("stale-copy-dismiss".into()));
        app.card_retention.insert(card, (false, false));
        let action = app.allocate_output_action();
        app.register_output_action(card, action, OutputActionKind::CardOutput, true);

        let generation = 1;
        let stale_rendered =
            RevisionedFrame::from_document(editor.document(), 1).expect("render revision 1");
        assert!(app.pipeline.post(Job::CopyImage {
            card,
            generation,
            rendered: Box::new(stale_rendered),
            action,
        }));

        let committed = RevisionedFrame::from_document(editor.document(), 2)
            .expect("render committed revision 2");
        app.commit_card_output(card, generation, committed, editor.document().data());

        drain_until(&mut app, |app| {
            !app.card_has_outstanding_card_level_output(card)
        });

        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "a stale Copy completion must never mark retention for the pre-edit revision"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a stale Copy completion must never dismiss a card now holding a newer \
             committed revision"
        );
    }

    #[test]
    fn a_stale_upload_completion_never_dismisses_a_card_holding_a_newer_committed_revision() {
        // Round 6, Finding #1 (upload side): a real successful upload cannot
        // be produced hermetically in this test binary -- `cloud` is not
        // part of the default feature set, so the real upload worker's
        // `share_artifact` call always refuses here. `inject_outcome_for_test`
        // stands in for the async upload worker's own answer, carrying the
        // same `version` a real completion would, so this exercises the
        // exact `Outcome::UploadDone` handling path a genuine stale share
        // completion would race.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(222);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("stale-upload-dismiss".into()));
        app.card_retention.insert(card, (false, false));
        // Round 12: a single registration replaces the previous
        // `pending_upload` (action-identity) / `card_pending_uploads`
        // (outstanding counter) pair -- this action's own entry now serves
        // both roles: the stale-completion check below races its exact id,
        // and keeps this generation's fate from being pruned immediately.
        app.register_output_action(card, 0, OutputActionKind::Upload, true);

        let generation = 1;
        let committed = RevisionedFrame::from_document(editor.document(), 2)
            .expect("render committed revision 2");
        app.commit_card_output(card, generation, committed, editor.document().data());
        assert_eq!(
            app.card_generation_fates
                .get(&card)
                .and_then(|fates| fates.get(&generation)),
            Some(&GenerationFate::Committed(2))
        );
        let trace_before = surface.trace();
        let notes_before = app.notes().to_vec();

        app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
            card,
            detail: "uploaded and copied the private share link".to_owned(),
            // Stale: answers for revision 1, the generation's pre-commit
            // revision, dispatched before the commit above landed.
            version: Some((generation, 1)),
            // Matches the registered action above exactly, so this
            // exercises the revision-staleness branch rather than the
            // action-identity branch that now runs before it.
            action: 0,
        });
        app.drain_pipeline();

        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "the stale upload's own bookkeeping must still retire"
        );
        assert!(!app.current_upload_action.contains_key(&card));
        assert_eq!(surface.trace(), trace_before);
        assert_eq!(app.notes(), notes_before.as_slice());
        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "a stale upload completion must never mark retention for the pre-edit \
             revision"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a stale upload completion must never dismiss a card now holding a newer \
             committed revision"
        );
    }

    #[test]
    fn a_cancelled_editors_stale_save_completion_never_marks_a_reverted_card_retained() {
        // Round 9, Finding #1: a Save dispatched against a live editor's
        // uncommitted revision can still be in flight when the user Cancels
        // that exact editing session, with no Done ever having committed
        // anything for it to disagree with -- `card_generation_fates` has no
        // entry for this card at all the whole time. Before this fix,
        // `output_version_is_stale` only checked for a *different* committed
        // revision, so this exact scenario -- a save answering a discarded
        // edit that was never superseded, only abandoned -- passed straight
        // through as though it answered the (untouched) pre-edit bytes.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(225);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("cancelled-save-stale".into()));
        app.card_retention.insert(card, (false, false));
        let action = app.allocate_output_action();
        app.register_output_action(card, action, OutputActionKind::CardOutput, true);
        assert!(
            !app.card_generation_fates.contains_key(&card),
            "nothing was ever committed for this generation to compare the stale \
             completion against"
        );

        let generation = 1;
        // Cancel: the editing session at generation 1 is discarded before
        // its already-dispatched Save's completion has drained.
        app.editor_closed(EditorSnapshot::new(card, generation, &editor), false);

        app.pipeline.inject_outcome_for_test(Outcome::Done {
            card,
            detail: "saved to Export Location".to_owned(),
            // Stale: answers for the exact generation Cancel just discarded.
            version: Some((generation, 1)),
            action,
        });
        app.drain_pipeline();

        assert!(
            !app.card_has_outstanding_card_level_output(card),
            "the cancelled edit's own close-after-output bookkeeping must still retire"
        );
        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "a stale completion for a cancelled edit must never mark the card's \
             untouched pre-edit bytes as saved/exported"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a stale completion for a cancelled edit must never dismiss the card"
        );
    }

    #[test]
    fn a_duplicate_done_for_an_already_resolved_action_never_re_marks_retention_after_its_fate_prunes()
     {
        // Round 13: `Outcome::Done` previously set the status line, logged a
        // note, and read `output_version_is_stale` -- all before ever
        // checking whether `action` was still outstanding at all. A same-
        // generation stale completion (every test above) is always caught
        // by that staleness check, because *something* committed or
        // cancelled after it leaves a fate behind to disagree with it. A
        // genuinely duplicate delivery of an already-resolved Done slips
        // past that guard in a way none of those tests exercise: once this
        // action was the card's only thing outstanding, resolving it also
        // let `prune_settled_generation_fates` erase every fate recorded
        // for this card -- so if a *newer* commit for the same generation
        // then lands with nothing else outstanding (pruning its own fresh
        // fate right back out in the same call), a later duplicate of the
        // original, already-resolved Done finds nothing left to compare its
        // `version` against and is wrongly trusted as current, exactly as
        // if it had never raced anything.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(226);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("duplicate-done".into()));
        app.card_retention.insert(card, (false, false));

        let generation = 1;
        let action = app.allocate_output_action();
        // `close_after: false` -- an ordinary Save/Copy dispatch must never
        // dismiss the card purely because it finished, keeping the card
        // around long enough for the later commit and its duplicate to
        // actually race.
        app.register_output_action(card, action, OutputActionKind::CardOutput, false);

        // The genuine, first completion of this exact action: not stale
        // (nothing has been committed or cancelled yet), so it is trusted
        // and marks retention.
        app.pipeline.inject_outcome_for_test(Outcome::Done {
            card,
            detail: "saved to Export Location".to_owned(),
            version: Some((generation, 1)),
            action,
        });
        app.drain_pipeline();
        assert_eq!(
            app.card_retention.get(&card),
            Some(&(true, true)),
            "the genuine, first completion of this action must mark retention"
        );
        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "the action must have fully resolved -- nothing else was ever \
             registered for this card"
        );

        // A newer revision of the same generation is committed with
        // nothing outstanding for `card` at all -- `commit_card_output`
        // prunes this card's fate map in the very same call, so no trace
        // of the commit (or of anything before it) survives to disagree
        // with a later, stale duplicate.
        let committed = RevisionedFrame::from_document(editor.document(), 2)
            .expect("render committed revision 2");
        app.commit_card_output(card, generation, committed, editor.document().data());
        assert!(
            !app.card_generation_fates.contains_key(&card),
            "nothing was outstanding when this commit landed, so its own fate \
             must have been pruned immediately -- the exact precondition this \
             test exercises"
        );
        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "the commit resets retention to describe the still-unexported new \
             revision"
        );

        // The pipeline redelivers the exact same, already-resolved Done a
        // second time: a genuine duplicate of the very first outcome above,
        // naming the same now-stale `action` and `version`.
        app.pipeline.inject_outcome_for_test(Outcome::Done {
            card,
            detail: "saved to Export Location".to_owned(),
            version: Some((generation, 1)),
            action,
        });
        app.drain_pipeline();

        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "a duplicate delivery of an already-resolved action must never \
             re-mark retention -- the newer, still-unexported committed \
             revision must not be reported as saved off a stale duplicate \
             whose own fate has already been pruned"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a duplicate completion must never dismiss the card either"
        );
    }

    #[test]
    fn a_stale_completion_for_an_editor_only_cards_sole_copy_never_dismisses_it() {
        // Round 6, Finding #1's most severe case: an editor-only card (no
        // durable history row at all -- `card_capture_ids` holds nothing for
        // it) whose live vault entry is the *only* copy in existence. A
        // wrongly-dismissed release here is not merely stale -- it is a total,
        // unrecoverable loss of the newer committed revision, since there is
        // no history-unavailable fallback to recover it from afterward.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(223);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.editor_only_cards.insert(card);
        assert!(
            !app.card_capture_ids.contains_key(&card),
            "an editor-only card's sole copy has no durable history row behind it"
        );
        app.card_retention.insert(card, (false, false));
        let action = app.allocate_output_action();
        app.register_output_action(card, action, OutputActionKind::CardOutput, true);

        let generation = 1;
        let stale_path = temp_export_path("stale-editor-only-dismiss");
        let stale_rendered =
            RevisionedFrame::from_document(editor.document(), 1).expect("render revision 1");
        assert!(app.pipeline.post(Job::SaveImageTo {
            card,
            generation,
            rendered: Box::new(stale_rendered),
            path: stale_path.clone(),
            action,
        }));

        let committed = RevisionedFrame::from_document(editor.document(), 2)
            .expect("render committed revision 2");
        app.commit_card_output(card, generation, committed, editor.document().data());

        drain_until(&mut app, |app| {
            !app.card_has_outstanding_card_level_output(card)
        });

        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "an editor-only card's sole copy must never be released on a stale \
             completion -- there is no durable history fallback to recover the \
             newer committed revision afterward"
        );
        assert!(
            app.pipeline.captures().get(card).is_some(),
            "the card's live vault entry -- its only surviving copy -- must remain"
        );
        let _ = std::fs::remove_file(&stale_path);
    }

    #[test]
    fn an_editors_own_copy_registers_as_editor_output_and_a_late_success_after_cancel_never_touches_retention_or_dismisses()
     {
        // Round 12, Finding #1's own regression: before the fix,
        // `copy_rendered` posted its job with no outstanding-action
        // registration at all, so Cancel could prune the generation's fate
        // immediately and a late in-flight result would then find nothing
        // to compare itself against, appearing "current" against a card
        // that no longer has any edit open. Exercised here through the
        // exact editor entry point (`copy_rendered`), not a hand-rolled
        // `register_output_action` call, so a regression that only broke
        // that one call site would still be caught.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(240);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("editor-copy-then-cancel".into()));
        app.card_retention.insert(card, (false, false));

        let generation = 1;
        let rendered =
            RevisionedFrame::from_document(editor.document(), 1).expect("render revision 1");
        let clipboard = app.reserve_clipboard_intent();
        app.copy_rendered(card, generation, rendered, clipboard);

        let action = *app
            .outstanding_output_actions
            .get(&card)
            .expect("copy_rendered must register an outstanding action on a successful post")
            .keys()
            .next()
            .expect("exactly one action was registered");
        assert_eq!(
            app.outstanding_output_actions
                .get(&card)
                .and_then(|actions| actions.get(&action))
                .map(|a| (a.kind, a.close_after)),
            Some((OutputActionKind::EditorOutput, false)),
            "the editor's own Copy must register as a non-exclusive, non-dismissing \
             EditorOutput action, never the exclusive CardOutput family"
        );

        // Cancel: the editing session this Copy answered for is discarded
        // before its already-dispatched job's completion has drained.
        app.editor_closed(EditorSnapshot::new(card, generation, &editor), false);

        app.pipeline.inject_outcome_for_test(Outcome::Done {
            card,
            detail: "copied to the clipboard".to_owned(),
            // Stale: answers for the exact generation Cancel just discarded.
            version: Some((generation, 1)),
            action,
        });
        app.drain_pipeline();

        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "a cancelled edit's own Copy completion must never mark the untouched card \
             retained"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "an in-editor Copy must never dismiss the card it was still open on"
        );
        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "the stale action's own bookkeeping must still resolve idempotently"
        );
        assert!(
            !app.card_generation_fates.contains_key(&card),
            "once nothing dispatched against any of this card's generations remains \
             outstanding, every fate recorded for it must prune"
        );
    }

    #[test]
    fn an_editors_own_save_registers_as_editor_output_and_a_late_success_after_a_newer_commit_never_marks_the_older_bytes_retained()
     {
        // Round 12, Finding #1's Save-side counterpart, and the matching
        // "editor Save-then-Done-newer-rev late success" regression-matrix
        // scenario: a Save dispatched through the editor's own toolbar
        // (`save_rendered`, not the card-level Save menu action) races a
        // later Done that commits a newer revision for the exact same
        // generation before the Save's own completion drains.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(241);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_capture_ids
            .insert(card, CaptureId("editor-save-then-newer-done".into()));
        app.card_retention.insert(card, (false, false));

        let generation = 1;
        let stale_rendered =
            RevisionedFrame::from_document(editor.document(), 1).expect("render revision 1");
        app.save_rendered(card, generation, stale_rendered);

        let action = *app
            .outstanding_output_actions
            .get(&card)
            .expect("save_rendered must register an outstanding action on a successful post")
            .keys()
            .next()
            .expect("exactly one action was registered");
        assert_eq!(
            app.outstanding_output_actions
                .get(&card)
                .and_then(|actions| actions.get(&action))
                .map(|a| (a.kind, a.close_after)),
            Some((OutputActionKind::EditorOutput, false)),
            "the editor's own Save must register as a non-exclusive, non-dismissing \
             EditorOutput action, never the exclusive CardOutput family"
        );

        let committed = RevisionedFrame::from_document(editor.document(), 2)
            .expect("render committed revision 2");
        app.commit_card_output(card, generation, committed, editor.document().data());

        app.pipeline.inject_outcome_for_test(Outcome::Done {
            card,
            detail: "saved to Export Location".to_owned(),
            // Stale: answers for revision 1, superseded by the commit above.
            version: Some((generation, 1)),
            action,
        });
        app.drain_pipeline();

        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "the later Done's own reset retention must be left exactly as it was, \
             undisturbed by the stale editor Save completion"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a stale editor Save completion must never dismiss a card holding a newer \
             committed revision"
        );
        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "the stale action's own bookkeeping must still resolve idempotently"
        );
    }

    #[test]
    fn card_level_and_editor_level_outputs_for_the_same_card_resolve_independently() {
        // Round 12 regression matrix: "card and editor outputs concurrent".
        // A card-level Save (dispatched from the menu while an editor owns
        // the card) and that same editor's own in-toolbar Copy can both be
        // outstanding for the same card at once, keyed by their own action
        // ids -- completing one must never disturb the other's bookkeeping.
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(242);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_retention.insert(card, (false, false));

        let generation = 1;
        // The editor's own in-toolbar Copy.
        let editor_rendered =
            RevisionedFrame::from_document(editor.document(), 1).expect("render revision 1");
        let clipboard = app.reserve_clipboard_intent();
        app.copy_rendered(card, generation, editor_rendered, clipboard);
        let editor_action = *app
            .outstanding_output_actions
            .get(&card)
            .expect("copy_rendered registers an outstanding action")
            .keys()
            .next()
            .expect("exactly one action was registered so far");

        // A concurrent card-level Save (menu action), rendered against the
        // same live editor revision, dispatched separately with its own id.
        let card_level_action = app.allocate_output_action();
        app.register_output_action(card, card_level_action, OutputActionKind::CardOutput, true);

        assert_eq!(
            app.outstanding_output_actions.get(&card).map(HashMap::len),
            Some(2),
            "both the editor's own output and the concurrent card-level output must be \
             tracked as distinct outstanding actions"
        );

        // The editor's Copy completes first -- must resolve only its own
        // entry, leaving the card-level Save's entry (and its close-after
        // policy) untouched.
        app.pipeline.inject_outcome_for_test(Outcome::Done {
            card,
            detail: "copied to the clipboard".to_owned(),
            version: Some((generation, 1)),
            action: editor_action,
        });
        app.drain_pipeline();

        assert!(
            app.card_has_outstanding_card_level_output(card),
            "the concurrent card-level Save must remain outstanding after the editor's \
             own Copy resolves"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "the editor's own Copy must never dismiss the card, and the card-level \
             Save is still outstanding regardless"
        );

        // The card-level Save then completes -- its own close_after policy
        // now applies, since nothing else remains outstanding.
        app.pipeline.inject_outcome_for_test(Outcome::Done {
            card,
            detail: "saved to Export Location".to_owned(),
            version: None,
            action: card_level_action,
        });
        app.drain_pipeline();

        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "both actions must have resolved by now"
        );
        assert!(
            surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "the card-level Save's own close_after policy must apply once it is the \
             last thing outstanding"
        );
    }

    #[test]
    fn a_pending_close_stays_frozen_until_its_commit_ack_arrives_then_reports_failure() {
        // Round 5, Finding #1: `take_editor_close_result` must report nothing
        // at all -- not a premature success -- while a Done-triggered close
        // is still waiting on `CommitCardOutput`'s answer, and the card's own
        // exportable bytes must never appear to update on the strength of a
        // commit that has not actually landed. Only the ack decides.
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(208);
        // Deliberately never stored in the vault, so the commit this test
        // posts deterministically fails ("already left the vault") without
        // needing to simulate a real storage error.
        assert!(app.pipeline.captures().get(card).is_none());

        let generation = 1;
        let revision = 2;
        let rendered =
            RevisionedFrame::from_document(editor.document(), revision).expect("render revision");
        let frame = rendered.frame().clone();
        app.begin_editor_close(card, generation, revision, frame, editor.document().data());
        app.commit_card_output(card, generation, rendered, editor.document().data());

        assert!(
            app.take_editor_close_result().is_none(),
            "the close must stay frozen (report nothing) until the commit's ack \
             actually arrives, however quickly the worker itself runs"
        );
        assert!(
            app.pipeline.captures().get(card).is_none(),
            "a not-yet-acknowledged commit must not leave any live bytes behind for \
             a plain Copy/Save/Upload or a native drag to read"
        );

        drain_until(&mut app, |app| {
            !app.pending_editor_closes.contains_key(&card)
        });

        match app.take_editor_close_result() {
            Some((got_card, EditorCloseOutcome::Failed(_))) => assert_eq!(got_card, card),
            other => panic!("expected the refused commit to resolve as Failed, got {other:?}"),
        }
        assert!(
            app.pipeline.captures().get(card).is_none(),
            "a refused commit must never leave partially-committed bytes behind either"
        );
    }

    #[test]
    fn a_refused_persist_for_an_editor_only_card_reports_failure_not_success() {
        // Round 5, Finding #1: for an editor-only card, the persist ack
        // gates the close exactly as much as the commit does -- a
        // history-only card's sole vault entry is released only once both
        // acks say the edit is durably filed. `store_test_capture` always
        // seeds `capture_id: None`, so `persist_document` deterministically
        // fails ("captured while history was unavailable") without needing
        // to simulate a real history-store error. The close resolving as
        // anything but `Failed` here would let the host release this card's
        // only cache entry despite the edit never having been saved to
        // history. The persist itself is never called directly -- it is
        // auto-dispatched from the commit's own `Outcome::CardOutputCommitted`
        // handler once that commit succeeds (round 6, Finding #3), exactly
        // as the production Done path now does.
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(209);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.editor_only_cards.insert(card);

        let generation = 1;
        let revision = editor.state().revision();
        let committed_frame = RevisionedFrame::from_document(editor.document(), revision)
            .expect("render the committed revision");
        let frame = committed_frame.frame().clone();
        let data = editor.document().data();
        app.begin_editor_close(card, generation, revision, frame, data.clone());
        // The commit itself succeeds (the card is in the vault); only
        // persist -- auto-dispatched once that commit's ack lands -- is
        // made to fail here.
        app.commit_card_output(card, generation, committed_frame, data);

        drain_until(&mut app, |app| {
            !app.pending_editor_closes.contains_key(&card)
        });

        match app.take_editor_close_result() {
            Some((got_card, EditorCloseOutcome::Failed(_))) => assert_eq!(got_card, card),
            other => panic!(
                "an editor-only card whose persist ack failed must resolve Failed, \
                 never Committed, got {other:?}"
            ),
        }
        assert!(
            app.editor_only_cards.contains(&card),
            "the App itself must never release an editor-only card's cache on a \
             Failed close -- only `editor_closed`, which the host calls solely on \
             Committed, does that"
        );
    }

    #[test]
    fn a_live_cards_refused_persist_does_not_block_its_commit_only_close() {
        // The counterpart to the editor-only case above: a live (not
        // editor-only) card's close needs only the commit ack, because its
        // own bytes are already safe once that lands -- gating it on persist
        // too would make Done unusable for any capture whose retention
        // policy never durably stores it (persist then fails every time,
        // deterministically, since `store_test_capture` always seeds
        // `capture_id: None`).
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(210);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        assert!(!app.editor_only_cards.contains(&card));

        let generation = 1;
        let revision = editor.state().revision();
        let committed_frame = RevisionedFrame::from_document(editor.document(), revision)
            .expect("render the committed revision");
        let frame = committed_frame.frame().clone();
        let data = editor.document().data();
        app.begin_editor_close(card, generation, revision, frame, data.clone());
        app.commit_card_output(card, generation, committed_frame, data);

        drain_until(&mut app, |app| {
            !app.pending_editor_closes.contains_key(&card)
        });

        match app.take_editor_close_result() {
            Some((got_card, EditorCloseOutcome::Committed)) => assert_eq!(got_card, card),
            other => panic!(
                "a live card must close committed off the commit ack alone, even \
                 though its (irrelevant, auto-dispatched) persist ack always fails, \
                 got {other:?}"
            ),
        }
    }

    #[test]
    fn a_worker_gone_persist_post_failure_resolves_the_pending_close_instead_of_hanging() {
        // Round 5, Finding #1 (and round 6, Finding #3's deferred persist
        // dispatch): a persist that could not even be posted (the capture
        // worker itself is gone, so `pipeline.post` returns false and no ack
        // will ever arrive) must still resolve any pending close waiting on
        // it -- otherwise a card whose worker died mid-session would freeze
        // its window shut forever, with no ack ever able to unfreeze it.
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(211);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.editor_only_cards.insert(card);

        let generation = 1;
        let revision = editor.state().revision();
        let committed_frame = RevisionedFrame::from_document(editor.document(), revision)
            .expect("render the committed revision");
        let frame = committed_frame.frame().clone();
        let data = editor.document().data();
        app.begin_editor_close(card, generation, revision, frame, data.clone());
        app.commit_card_output(card, generation, committed_frame, data);
        // `Job::Stop` is queued strictly after the commit job just posted,
        // so the worker still finishes that commit and answers it before
        // actually exiting -- only the persist this commit outcome's own
        // handler (`dispatch_deferred_persist`) tries to post next finds the
        // worker gone, exactly like a worker that died mid-session.
        app.pipeline.stop();

        drain_until(&mut app, |app| {
            !app.pending_editor_closes.contains_key(&card)
        });

        match app.take_editor_close_result() {
            Some((got_card, EditorCloseOutcome::Failed(_))) => assert_eq!(got_card, card),
            other => panic!(
                "a persist that could not even be posted must resolve the pending \
                 close as Failed immediately, not leave it hanging forever, got \
                 {other:?}"
            ),
        }
    }

    #[test]
    fn only_the_matching_persisted_editor_revision_refreshes_a_live_pin() {
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(212);
        let capture = CaptureId("pinned-editor-refresh".into());
        let generation = 7;
        let revision = editor.state().revision();
        let rendered = RevisionedFrame::from_document(editor.document(), revision)
            .expect("render persisted revision");
        let frame = rendered.frame().clone();
        let natural_size = scrozz_core::LogicalSize::new(
            frame.size.width / frame.scale.get(),
            frame.size.height / frame.scale.get(),
        );
        app.pending_editor_closes.insert(
            card,
            PendingEditorClose {
                generation,
                revision,
                frame: frame.clone(),
                editor_only: true,
                commit: Some(Ok(())),
                persist: None,
                persist_data: None,
            },
        );

        let stale =
            Thumbnail::from_frame(&frame, PIN_TEXTURE_MAX_EDGE).expect("stale pin texture fixture");
        app.pipeline
            .inject_outcome_for_test(Outcome::EditorClosePersisted {
                card,
                generation: generation - 1,
                revision,
                capture: capture.clone(),
                pin_texture: Ok(Some((stale, natural_size))),
            });
        app.drain_pipeline();
        assert!(
            !surface
                .trace()
                .contains(&SurfaceCall::RefreshPinTexture(capture.clone())),
            "a stale editor generation must not change durable pin pixels"
        );
        assert!(app.pending_editor_closes.contains_key(&card));

        let current = Thumbnail::from_frame(&frame, PIN_TEXTURE_MAX_EDGE)
            .expect("current pin texture fixture");
        app.pipeline
            .inject_outcome_for_test(Outcome::EditorClosePersisted {
                card,
                generation,
                revision,
                capture: capture.clone(),
                pin_texture: Ok(Some((current, natural_size))),
            });
        app.drain_pipeline();
        assert!(
            surface
                .trace()
                .contains(&SurfaceCall::RefreshPinTexture(capture)),
            "the exact durably persisted revision must replace live pin pixels"
        );
    }

    #[test]
    fn pending_editor_close_ready_resolves_a_known_failure_without_its_sibling_ack() {
        // Round 6, Finding #2, tested directly against `ready()` itself
        // rather than through the full pipeline: a required ack that has
        // already come back `Err` must decide failure immediately, however
        // long -- or whether at all -- any sibling ack ever answers.
        let editor = redacted_editor();
        let revision = editor.state().revision();
        let frame = RevisionedFrame::from_document(editor.document(), revision)
            .expect("render a frame")
            .frame()
            .clone();
        let pending =
            |editor_only: bool,
             commit: Option<Result<(), String>>,
             persist: Option<Result<(), String>>| PendingEditorClose {
                generation: 1,
                revision,
                frame: frame.clone(),
                editor_only,
                commit,
                persist,
                persist_data: None,
            };

        // Nothing has answered yet: still waiting, for both kinds of card.
        assert_eq!(pending(false, None, None).ready(), None);
        assert_eq!(pending(true, None, None).ready(), None);

        // A live (not editor-only) card only ever needs the commit ack --
        // persist for it is irrelevant, so it must resolve on the commit
        // alone regardless of what persist later does.
        assert_eq!(pending(false, Some(Ok(())), None).ready(), Some(true));
        assert_eq!(
            pending(false, Some(Err("refused".into())), None).ready(),
            Some(false)
        );

        // An editor-only card's commit failing must resolve `Failed`
        // immediately -- exactly Finding #2's freeze, since this card's
        // persist is only ever dispatched from a *successful* commit's own
        // handler and, on this path, will never be sent at all.
        assert_eq!(
            pending(true, Some(Err("refused".into())), None).ready(),
            Some(false),
            "a known commit failure must resolve failure without ever needing the \
             persist ack this path guarantees will never arrive"
        );

        // An editor-only card's persist failing, independent of what the
        // commit itself did, must also resolve `Failed` immediately without
        // needing the commit's sibling ack to already be in hand.
        assert_eq!(
            pending(true, None, Some(Err("refused".into()))).ready(),
            Some(false),
            "a known persist failure must resolve failure without waiting for the \
             commit ack too"
        );
        assert_eq!(
            pending(true, Some(Ok(())), Some(Err("refused".into()))).ready(),
            Some(false)
        );

        // An editor-only card still genuinely needs both acks to agree
        // before reporting success -- this is not a blanket "any ack
        // resolves immediately" change, only known failures do.
        assert_eq!(pending(true, Some(Ok(())), None).ready(), None);
        assert_eq!(
            pending(true, Some(Ok(())), Some(Ok(()))).ready(),
            Some(true)
        );
    }

    #[test]
    fn a_failed_commit_resolves_an_editor_only_close_promptly_without_ever_needing_persist() {
        // Round 6, Findings #2 and #3, end to end: an editor-only card whose
        // commit fails deterministically (never stored in the vault) must
        // resolve `Failed` well within `drain_until`'s bounded deadline --
        // pre-fix, `ready()` also required the persist ack, which
        // `dispatch_deferred_persist` only ever sends from a *successful*
        // commit's own outcome handler, so this exact scenario would have
        // hung forever (never answering `take_editor_close_result` at all)
        // instead of promptly reporting failure. That the close resolves at
        // all here is itself the regression signal for Finding #3 too: it is
        // only possible because no persist -- and so no durable history
        // write -- is ever dispatched down a failed-commit path.
        let (mut app, _surface) = app();
        let editor = redacted_editor();
        let card = CardId(212);
        // Deliberately never stored in the vault: `commit_rendered` then
        // reports "already left the vault", a deterministic, network-free
        // `CardOutputCommitFailed`.
        app.editor_only_cards.insert(card);

        let generation = 1;
        let revision = editor.state().revision();
        let committed_frame = RevisionedFrame::from_document(editor.document(), revision)
            .expect("render the attempted commit");
        let frame = committed_frame.frame().clone();
        let data = editor.document().data();
        app.begin_editor_close(card, generation, revision, frame, data.clone());
        app.commit_card_output(card, generation, committed_frame, data);

        drain_until(&mut app, |app| {
            !app.pending_editor_closes.contains_key(&card)
        });

        match app.take_editor_close_result() {
            Some((got_card, EditorCloseOutcome::Failed(_))) => assert_eq!(got_card, card),
            other => panic!(
                "a commit failure must resolve the close as Failed promptly, not \
                 hang waiting for a persist ack that a failed commit guarantees \
                 will never be dispatched, got {other:?}"
            ),
        }
        assert!(
            app.editor_only_cards.contains(&card),
            "the App itself must never release an editor-only card's cache on a \
             Failed close"
        );
    }

    #[test]
    fn an_overflow_racing_a_fresh_open_defers_instead_of_retiring_the_opening_card() {
        // Finding #4 (round 4), the same deadline race as the sibling
        // auto-close test above but for capacity retirement: `editors.for_card`
        // cannot see a decode that only just got queued (it produces no live
        // editor until the async round trip completes), so without also
        // checking `opening_cards` overflow would retire the card -- and, per
        // `handle_overflow`'s no-live-editor branches, potentially dismiss it
        // outright -- while its editor is still being created.
        let (mut app, surface) = app();
        let card = CardId(74);
        surface.inject(CardEvent::Open(card));
        surface.inject(CardEvent::Overflow(card));

        app.drain_cards(EditorSnapshots::EMPTY);

        assert!(
            app.opening_cards.contains(&card),
            "the open must still proceed"
        );
        assert!(
            app.deferred_overflow.contains(&card),
            "the same-frame overflow must defer rather than race the open"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a card must not be retired the same frame its editor starts opening"
        );
    }

    #[test]
    fn capacity_overflow_keeps_an_open_editors_source_until_editor_close() {
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(49);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_retention.insert(card, (true, false));
        app.card_capture_ids
            .insert(card, CaptureId("editor-overflow".into()));
        surface.inject(CardEvent::Overflow(card));

        app.tick_with_editor(EditorSnapshots::new(std::slice::from_ref(
            &EditorSnapshot::new(card, 1, &editor),
        )));

        assert!(app.deferred_overflow.contains(&card));
        assert!(
            app.pipeline.captures().get(card).is_some(),
            "overflow must not release source pixels still owned by the editor"
        );
        assert!(!app.pending_retention_overflow.contains(&card));
        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));

        app.editor_closed(EditorSnapshot::new(card, 1, &editor), true);

        assert!(!app.deferred_overflow.contains(&card));
        assert!(
            app.card_has_outstanding_card_level_output(card)
                && app.overflow_recovery_in_flight.contains(&card),
            "the exact edited revision must be saved even when history retains only the original"
        );
        assert!(!app.pending_retention_overflow.contains(&card));
    }

    #[test]
    fn a_cancelled_editor_resolves_deferred_overflow_without_exporting_discarded_edits() {
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(55);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("editor source");
        app.card_retention.insert(card, (true, false));
        app.card_capture_ids
            .insert(card, CaptureId("editor-cancel-overflow".into()));
        surface.inject(CardEvent::Overflow(card));

        app.tick_with_editor(EditorSnapshots::new(std::slice::from_ref(
            &EditorSnapshot::new(card, 1, &editor),
        )));

        assert!(app.deferred_overflow.contains(&card));

        // Cancel: `committed` is false, so deferred overflow recovery must
        // resolve as though no editor were open at all -- never rendering,
        // and certainly never exporting, the discarded annotations.
        app.editor_closed(EditorSnapshot::new(card, 1, &editor), false);

        assert!(!app.deferred_overflow.contains(&card));
        assert!(
            !app.card_has_outstanding_card_level_output(card)
                && !app.overflow_recovery_in_flight.contains(&card),
            "a cancelled edit must not be routed through the live-revision export path"
        );
        match app.pending_overflow_recovery.get(&card) {
            Some(Job::Save { card: got, .. }) => assert_eq!(*got, card),
            other => panic!(
                "a cancelled edit's overflow recovery must fall back to the card's own bytes, \
                 not {other:?}"
            ),
        }
    }

    #[test]
    fn a_deferred_overflow_exports_the_revision_its_editor_ended_on() {
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(53);
        // A durable export exists, but it predates the editor session, so it
        // cannot stand in for the revision capacity is about to evict.
        app.card_retention.insert(card, (false, true));
        surface.inject(CardEvent::Overflow(card));

        app.tick_with_editor(EditorSnapshots::new(std::slice::from_ref(
            &EditorSnapshot::new(card, 1, &editor),
        )));

        assert!(app.deferred_overflow.contains(&card));
        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));

        app.editor_closed(EditorSnapshot::new(card, 1, &editor), true);

        assert!(!app.deferred_overflow.contains(&card));
        assert!(
            app.card_has_outstanding_card_level_output(card)
                && app.overflow_recovery_in_flight.contains(&card),
            "the stale export must not replace the edited revision"
        );
    }

    #[test]
    fn a_deferred_overflow_never_invalidates_an_in_flight_output() {
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(54);
        app.card_retention.insert(card, (false, false));
        let action = app.allocate_output_action();
        app.register_output_action(card, action, OutputActionKind::CardOutput, true);
        app.pending_save_dialog = Some(PendingSaveDialog {
            card,
            generation: None,
            rendered: None,
            future: Box::pin(std::future::pending()),
            action: app.allocate_output_action(),
        });
        surface.inject(CardEvent::Overflow(card));

        app.drain_cards(EditorSnapshots::new(std::slice::from_ref(
            &EditorSnapshot::new(card, 1, &editor),
        )));
        app.editor_closed(EditorSnapshot::new(card, 1, &editor), true);

        assert!(!app.deferred_overflow.contains(&card));
        assert!(app.overflow_recovery_in_flight.contains(&card));
        assert!(
            app.pending_save_dialog.is_some() && app.card_has_outstanding_card_level_output(card),
            "resuming a deferred overflow must not cancel the Save As already in flight"
        );
        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));
    }

    #[test]
    fn completed_output_waits_for_a_concurrent_upload_before_dismissal() {
        let (mut app, surface) = app();
        let card = CardId(56);
        let action = app.allocate_output_action();
        app.register_output_action(card, action, OutputActionKind::CardOutput, true);
        let upload_action = app.allocate_output_action();
        app.register_output_action(card, upload_action, OutputActionKind::Upload, true);

        app.complete_output_action(card, action);

        assert!(!app.card_has_outstanding_card_level_output(card));
        assert!(app.current_upload_close_after(card));
        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));

        app.complete_upload(card, true);

        assert!(surface.trace().contains(&SurfaceCall::Dismiss(card)));
    }

    #[test]
    fn overflow_waits_for_a_successful_upload_before_dismissal() {
        let (mut app, surface) = app();
        let card = CardId(57);
        app.card_retention.insert(card, (true, false));
        app.register_output_action(card, 0, OutputActionKind::Upload, true);

        app.handle_overflow(card, EditorSnapshots::EMPTY);

        assert!(app.overflow_recovery_in_flight.contains(&card));
        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));

        app.complete_upload(card, true);

        assert!(surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert!(!app.overflow_recovery_in_flight.contains(&card));
    }

    #[test]
    fn failed_upload_resumes_deferred_overflow_cleanup() {
        let (mut app, surface) = app();
        let card = CardId(58);
        app.card_retention.insert(card, (true, false));
        app.register_output_action(card, 0, OutputActionKind::Upload, true);
        app.handle_overflow(card, EditorSnapshots::EMPTY);

        // Mirrors what `Outcome::UploadRefused` does before calling
        // `fail_upload`: the action is validated and resolved first, so by
        // the time `fail_upload` (and the `handle_overflow` it may re-run)
        // sees the card, nothing is left outstanding that would make the
        // resumed overflow think another upload is still in flight.
        app.resolve_output_action(card, 0);
        app.fail_upload(card, true);

        assert!(surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert!(!app.overflow_recovery_in_flight.contains(&card));
    }

    #[test]
    fn a_stale_upload_success_for_a_superseded_action_never_completes_the_cards_current_upload() {
        // Round 7, Finding #2 (action identity), reinforced by round 8's
        // action-checked-first ordering: two Upload requests can be
        // dispatched for the same card before the first's outcome drains --
        // both workers are strictly FIFO, but nothing stopped a slow first
        // completion from draining after a second dispatch had already
        // replaced it. The completion for action 3 here answers a request
        // the card no longer recognizes as current (action 5): it must
        // retire quietly rather than mark retention, disturb the current
        // action's `pending_upload` entry, or dismiss the card out from
        // under the still-in-flight current request.
        let (mut app, surface) = app();
        let card = CardId(59);
        app.card_retention.insert(card, (false, false));
        app.register_output_action(card, 5, OutputActionKind::Upload, true);
        let trace_before = surface.trace();
        let notes_before = app.notes().to_vec();

        app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
            card,
            detail: "uploaded and copied the private share link".to_owned(),
            version: None,
            action: 3,
        });
        app.drain_pipeline();

        assert!(
            app.current_upload_close_after(card),
            "a stale completion must leave the current upload's own bookkeeping intact"
        );
        assert_eq!(
            app.card_retention.get(&card),
            Some(&(false, false)),
            "a stale completion must never mark retention on the card's behalf"
        );
        assert_eq!(
            app.current_upload_action.get(&card).copied(),
            Some(5),
            "a stale completion must not disturb the card's current action id"
        );
        assert_eq!(surface.trace(), trace_before);
        assert_eq!(app.notes(), notes_before.as_slice());
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a stale completion must never dismiss a card whose current upload is still \
             in flight"
        );

        // The genuine completion, for the card's actual current action, must
        // still behave exactly as an ordinary (non-stale) success would.
        app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
            card,
            detail: "uploaded and copied the private share link".to_owned(),
            version: None,
            action: 5,
        });
        app.drain_pipeline();

        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "the current action's own completion must clear its bookkeeping"
        );
        assert!(surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert!(
            !app.card_retention.contains_key(&card),
            "the card's own completion dismisses (and thereby retires) it entirely"
        );
    }

    #[test]
    fn a_stale_upload_refusal_for_a_superseded_action_never_fails_the_cards_current_upload() {
        // The refusal counterpart of the success case above: `UploadRefused`
        // is versioned by action id alone (it carries no editor generation/
        // revision the way `UploadDone` does), so this is the only guard
        // standing between an old failure and clearing the current request's
        // bookkeeping out from under it.
        let (mut app, surface) = app();
        let card = CardId(60);
        app.card_retention.insert(card, (false, false));
        app.register_output_action(card, 5, OutputActionKind::Upload, true);
        let trace_before = surface.trace();
        let notes_before = app.notes().to_vec();

        let stale_error = CliError::Core(CoreError::Platform("stale network error".to_owned()));
        app.pipeline
            .inject_outcome_for_test(Outcome::UploadRefused {
                card,
                error: stale_error,
                action: 3,
            });
        app.drain_pipeline();

        assert!(
            app.current_upload_close_after(card),
            "a stale refusal must leave the current upload's own bookkeeping intact"
        );
        assert_eq!(
            app.current_upload_action.get(&card).copied(),
            Some(5),
            "a stale refusal must not disturb the card's current action id"
        );
        assert_eq!(surface.trace(), trace_before);
        assert_eq!(app.notes(), notes_before.as_slice());
        assert_eq!(app.card_retention.get(&card), Some(&(false, false)));

        let current_error = CliError::Core(CoreError::Platform("current network error".to_owned()));
        app.pipeline
            .inject_outcome_for_test(Outcome::UploadRefused {
                card,
                error: current_error,
                action: 5,
            });
        app.drain_pipeline();

        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "the current action's own refusal must clear its bookkeeping"
        );
        assert!(
            app.notes()
                .iter()
                .any(|note| note.contains("upload refused")),
            "the current action's refusal must still be reported"
        );
    }

    #[test]
    fn duplicate_or_absent_upload_outcomes_are_total_noops() {
        let (mut app, surface) = app();
        let card = CardId(601);
        app.card_retention.insert(card, (false, true));
        app.register_output_action(card, 10, OutputActionKind::Upload, true);
        app.register_output_action(card, 11, OutputActionKind::Upload, false);
        assert!(app.resolve_output_action(card, 10).is_some());
        let trace_before = surface.trace();
        let notes_before = app.notes().to_vec();

        app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
            card,
            detail: "stale duplicate upload".to_owned(),
            version: Some((1, 1)),
            action: 10,
        });
        app.pipeline
            .inject_outcome_for_test(Outcome::UploadRefused {
                card,
                error: CliError::Core(CoreError::Platform("duplicate refusal".to_owned())),
                action: 10,
            });
        app.drain_pipeline();

        assert_eq!(surface.trace(), trace_before);
        assert_eq!(app.notes(), notes_before.as_slice());
        assert_eq!(app.card_retention.get(&card), Some(&(false, true)));
        assert_eq!(app.current_upload_action.get(&card), Some(&11));
        assert_eq!(
            app.outstanding_output_actions
                .get(&card)
                .map(|actions| actions.keys().copied().collect::<Vec<_>>()),
            Some(vec![11])
        );
        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));
    }

    #[test]
    fn cancelled_save_as_releases_an_already_overflowed_card() {
        let (mut app, surface) = app();
        let card = CardId(50);
        app.card_retention.insert(card, (false, false));
        let action = app.allocate_output_action();
        app.register_output_action(card, action, OutputActionKind::CardOutput, true);
        app.pending_save_dialog = Some(PendingSaveDialog {
            card,
            generation: None,
            rendered: None,
            future: Box::pin(std::future::ready(None)),
            action,
        });
        surface.inject(CardEvent::Overflow(card));

        app.drain_cards(EditorSnapshots::EMPTY);
        assert!(app.overflow_recovery_in_flight.contains(&card));
        app.drain_save_dialog();

        assert!(surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert!(!app.card_retention.contains_key(&card));
        assert!(!app.overflow_recovery_in_flight.contains(&card));
    }

    #[test]
    fn pending_save_as_keeps_source_bytes_after_other_card_retirement() {
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(55);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("source");
        app.card_retention.insert(card, (true, false));
        let action = app.allocate_output_action();
        app.register_output_action(card, action, OutputActionKind::CardOutput, true);
        app.pending_save_dialog = Some(PendingSaveDialog {
            card,
            generation: None,
            rendered: None,
            future: Box::pin(std::future::ready(None)),
            action,
        });

        app.dismiss_recent_capture(card, "accepted external drag");

        assert!(surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert!(
            app.pipeline.captures().get(card).is_some(),
            "card retirement must not invalidate the pending chosen-destination save"
        );
        assert!(app.card_has_outstanding_card_level_output(card));
        assert!(app.overflow_recovery_in_flight.contains(&card));

        app.drain_save_dialog();

        assert!(!app.card_has_outstanding_card_level_output(card));
        assert!(!app.overflow_recovery_in_flight.contains(&card));
    }

    #[test]
    fn auto_close_and_dismiss_wait_for_a_pending_save_as() {
        for event in [
            CardEvent::AutoClose(CardId(51), RecentCapturesAutoCloseAction::Hide),
            CardEvent::Dismiss(CardId(51)),
        ] {
            let (mut app, surface) = app();
            let card = CardId(51);
            app.card_retention.insert(card, (true, false));
            let action = app.allocate_output_action();
            app.register_output_action(card, action, OutputActionKind::CardOutput, true);
            app.pending_save_dialog = Some(PendingSaveDialog {
                card,
                generation: None,
                rendered: None,
                future: Box::pin(std::future::pending()),
                action,
            });
            surface.inject(event.clone());

            app.drain_cards(EditorSnapshots::EMPTY);

            assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));
            assert!(app.card_retention.contains_key(&card));
            assert!(app.pending_save_dialog.is_some());
        }
    }

    #[test]
    fn continuous_overlay_edits_apply_live_but_persist_once_when_flushed() {
        let root = scratch("overlay-settings-debounce");
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
        .expect("app");
        let mut first = app.recent_captures_overlay_settings();
        first.card_width += 8.0;
        let mut latest = first;
        latest.card_width += 8.0;

        app.edit_recent_captures_overlay(first);
        app.edit_recent_captures_overlay(latest);

        assert_eq!(app.recent_captures_overlay_settings(), latest.normalized());
        assert!(app.pending_recent_captures_settings_save.is_some());
        let before = store
            .load(store.inferred_profile())
            .expect("fresh settings");
        assert_ne!(
            crate::settings::recent_captures_overlay_settings(&before).expect("overlay settings"),
            latest.normalized(),
            "slider movement should not fsync every intermediate value"
        );

        app.flush_recent_captures_overlay_settings(true);

        let saved = store
            .load(store.inferred_profile())
            .expect("saved settings");
        assert_eq!(
            crate::settings::recent_captures_overlay_settings(&saved).expect("overlay settings"),
            latest.normalized()
        );
        assert!(app.pending_recent_captures_settings_save.is_none());
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_card_output_does_not_close_the_card() {
        let (mut app, surface) = app();
        let card = CardId(44);
        app.card_retention.insert(card, (false, false));
        surface.inject(CardEvent::Copy(card));

        app.tick();
        for _ in 0..100 {
            if !app.card_has_outstanding_card_level_output(card) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
            app.tick();
        }

        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a refused output dismissed the card"
        );
        assert!(!app.card_has_outstanding_card_level_output(card));
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

        // Presentation moved to Scenes: After Capture answers destinations only.
        assert!(
            rows.iter().all(|row| {
                row.screenshot_id != crate::settings::APPLY_SMART_FRAME_AFTER_CAPTURE_KEY
            }),
            "{rows:?}"
        );
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
    fn scene_assignments_persist_and_after_capture_no_longer_frames() {
        let root = std::env::temp_dir().join(format!("scrozz-app-scenes-{}", std::process::id()));
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

        // Presentation is not an After Capture action any more.
        assert!(
            app.after_capture_rows()
                .iter()
                .all(|row| !row.label.contains("Smart Frame")),
        );

        use scrozz_ui::settings::{SceneAssignment, SceneCapture, SceneChoice, ScenesEvent};
        app.handle_scenes_event(ScenesEvent::SetDefault(SceneChoice::None));
        app.handle_scenes_event(ScenesEvent::SetAssignment(
            SceneCapture::Window,
            SceneAssignment::Explicit(SceneChoice::Auto),
        ));

        // A window capture frames, everything else follows the `none` default.
        assert_eq!(
            crate::settings::scene_for_capture(&app.config.after_capture, "window").unwrap(),
            Some("auto".to_owned())
        );
        assert_eq!(
            crate::settings::scene_for_capture(&app.config.after_capture, "region").unwrap(),
            None
        );
        drop(app);

        let restarted = store.load(InstallProfile::Fresh).unwrap();
        assert_eq!(
            crate::settings::scene_for_capture(&restarted, "window").unwrap(),
            Some("auto".to_owned())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn deleting_a_preset_from_the_editor_takes_its_assignments_with_it() {
        // The editor's preset list and the Scenes pane delete the same preset.
        // If only one of them re-points the assignments, the rows that named it
        // keep reading as configured while resolving to nothing at all.
        use scrozz_ui::settings::{SceneAssignment, SceneCapture, SceneChoice, ScenesEvent};

        let root =
            std::env::temp_dir().join(format!("scrozz-app-preset-refs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = AfterCaptureStore::new(root.join("settings.json"));
        let mut config = Config::sealed();
        config.after_capture = AfterCaptureSettings::fresh();
        store.save(&config.after_capture).unwrap();
        config.after_capture_store = Some(store.clone());
        let mut app = App::new(
            config,
            Box::new(Recording::new()),
            Arc::new(UnsupportedSelector::headless()),
            false,
        )
        .expect("sealed app");

        let preset = SmartFramePreset::new(
            "notes",
            "Notes",
            scrozz_annotate::SmartFramePresetSettings::default(),
        )
        .unwrap();
        app.upsert_smart_frame_preset(preset).unwrap();
        app.handle_scenes_event(ScenesEvent::SetDefault(SceneChoice::Preset(
            "notes".to_owned(),
        )));
        app.handle_scenes_event(ScenesEvent::SetAssignment(
            SceneCapture::Window,
            SceneAssignment::Explicit(SceneChoice::Preset("notes".to_owned())),
        ));
        assert_eq!(
            crate::settings::scene_for_capture(&app.config.after_capture, "window").unwrap(),
            Some("preset:notes".to_owned())
        );

        // The editor-side entry point, not the Scenes pane's.
        app.delete_smart_frame_preset("notes").unwrap();

        assert_eq!(
            crate::settings::scene_default(&app.config.after_capture).unwrap(),
            "auto",
            "the default must not name a preset that is gone"
        );
        assert_eq!(
            crate::settings::scene_for_capture(&app.config.after_capture, "window").unwrap(),
            Some("auto".to_owned()),
            "an explicit row falls back to the default it was overriding"
        );
        drop(app);

        let restarted = store.load(InstallProfile::Fresh).unwrap();
        for (_, key) in crate::settings::SCENE_CAPTURE_KEYS {
            assert_ne!(restarted.value(key), Some("preset:notes"));
        }
        assert_ne!(
            restarted.value(crate::settings::SCENES_DEFAULT_KEY),
            Some("preset:notes")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn smart_frame_presets_persist_without_replacing_other_settings() {
        let root =
            std::env::temp_dir().join(format!("scrozz-app-smart-presets-{}", std::process::id()));
        let store = AfterCaptureStore::new(root.join("settings.json"));
        let mut config = Config::sealed();
        config.after_capture = AfterCaptureSettings::fresh();
        config.after_capture.set_value("capture.format", "webp");
        store.save(&config.after_capture).unwrap();
        config.after_capture_store = Some(store.clone());
        let surface = Recording::new();
        let mut app = App::new(
            config,
            Box::new(surface),
            Arc::new(UnsupportedSelector::headless()),
            false,
        )
        .expect("sealed app");
        let preset = SmartFramePreset::new(
            "notes",
            "Notes",
            scrozz_annotate::SmartFramePresetSettings::default(),
        )
        .unwrap();

        let presets = app.upsert_smart_frame_preset(preset).unwrap();
        assert_eq!(presets.len(), 1);
        assert_eq!(app.smart_frame_presets()[0].name, "Notes");
        let restarted = store.load(InstallProfile::Fresh).unwrap();
        assert_eq!(restarted.value("capture.format"), Some("webp"));
        assert_eq!(restarted.smart_frame_presets()[0].id, "notes");

        let presets = app.delete_smart_frame_preset("notes").unwrap();
        assert!(presets.is_empty());
        assert!(
            store
                .load(InstallProfile::Fresh)
                .unwrap()
                .smart_frame_presets()
                .is_empty()
        );
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
    fn host_reload_applies_settings_written_locally_then_notified_over_ipc() {
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
        app.observe_forwarded_command(
            &crate::cli::Command::Settings(crate::cli::SettingsArgs {
                command: SettingsCommand::Reload,
            }),
            false,
        );

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
        let capture = redacted_editor().document().source().clone();
        app.editor_requests.push_back(EditorRequest {
            card: CardId(1),
            generation: 1,
            document: scrozz_annotate::Document::new(capture.clone()),
            smart_frame_presets: Vec::new(),
        });
        app.editor_requests.push_back(EditorRequest {
            card: CardId(2),
            generation: 2,
            document: scrozz_annotate::Document::new(capture),
            smart_frame_presets: Vec::new(),
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
        let mut reservation = PickerSurfaceReservation::start(
            PickerMode::Window,
            selector,
            AfterCaptureSettings::default(),
            None,
        )
        .expect("start");
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
        let remedy = scrozz_shell::permissions::remedy(Capability::ScreenRecording);
        assert!(
            app.notes().iter().any(|note| note.contains(remedy)),
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
        let capture = scrozz_core::Capture {
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
        editor.state_mut().set_tool(scrozz_ui::editor::Tool::Redact);
        editor
            .state_mut()
            .pointer_pressed(LogicalPoint::new(8.0, 8.0));
        editor
            .state_mut()
            .pointer_dragged(LogicalPoint::new(56.0, 56.0), false);
        editor.state_mut().pointer_released();
        assert!(editor.state().revision() > 0);
        assert_eq!(
            editor.document().annotations()[0]
                .style
                .effective_redact_intensity(),
            Some(scrozz_annotate::REDACT_INTENSITY_DEFAULT)
        );
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
    fn opening_the_editor_keeps_capture_hotkeys_live() {
        let (mut app, _) = app();

        app.set_keyboard_owner(KeyboardOwner::Editor, true);
        assert!(!app.hotkeys_suspended());
        let accelerator = assign_known_default(&mut app);
        let event = |state| HotkeyEvent {
            action: ShortcutAction::CaptureRegion.id().to_owned(),
            accelerator,
            state,
        };
        assert_eq!(app.action_for_hotkey_event(event(KeyState::Released)), None);
        assert_eq!(
            app.action_for_hotkey_event(event(KeyState::Pressed)),
            Some(Action::Capture(CaptureKind::Region))
        );

        app.set_keyboard_owner(KeyboardOwner::Editor, false);
        assert!(
            !app.hotkeys_suspended(),
            "closing the editor leaves capture shortcuts live"
        );
    }

    #[test]
    fn the_editor_lifecycle_is_idempotent() {
        let (mut app, _) = app();

        // A per-frame sync calls this with the same answer over and over.
        for _ in 0..3 {
            app.set_keyboard_owner(KeyboardOwner::Editor, true);
        }
        assert!(!app.hotkeys_suspended());
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
    fn recorder_suspension_ends_even_when_the_editor_remains_open() {
        // Settings can open the editor, so the two claims genuinely overlap.
        // Editor focus never owns global bindings, so ending shortcut recording
        // must immediately restore capture shortcuts.
        let (mut app, _) = app();

        app.set_keyboard_owner(KeyboardOwner::ShortcutRecorder, true);
        app.set_keyboard_owner(KeyboardOwner::Editor, true);
        assert!(app.hotkeys_suspended());

        app.set_keyboard_owner(KeyboardOwner::ShortcutRecorder, false);
        assert!(!app.hotkeys_suspended());

        app.set_keyboard_owner(KeyboardOwner::Editor, false);
        assert!(
            !app.hotkeys_suspended(),
            "editor close keeps the already-restored shortcuts live"
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
            !app.hotkeys_suspended(),
            "editor ownership must never suspend capture shortcuts"
        );
    }

    #[test]
    fn suspension_does_not_disturb_the_configuration() {
        let (mut app, _) = app();
        let before = app.config.bindings.len();

        app.set_keyboard_owner(KeyboardOwner::ShortcutRecorder, true);

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
    fn forwarded_region_and_window_pixels_bypass_ambient_presentation_actions() {
        let (mut app, surface) = app();
        for (arguments, kind) in [
            (
                vec!["scrozz", "capture", "--region", "0,0,4,4"],
                CaptureKind::Region,
            ),
            (
                vec!["scrozz", "capture", "--window", "forwarded-window"],
                CaptureKind::Window,
            ),
        ] {
            let command = Cli::try_parse_from(arguments)
                .expect("valid forwarded capture")
                .command
                .expect("capture command");
            app.accept_forwarded(&command, Some(forwarded_capture(kind)))
                .expect("forwarded pixels accepted");
        }

        for _ in 0..200 {
            app.drain_pipeline();
            if app.captures >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            surface.presented().is_empty(),
            "an explicit command must not inherit the GUI's overlay action"
        );
        assert_eq!(
            app.captures, 2,
            "the app still accepted both moved captures"
        );
        // `pipeline::selector_outputs_receive_durable_identity_and_pin_ready_provenance`
        // covers the durable Pin to Screen identity with an isolated store.
        assert_eq!(app.captures, 2, "each capture is accounted exactly once");
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
    fn history_show_is_routed_to_the_running_viewport() {
        let show = Cli::try_parse_from(["scrozz", "history", "show"])
            .expect("valid history show")
            .command
            .expect("command");
        assert!(forwarded_open_history(&show));

        let list = Cli::try_parse_from(["scrozz", "history", "list"])
            .expect("valid history list")
            .command
            .expect("command");
        assert!(!forwarded_open_history(&list));
    }

    #[test]
    fn pin_settlements_require_the_current_identity_generation() {
        let (mut app, _) = app();
        let capture = CaptureId("same-capture".into());

        let first = app.set_pin_intent(capture.clone(), true);
        let closed = app.set_pin_intent(capture.clone(), false);
        let repinned = app.set_pin_intent(capture.clone(), true);

        assert!(first < closed && closed < repinned);
        assert!(!app.pin_is_current(&capture, first, true));
        assert!(!app.pin_is_current(&capture, closed, false));
        assert!(app.pin_is_current(&capture, repinned, true));
    }

    #[test]
    fn a_restore_queued_before_unpin_cannot_resurrect_the_pin() {
        let (mut app, _) = app();
        let capture = CaptureId("restore-race".into());

        app.set_pin_intent(capture.clone(), false);
        assert!(!app.accept_pin_restore(&capture));

        app.set_pin_intent(capture.clone(), true);
        assert!(app.accept_pin_restore(&capture));
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
        assert_eq!(app.tick(), Tick::Stop(ExitReason::Deadline));
        assert_eq!(app.exit_reason(), Some(ExitReason::Deadline));
    }

    #[test]
    fn quitting_stops_the_app() {
        let (mut app, _) = app();
        assert_eq!(
            app.perform(Action::Quit),
            Tick::Stop(ExitReason::Quit(CaptureOrigin::Direct))
        );
        assert_eq!(
            app.exit_reason(),
            Some(ExitReason::Quit(CaptureOrigin::Direct))
        );
        assert!(app.notes().iter().any(|n| n == "quit"));
        assert!(
            app.report()
                .data
                .to_compact_string()
                .contains(r#""exit_reason":"direct-quit""#)
        );
    }

    #[test]
    fn settings_only_menu_batches_cannot_stop_or_manufacture_quit() {
        let (mut app, _) = app();
        assert_eq!(
            app.dispatch_tray_batch([
                TrayAction::OpenSettings,
                TrayAction::OpenHistory,
                TrayAction::OpenSettings,
            ]),
            Tick::Continue
        );
        assert!(app.take_settings_request());
        assert_eq!(app.exit_reason(), None);
        assert!(!app.notes().iter().any(|note| note == "quit"));
    }

    #[test]
    fn an_explicit_menu_quit_remains_legitimate_after_other_pending_actions() {
        let (mut app, _) = app();
        assert_eq!(
            app.dispatch_tray_batch([TrayAction::OpenSettings, TrayAction::Quit]),
            Tick::Stop(ExitReason::Quit(CaptureOrigin::MenuBar))
        );
        assert!(app.take_settings_request());
        assert_eq!(
            app.exit_reason(),
            Some(ExitReason::Quit(CaptureOrigin::MenuBar))
        );
    }

    #[test]
    fn native_event_loop_exit_is_explicit_and_does_not_replace_an_app_reason() {
        let (mut native_exit_app, _) = app();
        native_exit_app.record_native_event_loop_exit();
        assert_eq!(
            native_exit_app.exit_reason(),
            Some(ExitReason::NativeEventLoop)
        );

        let (mut quit_app, _) = app();
        assert_eq!(
            quit_app.perform(Action::Quit),
            Tick::Stop(ExitReason::Quit(CaptureOrigin::Direct))
        );
        quit_app.record_native_event_loop_exit();
        assert_eq!(
            quit_app.exit_reason(),
            Some(ExitReason::Quit(CaptureOrigin::Direct))
        );

        let (mut failed_app, _) = app();
        assert_eq!(
            failed_app.stop_for_native_lifecycle("lease unavailable"),
            Tick::Stop(ExitReason::NativeLifecycle)
        );
        assert_eq!(failed_app.exit_reason(), Some(ExitReason::NativeLifecycle));
        assert!(
            failed_app
                .report()
                .data
                .to_compact_string()
                .contains(r#""exit_reason":"native-lifecycle""#)
        );
    }

    #[test]
    fn toggling_recording_reaches_the_machine_and_never_silently_does_nothing() {
        let (mut app, _) = app();
        assert_eq!(app.perform(Action::ToggleRecording), Tick::Continue);
        let notes = app.notes().join("\n");

        // Whatever this build can do, it must say something specific: either it
        // refused for a nameable reason, or it actually started the lifecycle.
        assert!(
            !notes.contains("not wired up"),
            "recording must never report itself as unimplemented: {notes}"
        );
        assert!(
            notes.contains("recording")
                || notes.contains("Recording")
                || notes.contains("Screen Recording"),
            "toggling recording said nothing at all: {notes}"
        );
        assert!(
            app.finalized_media_handoff().is_none(),
            "no recording has finished, so nothing may be waiting for a card"
        );
    }

    /// Builds a validated durable handoff over a real file on disk.
    ///
    /// Deliberately *not* a synthetic value: the seam's whole contract is that
    /// the file outlives the recorder, so a fixture that never touches the
    /// filesystem would test nothing.
    fn durable_handoff(root: &std::path::Path) -> FinalizedMediaHandoff {
        std::fs::create_dir_all(root).expect("scratch directory");
        let path = root.join("Scrozz Recording.mp4");
        std::fs::write(&path, b"durable-finalized-media").expect("durable media");
        FinalizedMediaHandoff {
            path: std::fs::canonicalize(&path).expect("canonical durable media"),
            ownership: scrozz_record::handoff::FinalizedMediaOwnership::ApplicationRetained,
            media_kind: scrozz_record::handoff::FinalizedMediaKind::Video,
            content_type: "video/mp4".to_owned(),
            codec: "h264".to_owned(),
            poster: scrozz_record::handoff::VideoPoster {
                timestamp: Duration::ZERO,
                width: 2,
                height: 2,
                stride: 8,
                pixel_format: scrozz_core::PixelFormat::Rgba8,
                color_space: scrozz_core::ColorSpace::Srgb,
                bytes: vec![255; 16],
            },
            duration: Duration::from_secs(12),
            dimensions: (1920, 1080),
            file_size_bytes: 23,
            audio_present: true,
            open_action: scrozz_record::handoff::FinalizedVideoAction::OpenEditor,
        }
    }

    fn scratch(label: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "scrozz-recording-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn the_handoff_seam_is_taken_once_and_never_replayed() {
        let (mut app, _) = app();
        assert!(app.take_finalized_media_handoff().is_none());
        assert!(app.finalized_media_handoff().is_none());
        assert!(!app.video_editor_is_open());
        assert!(app.video_editor_snapshot().is_none());

        let root = scratch("seam");
        app.recording.handoff = Some(durable_handoff(&root));
        assert!(app.finalized_media_handoff().is_some());
        let taken = app.take_finalized_media_handoff().expect("handoff");
        assert_eq!(
            taken.media_kind,
            scrozz_record::handoff::FinalizedMediaKind::Video
        );
        assert!(
            app.take_finalized_media_handoff().is_none(),
            "the seam hands the recording over exactly once"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_finished_recording_becomes_a_video_card_whose_media_outlives_it() {
        let (mut app, surface) = app();
        let root = scratch("card");
        let handoff = durable_handoff(&root);
        let media = handoff.path.clone();
        app.cloud_settings.upload_enabled = true;
        app.cloud_settings.unavailable_reason = None;

        app.recording.handoff = Some(handoff);
        app.present_recorded_media(None);

        let cards = surface.presented();
        assert_eq!(cards.len(), 1, "one finished recording makes one card");
        let card = &cards[0];
        assert_eq!(
            card.media,
            scrozz_ui::card::CardMedia::video(Duration::from_secs(12), true),
            "the card carries the recording's duration and audio flag"
        );
        assert_eq!(card.source_px(), (1920, 1080), "encoded source dimensions");
        assert!(
            card.thumbnail.is_some(),
            "the bounded poster becomes the card's thumbnail with no decode here"
        );
        assert_eq!(
            card.written,
            vec![media.to_string_lossy().into_owned()],
            "the card points at the durable file rather than owning a copy"
        );
        assert!(card.upload_available);
        assert!(app.visible_cards.contains(&card.id));
        assert!(
            app.finalized_media_handoff().is_none(),
            "the handoff was consumed by the card it produced"
        );

        // Dismissing the card must not remove the video.
        app.recorded_media
            .keys()
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .for_each(|id| {
                assert!(app.route_recorded_card_event(&CardEvent::Dismiss(id)));
            });
        assert!(
            media.is_file(),
            "dismissing a recording card must never delete its durable media"
        );
        assert!(app.recorded_media.is_empty());
        assert!(app.visible_cards.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stale_recorded_media_upload_outcome_never_overwrites_a_newer_ones_status() {
        // Round 8, Finding #3: `upload_recorded_media` used to allocate an
        // action id without ever recording it in `pending_upload`, on the
        // theory that a recording's own upload never dismisses the card so
        // nothing needed to be "current". That missed that a *missing*
        // `pending_upload` entry is exactly what `Outcome::UploadDone`/
        // `Outcome::UploadRefused` treat as "trust this outcome" -- so if the
        // user pressed Upload twice (retrying after what looked like a
        // stall), a slow first completion could land after the second had
        // already updated the card's status, silently reverting it.
        //
        // A real successful dispatch through `upload_recorded_media` cannot
        // be produced hermetically in this test binary: it calls
        // `refresh_cloud_settings` first, which always resolves to
        // unavailable here since `cloud` is not part of the default feature
        // set (see `a_stale_upload_completion_never_dismisses_a_card_holding_a_newer_committed_revision`
        // for the same limitation on the plain-card upload path). This seeds
        // `pending_upload` directly the way that function's fixed body does
        // -- two dispatches, each a fresh action id with `close_after:
        // false` -- and exercises the exact `Outcome::UploadDone` handling
        // path a genuine stale recorded-media completion would race.
        let (mut app, _surface) = app();
        let root = scratch("recorded-media-stale-upload");
        let handoff = durable_handoff(&root);
        app.cloud_settings.upload_enabled = true;
        app.cloud_settings.unavailable_reason = None;
        app.recording.handoff = Some(handoff);
        app.present_recorded_media(None);
        let card = *app.recorded_media.keys().next().expect("recording card");

        // First dispatch.
        let first_action = app.allocate_output_action();
        app.register_output_action(card, first_action, OutputActionKind::Upload, false);

        // The user presses Upload again before the first request's outcome
        // has drained; this is a fresh dispatch, so it must replace the
        // tracked action rather than share it with the first.
        let second_action = app.allocate_output_action();
        app.register_output_action(card, second_action, OutputActionKind::Upload, false);
        assert_ne!(
            first_action, second_action,
            "each dispatch must allocate and track its own action id"
        );

        app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
            card,
            detail: "uploaded and copied the private share link".to_owned(),
            version: None,
            action: first_action,
        });
        app.drain_pipeline();

        assert_eq!(
            app.current_upload_action.get(&card).copied(),
            Some(second_action),
            "a stale first completion must never clear or overwrite the second \
             (current) dispatch's bookkeeping"
        );

        app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
            card,
            detail: "uploaded and copied the private share link".to_owned(),
            version: None,
            action: second_action,
        });
        app.drain_pipeline();

        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "the current (second) dispatch's own completion must clear its bookkeeping"
        );
        assert!(
            app.visible_cards.contains(&card),
            "a recording's own upload must never dismiss the card, even on its \
             genuine completion"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_recorded_media_upload_that_cannot_be_dispatched_sets_an_explicit_failure_status() {
        // Round 10, Finding #2: `upload_recorded_media` set the card's status
        // to "Uploading..." optimistically, then -- if the dispatch itself
        // failed (the capture worker had gone) -- only logged a note and
        // left that "Uploading..." status to linger on the card forever,
        // unlike the still-image path's `CardEvent::Upload` handler, which
        // replaces it with an explicit failure status (round 9, Finding #3).
        //
        // `upload_recorded_media` itself cannot be driven end to end in this
        // test binary: its very first line calls `refresh_cloud_settings`,
        // which always resolves to unavailable without the `cloud` feature
        // (see `a_stale_recorded_media_upload_outcome_never_overwrites_a_newer_ones_status`
        // for the identical, pre-existing limitation). This calls the
        // extracted `report_recorded_media_dispatch_outcome` directly with
        // `dispatched: false`, exercising exactly the branch this finding
        // fixed.
        let (mut app, surface) = app();
        let card = CardId(227);
        app.register_output_action(card, 300, OutputActionKind::Upload, false);
        surface.clear_trace();

        app.report_recorded_media_dispatch_outcome(card, false);

        let trace = surface.trace();
        assert_eq!(
            trace.last(),
            Some(&SurfaceCall::SetStatus {
                id: card,
                status: Some("upload could not be started".to_owned()),
            }),
            "a refused dispatch must replace the optimistic \"Uploading...\" status \
             with an explicit failure, not leave it showing forever: {trace:?}"
        );
        assert!(
            app.notes()
                .iter()
                .any(|note| note.contains("could not be queued for upload")),
            "the failure must still be surfaced in the notes log, not silently \
             swallowed: {:?}",
            app.notes()
        );
    }

    #[test]
    fn dispatch_upload_action_invalidates_the_superseded_action_before_attempting_the_new_one() {
        // Round 9, Finding #3: a second Upload dispatch for the same card
        // must retire whatever action preceded it *before* this attempt's
        // own enqueue is known to succeed, not only once it succeeds.
        // Previously, a failed re-dispatch (a render error, or the capture
        // worker having gone) left the previous action's bookkeeping -- and
        // its close policy -- fully intact, so a late outcome for that now
        // truly-superseded action was still matched and treated as current.
        let (mut app, _surface) = app();
        let card = CardId(226);
        app.register_output_action(card, 10, OutputActionKind::Upload, true);

        // The new dispatch's own enqueue fails (worker gone, render error,
        // ...).
        let dispatched = app.dispatch_upload_action(card, false, |_app, _action| false);

        assert!(
            !dispatched,
            "a failed enqueue must report failure to its caller"
        );
        assert!(
            !app.current_upload_action.contains_key(&card),
            "a failed re-dispatch must still invalidate the action it superseded, \
             leaving nothing tracked as current for this card"
        );

        // The old action no longer exists at all. Its late outcome is a total
        // no-op, not a user-visible diagnostic about a superseded request.
        let notes_before = app.notes().to_vec();
        app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
            card,
            detail: "uploaded and copied the private share link".to_owned(),
            version: None,
            action: 10,
        });
        app.drain_pipeline();
        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "the invalidated old action's own late outcome must not resurrect any \
             bookkeeping for it"
        );
        assert_eq!(app.notes(), notes_before.as_slice());
    }

    #[test]
    fn dispatch_upload_action_tracks_the_new_action_with_its_own_close_policy_on_success() {
        // Companion to the failure case above: a *successful* re-dispatch
        // must still fully replace the superseded action -- its own id, and
        // its own close policy, both independent of whatever the request it
        // replaced carried.
        let (mut app, _surface) = app();
        let card = CardId(227);
        app.register_output_action(card, 10, OutputActionKind::Upload, true);

        let dispatched = app.dispatch_upload_action(card, false, |_app, _action| true);

        assert!(dispatched, "a successful enqueue must report success");
        let current_action = app
            .current_upload_action
            .get(&card)
            .copied()
            .expect("a successful dispatch must be tracked as current");
        assert_ne!(
            current_action, 10,
            "the new dispatch must allocate its own action id rather than reuse the \
             superseded one"
        );
        assert!(
            !app.current_upload_close_after(card),
            "the new dispatch's own close policy must replace whatever the \
             superseded action carried"
        );
    }

    #[test]
    fn automatic_cleanup_hides_a_recording_card_without_touching_its_file() {
        let (mut app, surface) = app();
        let root = scratch("auto-close-card");
        let handoff = durable_handoff(&root);
        let media = handoff.path.clone();

        app.recording.handoff = Some(handoff);
        app.present_recorded_media(None);
        let card = surface.presented()[0].id;
        assert_eq!(app.card_retention.get(&card), Some(&(true, true)));

        surface.inject(CardEvent::AutoClose(
            card,
            RecentCapturesAutoCloseAction::SaveThenHide,
        ));
        app.tick();

        assert!(!app.recorded_media.contains_key(&card));
        assert!(!app.card_retention.contains_key(&card));
        assert!(
            media.is_file(),
            "cleanup must never delete durable recording media"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_gif_card_opens_the_file_rather_than_the_video_editor() {
        let (mut app, _surface) = app();
        let launches = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&launches);
        app.file_launcher = Arc::new(move |action, path| {
            observed
                .lock()
                .expect("file launch log")
                .push((action, path.to_path_buf()));
            Ok(())
        });
        let root = scratch("gif-card");
        std::fs::create_dir_all(&root).expect("scratch directory");
        let path = root.join("Scrozz Export.gif");
        std::fs::write(&path, b"durable-gif").expect("durable gif");
        let mut handoff = durable_handoff(&root);
        handoff.path = std::fs::canonicalize(&path).expect("canonical gif");
        handoff.media_kind = scrozz_record::handoff::FinalizedMediaKind::Gif;
        handoff.content_type = "image/gif".to_owned();
        handoff.codec = "gif".to_owned();
        handoff.audio_present = false;
        handoff.open_action = scrozz_record::handoff::FinalizedVideoAction::OpenFile;
        let expected_path = handoff.path.clone();

        app.recording.handoff = Some(handoff);
        app.present_recorded_media(None);
        let card = *app.recorded_media.keys().next().expect("gif card");
        app.open_recorded_media(card);

        // The editor is never opened over an animation: it has no decodable
        // media track, and the handoff already said so.
        assert!(
            !app.video_editor_is_open(),
            "a GIF must never be routed into the native video editor"
        );
        let notes = app.notes().join("\n");
        assert!(
            !notes.contains("video editor opened"),
            "a GIF card must not claim the editor opened: {notes}"
        );
        assert_eq!(
            *launches.lock().expect("file launch log"),
            vec![(FileLaunchAction::Open, expected_path)],
            "tests observe the requested native action without dispatching it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_apps_never_dispatch_the_native_file_launcher_by_default() {
        let (app, _) = app();
        let missing = Path::new("/this/path/must/not/be-opened-by-a-test.gif");

        assert!((app.file_launcher)(FileLaunchAction::Open, missing).is_ok());
        assert!((app.file_launcher)(FileLaunchAction::Reveal, missing).is_ok());
    }

    #[test]
    fn same_frame_video_drag_uses_the_media_path_not_the_image_vault() {
        let (mut app, surface) = app();
        let root = scratch("drag-card");
        app.recording.handoff = Some(durable_handoff(&root));
        app.present_recorded_media(None);
        let card = *app.recorded_media.keys().next().expect("video card");
        surface.arm(CardEvent::Drag {
            card,
            at: drag_spot(),
        });

        assert_eq!(app.pump_drag_starts(), 1);
        let notes = app.notes().join("\n");
        assert!(
            !notes.contains("has no capture to drag"),
            "video drag fell through to the screenshot vault: {notes}"
        );
        assert!(
            app.recorded_media.contains_key(&card),
            "a refused native drag must keep the durable card"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recording_card_actions_are_honestly_gated_rather_than_silently_ignored() {
        let (mut app, _) = app();
        let root = scratch("actions");
        app.recording.handoff = Some(durable_handoff(&root));
        app.present_recorded_media(None);
        let card = *app
            .recorded_media
            .keys()
            .next()
            .expect("a recording card exists");

        let before = app.notes().len();
        assert!(app.route_recorded_card_event(&CardEvent::Copy(card)));
        assert!(app.route_recorded_card_event(&CardEvent::Save {
            card,
            choose_destination: false,
        }));
        assert!(app.route_recorded_card_event(&CardEvent::Pin(
            card,
            CaptureId("recording-01".to_owned()),
            scrozz_core::PinState::new(
                scrozz_core::LogicalRect::new(
                    scrozz_core::LogicalPoint::new(0.0, 0.0),
                    scrozz_core::LogicalSize::new(288.0, 180.0),
                ),
                scrozz_core::PinScale::fit(scrozz_core::LogicalSize::new(288.0, 180.0), 288.0),
                None,
            ),
        )));
        let said = app.notes()[before..].join("\n");
        assert!(
            said.contains("clipboard") && said.contains("destination"),
            "each unavailable recording action must name its own gap: {said}"
        );
        assert!(
            said.contains("still image"),
            "Pin must be refused with its own reason rather than falling through \
             to the still path's \"card owns no persisted capture\": {said}"
        );

        // An unrelated card is not claimed by the recording path at all.
        let stranger = app.pipeline.allocate();
        assert!(!app.route_recorded_card_event(&CardEvent::Copy(stranger)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn after_capture_policy_reaches_the_machine_as_two_independent_switches() {
        let (mut app, _) = app();
        let mut settings = AfterCaptureSettings::fresh();
        settings.set(
            MediaKind::Recording,
            AfterCaptureAction::ShowRecentCapturesOverlay,
            false,
        );
        settings.set(MediaKind::Recording, AfterCaptureAction::OpenEditor, true);
        app.config.after_capture = settings;

        let policy = app.config.after_capture.recording_policy();
        assert!(!policy.recent_captures_overlay && policy.open_editor);

        // The editor alone is a legal configuration: nothing forces a card.
        assert!(
            current_availability(MediaKind::Recording, AfterCaptureAction::OpenEditor).available
        );
        assert!(
            current_availability(
                MediaKind::Recording,
                AfterCaptureAction::ShowRecentCapturesOverlay
            )
            .available
        );
    }

    #[test]
    fn shutting_down_settles_recording_without_a_machine() {
        let (mut app, _) = app();
        // No engine, no editor, no finalisation: shutdown must still be a
        // no-op rather than a panic, because this is the ordinary path on a
        // platform with no recording backend.
        app.shut_down();
        app.shut_down();
    }

    #[test]
    fn settings_requests_are_delivered_once() {
        let (mut app, _) = app();
        assert!(!app.take_settings_request());
        assert_eq!(app.perform(Action::OpenSettings), Tick::Continue);
        assert!(app.take_settings_request());
        assert!(!app.take_settings_request());
    }

    fn scrolling_app() -> (App, Recording) {
        let surface = Recording::new();
        let handle = surface.handle();
        let app = App::new_with_scrolling_target_resolver(
            Config::sealed(),
            Box::new(surface),
            Arc::new(UnsupportedSelector::headless()),
            false,
            Box::new(|| Ok(fixture_scrolling_target())),
        )
        .expect("a sealed app must start");
        (app, handle)
    }

    #[test]
    fn scrolling_capture_opens_an_axis_picker_before_anything_is_posted() {
        let (mut app, surface) = scrolling_app();

        assert_eq!(
            app.perform(Action::Capture(CaptureKind::Scrolling)),
            Tick::Continue
        );

        let hud = surface.scrolling_hud().expect("axis picker");
        assert_eq!(hud.status, ScrollHudStatus::ChoosingAxis);
        assert!(
            app.scrolling_card.is_none(),
            "no card may be allocated until an axis is chosen"
        );
    }

    #[test]
    fn a_second_scrolling_invocation_cancels_an_unstarted_picker() {
        let (mut app, surface) = scrolling_app();
        app.perform(Action::Capture(CaptureKind::Scrolling));
        assert!(surface.scrolling_hud().is_some());

        app.perform(Action::Capture(CaptureKind::Scrolling));
        assert!(surface.scrolling_hud().is_none());
        assert!(!surface.scroll_passthrough_requested());
    }

    #[test]
    fn automatic_scrolling_waits_only_where_overlay_passthrough_is_required() {
        let (mut app, surface) = scrolling_app();
        surface.set_scroll_passthrough_ready(false);
        app.perform(Action::Capture(CaptureKind::Scrolling));

        app.start_scrolling_capture(ScrollAxis::Vertical);
        if cfg!(target_os = "macos") {
            assert!(
                !surface.scroll_passthrough_requested(),
                "macOS posts to the selected process and must keep its visible HUD interactive"
            );
            app.drain_scrolling_start();
            assert!(app.scrolling_start_pending.is_none());
            return;
        }
        assert!(surface.scroll_passthrough_requested());
        assert!(
            app.scrolling_start_pending.is_some(),
            "the session must not be posted before the overlay confirms transparency"
        );

        app.drain_scrolling_start();
        assert!(
            app.scrolling_start_pending.is_some(),
            "an unacknowledged request is not an acknowledgement"
        );

        surface.set_scroll_passthrough_ready(true);
        app.drain_scrolling_start();
        assert!(app.scrolling_start_pending.is_none());
    }

    #[test]
    fn manual_fallback_releases_forced_passthrough() {
        let (mut app, surface) = scrolling_app();
        let card = CardId(17);
        app.scrolling_card = Some(card);
        app.set_scroll_hud(ScrollHudState::prepared(ScrollAxis::Vertical, true));
        app.surface.request_scroll_passthrough(true);

        app.update_scroll_hud(
            card,
            &Progress::Prepared {
                driver: "manual fixture".to_owned(),
                automatic: false,
                manual_reason: Some("fixture".to_owned()),
            },
        );

        assert!(!surface.scroll_passthrough_requested());
        assert_eq!(
            surface.scrolling_hud().expect("hud").status,
            ScrollHudStatus::Prepared
        );
    }

    #[test]
    fn cancelled_and_failed_scrolling_sessions_release_every_ui_owner() {
        for error in [
            CliError::Core(CoreError::Cancelled),
            CliError::Core(CoreError::Platform("scroll driver failed".to_owned())),
        ] {
            let (mut app, surface) = scrolling_app();
            let card = CardId(18);
            app.scrolling_card = Some(card);
            app.scrolling_abort_pending = Some(card);
            app.scrolling_keep_pending = true;
            app.set_scroll_hud(ScrollHudState::prepared(ScrollAxis::Vertical, true));
            app.surface.request_scroll_passthrough(true);

            assert!(app.finish_failed_scrolling_capture(card, &error));

            assert!(app.scrolling_card.is_none());
            assert!(app.scrolling_abort_pending.is_none());
            assert!(!app.scrolling_keep_pending);
            assert!(surface.scrolling_hud().is_none());
            assert!(!surface.scroll_passthrough_requested());
        }
    }

    #[test]
    fn discard_after_publication_seal_is_refused_without_claiming_success() {
        let (mut app, surface) = scrolling_app();
        let card = CardId(19);
        app.scrolling_card = Some(card);
        app.set_scroll_hud(ScrollHudState::prepared(ScrollAxis::Vertical, false));
        assert!(app.pipeline.seal_scrolling_output_for_test());

        app.abort_scrolling_capture();

        assert!(surface.presented().is_empty());
        assert_eq!(app.scrolling_card, Some(card));
        assert!(app.scrolling_abort_pending.is_none());
        assert!(surface.scrolling_hud().is_some());
        assert!(app.notes().iter().any(
            |note| note.contains("already publishing") && note.contains("cannot be discarded")
        ));
    }

    #[test]
    fn shutting_down_never_leaves_the_overlay_click_through() {
        let (mut app, surface) = scrolling_app();
        app.perform(Action::Capture(CaptureKind::Scrolling));
        app.start_scrolling_capture(ScrollAxis::Vertical);
        assert_eq!(
            surface.scroll_passthrough_requested(),
            !cfg!(target_os = "macos")
        );

        app.shut_down();

        assert!(!surface.scroll_passthrough_requested());
        assert!(surface.scrolling_hud().is_none());
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
    fn history_unpin_and_delete_retire_any_live_screen_pin_generation() {
        let (mut app, _) = app();
        let capture = CaptureId("history-visible-pin".into());
        assert_eq!(app.set_pin_intent(capture.clone(), true), PinGeneration(1));

        assert!(app.perform_history_action(HistoryAction::SetPinned {
            id: capture.clone(),
            pinned: false,
        }));
        assert!(app.pin_is_current(&capture, PinGeneration(2), false));

        assert!(app.perform_history_action(HistoryAction::Delete(capture.clone())));
        assert!(app.pin_is_current(&capture, PinGeneration(3), false));
    }

    #[test]
    fn edited_pin_pixels_replace_the_provisional_texture_before_worker_commit() {
        let source = include_str!("app.rs");
        let pin = source
            .split("CardEvent::Pin(id, capture, state) =>")
            .nth(1)
            .and_then(|body| body.split_once("CardEvent::PinChanged"))
            .map(|(body, _)| body)
            .expect("pin event route");
        let refresh = pin
            .find("refresh_pin_texture")
            .expect("provisional texture replacement");
        let commit = pin.find("Job::PinCard").expect("durable pin job");
        assert!(refresh < commit);
        assert!(pin.contains("PinEditorSnapshot"));
    }

    #[test]
    fn changing_settings_invalidates_an_inflight_connection_test() {
        let (mut app, _) = app();
        let (_sender, receiver) = channel();
        app.connection_test = Some(receiver);
        app.cloud_settings.connection = scrozz_ui::CloudConnectionState::Testing;

        app.invalidate_connection_test();

        assert!(app.connection_test.is_none());
        assert!(matches!(
            app.cloud_settings.connection,
            scrozz_ui::CloudConnectionState::Idle
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
    fn card_copy_save_and_upload_use_the_live_revision_when_the_editor_owns_the_card() {
        let editor = redacted_editor();
        let card = CardId(42);
        let expected = editor.state().revision();
        let editor_snapshot = EditorSnapshot::new(card, 1, &editor);
        let snapshot = EditorSnapshots::new(std::slice::from_ref(&editor_snapshot));

        for output in [
            CardOutput::Copy(1),
            CardOutput::Save(None, 2),
            CardOutput::Upload(7),
        ] {
            let job = App::card_output_job(card, output, snapshot).expect("the document renders");
            let rendered = match job {
                Job::CopyImage {
                    card: got,
                    generation,
                    rendered,
                    ..
                }
                | Job::SaveImage {
                    card: got,
                    generation,
                    rendered,
                    ..
                } => {
                    assert_eq!(got, card);
                    assert_eq!(
                        generation, 1,
                        "the output must be bound to this editor lifetime"
                    );
                    rendered
                }
                Job::UploadImage {
                    card: got,
                    generation,
                    rendered,
                    action,
                } => {
                    assert_eq!(got, card);
                    assert_eq!(
                        generation, 1,
                        "the share must be bound to this editor lifetime"
                    );
                    assert_eq!(
                        action, 7,
                        "the upload action id must travel with the render"
                    );
                    rendered
                }
                Job::Copy { .. } | Job::Save { .. } | Job::Upload { .. } => {
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
                editor.document().source().frame.data,
                "the destructive redaction was bypassed with the original pixels"
            );
        }
    }

    #[test]
    fn card_copy_save_and_upload_fall_back_to_the_card_s_own_bytes_once_no_editor_owns_it() {
        // No editor owns the card -- exactly the state a Cancel, a clean
        // close, or a plain post-close Copy/Save/Upload sees. These must
        // route through the card's own committed bytes (updated only by a
        // prior Done, never by a live in-progress edit) rather than
        // rendering any editor's document.
        let card = CardId(43);

        for output in [
            CardOutput::Copy(1),
            CardOutput::Save(None, 2),
            CardOutput::Upload(9),
        ] {
            let job = App::card_output_job(card, output.clone(), EditorSnapshots::EMPTY)
                .expect("no editor to render means this never fails");
            match (output, job) {
                (CardOutput::Copy(want_action), Job::Copy { card: got, action }) => {
                    assert_eq!(got, card);
                    assert_eq!(
                        action, want_action,
                        "the copy action id must travel with the plain job"
                    );
                }
                (CardOutput::Save(None, want_action), Job::Save { card: got, action }) => {
                    assert_eq!(got, card);
                    assert_eq!(
                        action, want_action,
                        "the save action id must travel with the plain job"
                    );
                }
                (CardOutput::Upload(want_action), Job::Upload { card: got, action }) => {
                    assert_eq!(got, card);
                    assert_eq!(
                        action, want_action,
                        "the upload action id must travel with the plain job"
                    );
                }
                (_, other) => panic!(
                    "with no editor open, output must use the card's own bytes, not {other:?}"
                ),
            }
        }
    }

    #[test]
    fn drag_waits_for_the_live_redacted_revision_instead_of_using_the_card_cache() {
        let (mut app, _) = app();
        let editor = redacted_editor();
        let card = CardId(7);
        app.pipeline
            .captures()
            .store_test_capture(card, editor.document().source())
            .expect("the original capture encodes");
        let original = app
            .pipeline
            .captures()
            .get(card)
            .expect("the card capture is cached");
        let generation = 7;
        let snapshot = EditorSnapshot::new(card, generation, &editor);

        let stale =
            match app.drag_bytes(card, EditorSnapshots::new(std::slice::from_ref(&snapshot))) {
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
            .drag_bytes(card, EditorSnapshots::new(std::slice::from_ref(&snapshot)))
            .expect("the prepared edited revision is ready");
        let decoded = scrozz_export::decode(&bytes.full).expect("the drag payload is a PNG");

        assert_eq!(bytes.generation(), Some(generation));
        assert_eq!(bytes.revision(), editor.state().revision());
        assert_ne!(
            decoded.data,
            editor.document().source().frame.data,
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
            app.drag_bytes(card, EditorSnapshots::new(std::slice::from_ref(&reopened)))
                .is_err(),
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
            .store_test_capture(card, editor.document().source())
            .expect("the original capture encodes");
        let version = (generation, editor.state().revision());
        app.editor_render_failed.insert(card, version);

        app.prepare_editor(EditorSnapshot::new(card, generation, &editor));

        assert!(
            !app.editor_render_pending.contains_key(&card),
            "an unchanged terminal failure was queued again"
        );
    }

    #[test]
    fn a_released_card_is_not_prepared_again_for_later_editor_frames() {
        let (mut app, _) = app();
        let editor = redacted_editor();

        app.prepare_editor(EditorSnapshot::new(CardId(404), 1, &editor));

        assert!(
            !app.editor_render_pending.contains_key(&CardId(404)),
            "a missing card queued a render that can only fail"
        );
    }

    #[test]
    fn uploading_is_blocked_when_the_backend_or_provider_is_unavailable() {
        let (mut app, surface) = app();
        surface.inject(CardEvent::Upload(CardId(43)));
        app.tick();
        assert!(
            app.notes()
                .iter()
                .any(|note| note.contains("card:43 upload unavailable")),
            "{:?}",
            app.notes()
        );
    }

    #[test]
    fn a_superseding_upload_dispatch_replaces_a_stale_close_after_policy() {
        // Round 8, Finding #4: a second Upload dispatch for the same card
        // must always replace the tracked current-upload state wholesale --
        // fresh action id *and* the close-after policy read at that later
        // dispatch time -- never conditionally leaving a previous dispatch's
        // flag in place. This simulates what `CardEvent::Upload`'s handler
        // does on each dispatch (see `app.rs`'s `CardEvent::Upload` arm):
        // allocate a fresh action id and register it as outstanding,
        // capturing the *current* config read, which (via
        // `register_output_action`'s `current_upload_action` side effect)
        // necessarily supersedes whatever the previous entry held.
        let (mut app, surface) = app();
        let card = CardId(61);

        // First dispatch: close-after-upload is enabled.
        let first_action = 100;
        app.register_output_action(card, first_action, OutputActionKind::Upload, true);

        // The user (or a live config reload) disables close-after-upload
        // before the first request's outcome drains, then presses Upload
        // again for the same card -- the second dispatch unconditionally
        // becomes the new current upload, exactly as the real handler does.
        let second_action = 101;
        app.register_output_action(card, second_action, OutputActionKind::Upload, false);
        assert_ne!(
            first_action, second_action,
            "each dispatch must allocate and track its own action id"
        );
        assert!(
            !app.current_upload_close_after(card),
            "the second (current) dispatch must reflect the now-disabled \
             close-after policy, not inherit the first dispatch's stale flag"
        );

        // The stale first action's own completion must retire quietly.
        app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
            card,
            detail: "uploaded and copied the private share link".to_owned(),
            version: None,
            action: first_action,
        });
        app.drain_pipeline();
        assert!(
            app.current_upload_action.get(&card).copied() == Some(second_action)
                && !app.current_upload_close_after(card),
            "a stale completion must never disturb the current dispatch's own \
             (correct, disabled) close policy"
        );
        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));

        // The current (second) action's own completion must honor its own
        // (disabled) close policy rather than dismissing the card.
        app.pipeline.inject_outcome_for_test(Outcome::UploadDone {
            card,
            detail: "uploaded and copied the private share link".to_owned(),
            version: None,
            action: second_action,
        });
        app.drain_pipeline();
        assert!(
            !app.outstanding_output_actions.contains_key(&card),
            "the current dispatch's own completion must clear its bookkeeping"
        );
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "the current dispatch's disabled close-after policy must be honored, \
             not the earlier (superseded) dispatch's enabled one"
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
                keep_after_accept: false,
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
        let (mut app, surface) = app();
        app.surface.request_scroll_passthrough(true);
        app.shut_down();
        assert!(!surface.scroll_passthrough_requested());
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
            keep_after_accept: false,
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
    fn option_or_alt_keeps_a_card_after_an_accepted_drop() {
        let (mut app, surface) = app();
        let card = CardId(3);
        app.drag_keep_after_accept.insert(card);
        app.drag.adopt_finished(
            card,
            DragOutcome::Accepted(scrozz_shell::DragOperation::Copy),
        );

        app.drain_drags();

        assert_eq!(
            surface.trace(),
            vec![SurfaceCall::Settle {
                id: card,
                accepted: false,
            }]
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
