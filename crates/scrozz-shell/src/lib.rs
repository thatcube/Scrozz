//! OS integration: hotkeys, overlay windows, tray, permissions, drag source.
//!
//! Everything here is a place where the three platforms genuinely disagree, and
//! where Wayland may refuse outright. Nothing above this crate should contain a
//! `cfg(target_os)`.

// Platform APIs are reached through objc2 / windows-rs / x11rb, all of which
// require `unsafe`. It is confined to this crate: every crate above it in the
// dependency graph forbids unsafe outright.
#![deny(unsafe_op_in_unsafe_fn)]

pub mod autostart;
pub mod drag;
pub mod hotkey;
pub mod login;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod notify;
pub mod overlay;
pub mod package;
pub mod permissions;
pub mod picker;
pub mod selection;
pub mod tray;
pub mod url_scheme;

pub use drag::{
    ByteSource, DragCapability, DragFormat, DragOperation, DragOrigin, DragOutcome, DragPayload,
    DragPreview, DragSession, DragSource, NativeDragSource, NativeSurface, PromisedFile,
    byte_source, native_drag_source, native_surface_for_window,
};
pub use hotkey::{
    Accelerator, Compositor, Conflict, DisplayServer, GlobalHotkeys, HotkeyEvent, KeyState,
    ReservedShortcut, Session,
};
pub use login::SystemLaunchAtLogin;
pub use notify::{Notification, NotificationPlan};
pub use overlay::{
    AppKitRect, NativeOverlay, OverlayBehavior, OverlayLevel, OverlayReport, StackLayout,
    anchor_bottom_left, appkit_to_logical, logical_to_appkit,
};
pub use package::{PackageKind, package_kind};
pub use permissions::SystemPermissions;
pub use picker::{NativeFolderPicker, StubFolderPicker, native_folder_picker};
pub use selection::{SelectionIntegration, SelectionPlan, resolve_selection};
pub use tray::{Tray, TrayAction, TrayEntry};

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use scrozz_core::{Error, LogicalRect, Result};

/// A desktop platform supported by Scrozz's system-integration plans.
///
/// This is intentionally a value rather than a `cfg` branch. Tests can inspect
/// all three plans on one host, while [`Self::current`] still selects the plan
/// that may actually be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemPlatform {
    /// macOS.
    MacOS,
    /// Microsoft Windows.
    Windows,
    /// Linux desktops.
    Linux,
}

impl SystemPlatform {
    /// Resolves one of the three supported operating-system names.
    #[must_use]
    pub const fn from_os(os: &str) -> Option<Self> {
        match os.as_bytes() {
            b"macos" => Some(Self::MacOS),
            b"windows" => Some(Self::Windows),
            b"linux" => Some(Self::Linux),
            _ => None,
        }
    }

    /// The platform this binary targets.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] on an unrecognised target.
    pub fn current() -> Result<Self> {
        Self::from_os(std::env::consts::OS).ok_or_else(|| Error::Unsupported {
            what: "desktop system integration".to_owned(),
            why: format!(
                "{} is not one of the supported desktop targets",
                std::env::consts::OS
            ),
        })
    }

    /// The stable lowercase platform token.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::MacOS => "macos",
            Self::Windows => "windows",
            Self::Linux => "linux",
        }
    }
}

/// One subprocess invocation, represented without a shell.
///
/// Arguments and environment values remain separate OS strings so paths do not
/// need lossy quoting and user text can never become shell syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPlan {
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
}

impl CommandPlan {
    /// Creates a command plan.
    #[must_use]
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: Vec::new(),
        }
    }

    /// Adds one environment value.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The executable that will be invoked.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The exact argument vector.
    #[must_use]
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// Environment values added to the child.
    #[must_use]
    pub fn env(&self) -> &[(OsString, OsString)] {
        &self.env
    }

    pub(crate) fn output(&self) -> Result<Output> {
        Command::new(&self.program)
            .args(&self.args)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(Error::Io)
    }

    pub(crate) fn apply(&self, purpose: &str) -> Result<()> {
        let output = self.output()?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(Error::Platform(format!(
            "{purpose} command `{}` failed with status {}: {}",
            self.program.display(),
            output
                .status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
            stderr.trim()
        )))
    }

    pub(crate) fn arg_eq(&self, value: &OsStr) -> bool {
        self.args.iter().any(|argument| argument == value)
    }
}

