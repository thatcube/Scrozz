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

use scrozz_shell::TrayAction;

/// What kind of still capture was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureKind {
    /// Drag out a rectangle. Needs the selection overlay.
    Region,
    /// Pick a window. Needs the selection overlay.
    Window,
    /// The whole display under the pointer. Needs nothing.
    Fullscreen,
}

impl CaptureKind {
    /// Every kind, in menu order.
    pub const ALL: [Self; 3] = [Self::Region, Self::Window, Self::Fullscreen];

    /// The stable identifier, shared with the tray and the hotkey table.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Region => "capture.region",
            Self::Window => "capture.window",
            Self::Fullscreen => "capture.fullscreen",
        }
    }

    /// Whether choosing the target needs the on-screen selection overlay.
    ///
    /// The distinction is the whole reason fullscreen works before the overlay
    /// does: it is the one capture with nothing to choose, so it can run the
    /// moment the capture backend exists.
    #[must_use]
    pub const fn needs_selection(self) -> bool {
        matches!(self, Self::Region | Self::Window)
    }

    /// A short phrase for logs and card labels.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Region => "region",
            Self::Window => "window",
            Self::Fullscreen => "display",
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
            TrayAction::CaptureRegion => Self::Capture(CaptureKind::Region),
            TrayAction::CaptureWindow => Self::Capture(CaptureKind::Window),
            TrayAction::CaptureFullscreen => Self::Capture(CaptureKind::Fullscreen),
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
            Self::Capture(CaptureKind::Region) => "scrozz capture --interactive",
            Self::Capture(CaptureKind::Window) => "scrozz capture --interactive-window",
            Self::Capture(CaptureKind::Fullscreen) => "scrozz capture --display active",
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
        assert!(!CaptureKind::Fullscreen.needs_selection());
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
