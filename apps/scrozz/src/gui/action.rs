//! One vocabulary for three input sources.
//!
//! A hotkey, a tray menu item and a forwarded CLI invocation are three
//! different things arriving on three different threads, and all of them mean
//! "capture a region". Translating each into an [`Action`] at the edge means
//! [`crate::gui::App`] has one `match` rather than three, and — more usefully —
//! that the tray menu and the hotkey table cannot drift apart, because they
//! resolve through the same identifiers.
//!
//! Those identifiers are `scrozz-shell`'s: [`TrayAction::id`] promises they are
//! also the natural name for the corresponding hotkey action and CLI
//! subcommand, and that the three agree. This module is where that promise is
//! kept.

use crate::shortcuts::ShortcutAction;
use scrozz_core::{SelectionCapabilities, SelectionMode};
use scrozz_shell::{DisplayServer, Session, TrayAction};

/// What kind of still capture was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureKind {
    /// Open the mode HUD and choose after the overlay appears.
    AllInOne,
    /// Drag out a rectangle. Needs the selection overlay.
    Region,
    /// Pick a window. Needs the selection overlay.
    Window,
    /// The whole display under the pointer. Needs nothing.
    Fullscreen,
    /// Every connected display. Needs nothing.
    AllDisplays,
}

/// Where a live-app capture request entered the shared dispatcher.
///
/// This is diagnostic context only. It must never alter selector or capture
/// semantics; keeping it on the job makes route-specific lifecycle regressions
/// visible without forking the implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureOrigin {
    /// Clicked in the menu-bar/tray menu.
    MenuBar,
    /// Fired by a registered global shortcut.
    GlobalHotkey,
    /// Requested by the app's automated startup option.
    Startup,
    /// Called directly by an internal caller or test.
    Direct,
}

impl CaptureOrigin {
    /// Stable spelling used in diagnostic traces.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MenuBar => "menu-bar",
            Self::GlobalHotkey => "global-hotkey",
            Self::Startup => "startup",
            Self::Direct => "direct",
        }
    }
}

impl CaptureKind {
    /// Every kind, in menu order.
    pub const ALL: [Self; 5] = [
        Self::AllInOne,
        Self::Region,
        Self::Window,
        Self::Fullscreen,
        Self::AllDisplays,
    ];

    /// The stable identifier, shared with the tray and the hotkey table.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::AllInOne => "capture.all-in-one",
            Self::Region => "capture.region",
            Self::Window => "capture.window",
            Self::Fullscreen => "capture.fullscreen",
            Self::AllDisplays => "capture.all-displays",
        }
    }

    /// Whether choosing the target needs the on-screen selection overlay.
    ///
    /// The distinction is the whole reason fullscreen works before the overlay
    /// does: it is the one capture with nothing to choose, so it can run the
    /// moment the capture backend exists.
    #[must_use]
    pub const fn needs_selection(self) -> bool {
        matches!(self, Self::AllInOne | Self::Region | Self::Window)
    }

    /// Whether this capture can complete with the measured backend and selector.
    #[must_use]
    pub fn is_available(
        self,
        capabilities: SelectionCapabilities,
        session: &Session,
        capture_backend_ready: bool,
    ) -> bool {
        if !capture_backend_ready || session.server == DisplayServer::Headless {
            return false;
        }
        match self {
            Self::AllInOne => capabilities != SelectionCapabilities::NONE,
            Self::Region => capabilities.supports(SelectionMode::Region),
            Self::Window => capabilities.supports(SelectionMode::Window),
            Self::Fullscreen => true,
            Self::AllDisplays => !matches!(
                session.server,
                DisplayServer::Wayland | DisplayServer::Headless
            ),
        }
    }

    /// A short phrase for logs and card labels.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AllInOne => "all-in-one",
            Self::Region => "region",
            Self::Window => "window",
            Self::Fullscreen => "display",
            Self::AllDisplays => "all displays",
        }
    }
}

