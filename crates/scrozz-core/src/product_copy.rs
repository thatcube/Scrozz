//! Canonical user-facing product vocabulary.
//!
//! Stable identifiers, CLI values, persisted setting keys, and IPC names live
//! with their owning types. These strings are presentation copy: keeping them
//! here lets menus, shortcut settings, selector HUDs, accessibility, and help
//! use the same terms without coupling those surfaces to one another.

/// Opens the unified capture-mode chooser.
pub const ALL_IN_ONE: &str = "All-in-One…";
/// Captures a rectangle selected on screen.
pub const CAPTURE_AREA: &str = "Capture Area";
/// Reuses the last captured area.
pub const CAPTURE_PREVIOUS_AREA: &str = "Capture Previous Area";
/// Captures the display under the pointer.
pub const CAPTURE_FULLSCREEN: &str = "Capture Fullscreen";
/// Captures a selected application window.
pub const CAPTURE_WINDOW: &str = "Capture Window";
/// Captures every connected display.
pub const CAPTURE_ALL_DISPLAYS: &str = "Capture All Displays";
/// Captures content longer than one viewport.
pub const SCROLLING_CAPTURE: &str = "Scrolling Capture";
/// Delays a capture.
pub const SELF_TIMER: &str = "Self-Timer";
/// Recognises text from the screen or an image.
pub const CAPTURE_TEXT_OCR: &str = "Capture Text (OCR)";
/// Records screen video.
pub const RECORD_SCREEN: &str = "Record Screen";
/// Hides desktop icons for a clean capture.
pub const HIDE_DESKTOP_ICONS: &str = "Hide Desktop Icons";
/// Opens an image file.
pub const OPEN_IMAGE: &str = "Open Image…";
/// Opens image data currently on the clipboard.
pub const OPEN_FROM_CLIPBOARD: &str = "Open from Clipboard";
/// Pins a capture in a floating window.
pub const PIN_TO_SCREEN: &str = "Pin to Screen…";
/// Opens the capture history window.
pub const CAPTURE_HISTORY: &str = "Capture History…";
/// Names the always-above corner surface containing the latest captures.
pub const RECENT_CAPTURES_OVERLAY: &str = "Recent Captures Overlay";
/// Opens native application information.
pub const ABOUT_SCROZZ: &str = "About Scrozz";
/// Opens application settings.
pub const SETTINGS: &str = "Settings…";
/// Quits the application.
pub const QUIT_SCROZZ: &str = "Quit Scrozz";

/// Settings description for the All-in-One shortcut.
pub const SHORTCUT_ALL_IN_ONE: &str = "Uses this shortcut to open All-in-One…";
/// Settings description for Capture Area.
pub const SHORTCUT_CAPTURE_AREA: &str = "Uses this shortcut for Capture Area.";
/// Settings description for Capture Window.
pub const SHORTCUT_CAPTURE_WINDOW: &str = "Uses this shortcut for Capture Window.";
/// Settings description for Capture Fullscreen.
pub const SHORTCUT_CAPTURE_FULLSCREEN: &str = "Uses this shortcut for Capture Fullscreen.";
/// Settings description for Capture All Displays.
pub const SHORTCUT_CAPTURE_ALL_DISPLAYS: &str = "Uses this shortcut for Capture All Displays.";
/// Settings description for Scrolling Capture.
pub const SHORTCUT_SCROLLING_CAPTURE: &str = "Uses this shortcut for Scrolling Capture.";
/// Settings description for Record Screen.
pub const SHORTCUT_RECORD_SCREEN: &str = "Uses this shortcut for Record Screen.";

/// The approved vocabulary, in product-menu order.
pub const APPROVED_COMMANDS: [&str; 19] = [
    ALL_IN_ONE,
    CAPTURE_AREA,
    CAPTURE_PREVIOUS_AREA,
    CAPTURE_FULLSCREEN,
    CAPTURE_WINDOW,
    CAPTURE_ALL_DISPLAYS,
    SCROLLING_CAPTURE,
    SELF_TIMER,
    CAPTURE_TEXT_OCR,
    RECORD_SCREEN,
    HIDE_DESKTOP_ICONS,
    OPEN_IMAGE,
    OPEN_FROM_CLIPBOARD,
    PIN_TO_SCREEN,
    CAPTURE_HISTORY,
    RECENT_CAPTURES_OVERLAY,
    ABOUT_SCROZZ,
    SETTINGS,
    QUIT_SCROZZ,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_vocabulary_stays_unique_and_action_led() {
        let mut unique = APPROVED_COMMANDS.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), APPROVED_COMMANDS.len());

        for command in APPROVED_COMMANDS {
            assert!(!command.contains("Region"), "{command}");
        }
        assert_eq!(ABOUT_SCROZZ, "About Scrozz");
    }
}
