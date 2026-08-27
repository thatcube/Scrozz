//! The menu-bar item on macOS, the system tray item on Windows and Linux.
//!
//! Per decision D27 Scrozz is **invisible at rest**: no window, no Dock icon, no
//! taskbar entry. This item and the global hotkey are the entire "the app is
//! running" surface, and between captures they are the only two ways to reach
//! it. That makes the tray item load-bearing in a way it is not for an ordinary
//! app — if it fails to appear, Scrozz is unreachable except from the CLI.
//!
//! # Being invisible is an explicit act on macOS
//!
//! An `NSApplication` defaults to `NSApplicationActivationPolicyRegular`, which
//! means a Dock icon and an application menu bar that replaces the frontmost
//! app's when Scrozz activates. Both are wrong here.
//! [`use_accessory_activation_policy`] switches to
//! `NSApplicationActivationPolicyAccessory`, which is what "menu-bar app" means
//! concretely. It must be called on the main thread, before or early in
//! `applicationDidFinishLaunching`.
//!
//! A shipped `.app` should *also* set `LSUIElement` in its `Info.plist`; the
//! runtime call covers `cargo run`, the CLI and tests, where there is no bundle.

use scrozz_core::{Error, Result, SelectionMode};
use tray_icon::menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::{
    hotkey::{DisplayServer, Session},
    selection::{SelectionIntegration, resolve_selection},
};

// ---------------------------------------------------------------------------
// The menu model
// ---------------------------------------------------------------------------

/// Everything reachable from the tray.
///
/// This is the whole menu, not a subset: with no window, anything absent here is
/// reachable only by hotkey or CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrayAction {
    /// Open the selector with every still-capture mode available.
    CaptureAllInOne,
    /// Drag out a region and capture it.
    CaptureRegion,
    /// Pick a window and capture it.
    CaptureWindow,
    /// Capture the display under the pointer.
    CaptureFullscreen,
    /// Capture every connected display.
    CaptureAllDisplays,
    /// Capture a scrolling page on the display under the pointer.
    CaptureScrolling,
    /// Start recording; becomes "Stop Recording" while recording runs.
    ToggleRecording,
    /// Bring back the most recently closed capture card.
    RestoreRecent,
    /// Temporarily hide or show the capture overlay.
    ToggleOverlay,
    /// Show previous captures.
    OpenHistory,
    /// Open settings — an ordinary, movable window, per D27.
    OpenSettings,
    /// Quit Scrozz.
    Quit,
}

impl TrayAction {
    /// Every action, in menu order.
    pub const ALL: [Self; 12] = [
        Self::CaptureAllInOne,
        Self::CaptureRegion,
        Self::CaptureWindow,
        Self::CaptureFullscreen,
        Self::CaptureAllDisplays,
        Self::CaptureScrolling,
        Self::ToggleRecording,
        Self::RestoreRecent,
        Self::ToggleOverlay,
        Self::OpenHistory,
        Self::OpenSettings,
        Self::Quit,
    ];

    /// The stable identifier used as the menu item id.
    ///
    /// Stable across releases because it is also the natural name for the
    /// corresponding hotkey action and CLI subcommand, and those three must
    /// agree.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::CaptureAllInOne => "capture.all-in-one",
            Self::CaptureRegion => "capture.region",
            Self::CaptureWindow => "capture.window",
            Self::CaptureFullscreen => "capture.fullscreen",
            Self::CaptureAllDisplays => "capture.all-displays",
            Self::CaptureScrolling => "capture.scrolling",
            Self::ToggleRecording => "record.toggle",
            Self::RestoreRecent => "capture.restore-recent",
            Self::ToggleOverlay => "overlay.toggle",
            Self::OpenHistory => "history.open",
            Self::OpenSettings => "settings.open",
            Self::Quit => "app.quit",
        }
    }

    /// The label shown in the menu, in its resting state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CaptureAllInOne => "All-in-One Capture…",
            Self::CaptureRegion => "Capture Region",
            Self::CaptureWindow => "Capture Window",
            Self::CaptureFullscreen => "Capture Fullscreen",
            Self::CaptureAllDisplays => "Capture All Displays",
            Self::CaptureScrolling => "Capture Scrolling Page",
            Self::ToggleRecording => "Start Recording",
            Self::RestoreRecent => "Restore Last Capture",
            Self::ToggleOverlay => "Hide/Show Capture Stack",
            Self::OpenHistory => "History…",
            Self::OpenSettings => "Settings…",
            Self::Quit => "Quit Scrozz",
        }
    }

    /// Resolves a menu item id back to its action.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }

    /// Whether this menu row has an end-to-end implementation in this build.
    ///
    /// An enabled item that only writes "not wired up yet" to a log the user
    /// cannot see is indistinguishable from a broken button. Keep unfinished
    /// rows visible so the intended product surface is legible, but disabled
    /// until they can actually fulfil the click.
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(
            self,
            Self::CaptureAllInOne
                | Self::CaptureRegion
                | Self::CaptureWindow
                | Self::CaptureFullscreen
                | Self::CaptureAllDisplays
                | Self::CaptureScrolling
                | Self::RestoreRecent
                | Self::ToggleOverlay
                | Self::OpenHistory
                | Self::OpenSettings
                | Self::Quit
        )
    }

    /// Whether this row can complete in the measured desktop session.
    #[must_use]
    pub fn is_available_for(self, session: &Session) -> bool {
        if !self.is_available() {
            return false;
        }
        match self {
            Self::CaptureAllInOne | Self::CaptureRegion | Self::CaptureWindow => {
                let plan = resolve_selection(SelectionIntegration::for_session(session));
                match self {
                    Self::CaptureAllInOne => plan.is_available(),
                    Self::CaptureRegion => plan.capabilities.supports(SelectionMode::Region),
                    Self::CaptureWindow => plan.capabilities.supports(SelectionMode::Window),
                    _ => unreachable!(),
                }
            }
            Self::CaptureFullscreen => session.server != DisplayServer::Headless,
            Self::CaptureAllDisplays => !matches!(
                session.server,
                DisplayServer::Wayland | DisplayServer::Headless
            ),
            Self::CaptureScrolling => session.server != DisplayServer::Headless,
            Self::RestoreRecent
            | Self::ToggleOverlay
            | Self::OpenHistory
            | Self::OpenSettings
            | Self::Quit => true,
            Self::ToggleRecording => false,
        }
    }
}