const REGISTRY_VALUE_INSPECT_SCRIPT: &str = concat!(
    "$ErrorActionPreference='Stop';",
    "try {",
    "$k=[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey(",
    "$env:SCROZZ_REGISTRY_SUBKEY);",
    "if ($null -eq $k) { exit 3 };",
    "try {",
    "$v=$k.GetValue($env:SCROZZ_REGISTRY_VALUE,$null,",
    "[Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)",
    "} finally { $k.Dispose() };",
    "if ($null -eq $v) { exit 3 };",
    "if ([System.StringComparer]::Ordinal.Equals(",
    "[string]$v,$env:SCROZZ_REGISTRY_EXPECTED)) { exit 0 };",
    "exit 4",
    "} catch {",
    "[Console]::Error.WriteLine($_.Exception.Message);",
    "exit 2",
    "}"
);

pub(crate) fn registry_value_inspection(
    subkey: &str,
    value_name: &str,
    expected: &str,
) -> CommandPlan {
    CommandPlan::new(
        "powershell.exe",
        [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            REGISTRY_VALUE_INSPECT_SCRIPT,
        ],
    )
    .with_env("SCROZZ_REGISTRY_SUBKEY", subkey)
    .with_env("SCROZZ_REGISTRY_VALUE", value_name)
    .with_env("SCROZZ_REGISTRY_EXPECTED", expected)
}

