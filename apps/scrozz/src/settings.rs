//! The settings schema.
//!
//! # Why the schema lives here and not in the store
//!
//! The schema is deliberately separate from [`crate::settings_store`]. A key's
//! name, type and default are compile-time product decisions; the value currently
//! in force and where it came from are per-user state. Keeping those concerns
//! apart lets the CLI, GUI and migration code share validation without making a
//! schema lookup touch the filesystem.
//!
//! # Naming
//!
//! `area.key-name`: a dotted area, hyphenated words. It matches the action slugs
//! the hotkey commands already use, and it survives being a TOML table, a JSON
//! object path and a command-line argument without being quoted.

use scrozz_export::NameTemplate;

use crate::{
    fault::{CliError, CliResult},
    hotkey_config::Accelerator,
    json::Json,
};

/// The settings window section that owns a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    /// Capture output and image treatment.
    Capture,
    /// Clipboard behavior.
    Clipboard,
    /// Recording behavior.
    Recording,
    /// Global shortcuts.
    Shortcuts,
    /// Capture history retention.
    History,
    /// Text recognition.
    Ocr,
    /// Quick-access panel behavior.
    QuickAccess,
    /// Annotation behavior.
    Annotation,
    /// Desktop integration.
    System,
    /// Update checks.
    Updates,
    /// Onboarding state.
    Onboarding,
}

impl Section {
    /// The user-facing heading.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Capture => "Capture",
            Self::Clipboard => "Clipboard",
            Self::Recording => "Recording",
            Self::Shortcuts => "Shortcuts",
            Self::History => "History",
            Self::Ocr => "Text recognition",
            Self::QuickAccess => "Quick access",
            Self::Annotation => "Annotation",
            Self::System => "System",
            Self::Updates => "Updates",
            Self::Onboarding => "Getting started",
        }
    }
}

/// Where the current value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSource {
    /// The user settings document contains an override.
    User,
    /// The schema default is in force.
    Default,
}

impl ValueSource {
    /// The stable JSON token.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Default => "default",
        }
    }
}

/// What a setting accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `true` or `false`.
    Bool,
    /// A bounded integer.
    Int {
        /// Smallest accepted value.
        min: i64,
        /// Largest accepted value.
        max: i64,
    },
    /// A filesystem path. It need not exist yet.
    Path,
    /// Free-form text. Individual consumers may interpret an empty value.
    Text,
    /// A capture filename template.
    Template,
    /// One of a fixed set of strings.
    Choice(&'static [&'static str]),
    /// A key combination, validated by the CLI parser.
    Accelerator,
}

impl Kind {
    /// The type name reported in JSON.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int { .. } => "int",
            Self::Path => "path",
            Self::Text => "text",
            Self::Template => "template",
            Self::Choice(_) => "choice",
            Self::Accelerator => "accelerator",
        }
    }
}

/// One setting.
#[derive(Debug, Clone, Copy)]
pub struct Setting {
    /// The dotted key.
    pub key: &'static str,
    /// The settings window section.
    pub section: Section,
    /// The concise user-facing control label.
    pub label: &'static str,
    /// What it accepts.
    pub kind: Kind,
    /// The value in force when nothing has been set.
    pub default: &'static str,
    /// One line shown under the control.
    pub description: &'static str,
}

impl Setting {
    /// Declares one setting.
    #[must_use]
    pub const fn new(
        key: &'static str,
        section: Section,
        label: &'static str,
        kind: Kind,
        default: &'static str,
        description: &'static str,
    ) -> Self {
        Self {
            key,
            section,
            label,
            kind,
            default,
            description,
        }
    }