/// One row of the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEntry {
    /// A clickable action.
    Item(TrayAction),
    /// A divider.
    Separator,
}

/// The tray menu, in order.
///
/// Pure data, so the menu's shape is testable without a display server — which
/// matters, because building a real tray icon in a test would put something on
/// the developer's screen.
#[must_use]
pub const fn menu_model() -> &'static [TrayEntry] {
    &[
        TrayEntry::Item(TrayAction::CaptureAllInOne),
        TrayEntry::Item(TrayAction::CaptureRegion),
        TrayEntry::Item(TrayAction::CaptureWindow),
        TrayEntry::Item(TrayAction::CaptureFullscreen),
        TrayEntry::Item(TrayAction::CaptureAllDisplays),
        TrayEntry::Item(TrayAction::CaptureScrolling),
        TrayEntry::Separator,
        TrayEntry::Item(TrayAction::ToggleRecording),
        TrayEntry::Item(TrayAction::RestoreRecent),
        TrayEntry::Item(TrayAction::ToggleOverlay),
        TrayEntry::Separator,
        TrayEntry::Item(TrayAction::OpenHistory),
        TrayEntry::Item(TrayAction::OpenSettings),
        TrayEntry::Separator,
        TrayEntry::Item(TrayAction::Quit),
    ]
}

// ---------------------------------------------------------------------------
// The icon
// ---------------------------------------------------------------------------

/// Pixel dimensions of Brandon's menu-bar template.
///
/// `tray-icon` always displays this at 18 points on macOS, so this exact 36px
/// master maps 2:1 onto a Retina menu bar with no resampling.
const ICON_SIZE: u32 = 36;

/// Draws the Scrozz mark for the menu bar.
///
/// This is Brandon's exact 32px SVG mark — the same four crop corners, `zz`
/// eyes and smile as the app icon — drawn by Brandon specifically for the
/// 36×36 Retina menu-bar slot. The pre-rasterized bytes avoid adding an image
/// decoder to the shell crate.
///
/// RGB stays pure black and identity lives in alpha. That is exactly what macOS
/// wants from a template image: the system recolours it for light, dark and
/// highlighted menu bars. A purple menu-bar bitmap would look wrong in at least
/// one of those states.
#[must_use]
pub fn default_icon_rgba() -> Vec<u8> {
    include_bytes!("../assets/scrozz-menu-36.rgba").to_vec()
}

// ---------------------------------------------------------------------------
// macOS activation policy
// ---------------------------------------------------------------------------

/// Makes this process a menu-bar app: no Dock icon, no menu bar takeover.
///
/// A no-op with `Ok(())` off macOS, where tray apps have no equivalent
/// requirement.
///
/// # Errors
///
/// Returns [`Error::Platform`] if called off the main thread, or if AppKit
/// refused the change.
#[cfg(target_os = "macos")]
pub fn use_accessory_activation_policy() -> Result<()> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    let mtm = MainThreadMarker::new().ok_or_else(|| {
        Error::Platform(
            "the activation policy can only be set from the main thread; AppKit ignores it \
             elsewhere and the app keeps its Dock icon"
                .to_owned(),
        )
    })?;

    let app = NSApplication::sharedApplication(mtm);
    if app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        Ok(())
    } else {
        Err(Error::Platform(
            "AppKit refused NSApplicationActivationPolicyAccessory; set LSUIElement in the \
             bundle's Info.plist instead"
                .to_owned(),
        ))
    }
}

/// Makes this process a menu-bar app: no Dock icon, no menu bar takeover.
///
/// A no-op off macOS.
///
/// # Errors
///
/// Never fails on this platform.
#[cfg(not(target_os = "macos"))]
pub fn use_accessory_activation_policy() -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// The tray item
// ---------------------------------------------------------------------------