pub(crate) fn registry_value_status(
    command: &CommandPlan,
    purpose: &str,
) -> Result<RegistrationStatus> {
    let output = command.output()?;
    match output.status.code() {
        Some(0) => Ok(RegistrationStatus::Enabled),
        Some(3) => Ok(RegistrationStatus::Disabled),
        Some(4) => Ok(RegistrationStatus::Drifted),
        _ => Err(Error::Platform(format!(
            "{purpose} command failed with status {}: {}",
            output
                .status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

/// Whether a planned system registration matches what is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationStatus {
    /// No registration exists.
    Disabled,
    /// The registration exactly matches the current plan.
    Enabled,
    /// Something exists under Scrozz's identity, but its contents have drifted.
    Drifted,
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NONCE: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn next_nonce() -> u64 {
        NONCE.fetch_add(1, Ordering::Relaxed)
    }
}

/// A floating, chrome-less window that lives over the desktop.
///
/// Scrozz's capture stack, capture dock and selection overlay are not
/// application windows — they are anchored to the *screen*, borderless, mostly
/// transparent, always on top, and must not steal focus from whatever the user
/// is typing in.
///
/// # Platform reality
///
/// - **macOS** — a plain `NSWindow` activates its app on click, so the capture
///   stack would yank focus out of the user's editor. The correct construct is
///   an `NSPanel` with `NSWindowStyleMaskNonactivatingPanel`. The selection
///   overlay additionally needs a level above the menu bar, which is higher than
///   winit's `WindowLevel::AlwaysOnTop` reaches.
/// - **Windows** — `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED` with
///   `HWND_TOPMOST`.
/// - **Linux/X11** — override-redirect plus `_NET_WM_WINDOW_TYPE_DOCK`.
/// - **Linux/Wayland** — clients **cannot set absolute window position**;
///   `xdg_shell` omits it deliberately. Overlays require the `wlr-layer-shell`
///   protocol, which KDE implements and GNOME/Mutter does not. Per decision D8
///   this degrades explicitly rather than silently misplacing the overlay.
pub trait OverlayWindow {
    /// Anchors this overlay within a display's work area.
    ///
    /// Callers pass [`scrozz_core::Display::work_area`], never
    /// [`scrozz_core::Display::bounds`]: anchoring
    /// to raw display bounds puts the overlay behind the Dock or taskbar.
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::Unsupported`] where the compositor forbids
    /// client positioning.
    fn set_frame(&mut self, frame: LogicalRect) -> Result<()>;

    /// Sets whether clicks pass through to whatever is beneath.
    ///
    /// Per-window and all-or-nothing on every platform, while the capture stack
    /// is mostly empty space around a few opaque cards. Implementations toggle
    /// this per frame from the pointer position, or give each card its own
    /// window.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform refused.
    fn set_click_through(&mut self, passthrough: bool) -> Result<()>;
}

/// A key combination bound globally.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hotkey {
    /// Platform-independent accelerator description, e.g. `"Cmd+Shift+4"`.
    pub accelerator: String,
}

/// Registers system-wide hotkeys.
pub trait HotkeyManager {
    /// Binds a hotkey to an action name.
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::Unsupported`] on wlroots compositors, which
    /// implement no global-shortcut portal. That is not a defect to work around:
    /// per decision D11 the remedy is that the user binds a compositor keybinding
    /// to the Scrozz CLI, and onboarding generates that config line for them.
    /// This is the reason the CLI is a platform requirement rather than a
    /// convenience.
    fn register(&mut self, hotkey: &Hotkey, action: &str) -> Result<()>;

    /// Releases a binding.
    ///
    /// # Errors
    ///
    /// Returns an error if the hotkey was not registered.
    fn unregister(&mut self, hotkey: &Hotkey) -> Result<()>;
}

/// An OS capability that must be granted before a feature works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Reading screen contents.
    ScreenRecording,
    /// Capturing microphone audio.
    Microphone,
    /// Synthesising input and reading window metadata.
    Accessibility,
}

/// Queries and requests OS permissions.
///
/// Per decision D15 Scrozz attempts everything and asks at the moment a feature
/// is first used, never during onboarding. A permission wall before the user has
/// seen the app do anything is the single most common way these tools lose
/// people.
pub trait Permissions {
    /// Whether a capability is currently granted.
    fn is_granted(&self, capability: Capability) -> bool;

    /// Prompts for a capability, or opens the relevant settings pane.
    ///
    /// # Errors
    ///
    /// Returns an error if the request could not be presented.
    fn request(&self, capability: Capability) -> Result<()>;
}

/// Registers Scrozz to launch automatically when the user logs in.
///
/// Feature SYS-03 in the settings/onboarding plan: a single toggle in
/// preferences, backed by a completely different mechanism per platform — a
/// `launchd` agent under `~/Library/LaunchAgents` on macOS, the `Run` key
/// under `HKEY_CURRENT_USER` on Windows, and a freedesktop autostart entry
/// under `~/.config/autostart` on Linux. Nothing above [`login`] needs to know
/// which; see [`login::SystemLaunchAtLogin`] for the concrete implementation.
pub trait LaunchAtLogin {
    /// Whether Scrozz is currently registered to launch at login.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform's registration store exists but could
    /// not be read.
    fn is_enabled(&self) -> Result<bool>;

    /// Registers Scrozz to launch at the next login.
    ///
    /// Idempotent, and overwrites rather than refuses an existing
    /// registration: a settings toggle calls this on every switch-on without
    /// first checking [`LaunchAtLogin::is_enabled`], and the executable's path
    /// may have changed since it was first registered (an app bundle moved, or
    /// an updater replaced it under a new path).
    ///
    /// # Errors
    ///
    /// Returns an error if the registration could not be written.
    fn enable(&self) -> Result<()>;

    /// Removes the launch-at-login registration.
    ///
    /// Idempotent: disabling a registration that does not exist is success,
    /// not an error, for the same reason [`LaunchAtLogin::enable`] is
    /// unconditional — a settings toggle switches off without checking first.
    ///
    /// # Errors
    ///
    /// Returns an error if the registration exists but could not be removed.
    fn disable(&self) -> Result<()>;
}

/// A request to choose a single folder from the filesystem.
///
/// Backs feature SYS-04 (configurable save location) and decision D18's
/// arbitrary save/import folder: the export path accepts any mounted path, and
/// this is how the user names one.
#[derive(Debug, Clone, Default)]
pub struct FolderPickerRequest {
    /// The dialog's window or task title, where the platform shows one.
    pub title: Option<String>,
    /// A short instruction shown above the file browser, where the platform
    /// supports one. `kdialog` shows this as its caption; `zenity` and
    /// `NSOpenPanel` ignore it.
    pub prompt: Option<String>,
    /// The folder to open the dialog on. Platforms fall back to their own
    /// default (usually the last-used folder) when this is `None` or no
    /// longer exists.
    pub starting_directory: Option<PathBuf>,
}

/// Presents a native "choose a folder" dialog.
///
/// A trait, rather than a single native type, precisely so the app coordinator
/// can substitute [`picker::StubFolderPicker`] in tests instead of a real one.
/// `scrozz-ui` emits a browse intent and never depends on this crate: opening `NSOpenPanel`, an
/// `IFileOpenDialog`, or a `zenity` subprocess from a test suite would hang
/// waiting for a human, or fail outright in CI with no display server.
///
/// # Threading
///
/// On macOS this must be called from the main thread, the same constraint as
/// every other AppKit-backed type in this crate.
///
/// # Cancellation
///
/// A user closing the dialog without choosing anything is
/// [`scrozz_core::Error::Cancelled`], never
/// [`scrozz_core::Error::Unsupported`] or a generic platform error — callers
/// must not treat "the user changed their mind" the same as "this doesn't
/// work here", and must not silently swallow either outcome.
pub trait FolderPicker {
    /// Presents the dialog and returns the chosen folder.
    ///
    /// # Errors
    ///
    /// - [`scrozz_core::Error::Cancelled`] if the user dismissed the dialog
    ///   without choosing a folder.
    /// - [`scrozz_core::Error::Unsupported`] if no picker mechanism is
    ///   available at all, for example when neither `zenity` nor `kdialog` is
    ///   installed on a Linux session with no other portal wired up.
    /// - [`scrozz_core::Error::Platform`] for any other failure to present the
    ///   dialog or read its result.
    fn pick_folder(&self, request: &FolderPickerRequest) -> Result<PathBuf>;
}