/// Something the user asked Scrozz to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Take a still capture.
    Capture(CaptureKind),
    /// Start or stop recording.
    ToggleRecording,
    /// Show previous captures.
    OpenHistory,
    /// Open settings.
    OpenSettings,
    /// Unlock every click-through pinned capture.
    UnlockPins,
    /// Quit.
    Quit,
}

impl Action {
    /// Every action, in tray-menu order.
    #[must_use]
    pub fn all() -> Vec<Self> {
        TrayAction::ALL.into_iter().map(Self::from_tray).collect()
    }

    /// The action a tray menu item means.
    #[must_use]
    pub const fn from_tray(action: TrayAction) -> Self {
        match action {
            TrayAction::CaptureAllInOne => Self::Capture(CaptureKind::AllInOne),
            TrayAction::CaptureRegion => Self::Capture(CaptureKind::Region),
            TrayAction::CaptureWindow => Self::Capture(CaptureKind::Window),
            TrayAction::CaptureFullscreen => Self::Capture(CaptureKind::Fullscreen),
            TrayAction::CaptureAllDisplays => Self::Capture(CaptureKind::AllDisplays),
            TrayAction::ToggleRecording => Self::ToggleRecording,
            TrayAction::OpenHistory => Self::OpenHistory,
            TrayAction::OpenSettings => Self::OpenSettings,
            TrayAction::UnlockPinnedCaptures => Self::UnlockPins,
            TrayAction::Quit => Self::Quit,
        }
    }

    /// The stable identifier.
    #[must_use]
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Capture(kind) => kind.id(),
            Self::ToggleRecording => "record.toggle",
            Self::OpenHistory => "history.open",
            Self::OpenSettings => "settings.open",
            Self::UnlockPins => "pins.unlock",
            Self::Quit => "app.quit",
        }
    }

    /// Resolves an identifier — a hotkey binding's action name, or a tray menu
    /// item id — back to an action.
    ///
    /// Returns `None` for ids that are not ours. That matters more than it
    /// looks: `muda`'s and `global-hotkey`'s event channels are process-global,
    /// so anything else in the process feeds the same queue, and silently
    /// treating a stranger's id as one of ours would fire a capture the user
    /// did not ask for.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::all().into_iter().find(|action| action.id() == id)
    }

    /// The CLI invocation equivalent to this action, when one exists.
    ///
    /// Used for the compositor config lines Scrozz generates on wlroots, where
    /// there is no global-shortcut portal and the user must bind the CLI
    /// themselves (D11). It is the same string in the settings pane and in the
    /// generated `sway` config, because it is produced here once.
    #[must_use]
    pub fn command_line(&self) -> Option<String> {
        let line = match self {
            Self::Capture(CaptureKind::AllInOne) => "scrozz capture --interactive all-in-one",
            Self::Capture(CaptureKind::Region) => "scrozz capture --interactive",
            Self::Capture(CaptureKind::Window) => "scrozz capture --interactive window",
            Self::Capture(CaptureKind::Fullscreen) => "scrozz capture --display active",
            Self::Capture(CaptureKind::AllDisplays) => "scrozz capture --all-displays",
            Self::ToggleRecording => "scrozz record --toggle",
            Self::OpenHistory => "scrozz history list",
            Self::OpenSettings => "scrozz settings show",
            // This must execute in the live GUI process. Pretending a second
            // `scrozz gui` invocation unlocks pins would create a false escape.
            Self::UnlockPins => return None,
            Self::Quit => "scrozz quit",
        };
        Some(line.to_owned())
    }

    /// Whether this action ends the process.
    #[must_use]
    pub const fn is_quit(&self) -> bool {
        matches!(self, Self::Quit)
    }

    /// Whether this action has an end-to-end implementation in the live app.
    #[must_use]
    pub fn is_available(
        self,
        capabilities: SelectionCapabilities,
        session: &Session,
        capture_backend_ready: bool,
    ) -> bool {
        match self {
            Self::Capture(kind) => kind.is_available(capabilities, session, capture_backend_ready),
            Self::Quit | Self::OpenHistory | Self::OpenSettings | Self::UnlockPins => true,
            Self::ToggleRecording => false,
        }
    }

    /// The bindable action this is, if a global shortcut can trigger it.
    ///
    /// `None` for quit, settings and history: they are one click away in the
    /// tray, and a system-wide key grab is a scarce resource taken from every
    /// other application on the machine.
    #[must_use]
    pub fn shortcut(self) -> Option<ShortcutAction> {
        ShortcutAction::from_id(self.id())
    }
}

