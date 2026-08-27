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

use scrozz_core::{Error, Result};
use tray_icon::menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

// ---------------------------------------------------------------------------
// The menu model
// ---------------------------------------------------------------------------

/// Everything reachable from the tray.
///
/// This is the whole menu, not a subset: with no window, anything absent here is
/// reachable only by hotkey or CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrayAction {
    /// Drag out a region and capture it.
    CaptureRegion,
    /// Pick a window and capture it.
    CaptureWindow,
    /// Capture the display under the pointer.
    CaptureFullscreen,
    /// Start recording; becomes "Stop Recording" while recording runs.
    ToggleRecording,
    /// Show previous captures.
    OpenHistory,
    /// Open settings — an ordinary, movable window, per D27.
    OpenSettings,
    /// Quit Scrozz.
    Quit,
}

impl TrayAction {
    /// Every action, in menu order.
    pub const ALL: [Self; 7] = [
        Self::CaptureRegion,
        Self::CaptureWindow,
        Self::CaptureFullscreen,
        Self::ToggleRecording,
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
            Self::CaptureRegion => "capture.region",
            Self::CaptureWindow => "capture.window",
            Self::CaptureFullscreen => "capture.fullscreen",
            Self::ToggleRecording => "record.toggle",
            Self::OpenHistory => "history.open",
            Self::OpenSettings => "settings.open",
            Self::Quit => "app.quit",
        }
    }

    /// The label shown in the menu, in its resting state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CaptureRegion => "Capture Region",
            Self::CaptureWindow => "Capture Window",
            Self::CaptureFullscreen => "Capture Fullscreen",
            Self::ToggleRecording => "Start Recording",
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
        TrayEntry::Item(TrayAction::CaptureRegion),
        TrayEntry::Item(TrayAction::CaptureWindow),
        TrayEntry::Item(TrayAction::CaptureFullscreen),
        TrayEntry::Separator,
        TrayEntry::Item(TrayAction::ToggleRecording),
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

/// The dimension of the generated menu-bar glyph, in pixels.
const ICON_SIZE: u32 = 22;

/// Draws Scrozz's menu-bar glyph: a camera body with a lens.
///
/// Generated rather than shipped as a file so the crate has no asset
/// dependency and the tray works from a bare `cargo run`. It is drawn in pure
/// black with a shaped alpha channel, which is exactly what macOS wants from a
/// template image — the system recolours it for light, dark and highlighted
/// menu bars, and a coloured icon there looks broken.
#[must_use]
pub fn default_icon_rgba() -> Vec<u8> {
    let size = ICON_SIZE as f32;
    let mut rgba = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];

    let centre = size / 2.0;
    let lens_outer = size * 0.26;
    let lens_inner = size * 0.16;
    let body_half_w = size * 0.44;
    let body_half_h = size * 0.34;
    let border = size * 0.09;

    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            #[allow(clippy::cast_precision_loss)]
            let (px, py) = (x as f32 + 0.5 - centre, y as f32 + 0.5 - centre);

            // Camera body: a rounded rectangle outline, with a small hump for
            // the viewfinder on its top-left.
            let in_body = px.abs() <= body_half_w && py.abs() <= body_half_h;
            let in_body_hole = px.abs() <= body_half_w - border && py.abs() <= body_half_h - border;
            let hump = (-body_half_w * 0.75..=-body_half_w * 0.15).contains(&px)
                && (-body_half_h - border * 1.2..=-body_half_h).contains(&py);

            let distance = px.hypot(py);
            let in_lens = distance <= lens_outer && distance >= lens_inner;

            let ink = (in_body && !in_body_hole) || hump || in_lens;
            if ink {
                let index = ((y * ICON_SIZE + x) * 4) as usize;
                rgba[index + 3] = 255;
            }
        }
    }

    rgba
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
        let menu = Menu::new();
        let mut record_item = None;

        for entry in menu_model() {
            match *entry {
                TrayEntry::Separator => {
                    let separator = PredefinedMenuItem::separator();
                    append(&menu, &separator)?;
                }
                TrayEntry::Item(action) => {
                    let item = MenuItem::with_id(action.id(), action.label(), true, None);
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