    /// The JSON representation of one resolved value.
    #[must_use]
    pub fn to_json(self, value: &str, source: ValueSource) -> Json {
        let mut fields = vec![
            ("key", Json::str(self.key)),
            ("type", Json::str(self.kind.slug())),
            ("value", Json::str(value)),
            ("default", Json::str(self.default)),
            ("source", Json::str(source.slug())),
            ("description", Json::str(self.description)),
        ];
        if let Kind::Choice(options) = self.kind {
            fields.push(("choices", Json::arr(options.iter().map(|o| Json::str(*o)))));
        }
        if let Kind::Int { min, max } = self.kind {
            fields.push(("minimum", Json::Int(min)));
            fields.push(("maximum", Json::Int(max)));
        }
        Json::Obj(
            fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }

    /// Checks a candidate value.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] or the underlying template validation error.
    pub fn validate(&self, value: &str) -> CliResult<()> {
        match self.kind {
            Kind::Bool => match value {
                "true" | "false" => Ok(()),
                other => Err(CliError::usage(format!(
                    "{} takes `true` or `false`, not {other:?}",
                    self.key
                ))),
            },
            Kind::Int { min, max } => {
                let parsed: i64 = value.parse().map_err(|_| {
                    CliError::usage(format!(
                        "{} takes a whole number between {min} and {max}, not {value:?}",
                        self.key
                    ))
                })?;
                if (min..=max).contains(&parsed) {
                    Ok(())
                } else {
                    Err(CliError::usage(format!(
                        "{} must be between {min} and {max}; {parsed} is outside that",
                        self.key
                    )))
                }
            }
            Kind::Path => {
                if value.trim().is_empty() {
                    Err(CliError::usage(format!("{} cannot be empty", self.key)))
                } else {
                    Ok(())
                }
            }
            Kind::Text => Ok(()),
            Kind::Template => NameTemplate::parse(value)
                .map(|_| ())
                .map_err(CliError::from),
            Kind::Choice(options) => {
                if options.contains(&value) {
                    Ok(())
                } else {
                    Err(CliError::usage(format!(
                        "{} takes one of {}, not {value:?}",
                        self.key,
                        options.join(", ")
                    )))
                }
            }
            Kind::Accelerator => Accelerator::parse(value).map(|_| ()),
        }
    }
}

use Section::{
    Annotation, Capture, Clipboard, History, Ocr, Onboarding, QuickAccess, Recording, Shortcuts,
    System,
};

const CAPTURE_REGION_HOTKEY: &str = if cfg!(target_os = "macos") {
    "Cmd+Shift+8"
} else {
    "Super+Shift+4"
};
const CAPTURE_WINDOW_HOTKEY: &str = if cfg!(target_os = "macos") {
    "Cmd+Shift+9"
} else {
    "Super+Shift+5"
};
const CAPTURE_DISPLAY_HOTKEY: &str = if cfg!(target_os = "macos") {
    "Cmd+Shift+7"
} else {
    "Super+Shift+3"
};
const CAPTURE_ALL_DISPLAYS_HOTKEY: &str = if cfg!(target_os = "macos") {
    "Cmd+Shift+0"
} else {
    "Super+Shift+6"
};
const RECORD_START_HOTKEY: &str = if cfg!(target_os = "macos") {
    "Cmd+Shift+R"
} else {
    "Super+Shift+R"
};
const RECORD_STOP_HOTKEY: &str = if cfg!(target_os = "macos") {
    "Cmd+Shift+Escape"
} else {
    "Super+Shift+Escape"
};

/// Every setting, in the order the CLI and settings window report them.
pub const SETTINGS: &[Setting] = &[
    Setting::new(
        "capture.folder",
        Capture,
        "Save captures to",
        Kind::Path,
        "~/Pictures/Scrozz",
        "Where captures are saved when no output path is given.",
    ),
    Setting::new(
        "capture.format",
        Capture,
        "Image format",
        Kind::Choice(&["png", "jpeg", "webp"]),
        "png",
        "Default image format.",
    ),
    Setting::new(
        "capture.quality",
        Capture,
        "Image quality",
        Kind::Int { min: 1, max: 100 },
        "90",
        "Encoder quality for lossy formats. Ignored for PNG.",
    ),
    Setting::new(
        "capture.cursor",
        Capture,
        "Include pointer",
        Kind::Bool,
        "false",
        "Include the mouse pointer in captures.",
    ),
    Setting::new(
        "capture.copy-to-clipboard",
        Capture,
        "Copy after capture",
        Kind::Bool,
        "false",
        "Also copy every capture to the clipboard.",
    ),
    Setting::new(
        "capture.window-shadow",
        Capture,
        "Include window shadow",
        Kind::Bool,
        "true",
        "Include the window's drop shadow in window captures.",
    ),
    Setting::new(
        "capture.filename-template",
        Capture,
        "Filename",
        Kind::Template,
        NameTemplate::DEFAULT,
        "Name new captures from date, time, window and sequence fields.",
    ),
    Setting::new(
        "capture.retina-suffix",
        Capture,
        "Add @2x suffix",
        Kind::Bool,
        "false",
        "Append @2x when a capture contains two pixels per point.",
    ),
    Setting::new(
        "capture.convert-to-srgb",
        Capture,
        "Convert to sRGB",
        Kind::Bool,
        "true",
        "Convert captured color into sRGB for consistent sharing.",
    ),
    Setting::new(
        "capture.border",
        Capture,
        "Add border",
        Kind::Bool,
        "false",
        "Draw a one-pixel border around exported captures.",
    ),
    Setting::new(
        "capture.border-color",
        Capture,
        "Border color",
        Kind::Text,
        "#000000",
        "Color used when an exported capture has a border.",
    ),
    Setting::new(
        "clipboard.mode",
        Clipboard,
        "Copy as",
        Kind::Choice(&["image", "file", "image-and-file"]),
        "image",
        "Choose whether the clipboard receives image data, a file, or both.",
    ),
    Setting::new(
        "record.fps",
        Recording,
        "Frame rate",
        Kind::Int { min: 1, max: 240 },
        "30",
        "Recording frame rate.",
    ),
    Setting::new(
        "record.microphone",
        Recording,
        "Microphone",
        Kind::Bool,
        "false",
        "Record microphone input.",
    ),
    Setting::new(
        "record.system-audio",
        Recording,
        "System audio",
        Kind::Bool,
        "false",
        "Record system audio output.",
    ),
    Setting::new(
        "record.countdown-seconds",
        Recording,
        "Countdown",
        Kind::Int { min: 0, max: 10 },
        "3",
        "Wait this many seconds before recording begins.",
    ),
    Setting::new(
        "record.dim-outside-selection",
        Recording,
        "Dim outside selection",
        Kind::Bool,
        "true",
        "Dim the desktop outside the selected recording area.",
    ),
    Setting::new(
        "record.remember-selection",
        Recording,
        "Remember selection",
        Kind::Bool,
        "false",
        "Reuse the previous recording area for the next recording.",
    ),
    Setting::new(
        "hotkey.capture-region",
        Shortcuts,
        "Capture region",
        Kind::Accelerator,
        CAPTURE_REGION_HOTKEY,
        "Hotkey for an interactive region capture.",
    ),
    Setting::new(
        "hotkey.capture-window",
        Shortcuts,
        "Capture window",
        Kind::Accelerator,
        CAPTURE_WINDOW_HOTKEY,
        "Hotkey for an interactive window capture.",
    ),
    Setting::new(
        "hotkey.capture-display",
        Shortcuts,
        "Capture active display",
        Kind::Accelerator,
        CAPTURE_DISPLAY_HOTKEY,
        "Hotkey for capturing the active display.",
    ),
    Setting::new(
        "hotkey.capture-all-displays",
        Shortcuts,
        "Capture all displays",
        Kind::Accelerator,
        CAPTURE_ALL_DISPLAYS_HOTKEY,
        "Hotkey for capturing every display.",
    ),
    Setting::new(
        "hotkey.record-start",
        Shortcuts,
        "Start recording",
        Kind::Accelerator,
        RECORD_START_HOTKEY,
        "Hotkey for starting a recording.",
    ),
    Setting::new(
        "hotkey.record-stop",
        Shortcuts,
        "Stop recording",
        Kind::Accelerator,
        RECORD_STOP_HOTKEY,
        "Hotkey for stopping a recording.",
    ),
    Setting::new(
        "history.max-image-bytes",
        History,
        "Storage limit",
        Kind::Int {
            min: 0,
            max: 1 << 53,
        },
        "10737418240",
        "Disk budget for stored source images. Pinned captures are never evicted.",
    ),
    Setting::new(
        "history.max-age-days",
        History,
        "Keep unpinned source images",
        Kind::Choice(&["0", "1", "3", "7", "30"]),
        "0",
        "Evict unpinned source images after this many days, or never when set to zero. Records, metadata, OCR, and edits remain available.",
    ),
    Setting::new(
        "ocr.languages",
        Ocr,
        "Languages",
        Kind::Text,
        "",
        "Comma-separated BCP-47 language tags, or empty to use system languages.",
    ),
    Setting::new(
        "ocr.keep-line-breaks",
        Ocr,
        "Keep line breaks",
        Kind::Bool,
        "true",
        "Preserve recognized line breaks when copying text.",
    ),
    Setting::new(
        "ocr.detect-links",
        Ocr,
        "Detect links",
        Kind::Bool,
        "true",
        "Make recognized web addresses available as links.",
    ),
    Setting::new(
        "quick-access.enabled",
        QuickAccess,
        "Show quick access",
        Kind::Bool,
        "true",
        "Show the quick-access capture panel.",
    ),
    Setting::new(
        "quick-access.position",
        QuickAccess,
        "Screen edge",
        Kind::Choice(&["left", "right", "top", "bottom"]),
        "right",
        "Anchor quick access to this edge of the active display.",
    ),
    Setting::new(
        "quick-access.size",
        QuickAccess,
        "Panel size",
        Kind::Int { min: 96, max: 512 },
        "192",
        "Set the quick-access panel size in logical pixels.",
    ),
    Setting::new(
        "quick-access.active-display",
        QuickAccess,
        "Follow active display",
        Kind::Bool,
        "true",
        "Move quick access to the display containing the pointer.",
    ),
    Setting::new(
        "quick-access.auto-close-seconds",
        QuickAccess,
        "Close after",
        Kind::Int { min: 0, max: 60 },
        "8",
        "Close quick access after this many idle seconds, or never when set to zero.",
    ),
    Setting::new(
        "quick-access.close-after-drag",
        QuickAccess,
        "Close after drag",
        Kind::Bool,
        "true",
        "Close quick access after dragging a capture out.",
    ),
    Setting::new(
        "quick-access.save-on-close",
        QuickAccess,
        "Save on close",
        Kind::Bool,
        "true",
        "Save unsaved captures when quick access closes.",
    ),
    Setting::new(
        "annotate.show-color-names",
        Annotation,
        "Show color names",
        Kind::Bool,
        "true",
        "Show readable names beside annotation colors.",
    ),
    Setting::new(
        "system.launch-at-login",
        System,
        "Launch at login",
        Kind::Bool,
        "false",
        "Start Scrozz when the desktop session begins.",
    ),
    Setting::new(
        "system.url-scheme-enabled",
        System,
        "Open scrozz links",
        Kind::Bool,
        "false",
        "Allow registered scrozz links to trigger fixed, allow-listed actions.",
    ),
    Setting::new(
        "system.xwayland",
        System,
        "Allow XWayland fallback",
        Kind::Bool,
        "false",
        "Allow XWayland integration when native Wayland support is unavailable.",
    ),
    Setting::new(
        "system.tray-icon",
        System,
        "Show menu-bar or tray icon",
        Kind::Bool,
        "true",
        "Keep Scrozz available from the menu bar or system tray.",
    ),
    Setting::new(
        "onboarding.completed",
        Onboarding,
        "Getting started completed",
        Kind::Bool,
        "false",
        "Remember whether the getting-started guide has been completed or skipped.",
    ),
    Setting::new(
        "onboarding.version",
        Onboarding,
        "Getting started version",
        Kind::Int { min: 0, max: 1000 },
        "0",
        "Remember the version of the getting-started guide last completed.",
    ),
];

/// Looks up a setting.
///
/// # Errors
///
/// Returns [`CliError::Usage`] naming the closest match, if there is one.
pub fn lookup(key: &str) -> CliResult<&'static Setting> {
    if let Some(setting) = SETTINGS.iter().find(|setting| setting.key == key) {
        return Ok(setting);
    }
    let suggestion = closest(key);
    Err(CliError::usage(match suggestion {
        Some(near) => format!("unknown setting {key:?}; did you mean {near:?}?"),
        None => format!("unknown setting {key:?}; run `scrozz settings get` to list every key"),
    }))
}

