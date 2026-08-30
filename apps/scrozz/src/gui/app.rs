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
    CaptureTarget, Error as CoreError, LockEscape, SelectionCapabilities, SelectionMode,
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
use scrozz_store::{CaptureId, RetentionPolicy, Timestamp};
use scrozz_ui::history::{HistoryAction, HistoryViewModel};
use scrozz_ui::settings::RecordingSettingsAction;

use crate::{
    after_capture::{
        ActionEffect, AfterCaptureAction, AfterCaptureSettings, AfterCaptureStore, InstallProfile,
        MediaKind, current_availability,
    },
    cli::{Cli, InteractiveMode, SettingsCommand},
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
            CaptureBytes, DragGeometry, DragSubject, HistoryOperation, Job, Outcome,
            PinEditorSnapshot, Pipeline, PreparedHistoryDrag,
        },
        recording::{
            ActiveVideoEditor, ArmedStart, Completion, FinalisedRecording, PendingSelection,
            PendingStart, RecordingState, SelectionStart,
        },
        selection::CaptureSelector,
        server::{Admission, ForwardedCapture, Forwarder, Request, Server},
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
    recent_captures_overlay::RecentCapturesAutoCloseAction,
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
                    config.after_capture = match profile {
                        InstallProfile::Fresh => AfterCaptureSettings::fresh(),
                        InstallProfile::Existing => AfterCaptureSettings::legacy(),
                    };
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
    selector: Arc<dyn CaptureSelector>,
    file_launcher: FileLauncher,
    active_editor_card: Option<CardId>,
    drag: DragHost,
    /// A native modal drag can consume mouse-up and modifier-release events.
    /// The window host clears that stale egui input before another interaction.
    modal_drag_input_release_pending: bool,
    /// Option/Alt override captured at the exact native drag hand-off.
    drag_keep_after_accept: HashSet<CardId>,
    /// Cards that retire only after a queued copy/save reports success.
    close_after_output: HashSet<CardId>,
    /// Cards waiting for a *confirmed* upload before they may be retired.
    ///
    /// Separate from `close_after_output` because "Hide a card only after a
    /// cloud upload is confirmed successful" means exactly that: a copy or a
    /// save finishing first must not take the card away before the link exists.
    close_after_upload: HashSet<CardId>,
    /// Whether each live card has another retained artifact and a visible export.
    card_retention: HashMap<CardId, (bool, bool)>,
    /// Durable identities used to re-check source retention at cleanup time.
    card_capture_ids: HashMap<CardId, CaptureId>,
    /// Cards awaiting a current history-retention answer before cleanup.
    pending_retention_close: HashSet<CardId>,
    /// Cards removed by capacity while an atomic release check is pending.
    pending_retention_overflow: HashSet<CardId>,
    /// Cleanup actions that expired while the card's editor owned the live revision.
    deferred_auto_close: HashMap<CardId, RecentCapturesAutoCloseAction>,
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
    /// The one editor revision currently being prepared on the worker.
    editor_render_pending: Option<(CardId, u64, u64)>,
    /// A failed version is not retried until the document or editor changes.
    editor_render_failed: Option<(CardId, u64, u64)>,
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
    /// Secret-free private-sharing configuration, as the UI sees it.
    cloud_settings: scrozz_ui::CloudSettingsModel,
    /// A pending request to open the Sharing settings viewport.
    sharing_settings_requested: bool,
    /// An in-flight provider reachability probe, run off the main thread.
    connection_test: Option<Receiver<CliResult<()>>>,
    /// Cards currently on screen, so Upload availability can be refreshed.
    visible_cards: BTreeSet<CardId>,
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

#[derive(Clone)]
enum CardOutput {
    Copy,
    Save(Option<PathBuf>),
    Upload,
}

struct PendingSaveDialog {
    card: CardId,
    rendered: Option<Box<RevisionedFrame>>,
    future: Pin<Box<dyn Future<Output = Option<rfd::FileHandle>>>>,
}