/// The bridge back the other way.
///
/// Lives here rather than in [`crate::shortcuts`] so that all three vocabularies
/// — tray, action, shortcut — are reconciled in one file, and a new action
/// cannot be added to one of them without this module failing to compile.
impl ShortcutAction {
    /// What the dispatcher should do when this shortcut fires.
    #[must_use]
    pub const fn action(self) -> Action {
        match self {
            Self::CaptureAllInOne => Action::Capture(CaptureKind::AllInOne),
            Self::CaptureRegion => Action::Capture(CaptureKind::Region),
            Self::CaptureWindow => Action::Capture(CaptureKind::Window),
            Self::CaptureFullscreen => Action::Capture(CaptureKind::Fullscreen),
            Self::CaptureAllDisplays => Action::Capture(CaptureKind::AllDisplays),
            Self::ToggleRecording => Action::ToggleRecording,
        }
    }

    /// The tray item whose label should show this shortcut.
    #[must_use]
    pub const fn tray(self) -> TrayAction {
        match self {
            Self::CaptureAllInOne => TrayAction::CaptureAllInOne,
            Self::CaptureRegion => TrayAction::CaptureRegion,
            Self::CaptureWindow => TrayAction::CaptureWindow,
            Self::CaptureFullscreen => TrayAction::CaptureFullscreen,
            Self::CaptureAllDisplays => TrayAction::CaptureAllDisplays,
            Self::ToggleRecording => TrayAction::ToggleRecording,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tray_item_maps_to_an_action() {
        for tray in TrayAction::ALL {
            let action = Action::from_tray(tray);
            assert_eq!(
                action.id(),
                tray.id(),
                "the tray and the action vocabulary must agree on {tray:?}"
            );
        }
    }

    #[test]
    fn every_bindable_shortcut_names_a_real_action_and_a_real_tray_item() {
        // Three vocabularies, one set of identifiers. If they ever disagree, a
        // hotkey fires into nothing and a menu item shows the wrong keys.
        for shortcut in ShortcutAction::ALL {
            assert_eq!(
                shortcut.id(),
                shortcut.action().id(),
                "{shortcut:?} does not name its own action"
            );
            assert_eq!(
                shortcut.id(),
                shortcut.tray().id(),
                "{shortcut:?} does not name its own tray item"
            );
            assert_eq!(shortcut.action().shortcut(), Some(shortcut));
        }
    }

    #[test]
    fn shortcuts_hud_and_menu_share_the_approved_capture_copy() {
        use scrozz_core::product_copy;

        for (shortcut, mode, expected) in [
            (
                ShortcutAction::CaptureRegion,
                SelectionMode::Region,
                product_copy::CAPTURE_AREA,
            ),
            (
                ShortcutAction::CaptureWindow,
                SelectionMode::Window,
                product_copy::CAPTURE_WINDOW,
            ),
            (
                ShortcutAction::CaptureFullscreen,
                SelectionMode::Display,
                product_copy::CAPTURE_FULLSCREEN,
            ),
            (
                ShortcutAction::CaptureAllDisplays,
                SelectionMode::AllDisplays,
                product_copy::CAPTURE_ALL_DISPLAYS,
            ),
        ] {
            assert_eq!(shortcut.label(), expected);
            assert_eq!(shortcut.tray().label(), expected);
            assert_eq!(mode.label(), expected);
        }
        assert_eq!(
            ShortcutAction::CaptureAllInOne.label(),
            product_copy::ALL_IN_ONE
        );
        assert_eq!(
            ShortcutAction::ToggleRecording.label(),
            product_copy::RECORD_SCREEN
        );
    }

    #[test]
    fn the_actions_without_shortcuts_are_the_ones_reachable_from_the_tray() {
        for action in Action::all() {
            let bindable = action.shortcut().is_some();
            let expected = !matches!(
                action,
                Action::Quit | Action::OpenSettings | Action::OpenHistory | Action::UnlockPins
            );
            assert_eq!(
                bindable, expected,
                "{action:?} is on the wrong side of the bindable line"
            );
        }
    }

    #[test]
    fn capture_history_is_available_from_the_tray_without_a_global_shortcut() {
        assert!(Action::OpenHistory.is_available(
            SelectionCapabilities::NONE,
            &Session::detect(),
            false,
        ));
        assert!(Action::OpenHistory.shortcut().is_none());
    }

    #[test]
    fn an_identifier_round_trips() {
        for action in Action::all() {
            assert_eq!(Action::from_id(action.id()).as_ref(), Some(&action));
        }
    }

    #[test]
    fn a_foreign_identifier_is_not_ours() {
        // The process-global event channels mean this is a real case, not a
        // defensive one.
        assert_eq!(Action::from_id("com.other.app.doThing"), None);
        assert_eq!(Action::from_id(""), None);
        assert_eq!(Action::from_id("capture"), None);
    }

    #[test]
    fn the_actions_are_distinct() {
        let mut ids: Vec<&str> = Action::all().iter().map(|a| a.id()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two actions share an id");
    }

    #[test]
    fn only_the_pointing_captures_need_the_overlay() {
        assert!(CaptureKind::Region.needs_selection());
        assert!(CaptureKind::Window.needs_selection());
        assert!(CaptureKind::AllInOne.needs_selection());
        assert!(!CaptureKind::Fullscreen.needs_selection());
        assert!(!CaptureKind::AllDisplays.needs_selection());
    }

    #[test]
    fn capture_actions_require_both_backend_and_selector_capabilities() {
        let desktop = Session::from_env(None, None, None, Some(":0"));
        assert!(!Action::Capture(CaptureKind::Region).is_available(
            SelectionCapabilities::CLIENT_OVERLAY,
            &desktop,
            false,
        ));
        assert!(!Action::Capture(CaptureKind::Region).is_available(
            SelectionCapabilities::NONE,
            &desktop,
            true,
        ));
        assert!(Action::Capture(CaptureKind::Region).is_available(
            SelectionCapabilities::CLIENT_OVERLAY,
            &desktop,
            true,
        ));
    }

    #[test]
    fn wayland_never_advertises_unimplemented_all_display_composition() {
        let wayland = Session::from_env(Some("GNOME"), Some("wayland-0"), None, Some(":0"));
        assert!(!Action::Capture(CaptureKind::AllDisplays).is_available(
            SelectionCapabilities::CLIENT_OVERLAY,
            &wayland,
            true,
        ));
    }

    #[test]
    fn external_actions_generate_command_lines() {
        for action in Action::all() {
            if action == Action::UnlockPins {
                assert_eq!(action.command_line(), None);
            } else {
                let line = action.command_line().expect("external action");
                assert!(line.starts_with("scrozz "), "{line}");
            }
        }
    }

    #[test]
    fn quitting_is_the_only_action_that_ends_the_process() {
        let quitters: Vec<_> = Action::all().into_iter().filter(Action::is_quit).collect();
        assert_eq!(quitters, vec![Action::Quit]);
    }
}
