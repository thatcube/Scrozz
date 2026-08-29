//! The just-in-time screen-capture permission state machine.
//!
//! The state machine is deliberately pure. It receives what macOS reported and
//! returns one effect for the app to carry out; it never calls TCC, opens a
//! window, or reads the clock itself. That separation is what makes the
//! exact-once resume contract testable without changing a developer's real
//! Screen Recording grant.

use std::{
    fs::{self, File},
    io::Write as _,
    path::{Path, PathBuf},
};

use scrozz_core::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::gui::action::{CaptureKind, CaptureOrigin};

/// A dismissal suppresses shortcut-triggered reminders for one day.
pub const DISMISSAL_COOLDOWN_SECS: u64 = 24 * 60 * 60;

const STORE_VERSION: u32 = 1;
const APP_DIR: &str = "Scrozz";
const FILE_NAME: &str = "capture-permission.json";

/// What the operating system currently permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Direct ScreenCaptureKit access is available.
    Granted,
    /// Access has not been granted. Apple's public screen-capture preflight API
    /// does not distinguish a first run from a user denial.
    NotGranted,
    /// A managed-device or parental-control policy prevents a user grant.
    Restricted,
    /// The required direct-capture API is absent on this OS.
    Unavailable,
}

/// How a capture entered the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invocation {
    /// A visible menu/button action. It may always reopen a dismissed choice.
    Explicit,
    /// A global shortcut. The first use explains the grant; subsequent
    /// reminders respect the dismissal cooldown.
    Shortcut,
    /// Test-only launch autocapture. It must never put permission UI on screen.
    Launch,
}

/// One capture request retained while permission UI is in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingCapture {
    /// The requested capture, unchanged across a grant round trip.
    pub kind: CaptureKind,
    /// Where it came from, for diagnostics when the job is eventually queued.
    pub origin: CaptureOrigin,
    /// Whether it may raise permission UI.
    pub invocation: Invocation,
}

impl PendingCapture {
    /// Builds a request from the app's typed capture route.
    #[must_use]
    pub fn new(kind: CaptureKind, origin: CaptureOrigin) -> Self {
        let invocation = match origin {
            CaptureOrigin::GlobalHotkey => Invocation::Shortcut,
            CaptureOrigin::Startup => Invocation::Launch,
            CaptureOrigin::MenuBar | CaptureOrigin::Direct => Invocation::Explicit,
        };
        Self {
            kind,
            origin,
            invocation,
        }
    }
}

/// Runtime availability of Apple's privacy-preserving content picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerAvailability {
    /// `SCContentSharingPicker` and the required selectors are present.
    Available,
    /// The OS predates `SCContentSharingPicker` (macOS 14).
    OlderMacOs,
    /// The API exists but cannot be used by this process.
    Unavailable,
}

/// The selection modes Scrozz may ask Apple's picker to expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    /// Exactly one window.
    Window,
    /// Exactly one display.
    Display,
    /// One window or one display, chosen in Apple's UI.
    WindowOrDisplay,
}

/// The fallback capabilities implemented by this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FallbackCapabilityMatrix {
    /// One still image of the exact window Apple returns.
    pub window_still: bool,
    /// One still image of the exact display Apple returns.
    pub display_still: bool,
    /// Scrozz's custom area selector.
    pub custom_area: bool,
    /// A single image composited from every display.
    pub all_displays: bool,
    /// Capture without a foreground Apple picker.
    pub unattended_global: bool,
    /// Video recording through the picker in this build.
    pub recording_video: bool,
    /// System-audio recording through the picker in this build.
    pub recording_system_audio: bool,
}

/// The exact fallback matrix shipped today.
pub const FALLBACK_CAPABILITIES: FallbackCapabilityMatrix = FallbackCapabilityMatrix {
    window_still: true,
    display_still: true,
    custom_area: false,
    all_displays: false,
    unattended_global: false,
    recording_video: false,
    recording_system_audio: false,
};

/// Which picker mode can truthfully complete `kind`.
#[must_use]
pub const fn picker_mode(
    kind: CaptureKind,
    availability: PickerAvailability,
) -> Option<PickerMode> {
    if !matches!(availability, PickerAvailability::Available) {
        return None;
    }
    match kind {
        CaptureKind::AllInOne => Some(PickerMode::WindowOrDisplay),
        CaptureKind::Window => Some(PickerMode::Window),
        CaptureKind::Fullscreen => Some(PickerMode::Display),
        CaptureKind::Region | CaptureKind::AllDisplays => None,
    }
}

