//! Which windows belong in a picker, and how a display is labelled in one,
//! expressed as pure functions.
//!
//! Free of every `windows` crate type, so `tests/windows.rs` can
//! `#[path]`-include it and test the rules on any platform.
//!
//! # Why this file is disproportionately important
//!
//! A raw `EnumWindows` walk on a normal Windows 11 desktop returns on the order
//! of a hundred `HWND`s, and the great majority of them are things the user has
//! never seen: suspended UWP shells that DWM has cloaked, `WorkerW` desktop
//! wallpaper hosts, XAML island bridges, tooltips, IME candidate windows,
//! zero-size message-only helpers. A picker that lists them is unusable, and
//! this is the single most visible difference between a good and a bad Windows
//! capture tool. Every rule below removes a specific, identifiable class of
//! garbage, and each is annotated with what it removes.

/// `WS_EX_TOOLWINDOW` — a floating palette, deliberately absent from the
/// taskbar and the Alt-Tab list.
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;

/// `WS_EX_APPWINDOW` — forces a window onto the taskbar even when it would
/// otherwise be excluded. Overrides [`WS_EX_TOOLWINDOW`].
pub const WS_EX_APPWINDOW: u32 = 0x0004_0000;

/// `WS_EX_NOREDIRECTIONBITMAP` — the window has no redirection surface, so GDI
/// `BitBlt` reads black from it. Not a reason to hide it; a reason to prefer
/// WGC.
pub const WS_EX_NOREDIRECTIONBITMAP: u32 = 0x0020_0000;

/// `DWMWA_CLOAKED` attribute index for `DwmGetWindowAttribute`.
pub const DWMWA_CLOAKED: u32 = 14;

/// `DWMWA_EXTENDED_FRAME_BOUNDS` attribute index.
///
/// `GetWindowRect` includes the invisible resize border DWM adds around a
/// window; this attribute is the rectangle the user actually sees.
pub const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;

/// Smallest edge, in device pixels, a window must have to be worth listing.
///
/// Message-only and helper windows are routinely 0×0 or 1×1.
pub const MIN_WINDOW_EDGE: i32 = 16;

/// Everything the filter needs to know about one window.
///
/// A plain data struct so the rules can be tested without a `HWND`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowFacts {
    /// `IsWindowVisible`.
    pub visible: bool,
    /// `IsIconic` — minimised windows have no capturable surface.
    pub minimized: bool,
    /// `DwmGetWindowAttribute(DWMWA_CLOAKED)` was non-zero.
    ///
    /// The single highest-value rule here: suspended UWP applications keep a
    /// visible, correctly sized, correctly titled `HWND` that DWM has cloaked.
    /// Without this check a picker lists "Mail", "Photos" and "Settings"
    /// permanently, whether or not they are running.
    pub cloaked: bool,
    /// Extended window styles.
    pub ex_style: u32,
    /// Whether `GetAncestor(hwnd, GA_ROOTOWNER)` returned the window itself.
    ///
    /// False for owned popups, dialogs' children and detached menus.
    pub is_root_owner: bool,
    /// Whether this is `GetShellWindow()`, the desktop's `Progman`.
    pub is_shell_window: bool,
    /// Window class name.
    pub class_name: String,
    /// Window title, already trimmed.
    pub title: String,
    /// Visible width in device pixels.
    pub width: i32,
    /// Visible height in device pixels.
    pub height: i32,
}

/// Window classes that are never worth showing a user.
///
/// Ordered roughly by how often they appear on a stock desktop.
const IGNORED_CLASSES: &[&str] = &[
    // The desktop itself and its wallpaper hosts.
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Windows.UI.Core.CoreWindow",
    // Task view, Alt-Tab and the window-switcher chrome.
    "MultitaskingViewFrame",
    "ForegroundStaging",
    "TaskListThumbnailWnd",
    "TaskListOverlayWnd",
    "Windows.Internal.Shell.TabProxyWindow",
    "XamlExplorerHostIslandWindow",
    // XAML islands hosted inside other applications.
    "Windows.UI.Composition.DesktopWindowContentBridge",
    "Xaml_WindowedPopupClass",
    // Transient shell furniture.
    "EdgeUiInputTopWndClass",
    "NativeHWNDHost",
    "Static",
    "tooltips_class32",
    "SysShadow",
    "DV2ControlHost",
    "Internet Explorer_Hidden",
    // Input method chrome.
    "IME",
    "MSCTFIME UI",
    "Default IME",
];