/// Scrozz's menu-bar / tray item.
///
/// # Threading
///
/// Must be created on the thread running the platform event loop, and on macOS
/// that must be the main thread with the loop **already running** — a tray icon
/// created before the loop starts misbehaves around fullscreen apps.
///
/// # Lifetime
///
/// The item is removed when this value is dropped. That is not incidental
/// tidiness: a tray item outliving its process is a stray artefact on the user's
/// screen, which D27 forbids.
pub struct Tray {
    icon: TrayIcon,
    /// Held so its label can be swapped between "Start" and "Stop Recording".
    record_item: MenuItem,
    /// Kept alive alongside the icon; dropping the menu unhooks the items.
    _menu: Menu,
}

impl Tray {
    /// Creates the tray item with Scrozz's menu and generated icon.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if the platform has no usable tray — a Linux
    /// session without a StatusNotifier host, most often — which is a
    /// degradation to report, not a crash: the hotkey and CLI still work.
    pub fn new() -> Result<Self> {
        Self::with_tooltip("Scrozz")
    }

    /// Creates the tray item with a custom tooltip.
    ///
    /// # Errors
    ///
    /// As [`Tray::new`].
    pub fn with_tooltip(tooltip: &str) -> Result<Self> {
        let session = Session::detect();
        Self::with_tooltip_and_availability(tooltip, |action| action.is_available_for(&session))
    }

    /// Creates the tray item using application-measured action readiness.
    ///
    /// This lets the app combine the shell's session facts with backend and
    /// selector readiness, so a visible row is never enabled merely because the
    /// desktop protocol could support it.
    ///
    /// # Errors
    ///
    /// As [`Tray::new`].
    pub fn with_tooltip_and_availability(
        tooltip: &str,
        mut is_available: impl FnMut(TrayAction) -> bool,
    ) -> Result<Self> {
        let menu = Menu::new();
        let mut record_item = None;

        for entry in menu_model() {
            match *entry {
                TrayEntry::Separator => {
                    let separator = PredefinedMenuItem::separator();
                    append(&menu, &separator)?;
                }
                TrayEntry::Item(action) => {
                    let item = MenuItem::with_id(
                        action.id(),
                        action.label(),
                        action.is_available() && is_available(action),
                        None,
                    );
                    append(&menu, &item)?;
                    if action == TrayAction::ToggleRecording {
                        record_item = Some(item);
                    }
                }
            }
        }

        let record_item = record_item.ok_or_else(|| {
            Error::Platform("the tray menu model is missing its recording item".to_owned())
        })?;

        let icon = Icon::from_rgba(default_icon_rgba(), ICON_SIZE, ICON_SIZE)
            .map_err(|err| Error::Platform(format!("could not build the tray icon: {err}")))?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_icon(icon)
            .with_icon_as_template(true)
            .with_tooltip(tooltip)
            // The menu is the whole interface. Opening it on either button
            // means the user never has to discover which one works.
            .with_menu_on_left_click(true)
            .with_menu_on_right_click(true)
            .build()
            .map_err(|err| {
                Error::Platform(format!(
                    "could not create the tray item: {err}. Scrozz is still reachable by \
                     hotkey and from the `scrozz` CLI."
                ))
            })?;

        Ok(Self {
            icon: tray,
            record_item,
            _menu: menu,
        })
    }

    /// Takes one pending menu activation, if there is one. Never blocks.
    ///
    /// Ids that are not ours are skipped, because `muda`'s event channel is
    /// process-global and shared with any other menu in the process.
    #[must_use]
    pub fn poll(&self) -> Option<TrayAction> {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(action) = TrayAction::from_id(event.id.as_ref()) {
                return Some(action);
            }
        }
        None
    }

    /// Delivers every pending menu activation to `handler`.
    ///
    /// Prefer this to [`MenuEvent::set_event_handler`], a process-global
    /// `OnceCell` whose first setter silently starves every other consumer.
    pub fn drain(&self, mut handler: impl FnMut(TrayAction)) {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(action) = TrayAction::from_id(event.id.as_ref()) {
                handler(action);
            }
        }
    }

    /// Switches the recording entry between "Start" and "Stop Recording".
    pub fn set_recording(&self, recording: bool) {
        self.record_item.set_text(recording_label(recording));
    }

    /// Shows or hides the item without destroying it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if the platform refused.
    pub fn set_visible(&self, visible: bool) -> Result<()> {
        self.icon
            .set_visible(visible)
            .map_err(|err| Error::Platform(format!("could not change tray visibility: {err}")))
    }

    /// Removes the item now, rather than at drop.
    pub fn close(self) {
        drop(self);
    }
}

/// The label the recording entry carries in each state.
#[must_use]
pub const fn recording_label(recording: bool) -> &'static str {
    if recording {
        "Stop Recording"
    } else {
        "Start Recording"
    }
}

fn append(menu: &Menu, item: &dyn IsMenuItem) -> Result<()> {
    menu.append(item)
        .map_err(|err| Error::Platform(format!("could not build the tray menu: {err}")))
}