/// The permission surface currently visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptStage {
    /// Scrozz's explanation shown before macOS is allowed to prompt.
    Preflight,
    /// macOS did not grant direct access.
    Denied,
    /// The process is waiting for the user to come back from System Settings.
    WaitingForSettings,
    /// Access is controlled by policy rather than by the current user.
    Restricted,
    /// Direct capture is unavailable on this OS.
    Unavailable,
}

/// Everything the permission window needs to draw one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prompt {
    /// Which copy and controls to show.
    pub stage: PromptStage,
    /// The original action that will resume after a grant.
    pub pending: PendingCapture,
    /// Whether and how Apple's picker can complete this exact action.
    pub picker_mode: Option<PickerMode>,
    /// Why no picker button is shown, when it is absent.
    pub picker_availability: PickerAvailability,
}

/// A button or close action from the permission window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    /// Let macOS show its Screen Recording prompt.
    Continue,
    /// Use Apple's limited content picker for this one action.
    UseApplePicker,
    /// Open the exact privacy pane.
    OpenSystemSettings,
    /// Dismiss without changing access.
    NotNow,
}

/// One side effect for the app shell to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// No work. The current state remains authoritative.
    None,
    /// Queue the original action through broad direct access.
    RunDirect(PendingCapture),
    /// Queue the original action after its permission window has closed.
    RunDirectAfterPermission(PendingCapture),
    /// Invoke the public macOS request API after Scrozz's preflight.
    RequestSystemAccess,
    /// Open the Screen & System Audio Recording settings pane.
    OpenSystemSettings,
    /// Present Apple's picker with only the modes needed for this action.
    PresentApplePicker(PickerMode),
    /// Persist a dismissal timestamp.
    RememberDismissal(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Preflight {
        pending: PendingCapture,
        picker: PickerAvailability,
    },
    AwaitingSystem {
        pending: PendingCapture,
        picker: PickerAvailability,
    },
    Denied {
        pending: PendingCapture,
        picker: PickerAvailability,
    },
    DeniedWatchingReturn {
        pending: PendingCapture,
        picker: PickerAvailability,
        observed_inactive: bool,
    },
    WaitingForSettings {
        pending: PendingCapture,
        picker: PickerAvailability,
        observed_inactive: bool,
    },
    Restricted {
        pending: PendingCapture,
        picker: PickerAvailability,
    },
    Unavailable {
        pending: PendingCapture,
        picker: PickerAvailability,
    },
    Picking {
        pending: PendingCapture,
        picker: PickerAvailability,
    },
}

/// The permission coordinator for one app process.
#[derive(Debug)]
pub struct Flow {
    phase: Phase,
    dismissed_at_unix: Option<u64>,
}

impl Flow {
    /// Starts idle with the last persisted dismissal, if any.
    #[must_use]
    pub const fn new(dismissed_at_unix: Option<u64>) -> Self {
        Self {
            phase: Phase::Idle,
            dismissed_at_unix,
        }
    }

