//! Truthful availability for the settings viewport.
//!
//! The persisted schema remains stable across machines. This matrix decides
//! which rows can be changed in the running build, based on whether a real
//! consumer exists and, for shortcuts, whether that action is available in the
//! current desktop session.

use crate::gui::action::{Action, CaptureKind};

const ADVANCED_EXPORT: &str =
    "Unavailable until the export pipeline applies this option to encoded images.";
const RECORDING: &str = "Unavailable until the recording runtime consumes this setting end to end.";
const OCR: &str = "Unavailable because this build has no OCR engine.";
const OCR_AUTO_DETECT: &str =
    "Unavailable because this platform's OCR engine cannot infer language from image content.";
const QUICK_ACCESS_POLICY: &str =
    "Unavailable until the capture stack implements this persistent policy.";
const ANNOTATION: &str =
    "Unavailable until the annotation editor consumes this display preference.";
const XWAYLAND: &str =
    "Unavailable until capture backend selection implements an XWayland fallback policy.";
const ACTION_UNAVAILABLE: &str =
    "Unavailable in this desktop session; Scrozz will not register this shortcut.";
const WINDOW_CAPTURE: &str =
    "Unavailable until window selection retains one capture backend from listing through capture.";

/// Whether one visible setting has an honest live or next-launch consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// The row may be edited.
    Enabled,
    /// The row stays visible but cannot be edited.
    Disabled(&'static str),
}

/// Runtime-dependent action availability captured when the window is built.
#[derive(Debug, Clone, Copy)]
pub struct SettingsSupport {
    region: bool,
    window: bool,
    display: bool,
    all_displays: bool,
    scrolling: bool,
    ocr: bool,
    ocr_auto_detect: bool,
}

impl SettingsSupport {
    /// Captures runtime action support without giving the UI model access to the
    /// app or shell layers.
    pub fn from_actions(mut available: impl FnMut(Action) -> bool) -> Self {
        Self {
            region: available(Action::Capture(CaptureKind::Region)),
            window: available(Action::Capture(CaptureKind::Window)),
            display: available(Action::Capture(CaptureKind::Fullscreen)),
            all_displays: available(Action::Capture(CaptureKind::AllDisplays)),
            scrolling: available(Action::Capture(CaptureKind::Scrolling)),
            ocr: crate::platform::ocr_available(),
            ocr_auto_detect: scrozz_ocr::SystemOcr::supports_automatic_language_detection(),
        }
    }

    /// Resolves one schema key.
    #[must_use]
    pub fn setting(self, key: &str) -> Support {
        match key {
            "capture.folder"
            | "capture.format"
            | "capture.quality"
            | "capture.cursor"
            | "capture.copy-to-clipboard"
            | "capture.window-shadow"
            | "capture.filename-template"
            | "capture.ask-for-filename"
            | "clipboard.mode"
            | "history.max-image-bytes"
            | "history.max-age-days"
            | "quick-access.size"
            | "quick-access.active-display"
            | "quick-access.auto-close-seconds"
            | "system.launch-at-login"
            | "system.url-scheme-enabled"
            | "system.tray-icon" => Support::Enabled,

            "hotkey.capture-region" => action_support(self.region),
            "hotkey.capture-window" if self.window => Support::Enabled,
            "hotkey.capture-window" => Support::Disabled(WINDOW_CAPTURE),
            "hotkey.capture-display" => action_support(self.display),
            "hotkey.capture-all-displays" => action_support(self.all_displays),
            "hotkey.capture-scrolling" => action_support(self.scrolling),

            "capture.retina-suffix"
            | "capture.convert-to-srgb"
            | "capture.border"
            | "capture.border-color" => Support::Disabled(ADVANCED_EXPORT),
            "record.fps"
            | "record.microphone"
            | "record.system-audio"
            | "record.countdown-seconds"
            | "record.dim-outside-selection"
            | "record.remember-selection"
            | "hotkey.record-start"
            | "hotkey.record-stop" => Support::Disabled(RECORDING),
            "ocr.languages" | "ocr.keep-line-breaks" | "ocr.detect-links" if self.ocr => {
                Support::Enabled
            }
            "ocr.auto-detect-language" if self.ocr_auto_detect => Support::Enabled,
            "ocr.auto-detect-language" if self.ocr => Support::Disabled(OCR_AUTO_DETECT),
            "ocr.languages"
            | "ocr.auto-detect-language"
            | "ocr.keep-line-breaks"
            | "ocr.detect-links" => Support::Disabled(OCR),
            "quick-access.enabled"
            | "quick-access.position"
            | "quick-access.close-after-drag"
            | "quick-access.save-on-close" => Support::Disabled(QUICK_ACCESS_POLICY),
            "annotate.show-color-names" => Support::Disabled(ANNOTATION),
            "system.xwayland" => Support::Disabled(XWAYLAND),
            _ => Support::Disabled("Unavailable because this setting has no runtime consumer."),
        }
    }
}

fn action_support(available: bool) -> Support {
    if available {
        Support::Enabled
    } else {
        Support::Disabled(ACTION_UNAVAILABLE)
    }
}

/// Whether a shortcut schema row represents a real action in some supported
/// runtime. Used by CLI conflict checks, which do not own a live GUI selector.
#[must_use]
pub fn actionable_shortcut(key: &str) -> bool {
    matches!(
        key,
        "hotkey.capture-region"
            | "hotkey.capture-display"
            | "hotkey.capture-all-displays"
            | "hotkey.capture-scrolling"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inert_schema_rows_stay_visible_but_disabled() {
        let support = SettingsSupport::from_actions(|_| true);
        assert!(matches!(
            support.setting("record.fps"),
            Support::Disabled(reason) if reason.contains("recording runtime")
        ));
        assert!(matches!(
            support.setting("quick-access.enabled"),
            Support::Disabled(reason) if reason.contains("capture stack")
        ));
        assert_eq!(support.setting("clipboard.mode"), Support::Enabled);
        assert_eq!(
            support.setting("capture.ask-for-filename"),
            Support::Enabled
        );
    }

    #[test]
    fn capture_shortcuts_follow_runtime_capabilities() {
        let support = SettingsSupport::from_actions(|action| {
            matches!(action, Action::Capture(CaptureKind::Scrolling))
        });
        assert_eq!(
            support.setting("hotkey.capture-region"),
            Support::Disabled(ACTION_UNAVAILABLE)
        );
        assert_eq!(
            support.setting("hotkey.capture-window"),
            Support::Disabled(WINDOW_CAPTURE)
        );
        assert_eq!(
            support.setting("hotkey.capture-scrolling"),
            Support::Enabled
        );
        assert!(!actionable_shortcut("hotkey.capture-window"));
    }
}