fn closest(key: &str) -> Option<&'static str> {
    let lower = key.to_ascii_lowercase();
    SETTINGS
        .iter()
        .map(|setting| (setting.key, distance(&lower, setting.key)))
        .filter(|(_, distance)| *distance <= 2)
        .min_by_key(|(_, distance)| *distance)
        .map(|(key, _)| key)
}

fn distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b_chars.len()).collect();
    for (i, ac) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, bc) in b_chars.iter().enumerate() {
            let insert_or_delete = row[j + 1].min(row[j]) + 1;
            let substitute = previous + usize::from(ac != *bc);
            previous = row[j + 1];
            row[j + 1] = insert_or_delete.min(substitute);
        }
    }
    row[b_chars.len()]
}

/// Every default setting as JSON.
#[must_use]
pub fn all_json() -> Json {
    Json::arr(
        SETTINGS
            .iter()
            .copied()
            .map(|setting| setting.to_json(setting.default, ValueSource::Default)),
    )
}

/// Every default setting as aligned text.
#[must_use]
pub fn all_human() -> String {
    let width = SETTINGS
        .iter()
        .map(|setting| setting.key.len())
        .max()
        .unwrap_or(0);
    SETTINGS
        .iter()
        .map(|setting| format!("{:width$}  {}", setting.key, setting.default))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_unique() {
        let mut keys: Vec<&str> = SETTINGS.iter().map(|setting| setting.key).collect();
        let count = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), count, "a settings key is declared twice");
    }

    #[test]
    fn every_key_follows_the_naming_rule() {
        for setting in SETTINGS {
            assert!(
                setting.key.contains('.'),
                "{} has no area prefix",
                setting.key
            );
            assert!(
                setting
                    .key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || matches!(c, '.' | '-')),
                "{} is not lowercase-dotted-hyphenated",
                setting.key
            );
            assert!(
                !setting.key.contains('_'),
                "{} uses an underscore",
                setting.key
            );
        }
    }

    #[test]
    fn every_default_validates_against_its_own_type() {
        for setting in SETTINGS {
            setting
                .validate(setting.default)
                .unwrap_or_else(|error| panic!("{} has an invalid default: {error}", setting.key));
        }
    }

    #[test]
    fn every_setting_has_ui_copy() {
        for setting in SETTINGS {
            assert!(!setting.label.is_empty(), "{}", setting.key);
            assert!(!setting.description.is_empty(), "{}", setting.key);
            assert!(
                setting.description.ends_with('.'),
                "{} reads as a fragment",
                setting.key
            );
            assert!(!setting.section.title().is_empty(), "{}", setting.key);
        }
    }

    #[test]
    fn hotkey_defaults_are_safe_for_the_desktop_platform() {
        for action in crate::cli::HotkeyAction::all() {
            let key = format!("hotkey.{}", action.slug());
            let setting = lookup(&key).unwrap_or_else(|_| panic!("{key} is missing"));
            let accelerator = scrozz_shell::Accelerator::parse(setting.default).unwrap();
            assert!(
                accelerator.system_owner().is_none(),
                "{} is reserved by the platform",
                setting.default
            );
            if !cfg!(target_os = "macos") {
                assert_eq!(
                    setting.default,
                    action.default_accelerator(),
                    "{key} disagrees with the compositor binding default"
                );
            }
        }
    }

    #[test]
    fn the_retention_default_matches_the_core_policy() {
        let setting = lookup("history.max-image-bytes").unwrap();
        assert_eq!(
            setting.default.parse::<u64>().unwrap(),
            scrozz_store::RetentionPolicy::default().max_image_bytes
        );

        let age = lookup("history.max-image-age").unwrap();
        assert_eq!(
            scrozz_store::RetentionWindow::from_token(age.default).unwrap(),
            scrozz_store::RetentionPolicy::default().max_image_age
        );
        let Kind::Choice(options) = age.kind else {
            panic!("history.max-image-age should be a choice")
        };
        assert_eq!(
            options,
            scrozz_store::RetentionWindow::all()
                .iter()
                .map(|window| window.as_token())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_format_choices_match_the_cli() {
        let setting = lookup("capture.format").unwrap();
        let Kind::Choice(options) = setting.kind else {
            panic!("capture.format should be a choice")
        };
        assert_eq!(options, ["png", "jpeg", "webp"]);
    }

    #[test]
    fn booleans_accept_only_true_and_false() {
        let setting = lookup("capture.cursor").unwrap();
        assert!(setting.validate("true").is_ok());
        assert!(setting.validate("false").is_ok());
        for bad in ["yes", "1", "TRUE", "True", ""] {
            assert!(setting.validate(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn integers_are_bounds_checked() {
        let setting = lookup("capture.quality").unwrap();
        assert!(setting.validate("1").is_ok());
        assert!(setting.validate("100").is_ok());
        for bad in ["0", "101", "-5", "90.5", "lots"] {
            assert!(setting.validate(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn free_text_can_be_empty_for_system_language_detection() {
        let setting = lookup("ocr.languages").unwrap();
        assert!(setting.validate("").is_ok());
        assert!(setting.validate("en-US,fr-FR").is_ok());
    }

    #[test]
    fn templates_use_the_export_parser() {
        let setting = lookup("capture.filename-template").unwrap();
        assert!(setting.validate("Capture {date} {seq}").is_ok());
        assert!(setting.validate("Capture {titel}").is_err());
        assert!(setting.validate("Capture {date").is_err());
    }

    #[test]
    fn a_bad_choice_lists_the_good_ones() {
        let error = lookup("capture.format")
            .unwrap()
            .validate("gif")
            .unwrap_err();
        let message = error.to_string();
        for option in ["png", "jpeg", "webp"] {
            assert!(message.contains(option), "{message}");
        }
    }

    #[test]
    fn accelerator_settings_use_the_real_parser() {
        let setting = lookup("hotkey.capture-region").unwrap();
        assert!(setting.validate("Ctrl+Alt+P").is_ok());
        assert!(setting.validate("Print").is_ok());
        assert!(setting.validate("Super+Nonsense").is_err());
        assert!(setting.validate("Super").is_err());
    }

    #[test]
    fn a_path_setting_rejects_only_emptiness() {
        let setting = lookup("capture.folder").unwrap();
        assert!(setting.validate("/anywhere/at/all").is_ok());
        assert!(setting.validate("/Volumes/Archive/Shots").is_ok());
        assert!(setting.validate("").is_err());
        assert!(setting.validate("   ").is_err());
    }

    #[test]
    fn an_unknown_key_suggests_the_near_miss() {
        let error = lookup("capture.formats").unwrap_err();
        assert!(error.to_string().contains("capture.format"), "{error}");
        let error = lookup("capture.qualtiy").unwrap_err();
        assert!(error.to_string().contains("capture.quality"), "{error}");
    }

    #[test]
    fn a_wildly_wrong_key_points_at_the_listing_instead() {
        let error = lookup("bananas").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("scrozz settings get"), "{message}");
        assert!(!message.contains("did you mean"), "{message}");
    }

    #[test]
    fn an_unknown_key_is_a_usage_error_not_a_crash() {
        assert_eq!(
            lookup("nope.nope").unwrap_err().exit(),
            crate::exit::Exit::Usage
        );
    }

    #[test]
    fn levenshtein_behaves() {
        assert_eq!(distance("", ""), 0);
        assert_eq!(distance("abc", "abc"), 0);
        assert_eq!(distance("abc", "abd"), 1);
        assert_eq!(distance("abc", "ab"), 1);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("kitten", "sitting"), 3);
    }

    #[test]
    fn the_json_listing_carries_the_type_and_constraints() {
        let Json::Arr(items) = all_json() else {
            panic!("expected an array")
        };
        assert_eq!(items.len(), SETTINGS.len());

        let rendered = lookup("capture.format")
            .unwrap()
            .to_json("webp", ValueSource::User)
            .to_compact_string();
        assert!(rendered.contains(r#""type":"choice""#), "{rendered}");
        assert!(
            rendered.contains(r#""choices":["png","jpeg","webp"]"#),
            "{rendered}"
        );

        let rendered = lookup("record.fps")
            .unwrap()
            .to_json("60", ValueSource::User)
            .to_compact_string();
        assert!(rendered.contains(r#""minimum":1"#), "{rendered}");
        assert!(rendered.contains(r#""maximum":240"#), "{rendered}");
    }

    #[test]
    fn the_json_shape_starts_with_the_same_six_keys_for_every_setting() {
        for setting in SETTINGS {
            let Json::Obj(pairs) = setting.to_json(setting.default, ValueSource::Default) else {
                panic!("expected an object")
            };
            let keys: Vec<&str> = pairs.iter().take(6).map(|(key, _)| key.as_str()).collect();
            assert_eq!(
                keys,
                ["key", "type", "value", "default", "source", "description"],
                "{}",
                setting.key
            );
        }
    }

    #[test]
    fn resolved_json_reports_the_supplied_source_deliberately() {
        let setting = lookup("capture.format").unwrap();
        let default = setting
            .to_json(setting.default, ValueSource::Default)
            .to_compact_string();
        let user = setting
            .to_json("webp", ValueSource::User)
            .to_compact_string();
        assert!(default.contains(r#""source":"default""#), "{default}");
        assert!(user.contains(r#""source":"user""#), "{user}");
        assert!(user.contains(r#""value":"webp""#), "{user}");
    }

    #[test]
    fn the_human_listing_names_every_key() {
        let text = all_human();
        for setting in SETTINGS {
            assert!(text.contains(setting.key), "{}", setting.key);
        }
        assert_eq!(text.lines().count(), SETTINGS.len());
    }
}