    /// Whether one user action already owns the permission flow.
    #[must_use]
    pub const fn has_pending_action(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// Handles a new capture invocation.
    pub fn begin(
        &mut self,
        pending: PendingCapture,
        access: Access,
        picker: PickerAvailability,
        now_unix: u64,
    ) -> Effect {
        if self.phase != Phase::Idle {
            return Effect::None;
        }
        if access == Access::Granted {
            return Effect::RunDirect(pending);
        }
        if pending.invocation == Invocation::Launch {
            return Effect::None;
        }
        if pending.invocation == Invocation::Shortcut && self.in_cooldown(now_unix) {
            return Effect::None;
        }

        self.phase = match access {
            Access::Granted => unreachable!("granted returned before building a prompt"),
            Access::NotGranted => Phase::Preflight { pending, picker },
            Access::Restricted => Phase::Restricted { pending, picker },
            Access::Unavailable => Phase::Unavailable { pending, picker },
        };
        Effect::None
    }

    /// The current surface model, if permission UI should be visible.
    #[must_use]
    pub const fn prompt(&self) -> Option<Prompt> {
        let (stage, pending, picker) = match self.phase {
            Phase::Idle | Phase::AwaitingSystem { .. } | Phase::Picking { .. } => return None,
            Phase::Preflight { pending, picker } => (PromptStage::Preflight, pending, picker),
            Phase::Denied { pending, picker }
            | Phase::DeniedWatchingReturn {
                pending, picker, ..
            } => (PromptStage::Denied, pending, picker),
            Phase::WaitingForSettings {
                pending, picker, ..
            } => (PromptStage::WaitingForSettings, pending, picker),
            Phase::Restricted { pending, picker } => (PromptStage::Restricted, pending, picker),
            Phase::Unavailable { pending, picker } => (PromptStage::Unavailable, pending, picker),
        };
        Some(Prompt {
            stage,
            pending,
            picker_mode: picker_mode(pending.kind, picker),
            picker_availability: picker,
        })
    }

    /// Applies one permission-window response.
    pub fn respond(&mut self, response: Response, now_unix: u64) -> Effect {
        let (pending, picker) = match self.phase {
            Phase::Preflight { pending, picker }
            | Phase::Denied { pending, picker }
            | Phase::DeniedWatchingReturn {
                pending, picker, ..
            }
            | Phase::WaitingForSettings {
                pending, picker, ..
            }
            | Phase::Restricted { pending, picker }
            | Phase::Unavailable { pending, picker } => (pending, picker),
            Phase::Idle | Phase::AwaitingSystem { .. } | Phase::Picking { .. } => {
                return Effect::None;
            }
        };

        match response {
            Response::NotNow => self.dismiss(now_unix),
            Response::Continue if matches!(self.phase, Phase::Preflight { .. }) => {
                self.phase = Phase::AwaitingSystem { pending, picker };
                Effect::RequestSystemAccess
            }
            Response::UseApplePicker => {
                let Some(mode) = picker_mode(pending.kind, picker) else {
                    return Effect::None;
                };
                self.phase = Phase::Picking { pending, picker };
                Effect::PresentApplePicker(mode)
            }
            Response::OpenSystemSettings
                if !matches!(
                    self.phase,
                    Phase::Restricted { .. } | Phase::Unavailable { .. }
                ) =>
            {
                self.phase = Phase::WaitingForSettings {
                    pending,
                    picker,
                    observed_inactive: false,
                };
                Effect::OpenSystemSettings
            }
            Response::Continue | Response::OpenSystemSettings => Effect::None,
        }
    }

    /// Supplies the result of the one system request raised after preflight.
    pub fn system_request_finished(&mut self, access: Access) -> Effect {
        let Phase::AwaitingSystem { pending, picker } = self.phase else {
            return Effect::None;
        };
        if access == Access::NotGranted {
            self.phase = Phase::DeniedWatchingReturn {
                pending,
                picker,
                observed_inactive: false,
            };
            Effect::None
        } else {
            self.resolve_access(pending, picker, access)
        }
    }

    /// Observes app activation while waiting for System Settings.
    ///
    /// A grant is consumed only after the app first became inactive and then
    /// active again. Repainting an already-active window cannot duplicate a job.
    pub fn application_active_changed(
        &mut self,
        active: bool,
        access: impl FnOnce() -> Access,
    ) -> Effect {
        let (pending, picker, observed_inactive, system_prompt) = match self.phase {
            Phase::WaitingForSettings {
                pending,
                picker,
                observed_inactive,
            } => (pending, picker, observed_inactive, false),
            Phase::DeniedWatchingReturn {
                pending,
                picker,
                observed_inactive,
            } => (pending, picker, observed_inactive, true),
            _ => return Effect::None,
        };

        if !active {
            self.phase = if system_prompt {
                Phase::DeniedWatchingReturn {
                    pending,
                    picker,
                    observed_inactive: true,
                }
            } else {
                Phase::WaitingForSettings {
                    pending,
                    picker,
                    observed_inactive: true,
                }
            };
            return Effect::None;
        }
        if !observed_inactive {
            return Effect::None;
        }

        self.resolve_access(pending, picker, access())
    }

    /// Consumes Apple's completed one-shot capture once.
    pub fn apple_picker_captured(&mut self) -> Option<PendingCapture> {
        let Phase::Picking { pending, .. } = self.phase else {
            return None;
        };
        self.phase = Phase::Idle;
        Some(pending)
    }

    /// Treats cancellation as a respectful dismissal, never as a capture.
    pub fn apple_picker_cancelled(&mut self, now_unix: u64) -> Effect {
        if !matches!(self.phase, Phase::Picking { .. }) {
            return Effect::None;
        }
        self.dismiss(now_unix)
    }

    /// Returns to the denied surface after a picker startup failure.
    pub fn apple_picker_failed(&mut self) {
        if let Phase::Picking { pending, picker } = self.phase {
            self.phase = Phase::Denied { pending, picker };
        }
    }

    /// Returns to the denied surface when System Settings could not be opened.
    pub fn settings_open_failed(&mut self) {
        if let Phase::WaitingForSettings {
            pending, picker, ..
        } = self.phase
        {
            self.phase = Phase::Denied { pending, picker };
        }
    }

    /// Re-enters the truthful denied/restricted surface when a queued capture
    /// loses access before ScreenCaptureKit reads its pixels.
    pub fn capture_access_revoked(
        &mut self,
        pending: PendingCapture,
        access: Access,
        picker: PickerAvailability,
        now_unix: u64,
    ) -> Effect {
        if self.phase != Phase::Idle {
            return Effect::None;
        }
        if pending.invocation == Invocation::Launch
            || (pending.invocation == Invocation::Shortcut && self.in_cooldown(now_unix))
            || access == Access::Granted
        {
            return Effect::None;
        }
        self.phase = match access {
            Access::Granted => unreachable!("granted returned before building a revocation prompt"),
            Access::NotGranted => Phase::Denied { pending, picker },
            Access::Restricted => Phase::Restricted { pending, picker },
            Access::Unavailable => Phase::Unavailable { pending, picker },
        };
        Effect::None
    }

    fn resolve_access(
        &mut self,
        pending: PendingCapture,
        picker: PickerAvailability,
        access: Access,
    ) -> Effect {
        match access {
            Access::Granted => {
                self.phase = Phase::Idle;
                Effect::RunDirectAfterPermission(pending)
            }
            Access::NotGranted => {
                self.phase = Phase::Denied { pending, picker };
                Effect::None
            }
            Access::Restricted => {
                self.phase = Phase::Restricted { pending, picker };
                Effect::None
            }
            Access::Unavailable => {
                self.phase = Phase::Unavailable { pending, picker };
                Effect::None
            }
        }
    }

    fn dismiss(&mut self, now_unix: u64) -> Effect {
        self.phase = Phase::Idle;
        self.dismissed_at_unix = Some(now_unix);
        Effect::RememberDismissal(now_unix)
    }

    fn in_cooldown(&self, now_unix: u64) -> bool {
        self.dismissed_at_unix
            .is_some_and(|dismissed| now_unix.saturating_sub(dismissed) < DISMISSAL_COOLDOWN_SECS)
    }
}

/// Persistent dismissal history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionStore {
    path: PathBuf,
}