/// Whether a window class is shell furniture rather than an application window.
#[must_use]
pub fn is_ignored_class(class_name: &str) -> bool {
    IGNORED_CLASSES.contains(&class_name)
}

/// Why a window was excluded, for `tracing` and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// `IsWindowVisible` was false.
    NotVisible,
    /// Minimised, so there is nothing to capture.
    Minimized,
    /// DWM has cloaked it — usually a suspended UWP app.
    Cloaked,
    /// A tool window without `WS_EX_APPWINDOW`.
    ToolWindow,
    /// An owned popup rather than a top-level window.
    NotRootOwner,
    /// The desktop shell window.
    ShellWindow,
    /// A known shell-furniture class.
    IgnoredClass,
    /// No title, so nothing meaningful could be shown in a picker.
    Untitled,
    /// Smaller than [`MIN_WINDOW_EDGE`] on at least one axis.
    TooSmall,
}

/// Decides whether a window belongs in the picker.
///
/// Returns `Err(reason)` rather than `false` so the caller can log *why* a
/// window vanished; "my window is missing from the list" is otherwise an
/// unfalsifiable bug report.
///
/// # Errors
///
/// Returns the first [`Rejection`] that applies.
pub fn classify(facts: &WindowFacts) -> Result<(), Rejection> {
    if !facts.visible {
        return Err(Rejection::NotVisible);
    }
    if facts.minimized {
        return Err(Rejection::Minimized);
    }
    if facts.cloaked {
        return Err(Rejection::Cloaked);
    }
    if facts.is_shell_window {
        return Err(Rejection::ShellWindow);
    }
    if is_ignored_class(&facts.class_name) {
        return Err(Rejection::IgnoredClass);
    }
    if !facts.is_root_owner {
        return Err(Rejection::NotRootOwner);
    }
    // WS_EX_APPWINDOW is an explicit request to be treated as a real window and
    // outranks WS_EX_TOOLWINDOW; several Electron and Qt apps rely on exactly
    // this combination.
    let forced = facts.ex_style & WS_EX_APPWINDOW != 0;
    if !forced && facts.ex_style & WS_EX_TOOLWINDOW != 0 {
        return Err(Rejection::ToolWindow);
    }
    if facts.title.trim().is_empty() {
        return Err(Rejection::Untitled);
    }
    if facts.width < MIN_WINDOW_EDGE || facts.height < MIN_WINDOW_EDGE {
        return Err(Rejection::TooSmall);
    }
    Ok(())
}

/// Convenience predicate over [`classify`].
#[must_use]
pub fn is_capturable(facts: &WindowFacts) -> bool {
    classify(facts).is_ok()
}

/// Whether GDI `BitBlt` can be expected to produce anything but black.
///
/// A window created with `WS_EX_NOREDIRECTIONBITMAP` — every UWP/WinUI 3 window,
/// and anything drawing through a `DirectComposition` swap chain — has no
/// redirection surface for GDI to read. This is the honest statement of the
/// fallback's central limitation.
#[must_use]
pub fn gdi_can_capture(facts: &WindowFacts) -> bool {
    facts.ex_style & WS_EX_NOREDIRECTIONBITMAP == 0
}

/// A human-readable label for a monitor.
///
/// `GetMonitorInfoExW` reports `szDevice` as `\\.\DISPLAY1`, which is a
/// perfectly good stable identifier and a terrible thing to show someone. The
/// manufacturer's real name ("DELL U2720Q") is only available through
/// `QueryDisplayConfig`, which needs a Cargo feature this crate does not have
/// enabled, so this is the best label obtainable here: readable, stable, and
/// not pretending to be more than it is.
#[must_use]
pub fn display_label(device_name: &str, is_primary: bool) -> String {
    let bare = device_name.trim_start_matches("\\\\.\\");
    let number = bare.strip_prefix("DISPLAY").filter(|n| !n.is_empty());

    let base = match number {
        Some(n) => format!("Display {n}"),
        // An unexpected shape is shown verbatim rather than mangled.
        None if bare.is_empty() => "Display".to_string(),
        None => bare.to_string(),
    };

    if is_primary {
        format!("{base} (primary)")
    } else {
        base
    }
}