impl CardOutput {
    const fn label(&self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Save(_) => "save",
            Self::Upload => "upload",
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
    pub fn new(
        config: Config,
        surface: Box<dyn CardSurface>,
        selector: Arc<dyn CaptureSelector>,
        permission_ui_available: bool,
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
            selector,
            file_launcher: default_file_launcher(),
            active_editor_card: None,
            drag: DragHost::new(),
            modal_drag_input_release_pending: false,
            drag_keep_after_accept: HashSet::new(),
            close_after_output: HashSet::new(),
            close_after_upload: HashSet::new(),
            card_retention: HashMap::new(),
            card_capture_ids: HashMap::new(),
            pending_retention_close: HashSet::new(),
            pending_retention_overflow: HashSet::new(),
            deferred_auto_close: HashMap::new(),
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
            editor_render_pending: None,
            editor_render_failed: None,
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
            cloud_settings,
            sharing_settings_requested: false,
            connection_test: None,
            visible_cards: BTreeSet::new(),
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
        self.active_editor_card = editor.map(|snapshot| snapshot.card);
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
        self.drain_connection_test();
        self.drain_cards(editor);
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
        self.pending_save_dialog.is_some()
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

    fn drain_pipeline(&mut self) {
        while let Some(outcome) = self.pipeline.poll() {
            match outcome {
                Outcome::HistoryRecording {
                    capture,
                    path,
                    duration_secs,
                    target,
                } => self.open_history_recording(&capture, path, duration_secs, target),
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
                    if history_changed {
                        self.with_history(|history| history.refresh_from_start(Timestamp::now()));
                    }
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
                    self.note(format!("{card} failed: {error}"));
                    if permission_denied {
                        self.handle_capture_permission_failure(kind, origin, Self::screen_access());
                    }
                }
                Outcome::Started { card, detail } => {
                    self.surface.set_status(card, Some(detail.clone()));
                    self.note(format!("{card} {detail}"));
                }
                Outcome::Done { card, detail } => {
                    self.surface.set_status(card, Some(detail.clone()));
                    self.note(format!("{card} {detail}"));
                    if let Some(retention) = self.card_retention.get_mut(&card)
                        && detail.starts_with("saved")
                    {
                        retention.0 = true;
                        retention.1 = true;
                    }
                    if detail.starts_with("uploaded") {
                        if let Some(retention) = self.card_retention.get_mut(&card) {
                            retention.0 = true;
                        }
                        if self.close_after_upload.remove(&card) {
                            self.dismiss_recent_capture(card, "completed its upload");
                        }
                    }
                    if self.close_after_output.remove(&card) {
                        self.dismiss_recent_capture(card, "completed its action");
                    }
                }
                Outcome::Opened {
                    card,
                    document,
                    editor_only,
                } => {
                    let generation = self.next_editor_generation;
                    self.next_editor_generation = self.next_editor_generation.wrapping_add(1);
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
                    // A failed upload leaves the card exactly where it is: the
                    // link the user asked for never arrived, so hiding it now
                    // would throw away the only copy they can still act on.
                    self.close_after_upload.remove(&card);
                    self.surface
                        .set_status(card, Some(format!("Action failed: {error}")));
                    self.note(format!("{card} refused: {error}"));
                }
                Outcome::OutputRefused { card, error } => {
                    self.close_after_output.remove(&card);
                    if self.overflow_recovery_in_flight.remove(&card) {
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
                                if self.pipeline.post(job) {
                                    self.close_after_output.insert(card);
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

    fn drain_cards(&mut self, editor: Option<EditorSnapshot<'_>>) {
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
                    if self.close_after_output.contains(&id) {
                        self.note(format!("{id} already has an action in progress"));
                        continue;
                    }
                    if self.post_card_output(id, CardOutput::Copy, editor) {
                        self.close_after_output.insert(id);
                    }
                }
                CardEvent::Save {
                    card,
                    choose_destination,
                } => {
                    if self.close_after_output.contains(&card) {
                        self.note(format!("{card} already has an action in progress"));
                        continue;
                    }
                    if !choose_destination
                        && self
                            .card_retention
                            .get(&card)
                            .is_some_and(|(_, exported)| *exported)
                        && editor
                            .and_then(|snapshot| snapshot.for_card(card))
                            .is_none()
                    {
                        self.dismiss_recent_capture(card, "used its existing export");
                        self.note(format!("{card} was already saved to Export Location"));
                        continue;
                    }
                    if choose_destination {
                        self.begin_save_dialog(card, editor);
                    } else if self.post_card_output(card, CardOutput::Save(None), editor) {
                        self.close_after_output.insert(card);
                    }
                }
                CardEvent::AutoClose(id, action) => {
                    if self.close_after_output.contains(&id) {
                        self.note(format!(
                            "{id} stayed visible while its output action finishes"
                        ));
                        continue;
                    }
                    if editor.and_then(|snapshot| snapshot.for_card(id)).is_some() {
                        self.deferred_auto_close.insert(id, action);
                        self.note(format!("{id} stayed visible while its editor is open"));
                        continue;
                    }
                    self.handle_auto_close(id, action, editor);
                }
                CardEvent::Upload(id) => {
                    self.refresh_cloud_settings(scrozz_ui::CloudConnectionState::Idle);
                    if self.cloud_settings.upload_enabled {
                        self.surface.set_status(id, Some("Uploading...".to_owned()));
                        if self.post_card_output(id, CardOutput::Upload, editor)
                            && self.config.recent_captures_overlay.close_after_upload
                        {
                            self.close_after_upload.insert(id);
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
                    if self.close_after_output.contains(&id) {
                        self.note(format!(
                            "{id} stayed visible while its output action finishes"
                        ));
                        continue;
                    }
                    self.visible_cards.remove(&id);
                    self.close_after_upload.remove(&id);
                    self.dismiss_recent_capture(id, "dismissed");
                    self.note(format!("{id} dismissed"));
                }
                CardEvent::Overflow(id) => {
                    // Releasing an editor's source would strand the window on
                    // pixels it can no longer re-read, so capacity retirement
                    // waits and resumes against the revision the editor ends on.
                    if editor.and_then(|snapshot| snapshot.for_card(id)).is_some() {
                        self.deferred_overflow.insert(id);
                        self.note(format!(
                            "{id} overflow cleanup was deferred while its editor is open"
                        ));
                    } else {
                        self.handle_overflow(id, editor);
                    }
                }
                CardEvent::Open(id) => {
                    // Decoding happens on the worker, so the click that opens
                    // the editor never inflates a 6K PNG on the UI thread.
                    self.pipeline.post(Job::Open(id));
                }
                CardEvent::Drag { card, at } => {
                    if at.keep_after_accept {
                        self.drag_keep_after_accept.insert(card);
                    }
                    self.begin_drag(card, at, editor);
                }
                // Collapsing into the dock is the capture stack's own animation
                // and belongs to the surface that raised the event once there
                // is one that can perform it.
                CardEvent::Collapse(id) => {
                    self.note(format!("{id}: {event:?} is not routed yet"));
                }
                CardEvent::Pin(id, capture, state) => {
                    let editor = if let Some(editor) = editor.and_then(|editor| editor.for_card(id))
                    {
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
                        if let Err(error) = self.surface.refresh_pin_texture(&capture, texture) {
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
    fn handle_overflow(&mut self, id: CardId, editor: Option<EditorSnapshot<'_>>) {
        // An in-flight Save As or output already owns this card's outcome.
        // Retiring it here would invalidate that work, so overflow only records
        // that the card is no longer reachable and lets the outcome decide.
        if self.close_after_output.contains(&id) {
            self.overflow_recovery_in_flight.insert(id);
            self.note(format!(
                "{id} left the display while its output action finishes"
            ));
            return;
        }
        if let Some(editor) = editor.and_then(|snapshot| snapshot.for_card(id)) {
            match Self::card_output_job(id, CardOutput::Save(None), Some(editor)) {
                Ok(recovery) => {
                    if self.pipeline.post(recovery) {
                        self.close_after_output.insert(id);
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
                match Self::card_output_job(id, CardOutput::Save(None), editor) {
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
        } else if self.post_card_output(id, CardOutput::Save(None), editor) {
            self.close_after_output.insert(id);
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

    /// Applies an expired automatic-close action to a card nothing is editing.
    ///
    /// Resumed with the closing editor's snapshot when the action was deferred,
    /// so **Save then hide** exports the revision the user finished with instead
    /// of reusing an export that predates their edits.
    fn handle_auto_close(
        &mut self,
        id: CardId,
        action: RecentCapturesAutoCloseAction,
        editor: Option<EditorSnapshot<'_>>,
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
                if exported && editor.and_then(|snapshot| snapshot.for_card(id)).is_none() =>
            {
                self.dismiss_recent_capture(id, "auto-closed after its existing save");
            }
            RecentCapturesAutoCloseAction::SaveThenHide => {
                if self.close_after_output.contains(&id) {
                    self.note(format!("{id} already has an action in progress"));
                } else if self.post_card_output(id, CardOutput::Save(None), editor) {
                    self.close_after_output.insert(id);
                }
            }
        }
    }

    fn post_card_output(
        &mut self,
        card: CardId,
        output: CardOutput,
        editor: Option<EditorSnapshot<'_>>,
    ) -> bool {
        let label = output.label();
        let job = match Self::card_output_job(card, output, editor) {
            Ok(job) => job,
            Err(error) => {
                self.note(format!(
                    "{card} could not be rendered for {}: {error}",
                    label
                ));
                return false;
            }
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

    fn begin_save_dialog(&mut self, card: CardId, editor: Option<EditorSnapshot<'_>>) {
        if self.pending_save_dialog.is_some() || self.close_after_output.contains(&card) {
            self.note(format!(
                "{card} stayed visible because another Save As dialog is already open"
            ));
            return;
        }
        let rendered = match editor
            .and_then(|editor| editor.for_card(card))
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
        self.pending_save_dialog = Some(PendingSaveDialog {
            card,
            rendered,
            future,
        });
        self.close_after_output.insert(card);
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
        let PendingSaveDialog { card, rendered, .. } = self
            .pending_save_dialog
            .take()
            .expect("polled dialog exists");
        let Some(file) = result else {
            self.close_after_output.remove(&card);
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
                rendered,
                path,
            },
            None => Job::SaveTo { card, path },
        };
        if !self.pipeline.post(job) {
            self.close_after_output.remove(&card);
            self.note(format!(
                "{card} could not be queued for save: the capture worker has gone"
            ));
        }
    }

    fn dismiss_recent_capture(&mut self, card: CardId, reason: &str) {
        let editor_owns_card = self.active_editor_card == Some(card);
        let save_dialog_owns_card = self
            .pending_save_dialog
            .as_ref()
            .is_some_and(|pending| pending.card == card);
        if !save_dialog_owns_card {
            self.close_after_output.remove(&card);
        }
        self.visible_cards.remove(&card);
        self.close_after_upload.remove(&card);
        self.drag_keep_after_accept.remove(&card);
        self.card_retention.remove(&card);
        self.card_capture_ids.remove(&card);
        self.pending_retention_close.remove(&card);
        self.pending_retention_overflow.remove(&card);
        self.deferred_auto_close.remove(&card);
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

    fn card_output_job(
        card: CardId,
        output: CardOutput,
        editor: Option<EditorSnapshot<'_>>,
    ) -> CliResult<Job> {
        let Some(editor) = editor.and_then(|editor| editor.for_card(card)) else {
            return Ok(match output {
                CardOutput::Copy => Job::Copy(card),
                CardOutput::Save(None) => Job::Save(card),
                CardOutput::Save(Some(path)) => Job::SaveTo { card, path },
                CardOutput::Upload => Job::Upload(card),
            });
        };

        let generation = editor.generation;
        let rendered = Box::new(editor.render()?);
        Ok(match output {
            CardOutput::Copy => Job::CopyImage { card, rendered },
            CardOutput::Save(None) => Job::SaveImage { card, rendered },
            CardOutput::Save(Some(path)) => Job::SaveImageTo {
                card,
                rendered,
                path,
            },
            CardOutput::Upload => Job::UploadImage {
                card,
                generation,
                rendered,
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
                if at.keep_after_accept {
                    self.drag_keep_after_accept.insert(card);
                }
                if self.recorded_media.contains_key(&card) {
                    self.begin_recorded_drag(card, at);
                } else {
                    self.begin_drag(card, at, editor);
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
    fn begin_drag(&mut self, card: CardId, at: DragSpot, editor: Option<EditorSnapshot<'_>>) {
        if !self.drag.is_attached()
            && let Some(surface) = self.surface.native_surface()
        {
            self.drag.attach(surface);
        }

        let bytes = match self.drag_bytes(card, editor) {
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
        let mut rows = vec![AfterCaptureRow {
            screenshot_id: crate::settings::APPLY_SMART_FRAME_AFTER_CAPTURE_KEY.to_owned(),
            recording_id: None,
            label: "Apply Smart Frame".to_owned(),
            description:
                "Create one adaptive presentation revision before any screenshot action runs."
                    .to_owned(),
            screenshot: AfterCaptureCell {
                enabled: self
                    .config
                    .after_capture
                    .value(crate::settings::APPLY_SMART_FRAME_AFTER_CAPTURE_KEY)
                    .is_some_and(|value| value == "true"),
                available: true,
                unavailable_reason: None,
            },
            recording: AfterCaptureCell {
                enabled: false,
                available: false,
                unavailable_reason: Some("Smart Frame applies only to screenshots.".to_owned()),
            },
        }];
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
        let mut apply_smart_frame = None;
        for edit in edits {
            if edit.id == crate::settings::APPLY_SMART_FRAME_AFTER_CAPTURE_KEY {
                if edit.media != AfterCaptureMedia::Screenshot {
                    self.note(format!(
                        "{} was not changed because it arrived from the wrong settings column",
                        edit.id
                    ));
                    continue;
                }
                apply_smart_frame = Some(edit.enabled);
                continue;
            }
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
        if changes.is_empty() && apply_smart_frame.is_none() {
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
            if let Some(enabled) = apply_smart_frame {
                latest.set_value(
                    crate::settings::APPLY_SMART_FRAME_AFTER_CAPTURE_KEY,
                    enabled.to_string(),
                );
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

    /// Takes one completed Smart Frame analysis for delivery by the viewport host.
    pub fn take_smart_frame_result(&mut self) -> Option<SmartFrameResult> {
        self.smart_frame_results.pop_front()
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
                settings.delete_smart_frame_preset(preset_id)
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
    /// editor's final snapshot, so any recovery export is rendered from the
    /// revision the user actually finished with. Resuming before the
    /// editor-only release means a card the deferred work retires still hands
    /// its live bytes back in the same pass.
    pub fn editor_closed(&mut self, editor: EditorSnapshot<'_>) {
        let card = editor.card;
        if let Some(action) = self.deferred_auto_close.remove(&card) {
            self.handle_auto_close(card, action, Some(editor));
        }
        if self.deferred_overflow.remove(&card) {
            self.handle_overflow(card, Some(editor));
        }
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

    /// Persists the exact edited scene before its editor-only cache is released.
    pub fn persist_editor(&mut self, editor: EditorSnapshot<'_>) {
        let revision = editor.editor.state().revision();
        let job = Job::PersistDocument {
            card: editor.card,
            generation: editor.generation,
            revision,
            data: Box::new(editor.editor.document().data()),
        };
        if !self.pipeline.post(job) {
            self.note(format!(
                "{} editor {} revision {revision} could not be queued for history persistence: \
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
        if !self.pipeline.post(Job::UploadRecording {
            card,
            capture,
            path,
            content_type,
            file_name,
        }) {
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
        Capture, CaptureTarget, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize,
        PhysicalSize, PixelFormat, Provenance, ScaleFactor, WindowId,
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

    fn forwarded_capture(kind: CaptureKind) -> ForwardedCapture {
        let (provenance, target) = match kind {
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
                && !app.close_after_output.contains(&card)
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
        assert!(!app.close_after_output.contains(&card));
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

        app.tick_with_editor(Some(EditorSnapshot::new(card, 1, &editor)));

        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "an old export must not discard the active editor revision"
        );
        assert!(
            app.close_after_output.contains(&card),
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

        app.drain_cards(Some(EditorSnapshot::new(card, 1, &editor)));

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

        app.editor_closed(EditorSnapshot::new(card, 1, &editor));

        assert!(!app.deferred_auto_close.contains_key(&card));
        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "an export that predates the editor must not stand in for its edits"
        );
        assert!(
            app.close_after_output.contains(&card),
            "the revision the editor ended on must be exported before the card closes"
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

        app.drain_cards(Some(EditorSnapshot::new(card, 1, &editor)));

        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert_eq!(
            app.deferred_auto_close.get(&card),
            Some(&RecentCapturesAutoCloseAction::Hide)
        );

        app.editor_closed(EditorSnapshot::new(card, 1, &editor));

        assert!(surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert!(!app.deferred_auto_close.contains_key(&card));
    }

    #[test]
    fn capacity_overflow_keeps_an_open_editors_source_until_editor_close() {
        let (mut app, surface) = app();
        let editor = redacted_editor();
        let card = CardId(49);
        app.pipeline
            .captures()
            .store_test_capture(card, &editor.document().source)
            .expect("editor source");
        app.card_retention.insert(card, (true, false));
        app.card_capture_ids
            .insert(card, CaptureId("editor-overflow".into()));
        surface.inject(CardEvent::Overflow(card));

        app.tick_with_editor(Some(EditorSnapshot::new(card, 1, &editor)));

        assert!(app.deferred_overflow.contains(&card));
        assert!(
            app.pipeline.captures().get(card).is_some(),
            "overflow must not release source pixels still owned by the editor"
        );
        assert!(!app.pending_retention_overflow.contains(&card));
        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));

        app.editor_closed(EditorSnapshot::new(card, 1, &editor));

        assert!(!app.deferred_overflow.contains(&card));
        assert!(
            app.close_after_output.contains(&card)
                && app.overflow_recovery_in_flight.contains(&card),
            "the exact edited revision must be saved even when history retains only the original"
        );
        assert!(!app.pending_retention_overflow.contains(&card));
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

        app.tick_with_editor(Some(EditorSnapshot::new(card, 1, &editor)));

        assert!(app.deferred_overflow.contains(&card));
        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));

        app.editor_closed(EditorSnapshot::new(card, 1, &editor));

        assert!(!app.deferred_overflow.contains(&card));
        assert!(
            app.close_after_output.contains(&card)
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
        app.close_after_output.insert(card);
        app.pending_save_dialog = Some(PendingSaveDialog {
            card,
            rendered: None,
            future: Box::pin(std::future::pending()),
        });
        surface.inject(CardEvent::Overflow(card));

        app.drain_cards(Some(EditorSnapshot::new(card, 1, &editor)));
        app.editor_closed(EditorSnapshot::new(card, 1, &editor));

        assert!(!app.deferred_overflow.contains(&card));
        assert!(app.overflow_recovery_in_flight.contains(&card));
        assert!(
            app.pending_save_dialog.is_some() && app.close_after_output.contains(&card),
            "resuming a deferred overflow must not cancel the Save As already in flight"
        );
        assert!(!surface.trace().contains(&SurfaceCall::Dismiss(card)));
    }

    #[test]
    fn cancelled_save_as_releases_an_already_overflowed_card() {
        let (mut app, surface) = app();
        let card = CardId(50);
        app.card_retention.insert(card, (false, false));
        app.close_after_output.insert(card);
        app.pending_save_dialog = Some(PendingSaveDialog {
            card,
            rendered: None,
            future: Box::pin(std::future::ready(None)),
        });
        surface.inject(CardEvent::Overflow(card));

        app.drain_cards(None);
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
            .store_test_capture(card, &editor.document().source)
            .expect("source");
        app.card_retention.insert(card, (true, false));
        app.close_after_output.insert(card);
        app.pending_save_dialog = Some(PendingSaveDialog {
            card,
            rendered: None,
            future: Box::pin(std::future::ready(None)),
        });

        app.dismiss_recent_capture(card, "accepted external drag");

        assert!(surface.trace().contains(&SurfaceCall::Dismiss(card)));
        assert!(
            app.pipeline.captures().get(card).is_some(),
            "card retirement must not invalidate the pending chosen-destination save"
        );
        assert!(app.close_after_output.contains(&card));
        assert!(app.overflow_recovery_in_flight.contains(&card));

        app.drain_save_dialog();

        assert!(!app.close_after_output.contains(&card));
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
            app.close_after_output.insert(card);
            app.pending_save_dialog = Some(PendingSaveDialog {
                card,
                rendered: None,
                future: Box::pin(std::future::pending()),
            });
            surface.inject(event.clone());

            app.drain_cards(None);

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
            if !app.close_after_output.contains(&card) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
            app.tick();
        }

        assert!(
            !surface.trace().contains(&SurfaceCall::Dismiss(card)),
            "a refused output dismissed the card"
        );
        assert!(!app.close_after_output.contains(&card));
    }

    #[test]
    fn after_capture_rows_expose_real_capabilities_not_inert_controls() {
        let (app, _) = app();
        let rows = app.after_capture_rows();
        assert_eq!(rows.len(), AfterCaptureAction::UI_ORDER.len() + 1);
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

        let smart_frame = rows
            .iter()
            .find(|row| row.screenshot_id == crate::settings::APPLY_SMART_FRAME_AFTER_CAPTURE_KEY)
            .expect("Smart Frame row");
        assert!(!smart_frame.screenshot.enabled);
        assert!(smart_frame.screenshot.available);
        assert!(smart_frame.recording_id.is_none());
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
    fn after_capture_smart_frame_is_opt_in_and_persists() {
        let root =
            std::env::temp_dir().join(format!("scrozz-app-smart-frame-{}", std::process::id()));
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
            id: crate::settings::APPLY_SMART_FRAME_AFTER_CAPTURE_KEY.to_owned(),
            media: AfterCaptureMedia::Screenshot,
            enabled: true,
        }]);
        assert!(crate::settings::smart_frame_after_capture(&app.config.after_capture).unwrap());
        drop(app);

        let restarted = store.load(InstallProfile::Fresh).unwrap();
        assert!(crate::settings::smart_frame_after_capture(&restarted).unwrap());
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
        let capture = redacted_editor().document().source.clone();
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
        let snapshot = Some(EditorSnapshot::new(card, 1, &editor));

        for output in [CardOutput::Copy, CardOutput::Save(None), CardOutput::Upload] {
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
                Job::UploadImage {
                    card: got,
                    generation,
                    rendered,
                } => {
                    assert_eq!(got, card);
                    assert_eq!(
                        generation, 1,
                        "the share must be bound to this editor lifetime"
                    );
                    rendered
                }
                Job::Copy(_) | Job::Save(_) | Job::Upload(_) => {
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