impl PermissionStore {
    /// Uses an explicit path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Resolves Scrozz's config path.
    pub fn default_location() -> Result<Self> {
        let base = dirs::config_dir().or_else(dirs::data_dir).ok_or_else(|| {
            Error::Storage(
                "no platform config directory is available for permission history".into(),
            )
        })?;
        Ok(Self::new(base.join(APP_DIR).join(FILE_NAME)))
    }

    /// Reads the last dismissal. Missing is the normal first run.
    pub fn load(&self) -> Result<Option<u64>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::Storage(format!(
                    "could not read {}: {error}",
                    self.path.display()
                )));
            }
        };
        let document: StoredHistory = serde_json::from_str(&text).map_err(|error| {
            Error::Storage(format!(
                "permission history {} is unreadable: {error}",
                self.path.display()
            ))
        })?;
        Ok(document.dismissed_at_unix)
    }

    /// Atomically records one dismissal.
    pub fn save(&self, dismissed_at_unix: u64) -> Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            Error::Storage(format!("{} has no parent directory", self.path.display()))
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            Error::Storage(format!("could not create {}: {error}", parent.display()))
        })?;
        let text = serde_json::to_string_pretty(&StoredHistory {
            version: STORE_VERSION,
            dismissed_at_unix: Some(dismissed_at_unix),
        })
        .map_err(|error| Error::Storage(format!("could not encode permission history: {error}")))?;
        let temporary = parent.join(format!(".{FILE_NAME}.{}", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut file = File::create(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()
        };
        write().map_err(|error| {
            let _ = fs::remove_file(&temporary);
            Error::Storage(format!("could not write {}: {error}", temporary.display()))
        })?;
        fs::rename(&temporary, &self.path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            Error::Storage(format!(
                "could not replace {}: {error}",
                self.path.display()
            ))
        })
    }

    /// The backing file, for diagnostics and tests.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredHistory {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    dismissed_at_unix: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 10_000_000;

    fn capture(invocation: Invocation) -> PendingCapture {
        PendingCapture {
            kind: CaptureKind::Window,
            origin: CaptureOrigin::Direct,
            invocation,
        }
    }

    fn denied_flow(picker: PickerAvailability) -> Flow {
        let mut flow = Flow::new(None);
        flow.begin(
            capture(Invocation::Explicit),
            Access::NotGranted,
            picker,
            NOW,
        );
        assert_eq!(
            flow.respond(Response::Continue, NOW),
            Effect::RequestSystemAccess
        );
        assert_eq!(
            flow.system_request_finished(Access::NotGranted),
            Effect::None
        );
        flow
    }

    #[test]
    fn first_shortcut_use_shows_preflight_without_requesting_mac_os() {
        let mut flow = Flow::new(None);
        assert_eq!(
            flow.begin(
                capture(Invocation::Shortcut),
                Access::NotGranted,
                PickerAvailability::Available,
                NOW,
            ),
            Effect::None
        );
        assert_eq!(flow.prompt().unwrap().stage, PromptStage::Preflight);
    }

    #[test]
    fn an_already_granted_action_runs_without_a_surface() {
        let pending = capture(Invocation::Explicit);
        let mut flow = Flow::new(None);
        assert_eq!(
            flow.begin(pending, Access::Granted, PickerAvailability::Available, NOW,),
            Effect::RunDirect(pending)
        );
        assert_eq!(flow.prompt(), None);
    }

    #[test]
    fn denial_becomes_a_choice_instead_of_opening_settings() {
        let flow = denied_flow(PickerAvailability::Available);
        let prompt = flow.prompt().unwrap();
        assert_eq!(prompt.stage, PromptStage::Denied);
        assert_eq!(prompt.picker_mode, Some(PickerMode::Window));
    }

    #[test]
    fn a_system_prompt_grant_resumes_after_the_app_returns() {
        let mut flow = denied_flow(PickerAvailability::Available);
        assert_eq!(
            flow.application_active_changed(false, || Access::NotGranted),
            Effect::None
        );
        assert!(matches!(
            flow.application_active_changed(true, || Access::Granted),
            Effect::RunDirectAfterPermission(_)
        ));
    }

    #[test]
    fn cooldown_suppresses_shortcut_nagging_but_expires() {
        let mut flow = Flow::new(Some(NOW));
        assert_eq!(
            flow.begin(
                capture(Invocation::Shortcut),
                Access::NotGranted,
                PickerAvailability::Available,
                NOW + 60,
            ),
            Effect::None
        );
        assert_eq!(flow.prompt(), None);
        flow.begin(
            capture(Invocation::Shortcut),
            Access::NotGranted,
            PickerAvailability::Available,
            NOW + DISMISSAL_COOLDOWN_SECS,
        );
        assert_eq!(flow.prompt().unwrap().stage, PromptStage::Preflight);
    }

    #[test]
    fn explicit_retry_can_reopen_during_cooldown() {
        let mut flow = Flow::new(Some(NOW));
        flow.begin(
            capture(Invocation::Explicit),
            Access::NotGranted,
            PickerAvailability::Available,
            NOW + 1,
        );
        assert_eq!(flow.prompt().unwrap().stage, PromptStage::Preflight);
    }

    #[test]
    fn launch_autocapture_never_raises_permission_ui() {
        let mut flow = Flow::new(None);
        flow.begin(
            capture(Invocation::Launch),
            Access::NotGranted,
            PickerAvailability::Available,
            NOW,
        );
        assert_eq!(flow.prompt(), None);
    }

    #[test]
    fn returning_from_settings_resumes_only_after_a_real_round_trip() {
        let mut flow = denied_flow(PickerAvailability::Available);
        assert_eq!(
            flow.respond(Response::OpenSystemSettings, NOW),
            Effect::OpenSystemSettings
        );
        assert_eq!(
            flow.application_active_changed(true, || Access::Granted),
            Effect::None,
            "an already-active repaint is not a return from Settings"
        );
        assert_eq!(
            flow.application_active_changed(false, || Access::Granted),
            Effect::None
        );
        let pending = capture(Invocation::Explicit);
        assert_eq!(
            flow.application_active_changed(true, || Access::Granted),
            Effect::RunDirectAfterPermission(pending)
        );
    }

    #[test]
    fn a_settings_grant_is_consumed_exactly_once() {
        let mut flow = denied_flow(PickerAvailability::Available);
        flow.respond(Response::OpenSystemSettings, NOW);
        flow.application_active_changed(false, || Access::NotGranted);
        assert!(matches!(
            flow.application_active_changed(true, || Access::Granted),
            Effect::RunDirectAfterPermission(_)
        ));
        assert_eq!(
            flow.application_active_changed(true, || Access::Granted),
            Effect::None
        );
    }

    #[test]
    fn a_second_action_cannot_jump_a_pending_exact_once_resume() {
        let mut flow = denied_flow(PickerAvailability::Available);
        flow.respond(Response::OpenSystemSettings, NOW);
        assert_eq!(
            flow.begin(
                PendingCapture {
                    kind: CaptureKind::Fullscreen,
                    origin: CaptureOrigin::MenuBar,
                    invocation: Invocation::Explicit,
                },
                Access::Granted,
                PickerAvailability::Available,
                NOW,
            ),
            Effect::None
        );
        flow.application_active_changed(false, || Access::Granted);
        assert_eq!(
            flow.application_active_changed(true, || Access::Granted),
            Effect::RunDirectAfterPermission(capture(Invocation::Explicit))
        );
    }

    #[test]
    fn idle_and_unanswered_permission_ui_never_poll_access() {
        let mut idle = Flow::new(None);
        let mut idle_checks = 0;
        for _ in 0..240 {
            assert_eq!(
                idle.application_active_changed(true, || {
                    idle_checks += 1;
                    Access::Granted
                }),
                Effect::None
            );
        }
        assert_eq!(
            idle_checks, 0,
            "60 seconds of idle ticks must not query TCC"
        );

        let mut preflight = Flow::new(None);
        preflight.begin(
            capture(Invocation::Explicit),
            Access::NotGranted,
            PickerAvailability::Available,
            NOW,
        );
        let mut preflight_checks = 0;
        for _ in 0..240 {
            assert_eq!(
                preflight.application_active_changed(true, || {
                    preflight_checks += 1;
                    Access::Granted
                }),
                Effect::None
            );
        }
        assert_eq!(
            preflight_checks, 0,
            "an unanswered permission choice must not query TCC"
        );
    }

    #[test]
    fn continue_requests_once_and_activation_refreshes_access_once() {
        let mut flow = Flow::new(None);
        flow.begin(
            capture(Invocation::Explicit),
            Access::NotGranted,
            PickerAvailability::Available,
            NOW,
        );
        assert_eq!(
            flow.respond(Response::Continue, NOW),
            Effect::RequestSystemAccess
        );
        assert_eq!(
            flow.respond(Response::Continue, NOW),
            Effect::None,
            "a second click cannot emit a second system request"
        );
        flow.system_request_finished(Access::NotGranted);

        let mut checks = 0;
        flow.application_active_changed(false, || {
            checks += 1;
            Access::NotGranted
        });
        assert_eq!(checks, 0, "deactivation must not query TCC");
        flow.application_active_changed(true, || {
            checks += 1;
            Access::NotGranted
        });
        for _ in 0..240 {
            flow.application_active_changed(true, || {
                checks += 1;
                Access::NotGranted
            });
        }
        assert_eq!(checks, 1, "one activation round trip gets one preflight");
    }

    #[test]
    fn picker_cancel_is_a_dismissal_and_never_a_capture() {
        let mut flow = denied_flow(PickerAvailability::Available);
        assert_eq!(
            flow.respond(Response::UseApplePicker, NOW),
            Effect::PresentApplePicker(PickerMode::Window)
        );
        assert_eq!(
            flow.apple_picker_cancelled(NOW + 10),
            Effect::RememberDismissal(NOW + 10)
        );
        assert_eq!(flow.apple_picker_captured(), None);
        assert_eq!(flow.prompt(), None);
    }

    #[test]
    fn picker_selection_is_consumed_exactly_once() {
        let mut flow = denied_flow(PickerAvailability::Available);
        flow.respond(Response::UseApplePicker, NOW);
        assert_eq!(
            flow.apple_picker_captured(),
            Some(capture(Invocation::Explicit))
        );
        assert_eq!(flow.apple_picker_captured(), None);
    }

    #[test]
    fn revocation_while_a_job_is_running_returns_to_denied() {
        let mut flow = Flow::new(None);
        flow.capture_access_revoked(
            capture(Invocation::Explicit),
            Access::NotGranted,
            PickerAvailability::Available,
            NOW,
        );
        assert_eq!(flow.prompt().unwrap().stage, PromptStage::Denied);
    }

    #[test]
    fn revocation_respects_launch_and_shortcut_snoozes() {
        let mut launch = Flow::new(None);
        launch.capture_access_revoked(
            capture(Invocation::Launch),
            Access::NotGranted,
            PickerAvailability::Available,
            NOW,
        );
        assert_eq!(launch.prompt(), None);

        let mut shortcut = Flow::new(Some(NOW));
        shortcut.capture_access_revoked(
            capture(Invocation::Shortcut),
            Access::NotGranted,
            PickerAvailability::Available,
            NOW + 1,
        );
        assert_eq!(shortcut.prompt(), None);
    }

    #[test]
    fn a_stale_permission_error_cannot_claim_a_live_grant_is_denied() {
        let mut flow = Flow::new(None);
        flow.capture_access_revoked(
            capture(Invocation::Explicit),
            Access::Granted,
            PickerAvailability::Available,
            NOW,
        );
        assert_eq!(flow.prompt(), None);
    }

    #[test]
    fn a_managed_restriction_does_not_offer_a_settings_fix() {
        let mut flow = Flow::new(None);
        flow.begin(
            capture(Invocation::Explicit),
            Access::Restricted,
            PickerAvailability::Available,
            NOW,
        );
        assert_eq!(flow.prompt().unwrap().stage, PromptStage::Restricted);
        assert_eq!(
            flow.respond(Response::OpenSystemSettings, NOW),
            Effect::None
        );
    }

    #[test]
    fn an_older_os_explains_that_the_picker_is_absent() {
        let flow = denied_flow(PickerAvailability::OlderMacOs);
        let prompt = flow.prompt().unwrap();
        assert_eq!(prompt.picker_mode, None);
        assert_eq!(prompt.picker_availability, PickerAvailability::OlderMacOs);
    }

    #[test]
    fn fallback_capability_matrix_never_claims_unimplemented_capture() {
        let matrix = std::hint::black_box(FALLBACK_CAPABILITIES);
        assert!(matrix.window_still);
        assert!(matrix.display_still);
        assert!(!matrix.custom_area);
        assert!(!matrix.all_displays);
        assert!(!matrix.unattended_global);
        assert!(!matrix.recording_video);
        assert!(!matrix.recording_system_audio);
        assert_eq!(
            picker_mode(CaptureKind::AllInOne, PickerAvailability::Available),
            Some(PickerMode::WindowOrDisplay)
        );
        assert_eq!(
            picker_mode(CaptureKind::Region, PickerAvailability::Available),
            None
        );
        assert_eq!(
            picker_mode(CaptureKind::AllDisplays, PickerAvailability::Available),
            None
        );
    }

    #[test]
    fn dismissal_history_round_trips() {
        let root = std::env::temp_dir().join(format!(
            "scrozz-permission-test-{}-{NOW}",
            std::process::id()
        ));
        let path = root.join(FILE_NAME);
        let store = PermissionStore::new(&path);
        assert_eq!(store.load().unwrap(), None);
        store.save(NOW).unwrap();
        assert_eq!(store.load().unwrap(), Some(NOW));
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir(root);
    }
}
